// Owns the island workspace state: tools, brushes, selection, both viewports
// and their GPU scenes, and input. The document lives in `doc`, the panes in
// `panels`, the island's look in `crate::terrain`.
//
// Terrain draws as one mesh per heightfield tile; a dab rebuilds only the
// tiles it touched and every other tile keeps its GPU buffers. Props draw as
// markers until model (.rdm) decoding exists.

/// Problems as a status line names them: "Start island without a harbour
/// coast; 3 × Props whose model the install lacks".
pub(crate) fn said(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(Problem::said)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod baselines;
mod blockers;
pub mod catalog;
mod cliffs;
pub mod convert;
mod doc;
pub use anno_core::fertility;
#[cfg(test)]
mod gpu_bench;
mod grids;
mod harbour;
mod import;
#[cfg(test)]
mod island_shots;
#[cfg(test)]
mod load_bench;
pub(crate) mod materials;
pub(crate) mod models;
mod motion;
#[cfg(test)]
mod motion_tests;
mod new;
mod panels;
mod placements;
pub mod port;
mod rail;
mod river;
mod scatter;
mod sound;
#[cfg(test)]
mod test_support;
mod tint;
mod vegetation;
// The Model workspace's island skies light as islands do.
pub(crate) use vegetation::NO_RT_AMBIENT;
#[cfg(test)]
mod water_shots;
mod wizard;
pub(crate) use anno_core::game::IslandKind;
pub(crate) use panels::{fold_summary, island_title, preset_name, pretty, selection_status};
mod worldmachine;

use anno_core::ambient::{self, Ambient, Baked, SunMoon};
use anno_core::game::{IslandRules, Module, Refs, Use};
use anno_core::harbour::Piece;
use anno_core::island::Problem;
pub use anno_core::island::{Item, Native, changed_samples};
pub use doc::island_data_dir;
use doc::{Doc, MaterialChange, Materials, Paint, Prop};
pub use import::Import;
pub use new::{Skeleton, Spec, Start, file_name, island_path, islands, skeletons};
pub use wizard::Wizard;
pub use worldmachine::{Link as WmLink, Previews as WmPreviews};

use crate::{
    project::{Listing, Project},
    terrain,
};
use anno_core::island::landscape::{self, Landscape};
use anno_core::island::objects;
use anno_formats::{
    a7m::{
        grids::{Grids, Kind},
        island::Island,
        tint::TintMap,
    },
    rda,
    vfs::Install,
};
use anno_render::a7m::Splat;
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    camera::{Camera, Extent},
    field::{EditEvent, Filter, TextEdit},
    gizmo::{self, Gesture, Marquee},
    gpu::{
        self, Batch, Fog, Instances, Light, Marker, Mesh, MeshData, Occlusion, Renderer, Scene,
        Sea, Shape, Sky, Tone, Viewport,
    },
    gpui::{prelude::*, *},
    heightfield::{Brush, Falloff, Heightfield, Mode, Rect, Sculpt, Stroke, files},
    history::History,
    icon::Icon,
    math::{self, Mat4, Vec3},
    paint,
    texture::{Dds, Texture},
    theme,
    tree::Tree,
    widgets::{self, Tooltip, button, info_row, section},
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Raise,
    Lower,
    Smooth,
    Flatten,
    Fill,
    Scrape,
    Noise,
    Paint,
    Place,
    Grid,
    Scatter,
    Tint,
    Harbour,
    Slot,
    River,
}

/// How the buildable overlay draws a ground point: unbuildable land hatched
/// red, the edge of buildable land bright, harbour pads teal, buildable land
/// left as it is so the ground still reads.
fn buildable_overlay(layer: &anno_formats::a7m::grids::Layer, p: Vec3) -> Option<([f32; 3], f32)> {
    let rgb = |c: u32| [(c >> 16) & 255, (c >> 8) & 255, c & 255].map(|v| v as f32 / 255.);
    let at = |x: f32, z: f32| grids::value_at(layer, x, z);
    let land = p[1] > 0.;
    let here = at(p[0], p[2]);
    let buildable = here == harbour::CONSTRUCTION;
    if !land {
        return buildable.then(|| (rgb(crate::colors::BUILD_HARBOUR), 0.55));
    }
    // Vertices sit every 0.5 m; a neighbour 0.75 m away with another value
    // puts this one on the edge.
    let edge = [(0.75, 0.), (-0.75, 0.), (0., 0.75), (0., -0.75)]
        .iter()
        .any(|(dx, dz)| (at(p[0] + dx, p[2] + dz) == harbour::CONSTRUCTION) != buildable);
    if edge {
        return Some((rgb(crate::colors::BUILD_EDGE), 0.9));
    }
    if buildable {
        return None;
    }
    // Diagonal stripes 4 m apart.
    let stripe = ((p[0] + p[2]) * 0.25).rem_euclid(1.) < 0.4;
    Some((
        rgb(crate::colors::BUILD_BLOCKED),
        if stripe { 0.6 } else { 0.2 },
    ))
}

/// Ground length of a harbour curve in metres.
fn curve_length(path: &[anno_core::island::objects::PathPoint]) -> f32 {
    path.windows(2)
        .map(|w| (w[1].p[0] - w[0].p[0]).hypot(w[1].p[2] - w[0].p[2]))
        .sum()
}

/// rd3d heights in 16-bit heightmaps: 1/256 m steps with sea level at 32768,
/// so other tools see sea level mid-range and export → import is lossless.
pub const HEIGHT_UNITS: files::Units16 = files::Units16 {
    per_metre: 256.,
    zero: 32768.,
};

/// A hollow being filled up to a line.
// A hollow filled up to a level, hatched below it: no arrow, which made
// it read as Lower at a glance.
const FILL_ICON: Icon = Icon(&[
    &[[2., 6.], [6., 18.], [18., 18.], [22., 6.]],
    &[[4., 11.], [20., 11.]],
    &[[8., 11.], [8., 17.]],
    &[[12., 11.], [12., 17.]],
    &[[16., 11.], [16., 17.]],
]);
/// A bump shaved off at a line.
const SCRAPE_ICON: Icon = Icon(&[
    &[[2., 18.], [8., 8.], [16., 8.], [22., 18.]],
    &[[4., 12.], [20., 12.]],
    &[[12., 3.], [12., 10.]],
    &[[9., 6.], [12., 3.], [15., 6.]],
]);

/// Small plus marks strewn about: bold enough at 21 px not to read as a
/// disabled glyph, which bare dots did.
const SCATTER_ICON: Icon = Icon(&[
    &[[4., 7.], [9., 7.]],
    &[[6.5, 4.5], [6.5, 9.5]],
    &[[13., 5.], [18., 5.]],
    &[[15.5, 2.5], [15.5, 7.5]],
    &[[9., 14.], [14., 14.]],
    &[[11.5, 11.5], [11.5, 16.5]],
    &[[15., 19.], [20., 19.]],
    &[[17.5, 16.5], [17.5, 21.5]],
    &[[3., 18.], [7., 18.]],
    &[[5., 16.], [5., 20.]],
]);
/// A 3×3 cell grid.
const GRID_ICON: Icon = Icon(&[
    &[[3., 3.], [21., 3.], [21., 21.], [3., 21.], [3., 3.]],
    &[[9., 3.], [9., 21.]],
    &[[15., 3.], [15., 21.]],
    &[[3., 9.], [21., 9.]],
    &[[3., 15.], [21., 15.]],
]);
/// A drop over a wash.
const TINT_ICON: Icon = Icon(&[
    &[
        [12., 3.],
        [7., 11.],
        [8., 15.],
        [12., 17.],
        [16., 15.],
        [17., 11.],
        [12., 3.],
    ],
    &[[3., 21.], [21., 21.]],
]);

/// An anchor: where ships tie up.
const HARBOUR_ICON: Icon = Icon(&[
    &[[12., 3.], [12., 20.]],
    &[[8., 7.], [16., 7.]],
    &[[4., 13.], [5., 17.], [12., 20.], [19., 17.], [20., 13.]],
]);
/// A river winding between banks.
const RIVER_ICON: Icon = Icon(&[
    &[[4., 3.], [9., 8.], [7., 13.], [12., 18.], [11., 21.]],
    &[[11., 3.], [16., 8.], [14., 13.], [19., 18.], [18., 21.]],
]);
/// A pick over a rock.
const SLOT_ICON: Icon = Icon(&[
    &[[5., 5.], [19., 19.]],
    &[[3., 11.], [8., 6.], [13., 3.]],
    &[[3., 21.], [9., 15.], [15., 17.], [21., 21.]],
]);

impl Tool {
    /// Tools that drag a brush over the ground.
    fn is_brush(self) -> bool {
        !matches!(
            self,
            Self::Select | Self::Place | Self::Harbour | Self::Slot | Self::River
        )
    }

    const ALL: [(Self, Icon, &'static str, &'static str, &'static str); 16] = [
        (
            Self::Select,
            Icon::SELECT,
            "Select",
            "Q",
            "Click or box-select props; W E R switch the gizmo",
        ),
        (
            Self::Raise,
            Icon::RAISE,
            "Raise",
            "1",
            "Drag to raise ground. Shift lowers",
        ),
        (
            Self::Lower,
            Icon::LOWER,
            "Lower",
            "2",
            "Drag to lower ground. Shift raises",
        ),
        (
            Self::Smooth,
            Icon::SMOOTH,
            "Smooth",
            "3",
            "Drag to average neighboring heights",
        ),
        (
            Self::Flatten,
            Icon::FLATTEN,
            "Flatten",
            "4",
            "Drag to pull ground to the target height (Alt-click samples it)",
        ),
        (
            Self::Fill,
            FILL_ICON,
            "Fill",
            "5",
            "Drag to raise hollows up to the target height, never lowering",
        ),
        (
            Self::Scrape,
            SCRAPE_ICON,
            "Scrape",
            "6",
            "Drag to cut bumps down to the target height, never raising",
        ),
        (
            Self::Noise,
            Icon::ROUGH,
            "Noise",
            "7",
            "Drag to add rough natural detail",
        ),
        (
            Self::Paint,
            Icon::PAINT,
            "Paint",
            "8",
            "Drag to paint the chosen material. Alt-click picks one",
        ),
        (
            Self::Place,
            Icon::ADD,
            "Place",
            "9",
            "Choose a model in the Library below, then click the ground to place it",
        ),
        (
            Self::Scatter,
            SCATTER_ICON,
            "Scatter",
            "S",
            "Drag to fill with props from the scatter set up to the density. Shift thins",
        ),
        (
            Self::Grid,
            GRID_ICON,
            "Grid",
            "G",
            "Drag to paint the chosen gameplay grid (land, river, trees …). Shift erases, Alt-click picks",
        ),
        (
            Self::Tint,
            TINT_ICON,
            "Tint",
            "T",
            "Drag to paint ground tint. Shift returns to neutral, Alt-click picks",
        ),
        (
            Self::Harbour,
            HARBOUR_ICON,
            "Harbour spot",
            "H",
            "Click the shore to add a harbour spot, where players can found a settlement. Shift-click removes one",
        ),
        (
            Self::Slot,
            SLOT_ICON,
            "Resource slot",
            "M",
            "Click to place a mine or river slot with its work area. Shift-click removes one",
        ),
        (
            Self::River,
            RIVER_ICON,
            "River & lake",
            "V",
            "Click to add points, drag points and width handles; Enter or double-click carves it. Click a river or lake to edit it",
        ),
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shading {
    /// Ground color baked from the material maps, palette and tint.
    Ground,
    /// The game's minimap render, draped over the whole island.
    Map,
    Height,
    Materials,
}

/// Items a gizmo drag moves on the CPU at most; more move on the GPU
/// (`Renderer::drag_selection`) and reach the document on release. 2k
/// items take about 0.5 ms a frame on the CPU; 20k took 5 ms.
const GPU_DRAG: usize = 2048;

/// High word of the IDs the editor gives objects it generates (sound spots,
/// build blockers), so a rerun replaces its own and leaves hand-placed ones.
/// It is a normal game object ID: bits 50..61 are a per-session editor tag
/// on shipped objects, and none of the 89 shipped islands uses 0xED1.
const GENERATED_ID_HIGH: u64 = (0x8000_0001 | 0xED1 << 18) << 32;

fn is_generated(id: u64) -> bool {
    id & !0xFFFF_FFFF == GENERATED_ID_HIGH
}

/// A hierarchy group: objects by asset GUID, props by mesh file index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Object(u32),
    Prop(u32),
}

/// A ground bake (`IslandEditor::bake_job`): the texture for cliff tops and
/// non-splat shading, the GPU splat when it was stale, and how far sinking
/// ground lies below the heights.
pub struct Ground {
    /// `None` when nothing draws it (`IslandEditor::bake_wanted`).
    pub texture: Option<Texture>,
    pub splat: Option<(Splat, Arc<terrain::SplatPlanes<doc::Layer>>)>,
    pub sink: Option<Arc<Vec<f32>>>,
}

/// What the viewports draw, toggled from the hierarchy pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layers {
    pub props: bool,
    pub objects: bool,
    /// Models the game draws only in its own editor (`Mesh/
    /// OnlyVisibleInEditor`): build blockers, herds, flocks, sound spots
    /// and markers. Off they show as markers, as in game.
    pub editor_only: bool,
    pub shipped: bool,
    /// The sea plane, and rivers and lakes as translucent surfaces.
    pub water: bool,
    /// Ground tint multiplies the terrain in Ground shading.
    pub tint: bool,
    /// A gameplay grid drawn over the ground; the Grid tool shows its own.
    pub grid: Option<Kind>,
    /// Lines on the ground every heightfield tile (32 m).
    pub chunks: bool,
    /// Where players may build: unbuildable land hatched, the edge drawn.
    pub buildable: bool,
    /// Sound spots as rings and build blockers as their squares.
    pub spots: bool,
    /// The props the game scatters at run time from the vegetation prop set.
    pub vegetation: bool,
    /// Sun shadows. Every caster draws a second time into the shadow map.
    pub shadows: bool,
    /// The ground raised and lit by its materials' heights, as the game
    /// displaces it (anno_render::a7m::Relief): rock ledges stand out. One more texture
    /// read per layer.
    pub relief: bool,
    /// Harbour coasts as cyan lines on the waterline; the Harbour tool
    /// shows them regardless.
    pub harbours: bool,
    /// The sea's waves move; off they hold still. Moving waves redraw
    /// every frame.
    pub waves: bool,
    /// Rain on the water, as in a shower in game (117 rivers and lakes).
    pub rain: bool,
    /// The flood season rather than the dry one, on islands whose region
    /// has seasons (117's Egypt rivers).
    pub flood: bool,
    /// The island seen as the game shows it when that isn't from the air
    /// (`IslandEditor::view`: 2070's Deep Ocean islands under water); off,
    /// from the air like any other.
    pub under_water: bool,
    /// The player colour dyed flags and trim show, an index into the
    /// game's `Profile::player_colors`: islands have no owner.
    pub player: usize,
}

impl Default for Layers {
    fn default() -> Self {
        Self {
            props: true,
            objects: true,
            editor_only: false,
            // The heightfield is the terrain; shipped meshes are a reference.
            shipped: false,
            water: true,
            tint: true,
            grid: None,
            chunks: false,
            buildable: false,
            spots: false,
            // Tens of thousands of instances; opt in.
            vegetation: false,
            shadows: true,
            relief: true,
            // The heaviest line on the island otherwise; the Harbour tool
            // draws them regardless.
            harbours: false,
            waves: true,
            rain: false,
            flood: false,
            under_water: true,
            player: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Radius,
    /// Inner radius as a percentage of the outer one.
    Inner,
    Strength,
    Level,
    Position(usize),
    /// Degrees about the up axis.
    Yaw,
    Scale,
    /// Scatter props per 100 m².
    Density,
    /// Scatter scale variation, percent.
    Jitter,
    /// Value the Grid tool paints.
    GridValue,
    /// The view's time of day, hours.
    TimeOfDay,
    /// River tool: the picked knot's flow speed at the left bank (0),
    /// centre (1) or right bank (2), looking downstream.
    Flow(usize),
    /// River tool: a setting of the bed carving makes.
    Bed(BedField),
    /// River tool: how far along the curve a moved point's neighbours
    /// follow it, metres.
    Falloff,
}

/// The River tool's bed settings ([`river::Bed`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BedField {
    Depth,
    Bank,
    Reach,
    Bends,
    Pools,
}

/// Shipped Albion marsh ground lies flat at this height, metres.
const MARSH_LEVEL: f32 = 0.5;

/// A resource slot as the slot tool marks it.
pub(super) struct SlotMark {
    pub at: [f32; 3],
    /// Kind, and the test-map deposit when one is assigned.
    pub label: String,
    pub resource: harbour::Resource,
    /// Always active: drawn square, random slots round.
    pub fixed: bool,
    /// Which way it faces, radians in the ground plane.
    pub yaw: f32,
    /// The kind the tool places now: drawn bright, the others faint.
    pub current: bool,
}

enum Drag {
    Brush {
        stroke: Stroke,
        before: Box<Doc>,
        level: f32,
        changed: bool,
        /// Shadows wait for the release (`continue_brush`); dropping the
        /// drag on any path lets them draw.
        hold: Option<gpu::ShadowHold>,
    },
    Gizmo {
        gesture: Gesture,
        targets: Vec<(Item, Mat4)>,
        before: Box<Doc>,
        changed: bool,
        /// The newest cursor and snap since the last frame applied a move:
        /// see `apply_gizmo`.
        pending: Option<(Point<Pixels>, bool)>,
        /// A large drag runs on the GPU (`GPU_DRAG`): the matrix the
        /// targets' start transforms take now. The document keeps their
        /// starts until the release.
        gpu: Option<Mat4>,
        /// The turn the targets stand on (`Islands::turn_step`), which a
        /// turn about the vertical snaps to; 0 for free. `None` while the
        /// game still reads it: the drag turns freely and asks again.
        turn: Option<f32>,
    },
    Marquee(Marquee),
    /// Harbour tool: drawing a harbour along the shore; the preview follows
    /// the cursor.
    Shore {
        from: Vec3,
        to: Vec3,
        preview: Option<Vec<anno_core::island::objects::PathPoint>>,
    },
    /// Harbour and slot tools: a ground rectangle to auto-place in, or with
    /// the lasso the ground points the cursor traced.
    Region {
        from: Vec3,
        to: Vec3,
        lasso: Option<Vec<[f32; 2]>>,
    },
    /// River tool: dragging a draft point or its width handle, from
    /// where every knot stood at the press (neighbours follow a moved
    /// point within the falloff).
    RiverKnot {
        index: usize,
        handle: river::Handle,
        start: Vec<[f32; 2]>,
    },
    /// River tool: the gizmo moving the picked point, its up arrow the
    /// water's height there (`surfaces`: every point's at the press).
    RiverGizmo {
        gesture: Gesture,
        index: usize,
        start: Vec<[f32; 2]>,
        surfaces: Vec<f32>,
    },
    /// Grid tool on a 2070 island: dragging one build-blocker polygon's
    /// point.
    ShapePoint {
        shape: usize,
        point: usize,
        before: Box<Doc>,
        changed: bool,
    },
    /// Dragging one harbour curve point along the shore.
    CoastPoint {
        id: u64,
        index: usize,
        before: Box<Doc>,
        changed: bool,
    },
    /// The path edit mode's gizmo moving the picked points, from where
    /// they were at the press.
    PathGizmo {
        gesture: Gesture,
        start: Vec<(usize, Vec3)>,
        before: Box<Doc>,
        changed: bool,
    },
    Orbit(Point<Pixels>),
    /// Last cursor, and the ground point grabbed (perspective pans keep it
    /// under the cursor).
    Pan(Point<Pixels>, Option<Vec3>),
}

/// What the Inspector's path preview offers for a slot.
pub struct PathPreview {
    pub slot: u64,
    /// Buildings that lay a model on one of the slot's paths: GUID, name.
    pub buildings: Vec<(u32, String)>,
    /// The choice: a building, 0 for none; None is Auto.
    pub current: Option<u32>,
}

/// The Slot tool's path edit mode: one work area's path, and the points
/// picked for the gizmo. Placing slots is off while it is on, and clicks
/// pick points only. The last point sits on the slot and is never picked.
#[derive(Clone, Debug)]
pub struct RoadEdit {
    pub work: u64,
    pub picked: BTreeSet<usize>,
}

struct View {
    camera: Camera,
    bounds: Bounds<Pixels>,
    viewport: Arc<Viewport>,
    drag: Option<Drag>,
}

/// A sky bake from `IslandEditor::sky_job`: the sky for `ambient` at
/// `time`, started at `clock` on the renderer's clock.
pub(crate) struct SkyBaked {
    ambient: Ambient,
    time: f32,
    clock: f32,
    baked: Baked,
}

/// How often a sky with drifting clouds is baked again: a bake is ~9 ms of
/// one core, and 117's clouds drift about 2 units a second, so the
/// reflected clouds lag the marched ones by under that.
const SKY_DRIFT_STEP: f32 = 1.;

pub struct IslandEditor {
    module: &'static dyn anno_core::game::Module,
    pub path: String,
    // Inputs matching `island.native`. The install mounts saved mods, so reading it
    // again after Save would apply edits to already edited terrain files.
    terrain_original: std::collections::BTreeMap<String, Option<Vec<u8>>>,
    /// The document and what tools and conversion change beside it (object
    /// store, harbour records, slot tool choices, prop ids, material set).
    pub island: anno_core::island::Island,
    map: Option<Arc<Texture>>,
    ground: Option<Arc<Texture>>,
    /// Native splat of the material maps; terrain draws with it in Ground.
    splat: Option<Arc<Splat>>,
    /// The ground drawing `splat`, made with it so the view keeps one
    /// mounted ground, and its pages, across scenes.
    splat_ground: gpu::GroundDraw,
    /// What the splat's control maps draw from: handed the material maps
    /// again after each edit (`Palette::update_splat`).
    splat_planes: Option<Arc<terrain::SplatPlanes<doc::Layer>>>,
    palette: Option<Arc<terrain::Palette>>,
    /// Material id and thumbnail, palette order.
    thumbs: Vec<(u8, Arc<RenderImage>)>,
    /// The colour ids of a game's own ground with their names and looks,
    /// by id: what conversion maps ground by in place of a palette's.
    own_materials: Vec<convert::GroundMaterial>,
    /// Material edits since the ground texture was baked; the editor re-bakes
    /// off the UI thread after a paint stroke.
    pub ground_stale: bool,
    /// The splat no longer matches the materials (undo, redo): the next
    /// bake rebuilds it. Paint strokes update it in place instead.
    splat_stale: bool,
    baking: bool,
    /// Heights changed since the land grids followed them: a released
    /// stroke leaves them to `land_job`, off the UI thread.
    land_stale: bool,
    /// The running land job: its number, and the heights the land grids
    /// followed before it, for `sync_land` to take over from.
    land_job: Option<(u64, Arc<Heightfield>)>,
    land_jobs: u64,
    layers: Layers,
    /// The Graphics pane's settings, shared by every island
    /// (`crate::graphics::current`); the pane sets the open island's too.
    graphics: crate::graphics::Graphics,
    /// ANNO_EDITOR_RENDER_SCALE, which wins over `graphics.render_scale`.
    render_scale_env: Option<f32>,
    /// Flatten target; `None` uses the height where each stroke starts.
    level: Option<f32>,
    /// Prop file index the Place tool adds.
    place: Option<u32>,
    /// What the Place tool puts down instead of a prop: an object record
    /// from a shipped island, and the model a decoration carries.
    pub place_object: Option<(anno_core::island::objects::Template, Option<String>)>,
    /// Library pane: which list, and its search.
    pub library_tab: catalog::Kind,
    pub library_filter: String,
    /// Props tab: only models the island already uses.
    pub library_here: bool,
    /// The Library's open variant group (a model name without its `_a`,
    /// `_b`… suffix), or all models when `None`.
    pub library_group: Option<String>,
    library_edit: Option<TextEdit<()>>,
    scatter: scatter::Scatter,
    /// Auto cliffs density and whether a rerun replaces its earlier pieces.
    pub cliffs: cliffs::Settings,
    /// Grid layer and value the Grid tool paints.
    grid: Kind,
    grid_value: u32,
    /// What the Grid tool edits on a 2070 island, whose grids are its
    /// blockers instead.
    pub blocker: grids::BlockerEdit,
    /// The build-blocker polygon being drawn: its points so far, island
    /// metres. Not in the document until it closes.
    outline: Vec<[f32; 2]>,
    /// Overlay, tint switch and path-blocker layer the terrain tiles were
    /// last built with, so a layer toggle rebuilds them.
    drawn_look: Option<(Option<Kind>, bool, Option<anno_core::island::PathLayer>)>,
    /// What the scene's water lines were built for: River tool, hovered
    /// water, the river being edited.
    drawn_lines: Option<(bool, Option<u64>, Option<u64>)>,
    /// Multiplier the Tint tool paints toward.
    tint_color: [f32; 3],
    /// Tint as loaded; save re-bakes tint tiles only when `doc.tint` moved off it.
    tint_loaded: Option<Arc<TintMap>>,
    /// Created in the editor: no file under `path` is shipped, so saves
    /// also write the headers, map images and the project entry.
    is_new: bool,
    /// How the game is offered a new island; unused for shipped ones.
    pub listing: Listing,
    /// The harbour records (`island.harbour`) are being read off the UI
    /// thread (`start_harbour_library`).
    harbour_loading: bool,
    /// Reading them failed for this island; not retried on every click.
    harbour_failed: bool,
    /// Harbour and slot tools: dragging marks a region to auto-place in.
    pub auto_region: bool,
    /// The region is traced freehand instead of dragged as a rectangle.
    pub auto_lasso: bool,
    /// Painting the Marsh grid also shapes the marsh: flat ground, Marsh
    /// environment, marsh textures (`shape_marsh`).
    pub marsh_shape: bool,
    /// How many auto placement adds per region.
    pub auto_count: u32,
    /// Shipped sound spot and build blocker records by GUID, loaded on the
    /// first auto placement.
    pub spot_records: Option<Arc<HashMap<u32, anno_core::island::objects::Template>>>,
    /// The curve point picked with the Harbour tool (a harbour) or the Slot
    /// tool (a work area's road): object, index.
    pub curve_point: Option<(u64, usize)>,
    /// The Slot tool's path edit mode, when on (`road_editing`).
    pub road_edit: Option<RoadEdit>,
    /// The river or lake the River tool is drawing or re-editing.
    pub river: Option<river::Draft>,
    /// Undo within the draft, and drafts written, which undoing reopens.
    river_history: river::DraftHistory,
    /// The river or lake under the cursor in the Select and River tools,
    /// drawn highlighted so it is clear what a click picks.
    hover_water: Option<u64>,
    /// The instance under the pointer in the Hierarchy, boxed in the
    /// viewport: hundreds of ID and coordinate rows gave no spatial cue to
    /// which bush a row was.
    pub hover_item: Option<Item>,
    /// The Hierarchy lists only the selection: after "Select all 549", or a
    /// box pick across hundreds of props, the working set without the rest.
    pub selected_only: bool,
    /// New River tool drafts are lakes.
    pub lake_mode: bool,
    /// How carving shapes and paints river beds.
    river_bed: river::Bed,
    /// [`Field::Falloff`].
    river_falloff: f32,
    /// Writing a river reshapes and repaints the ground under it.
    pub carve_ground: bool,
    /// The draft knot whose flow speed the last slider drag set, so one
    /// drag is one undo step.
    flow_step: Option<usize>,
    /// Flow brush: the last dab, whose direction to the next one sets the
    /// current.
    flow_from: Option<[f32; 2]>,
    /// `listing` changed since the last save.
    listing_dirty: bool,
    /// The Ground material pane's browser of the game's sets, when open.
    pub(crate) set_browser: Option<materials::SetBrowser>,
    /// The material the Ground material pane is removing, while it waits for
    /// the user to pick what its ground becomes.
    pub(crate) removing: Option<u8>,
    /// The install's prop sets; `None` when `prop_sets.xml` did not load.
    pub prop_sets: Option<Arc<vegetation::PropSets>>,
    /// How far foliage draws at each of the game's View Distance levels.
    pub view_distances: crate::graphics::ViewDistances,
    /// Ambient presets the inspector offers for this island's region.
    pub ambients: Vec<String>,
    /// How each of those presets, and the island's own, lights the scene.
    ambient_lights: Vec<(String, Ambient)>,
    /// The sky last baked, for the preset and time it was baked for: a bake
    /// takes tens of milliseconds, and scenes rebuild on every edit.
    sky: Option<(Ambient, f32, Baked)>,
    /// The preset and time a sky bake is running for (`sky_job`); one at a
    /// time, so bakes land in the order they were asked for.
    sky_baking: Option<(Ambient, f32)>,
    /// When the last bake started on the renderer's clock (`gpu::clock`),
    /// for skies whose clouds drift across them (`Sky::drifts`).
    sky_clock: f32,
    /// A wake-up for the next drifting bake is pending (`sky_due_in`).
    pub(crate) sky_ticking: bool,
    /// The preset's ground light at noon, which eye adaptation opens the
    /// view's exposure against at other times.
    /// The working tint as the GPU decodes it per pixel (`GroundTint`),
    /// drawn from the map itself (`TintSource`), and that map by pointer,
    /// when the splat draws the ground. A dab touches what it painted; a
    /// different map (undo, or the first dab after a checkpoint) makes it
    /// again.
    tint_gpu: Option<(usize, Arc<gpu::ControlMap>, Arc<TintSource>)>,
    /// The game's own water pass (117's ocean, rivers and lakes), made with
    /// the island and kept: its wave simulation and maps last across
    /// scenes. Listed in the scene's passes while a preset has a sea.
    water_pass: Option<Arc<dyn anno_core::water::Water>>,
    /// The game's own sky passes (the backdrop from the camera and the
    /// haze over the island), made with the island and kept like the water.
    sky_pass: Option<Arc<dyn anno_core::ambient::SkyPass>>,
    /// The way the game sees this island when not from the air (2070's
    /// Deep Ocean islands from under the water), shown while
    /// `Layers::under_water` is on; its passes stand in for the sky's and
    /// the water's.
    view: Option<anno_core::ambient::View>,
    /// Hours, 0-24: where the view's sun and moon stand. View-only, like
    /// Layers; the game runs its own day.
    pub time_of_day: f32,
    /// The ground vegetation preview last placed, for the scene panel.
    vegetation: Option<Arc<vegetation::Preview>>,
    /// A World Machine import is running.
    pub wm_busy: bool,
    /// Export write time of the last World Machine import.
    wm_stamp: Option<std::time::SystemTime>,
    open_groups: Tree<Group>,
    /// A viewport pick for the Hierarchy to scroll to on its next frame.
    reveal: std::cell::Cell<Option<Item>>,
    /// Where a Shift click in the Hierarchy extends a range from: the last
    /// instance clicked there without Shift.
    anchor: Option<Item>,
    /// How open Hierarchy groups order their instances: 0 by ID, 1-3 by X,
    /// Y or Z; true is descending.
    instance_sort: (u8, bool),
    /// Hierarchy sections folded, one bit per kind (0 Objects, 1 Props).
    /// Open by default, unlike groups.
    closed_sections: u8,
    /// Hierarchy groups switched off in the viewports: not drawn, not
    /// picked. View state only; saving ignores it.
    pub(crate) hidden_groups: BTreeSet<Group>,
    filter: String,
    filter_edit: Option<TextEdit<()>>,
    /// Load steps and their timings, shown over the viewport until dismissed.
    load_log: Vec<String>,
    show_log: bool,
    shipped: terrain::ShippedMeshes,
    /// Where `shipped` loads from when its layer first shows.
    shipped_source: Option<(Arc<Install>, String)>,
    /// When `trim_heap` last handed freed memory back.
    trimmed: Option<std::time::Instant>,
    /// `shipped` with the current shading's texture.
    shipped_meshes: Vec<Vec<Arc<Mesh>>>,
    water: Arc<Mesh>,
    /// Rivers and lakes as one surface: shaded as 117 does beside its
    /// ocean, else a translucent stand-in.
    inland_water: Option<Inland>,
    /// The rivers' and lakes' courses their outlines draw, from the same
    /// `inland_job` as their surface.
    water_courses: Vec<river::Course>,
    history: History<Doc>,
    pub tool: Tool,
    brush: Brush,
    paint: u8,
    shading: Shading,
    gizmo: gizmo::Mode,
    /// Metres per step when Ctrl snaps a move: 1 m was coarse for the
    /// metre-sized bushes and rocks most props are.
    snap_step: f32,
    selection: BTreeSet<Item>,
    /// Models by cfg path, filled in the background after load.
    library: models::Library,
    /// Every placed model piece, drawn GPU-driven; see `placements`.
    placements: placements::Placements,
    /// Visible items whose model is not loaded: drawn as markers.
    undrawn: Vec<Item>,
    /// Models finished loading: the renderer's piece table is out of date.
    pieces_stale: bool,
    /// Only markers changed (a drag moved items in place).
    markers_stale: bool,
    /// A work area's path changed: its rail preview rows are redone with
    /// the vegetation's (`set_grown`), not every row.
    rails_stale: bool,
    /// Bent path models by work area, and each model to bend, by cfg, once
    /// its pieces have loaded.
    rails: rail::Rails,
    rail_templates: HashMap<String, Arc<rail::Template>>,
    /// The rails are due a bend (`rails_job`), and whether one is running.
    /// The rails bent last stay drawn meanwhile.
    rails_wanted: bool,
    rails_building: bool,
    /// The Inspector's path preview per slot: the building whose path
    /// model its work areas show, 0 for none; absent is Auto
    /// (`Assets::work_area_path`). A view setting: not saved, not undone.
    pub path_previews: HashMap<u64, u32>,
    /// A drag moved a river or lake curve: its water mesh follows.
    inland_stale: bool,
    /// The rivers and lakes' surface is due a rebuild (`inland_job`), and
    /// whether one is running. The surface built last stays drawn meanwhile.
    inland_wanted: bool,
    inland_building: bool,
    /// The ground the last `inland_job` measured the flow map over: edits
    /// replace the field's `Arc`. Weak, so its address is never reused.
    inland_ground: std::sync::Weak<gpui_editor_kit::heightfield::Heightfield>,
    /// Placement rows planned by the last full refresh, waiting for
    /// `placement_job` to build them, and whether a build is running.
    /// Only while `background_scene`: else `refresh_scene` builds them.
    placement_plan: Option<placements::Plan>,
    placement_building: bool,
    /// A runner takes the scene's jobs (`inland_job`, `placement_job`) off
    /// the UI thread. Off for tests and harnesses, whose refresh builds
    /// everything itself.
    pub(crate) background_scene: bool,
    /// The selection the renderer outlines and marks, with the placement
    /// rows' generation it was resolved against.
    selection_sent: Option<(BTreeSet<Item>, u64)>,
    /// Selected items no placement row draws (no model, or not loaded):
    /// their rings stay scene markers.
    selection_unplaced: Vec<Item>,
    /// What a running stroke or drag changed under the vegetation preview
    /// since the last frame (world `[x0, z0, x1, z1]`), regrown on the next.
    vegetation_window: Vec<[f32; 4]>,
    /// Bumped while a gizmo drag or field edit moves items between checkpoints.
    moves: u64,
    loading_models: bool,
    /// What decided the models wanted when the last scan found none left
    /// to load: (revision, assets loaded, vegetation shown, vegetation
    /// preview, models loaded). Scanning every item's cfg costs 1.5 ms on
    /// a 512 m island, so `models_job` skips it until one of these moves.
    models_settled: Option<(u64, bool, bool, usize, usize)>,
    /// assets.xml, for object GUID → model cfg; set by the editor once loaded.
    pub assets: Option<Arc<anno_formats::assets::Assets>>,
    /// The app's install, where a game places models by copying a shipped
    /// record of their asset (2070, `IslandCodec::object_for`).
    pub install: Option<Arc<Install>>,
    views: [View; 2],
    /// Draws both views; each has a viewport on it.
    renderer: Arc<Renderer>,
    tiles: Vec<Arc<Mesh>>,
    /// Heights on the GPU; every tile mesh displaces by it, so a sculpt dab
    /// uploads only its rectangle and rebuilds no mesh.
    gpu_heights: Option<Arc<gpu::HeightMap>>,
    /// How far sinking ground (marsh) lies below the heights where the
    /// terrain is drawn, per height sample (`Palette::sinking`).
    sink: Option<Arc<Vec<f32>>>,
    /// With GPU heights: the ground's color per height sample, and the
    /// terrain the renderer displaces from both. `tiles` stays empty then;
    /// it holds CPU meshes only when the GPU copy is unavailable.
    terrain_colors: Option<Arc<gpu::ControlMap>>,
    terrain: Option<Arc<gpu::Terrain>>,
    /// Height-dependent vertex colors (water tint, Height shading) under
    /// the current sculpt stroke, refreshed when it ends.
    stale_colors: Rect,
    dirty: Rect,
    scene_stale: bool,
    /// One instance at the origin, shared by terrain tiles, water and shipped
    /// meshes. It must be the same `Arc` every refresh: the renderer keys
    /// batches by their mesh and instance `Arc`s, and a fresh one made every
    /// tile look new, so a gizmo drag re-uploaded ~100 tile batches and the
    /// moved item only showed once the drag stopped.
    identity: Arc<Instances>,
    cursor: Option<Vec3>,
    editing: Option<TextEdit<Field>>,
    pub message: Option<(String, bool)>,
    /// The game's ground for its own paint form (`SceneParts::ground`);
    /// `None` where the a7m splat or the kit's material draws the paint.
    own_ground: Option<Box<dyn anno_core::game::IslandGround>>,
}

/// What loading finds beside the document, for `IslandEditor::assemble`.
struct Parts {
    palette: Option<Arc<terrain::Palette>>,
    splat: Option<Arc<Splat>>,
    splat_planes: Option<Arc<terrain::SplatPlanes<doc::Layer>>>,
    ground: Option<Arc<Texture>>,
    shipped: terrain::ShippedMeshes,
    thumbs: Vec<(u8, Arc<RenderImage>)>,
    map: Option<Arc<Texture>>,
    tint_gpu: Option<(usize, Arc<gpu::ControlMap>, Arc<TintSource>)>,
    ambients: Vec<String>,
    ambient_lights: Vec<(String, Ambient)>,
    water_pass: Option<Arc<dyn anno_core::water::Water>>,
    sky_pass: Option<Arc<dyn anno_core::ambient::SkyPass>>,
    prop_sets: Option<Arc<vegetation::PropSets>>,
    library: models::Library,
    shipped_source: Option<(Arc<Install>, String)>,
    own_ground: Option<Box<dyn anno_core::game::IslandGround>>,
}

/// Rivers and lakes as the scene draws them.
/// The GPU's view of `map` for the ground's tint (`GroundTint`), keyed by
/// the map's pointer: a control map drawn from the tint map as the GPU
/// fills and refreshes it, where an RGBA copy was 36 MB on the continental
/// island. `None` past the 4096² a control map takes.
fn tint_control(map: &Arc<TintMap>) -> Option<(usize, Arc<gpu::ControlMap>, Arc<TintSource>)> {
    if map.width > 4096 || map.height > 4096 {
        return None;
    }
    let source = Arc::new(TintSource(std::sync::Mutex::new(Some(map.clone()))));
    let draw = source.clone();
    let control = gpu::ControlMap::drawn(map.width, map.height, move |rect| {
        let mut out = vec![255u8; (rect.x1 - rect.x0) * (rect.z1 - rect.z0) * 4];
        // Read under the lock: a dab waits rather than the map being
        // copied on write under a reader.
        if let Some(map) = draw.0.lock().unwrap().as_ref() {
            let mut texels = out.as_chunks_mut::<4>().0.iter_mut();
            for z in rect.z0..rect.z1 {
                for t in &map.texels[z * map.width + rect.x0..z * map.width + rect.x1] {
                    let o = texels.next().expect("sized for the rect");
                    o[..3].copy_from_slice(t);
                }
            }
        }
        out
    })
    .ok()?;
    Some((Arc::as_ptr(map) as usize, Arc::new(control), source))
}

/// The tint map a tint control map draws from. A dab takes it out while
/// it paints (the Tint tool's dab), so painting does not copy the
/// whole map for this second reference, and hands it back.
pub(crate) struct TintSource(std::sync::Mutex<Option<Arc<TintMap>>>);

/// What `rails_job` bends: the rails, and the templates it made for
/// models bent the first time.
pub(crate) struct RailsBuilt {
    rails: rail::Rails,
    templates: HashMap<String, Arc<rail::Template>>,
}

/// What `inland_job` builds: the rivers' and lakes' surface and the
/// courses their outlines draw.
pub(crate) struct InlandBuilt {
    surface: Option<Inland>,
    courses: Vec<river::Course>,
}

pub(crate) enum Inland {
    /// 117's own shading, beside its ocean.
    Shaded(Arc<anno_core::water::Inland>),
    /// A translucent surface where there is no ocean to shade them with.
    Mesh(Arc<Mesh>),
}

impl IslandEditor {
    /// The field keys go to, in the order its key handler tries them.
    pub(crate) fn typing(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        if self.library_edit.is_some() {
            return self
                .library_edit
                .as_mut()
                .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer);
        }
        if self.filter_edit.is_some() {
            return self
                .filter_edit
                .as_mut()
                .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer);
        }
        self.editing
            .as_mut()
            .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer)
    }

    /// Reads an island archive of `game` through the install and builds the
    /// document.
    pub fn load(module: &'static dyn Module, install: &Install, path: &str) -> Result<Self> {
        Self::load_with(module, install, path, None)
    }

    /// `load`, given the app's install and asset registry: the island's
    /// models then load on their own threads while the rest of the island
    /// does. Waiting for the document first put 0.8 s before them on the
    /// continental island.
    pub fn load_with(
        module: &'static dyn Module,
        install: &Install,
        path: &str,
        shared: Option<(Arc<Install>, Arc<anno_formats::assets::Assets>)>,
    ) -> Result<Self> {
        let mut log = Vec::new();
        let mut step = std::time::Instant::now();
        let mut lap = |what: &str, log: &mut Vec<String>| {
            log.push(format!("{what}: {} ms", step.elapsed().as_millis()));
            step = std::time::Instant::now();
        };
        let islands = module.islands().context("this game has no islands")?;
        let rules = islands.rules();
        // A game with its own codec builds the document there; the a7m
        // pipeline below reads, bakes and derives the rest.
        let assets = shared.as_ref().map(|s| &*s.1);
        let a7m = match islands.codec() {
            anno_core::game::Codec::Own(codec) => {
                let island = codec.read(install, path, assets)?;
                let mut editor = Self::load_native(module, install, path, island)?;
                // The database names the models its objects draw, and with
                // the install, the records new ones copy.
                (editor.install, editor.assets) = shared.unzip();
                return Ok(editor);
            }
            anno_core::game::Codec::A7m(a7m) => a7m,
        };
        let bytes = install.read(path)?;
        let archive = rda::Archive::parse(bytes, path).context("island archive")?;
        let member = |name: &str| {
            archive
                .read_path(name)
                .with_context(|| format!("{path}: {name}"))
        };
        let gamedata = member("gamedata.data")?;
        let rd3d = member("rd3d.data")?;
        let info_path = format!("{path}info");
        let info = install.read(&info_path).ok();
        let source = match &info {
            Some(info) => Island::parse_with_info(&gamedata, &rd3d, info)?,
            None => Island::parse(&gamedata, &rd3d)?,
        };
        let material_set = a7m.material_set(source.material_set_file_name()).to_owned();
        // The archive stays for `Island::park` below, its members go.
        drop((gamedata, rd3d));
        let metres = source.world_size()[0] as f32;
        // With the app's install the shipped terrain meshes wait for their
        // layer (64 MB on the continental island, off by default).
        let shipped_source = shared
            .as_ref()
            .map(|(install, _)| (install.clone(), path.to_owned()));
        let (object_store, objects) = match objects::Objects::read(source.gamedata()) {
            Ok(store) => {
                let list = store.objects.clone();
                (Some(store), list)
            }
            Err(e) => {
                eprintln!("{path}: objects unavailable: {e:#}");
                (None, Vec::new())
            }
        };
        let objects = Arc::new(objects);
        lap("Island file", &mut log);
        let mut library = models::Library {
            models: Default::default(),
            cache: Arc::new(models::Cache::new(&module.model_look())),
        };
        // Everything after the island's file needs only the file and the
        // install, and no part needs another: each on its own thread, the
        // material maps and the document on this one. One after another
        // they took 0.6 s of the pirate island's 0.7.
        let loaded = std::thread::scope(|s| -> Result<_> {
            let sky = s.spawn(|| module.scene().and_then(|scene| scene.sky(install)));
            // Kept only when a preset has a sea of its own, which is known
            // once they are read: read alongside them rather than after.
            let water = s.spawn(|| module.scene().and_then(|scene| scene.water(install)));
            let prop_sets = s.spawn(|| vegetation::PropSets::load(rules, install));
            let ambients = s.spawn(|| {
                let region = rules.island_region(path);
                let list = (module.scene())
                    .map_or_else(Vec::new, |scene| scene.ambient_names(install, region));
                let own = anno_formats::a7m::session::SessionSettings::read(source.gamedata())
                    .map(|s| s.global_ambient)
                    .unwrap_or_default();
                let names: Vec<&str> = (list.iter().chain([&own])).map(String::as_str).collect();
                let lights = module
                    .scene()
                    .map_or_else(Vec::new, |scene| scene.ambients(install, &names));
                (list, lights)
            });
            let palette = s.spawn(|| {
                let reload = shared.as_ref().map(|(install, _)| install);
                terrain::Palette::load(install, reload, &material_set)
            });
            // The models' cfgs and placements, then the models themselves on
            // every core (`Library::job`).
            let models = shared.as_ref().map(|(shared, assets)| {
                let (source, objects, library) = (&source, &objects, &mut library);
                s.spawn(move || {
                    let files = source.prop_files();
                    let props = (source.props().iter()).map(|p| {
                        let cfg = files.get(p.file as usize).map(|f| f.replace('\\', "/"));
                        (cfg, doc::trs(p.position, p.rotation, p.scale))
                    });
                    let placed: Vec<(Option<String>, Mat4)> = props
                        .chain(objects.iter().map(|o| {
                            let cfgs = assets.get(o.guid).map_or(&[][..], |a| a.cfgs.as_slice());
                            let cfg = o.model(cfgs).map(|m| m.replace('\\', "/"));
                            (cfg, doc::trs(o.position, o.rotation, o.scale))
                        }))
                        .collect();
                    let batch = library.batch(
                        placed
                            .iter()
                            .filter_map(|(c, m)| Some((c.as_deref()?, Some(m)))),
                    );
                    let loaded = library.job(shared.clone(), batch)();
                    library.models.finish(loaded);
                })
            });
            let map = s.spawn(|| load_map_image(install, path));
            let grids = s.spawn(|| {
                Grids::read(&source)
                    .map_err(|e| eprintln!("{path}: grids unavailable: {e:#}"))
                    .ok()
                    .map(Arc::new)
            });
            let tint = s.spawn(|| {
                let dir = island_data_dir(path).ok()?;
                let map = Arc::new(tint::load(install, &dir, source.world_size()));
                let gpu = tint_control(&map);
                Some((map, gpu))
            });
            let materials = match Materials::load_level0(a7m.material_files, install, path, &source)
            {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("{path}: material maps unavailable, painting disabled: {e:#}");
                    None
                }
            };
            let mut doc = Doc::from_island(&source, materials)?;
            doc.objects = objects.clone();
            doc.grids = grids.join().expect("grids");
            let (tint, tint_gpu) = tint.join().expect("tint").unzip();
            doc.tint = tint;
            let tint_gpu = tint_gpu.flatten();
            lap("Terrain / heightmap", &mut log);
            let palette = (palette.join().expect("material set"))
                .map_err(|e| eprintln!("{path}: material set unavailable: {e:#}"))
                .ok()
                .map(Arc::new);
            lap("Material textures", &mut log);
            // The GPU splat draws the ground; the baked texture only colours
            // shipped cliff tops, map images and the other shadings, so with
            // a splat it bakes off the UI thread after loading (`bake_job`),
            // not here: 100-180 ms of the continental island's load. Both
            // read the material maps' chunks in place.
            let splat = palette
                .as_ref()
                .zip(doc.materials.as_ref())
                .and_then(|(p, m)| {
                    let grit = doc.grids.as_ref().and_then(|g| g.layer(Kind::Grit));
                    let a = m.paint.a7m()?;
                    p.splat(a7m.ground_bake, &a.ids, &a.alphas, grit)
                        .map_err(|e| eprintln!("{path}: splat unavailable: {e:#}"))
                        .ok()
                });
            let (splat, splat_planes) = splat.map(|(s, p)| (Arc::new(s), p)).unzip();
            let ground = match (&palette, &doc.materials, &splat) {
                (
                    Some(p),
                    Some(Materials {
                        paint: Paint::A7m(a),
                        ..
                    }),
                    None,
                ) => p
                    .bake(&a.ids, &a.alphas, metres)
                    .map_err(|e| eprintln!("{path}: ground bake failed: {e:#}"))
                    .ok()
                    .map(Arc::new),
                _ => None,
            };
            let shipped = match &shipped_source {
                Some(_) => terrain::ShippedMeshes::empty(metres),
                None => terrain::ShippedMeshes::load(install, path, metres),
            };
            let thumbs = palette.as_ref().map_or_else(Vec::new, |p| {
                p.materials
                    .iter()
                    .map(|(&id, m)| (id, m.preview.render_image()))
                    .collect()
            });
            lap("Splat layers", &mut log);
            let (ambients, ambient_lights) = ambients.join().expect("ambient presets");
            // 50 MB of ripples: only for presets with a sea of their own.
            let water = water.join().expect("water");
            let water = ambient_lights
                .iter()
                .any(|(_, a)| a.sea.is_some())
                .then_some(water)
                .flatten();
            let sky = sky.join().expect("sky");
            let prop_sets = (prop_sets.join().expect("prop sets"))
                .map_err(|e| eprintln!("{}: {e:#}", rules.prop_sets_path))
                .ok()
                .map(Arc::new);
            lap("Ambient presets", &mut log);
            if let Some(models) = models {
                models.join().expect("model loader");
                lap("Models, alongside", &mut log);
            }
            let map = map.join().expect("map image");
            Ok((
                doc,
                (
                    palette,
                    (splat, splat_planes),
                    ground,
                    shipped,
                    thumbs,
                    map,
                    tint_gpu,
                ),
                (ambients, ambient_lights, water, sky, prop_sets),
            ))
        })?;
        let (
            doc,
            (palette, (splat, splat_planes), ground, shipped, thumbs, map, tint_gpu),
            (ambients, ambient_lights, water_pass, sky_pass, prop_sets),
        ) = loaded;
        let mut island = anno_core::island::Island::new(doc, object_store, material_set);
        island.set_native(Native::A7m(source));
        let mut editor = Self::assemble(
            module,
            install,
            path,
            island,
            Parts {
                palette,
                splat,
                splat_planes,
                ground,
                shipped,
                thumbs,
                map,
                tint_gpu,
                ambients,
                ambient_lights,
                water_pass,
                sky_pass,
                prop_sets,
                library,
                shipped_source,
                own_ground: None,
            },
        );
        lap("Terrain chunks", &mut log);
        log.push(format!(
            "{} objects · {} props · {} materials · {} triangles",
            editor.island.doc.objects.len(),
            editor.island.doc.props.len(),
            editor.thumbs.len(),
            editor.triangles()
        ));
        // The island's trees are read again only to save (`Island::park`):
        // from the game's archives when they serve both files, which no save
        // overwrites, else from the compressed archive read here.
        let served = |p: &str| install.mod_layer(p).is_none() && install.file(p).is_some();
        let archived = (shared.as_ref())
            .filter(|_| served(path) && (info.is_none() || served(&info_path)))
            .map(|(install, _)| install.clone());
        let Some(source) = editor.island.a7m_mut() else {
            return Ok(editor);
        };
        match archived {
            Some(install) => {
                let path = path.to_owned();
                source.park(move || {
                    let read =
                        |p: &str| install.file(p).map(|id| install.read_file(id)).transpose();
                    let bytes =
                        read(&path)?.with_context(|| format!("{path}: gone from the archives"))?;
                    let archive = rda::Archive::parse(bytes, &path)?;
                    let info = read(&info_path)?;
                    Ok((
                        archive.read_path("gamedata.data")?,
                        archive.read_path("rd3d.data")?,
                        info,
                    ))
                });
            }
            None => {
                let archive = Arc::new(archive);
                source.park(move || {
                    let (gamedata, rd3d) = (
                        archive.read_path("gamedata.data")?,
                        archive.read_path("rd3d.data")?,
                    );
                    Ok((gamedata, rd3d, info.clone()))
                });
            }
        }
        editor.assets = shared.map(|(_, assets)| assets);
        editor.load_log = log;
        // What the load let go goes back before the first frames pack the
        // arena and grow textures, on a thread of its own (13-30 ms): kept
        // in glibc's arenas it stood on top of what they allocate, 40-110
        // MB of the resident set's high on the 117 islands.
        gpui_editor_kit::gpu::trim_heap();
        Ok(editor)
    }

    /// The editor around `island`, read from `source`, with what loading
    /// found beside it.
    fn assemble(
        module: &'static dyn Module,
        install: &Install,
        path: &str,
        island: anno_core::island::Island,
        parts: Parts,
    ) -> Self {
        let Parts {
            palette,
            splat,
            splat_planes,
            ground,
            shipped,
            thumbs,
            map,
            tint_gpu,
            ambients,
            ambient_lights,
            water_pass,
            sky_pass,
            prop_sets,
            library,
            shipped_source,
            own_ground,
        } = parts;
        let metres = island.doc.field.grid.size()[0];
        let bake_later = ground.is_none() && splat.is_some();
        let size = island.doc.field.grid.size();
        // The families the game's models draw in, under its own scene
        // lighting where the island has a preset of its own to light by:
        // the set is the renderer's for good. Else the kit's alone.
        let preset = (ambient_lights.iter()).any(|(n, _)| *n == island.doc.session.global_ambient);
        let lit = module.model_look().surfaces.map(|s| {
            preset
                .then(|| s.0.lit())
                .flatten()
                .unwrap_or_else(|| s.0.families())
        });
        let renderer = Arc::new(lit.map_or_else(Renderer::default, Renderer::with_families));
        // Fewer pixels for slow GPUs and 5K panes; README, Graphics. Set
        // with the scene (`refresh_scene`).
        let render_scale_env = std::env::var("ANNO_EDITOR_RENDER_SCALE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok());
        // The 3D view is a real perspective; the Top view stays orthographic.
        // Anno's world is left-handed (+z is north): mirrored, the views show
        // islands as the game does, north up.
        let perspective = Camera {
            fov: 0.9,
            zoom: 1.3,
            mirror: true,
            ..Camera::perspective()
        };
        let mut editor = Self {
            module,
            path: path.to_owned(),
            island,
            history: History::new(true),
            // Select: a first click on an unfamiliar island inspects it
            // rather than reshaping its terrain. A new island starts on
            // Raise instead (see `create_island`).
            tool: Tool::Select,
            brush: Brush {
                radius: (size[0] / 24.).clamp(4., 40.),
                strength: 0.5,
                ..Brush::default()
            },
            // The first material shown, which a set's thumbnails start with.
            paint: thumbs.first().map_or(1, |t| t.0),
            load_log: Vec::new(),
            shading: if ground.is_some() || splat.is_some() || own_ground.is_some() {
                Shading::Ground
            } else if map.is_some() {
                Shading::Map
            } else {
                Shading::Height
            },
            shipped_meshes: Vec::new(),
            water: terrain::water(metres),
            inland_water: None,
            water_courses: Vec::new(),
            map,
            splat_ground: splat
                .as_ref()
                .map_or(gpu::GroundDraw::Kit, anno_render::a7m::ground),
            splat,
            splat_planes,
            ground,
            palette,
            thumbs,
            own_materials: Vec::new(),
            ground_stale: bake_later,
            splat_stale: false,
            baking: false,
            land_stale: false,
            land_job: None,
            land_jobs: 0,
            layers: Layers::default(),
            graphics: crate::graphics::current(),
            render_scale_env,
            level: None,
            place: None,
            scatter: scatter::Scatter::default(),
            cliffs: cliffs::Settings::default(),
            grid: Kind::Water,
            grid_value: 1,
            blocker: grids::BlockerEdit::Paths(anno_core::island::PathLayer::Blocked),
            outline: Vec::new(),
            drawn_look: None,
            drawn_lines: None,
            tint_color: tint::PRESETS[3].1,
            tint_loaded: None,
            terrain_original: std::collections::BTreeMap::new(),
            is_new: false,
            listing: Listing::new(module.islands().map_or(&IslandRules::BARE, |i| i.rules())),
            listing_dirty: false,
            set_browser: None,
            removing: None,
            prop_sets,
            view_distances: crate::graphics::ViewDistances::read(install, module),
            ambients,
            ambient_lights,
            sky: None,
            sky_baking: None,
            sky_clock: 0.,
            sky_ticking: false,
            tint_gpu,
            water_pass,
            sky_pass,
            view: None,
            // Noon: the sun at the preset's highest, as the view always was.
            time_of_day: 12.,
            vegetation: None,
            wm_busy: false,
            wm_stamp: None,
            harbour_loading: false,
            harbour_failed: false,
            auto_region: false,
            auto_lasso: false,
            marsh_shape: true,
            auto_count: 2,
            spot_records: None,
            curve_point: None,
            road_edit: None,
            river: None,
            river_history: river::DraftHistory::default(),
            lake_mode: false,
            river_bed: river::Bed::default(),
            river_falloff: 10.,
            carve_ground: true,
            flow_step: None,
            hover_water: None,
            hover_item: None,
            selected_only: false,
            flow_from: None,
            open_groups: Tree::default(),
            reveal: Default::default(),
            anchor: None,
            instance_sort: (0, false),
            closed_sections: 0,
            hidden_groups: BTreeSet::new(),
            filter: String::new(),
            filter_edit: None,
            place_object: None,
            library_tab: catalog::Kind::Props,
            library_filter: String::new(),
            library_here: false,
            library_group: None,
            library_edit: None,
            show_log: true,
            shipped,
            shipped_source,
            trimmed: None,
            gizmo: gizmo::Mode::Move,
            snap_step: 1.,
            selection: BTreeSet::new(),
            library,
            placements: placements::Placements::default(),
            undrawn: Vec::new(),
            pieces_stale: true,
            markers_stale: false,
            rails_stale: false,
            rails: rail::Rails::default(),
            rail_templates: HashMap::new(),
            rails_wanted: false,
            rails_building: false,
            path_previews: HashMap::new(),
            inland_stale: false,
            inland_wanted: false,
            inland_building: false,
            inland_ground: std::sync::Weak::new(),
            placement_plan: None,
            placement_building: false,
            background_scene: false,
            selection_sent: None,
            selection_unplaced: Vec::new(),
            vegetation_window: Vec::new(),
            moves: 0,
            loading_models: false,
            models_settled: None,
            assets: None,
            install: None,
            views: [
                View {
                    camera: perspective,
                    bounds: Bounds::default(),
                    viewport: renderer.viewport(),
                    drag: None,
                },
                View {
                    camera: Camera {
                        mirror: true,
                        ..Camera::default()
                    },
                    bounds: Bounds::default(),
                    viewport: renderer.viewport(),
                    drag: None,
                },
            ],
            renderer,
            tiles: Vec::new(),
            gpu_heights: None,
            sink: None,
            terrain_colors: None,
            terrain: None,
            stale_colors: Rect::default(),
            dirty: Rect::default(),
            scene_stale: true,
            own_ground,
            identity: Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![],
            }),
            cursor: None,
            editing: None,
            message: None,
        };
        editor.tint_loaded = editor.island.doc.tint.clone();
        editor.rebuild_tiles(None);
        editor
    }

    /// Loads an island a game's own codec reads (2070's `.isd`): the
    /// document `island` holds, its layer textures and the ground that
    /// draws its paint. Nothing of the a7m path's (material set, splat,
    /// grids, tint, props) applies.
    fn load_native(
        module: &'static dyn Module,
        install: &Install,
        path: &str,
        mut island: anno_core::island::Island,
    ) -> Result<Self> {
        // The island file names no light: the game's scene finds the
        // profile its worlds light it by.
        let scene = module.scene();
        let ambient = scene.and_then(|s| s.island_ambient(install, path));
        let (ambients, ambient_lights) = match (scene, ambient) {
            (Some(scene), Some(name)) => {
                let lights = scene.ambients(install, &[name.as_str()]);
                island.doc.session.global_ambient = name.clone();
                (vec![name], lights)
            }
            _ => (Vec::new(), Vec::new()),
        };
        // The game's ground for its own paint form, and the palette's
        // thumbnails and conversion looks from it.
        let own_ground = (scene.zip(island.doc.materials.as_ref()))
            .and_then(|(scene, m)| scene.ground(install, &m.paint))
            .and_then(|g| {
                g.map_err(|e| eprintln!("{path}: ground unavailable: {e:#}"))
                    .ok()
            });
        let (mut thumbs, mut looks) = (Vec::new(), Vec::new());
        for t in own_ground.iter().flat_map(|g| g.thumbs(install)) {
            // Conversion maps ground onto colour layers only.
            if t.colour {
                let look = convert::Swatch::of(&t.image.rgba);
                looks.push((t.id, t.name, Some(look)));
            }
            thumbs.push((t.id, t.image.render_image()));
        }
        let metres = island.doc.field.grid.size()[0];
        // The game's sea, kept only when the island's preset has one, as
        // the a7m path keeps it.
        let water_pass = (ambient_lights.iter().any(|(_, a)| a.sea.is_some()))
            .then(|| scene.and_then(|s| s.water(install)))
            .flatten();
        let parts = Parts {
            palette: None,
            splat: None,
            splat_planes: None,
            ground: None,
            shipped: terrain::ShippedMeshes::empty(metres),
            thumbs,
            map: None,
            tint_gpu: None,
            ambients,
            ambient_lights,
            water_pass,
            sky_pass: scene.and_then(|s| s.sky(install)),
            prop_sets: None,
            library: models::Library {
                models: Default::default(),
                cache: Arc::new(models::Cache::new(&module.model_look())),
            },
            shipped_source: None,
            own_ground,
        };
        let mut editor = Self::assemble(module, install, path, island, parts);
        looks.sort_by_key(|m: &convert::GroundMaterial| m.0);
        editor.own_materials = looks;
        editor.view = scene.and_then(|s| s.island_view(install, path));
        if let Some(note) = editor.placeholder_note() {
            editor.say(note, true);
        }
        gpui_editor_kit::gpu::trim_heap();
        Ok(editor)
    }

    pub fn dirty(&self) -> bool {
        self.history.dirty() || self.listing_dirty
    }

    /// Asks the next save to write this island even with no edits: a game
    /// original copied into the mod as it is. Save only staged dirty
    /// islands, so "Save to mod" on an untouched one did nothing.
    pub fn want_in_mod(&mut self) {
        self.listing_dirty = true;
    }

    /// Every model the island draws: prop files some prop uses, and the
    /// model of each object, normalized. For telling which mod provides
    /// them.
    pub fn model_paths(&self) -> Vec<String> {
        let mut used = vec![false; self.island.doc.meshes.len()];
        for p in self.island.doc.props.iter() {
            if let Some(u) = used.get_mut(p.source.file as usize) {
                *u = true;
            }
        }
        let props = (self.island.doc.meshes.iter().zip(used))
            .filter(|(_, u)| *u)
            .map(|(m, _)| m.clone());
        let objects = self.island.doc.objects.iter().filter_map(|o| {
            let cfgs = self
                .assets
                .as_ref()
                .and_then(|a| a.get(o.guid))
                .map_or(&[][..], |a| a.cfgs.as_slice());
            o.model(cfgs).map(str::to_owned)
        });
        let mut paths: Vec<String> = props
            .chain(objects)
            .map(|m| anno_formats::rda::normalize_path(&m).into_owned())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        paths
    }

    /// The cfg that draws one item.
    fn item_cfg(&self, item: Item) -> Option<String> {
        match item {
            Item::Prop(id) => {
                let p = self.island.doc.prop(id)?;
                self.island
                    .doc
                    .meshes
                    .get(p.source.file as usize)
                    .map(|m| m.replace('\\', "/"))
            }
            Item::Object(id) => {
                let o = self.island.doc.objects.iter().find(|o| o.id == id)?;
                let cfgs = self
                    .assets
                    .as_ref()
                    .and_then(|a| a.get(o.guid))
                    .map(|a| a.cfgs.as_slice())
                    .unwrap_or(&[]);
                o.model(cfgs).map(|m| m.replace('\\', "/"))
            }
        }
    }

    /// World-space box corners of each selected item's loaded model, for the
    /// selection outline. Items without a model keep their ring marker.
    fn selection_boxes(&self) -> Vec<[Vec3; 8]> {
        // Box-selecting a whole island would paint thousands of outlines.
        const MAX_OUTLINES: usize = 256;
        self.selection
            .iter()
            .take(MAX_OUTLINES)
            .filter_map(|&item| self.item_box(item))
            .collect()
    }

    /// World-space box corners of one item's loaded model.
    fn item_box(&self, item: Item) -> Option<[Vec3; 8]> {
        let pieces = self.library.models.loaded.get(&self.item_cfg(item)?)?;
        let local =
            math::bounds(pieces.iter().flat_map(|p| {
                math::corners(p.mesh.bounds()).map(|c| math::transform(&p.local, c))
            }));
        if local[0][0] > local[1][0] {
            return None;
        }
        let mut world = self.island.doc.matrix(item)?;
        // Mid GPU drag the document holds the start.
        if let Some(m) = self.gpu_drag()
            && self.selection.contains(&item)
            && self.selection_unplaced.binary_search(&item).is_err()
        {
            world = math::multiply(&m, &world);
        }
        Some(math::corners(local).map(|c| math::transform(&world, c)))
    }

    /// The matrix a running GPU drag applies to the selection.
    fn gpu_drag(&self) -> Option<Mat4> {
        self.views.iter().find_map(|v| match &v.drag {
            Some(Drag::Gizmo { gpu, .. }) => *gpu,
            _ => None,
        })
    }

    /// Boxes the Hierarchy's hovered instance in the viewport; `None` when
    /// the pointer leaves the row.
    pub fn set_hover_item(&mut self, item: Option<Item>) {
        if self.hover_item != item {
            self.hover_item = item;
            self.push_overlay();
        }
    }

    /// Every item's cfg and world matrix, in one pass. Prop cfgs come from
    /// the island's file list, object cfgs from assets.xml.
    fn item_cfgs(&self) -> Vec<(Item, Option<String>, Mat4)> {
        let files: Vec<String> = self
            .island
            .doc
            .meshes
            .iter()
            .map(|m| m.replace('\\', "/"))
            .collect();
        let mut out =
            Vec::with_capacity(self.island.doc.props.len() + self.island.doc.objects.len());
        for p in self.island.doc.props.iter() {
            out.push((
                Item::Prop(p.id),
                files.get(p.source.file as usize).cloned(),
                p.matrix,
            ));
        }
        for o in self.island.doc.objects.iter() {
            let asset = self.assets.as_ref().and_then(|a| a.get(o.guid));
            // The game draws these only in its own editor; blocker
            // footprints come from what it shows.
            let shown = !asset.is_some_and(|a| a.editor_only == Some(true));
            let cfgs = asset.map(|a| a.cfgs.as_slice()).unwrap_or(&[]);
            out.push((
                Item::Object(o.id),
                (o.model(cfgs).filter(|_| shown)).map(|m| m.replace('\\', "/")),
                doc::trs(o.position, o.rotation, o.scale),
            ));
        }
        out
    }

    /// The next batch of models to load off the UI thread, if any are missing.
    pub fn models_job(
        &mut self,
        install: Arc<Install>,
    ) -> Option<impl FnOnce() -> Vec<(String, Option<models::Loaded>)> + Send + 'static> {
        if self.loading_models {
            return None;
        }
        let key = (
            self.history.revision(),
            self.assets.is_some(),
            self.layers.vegetation,
            self.vegetation
                .as_ref()
                .map_or(0, |v| Arc::as_ptr(v) as usize),
            self.library.models.loaded.len(),
        );
        if self.models_settled == Some(key) {
            return None;
        }
        let mut rails: Vec<String> = self.rail_works().into_iter().map(|(_, c)| c).collect();
        rails.sort();
        rails.dedup();
        // Props' cfgs by their mesh file, objects' borrowed from the asset
        // registry: a String per item (68k on an 1800 island) held the UI
        // thread 10-40 ms whenever models landed.
        let files: Vec<String> = (self.island.doc.meshes.iter())
            .map(|m| m.replace('\\', "/"))
            .collect();
        let props = (self.island.doc.props.iter())
            .filter_map(|p| Some((files.get(p.source.file as usize)?.as_str(), Some(&p.matrix))));
        let assets = self.assets.as_deref();
        let objects: Vec<(std::borrow::Cow<str>, Mat4)> = (self.island.doc.objects.iter())
            .filter_map(|o| {
                let cfgs =
                    (assets.and_then(|a| a.get(o.guid))).map_or(&[][..], |a| a.cfgs.as_slice());
                let cfg = o.model(cfgs)?;
                let cfg = match cfg.contains('\\') {
                    true => std::borrow::Cow::Owned(cfg.replace('\\', "/")),
                    false => std::borrow::Cow::Borrowed(cfg),
                };
                Some((cfg, doc::trs(o.position, o.rotation, o.scale)))
            })
            .collect();
        let grown = (self.vegetation.as_ref())
            .filter(|_| self.layers.vegetation)
            .into_iter()
            .flat_map(|p| p.models.keys().map(|c| (c.as_str(), None)));
        let batch = self.library.batch(
            props
                .chain(objects.iter().map(|(c, m)| (c.as_ref(), Some(m))))
                .chain(grown)
                .chain(rails.iter().map(|c| (c.as_str(), None))),
        );
        if batch.is_empty() {
            self.models_settled = Some(key);
            return None;
        }
        self.loading_models = true;
        Some(self.library.job(install, batch))
    }

    pub fn models_loaded(&mut self, results: Vec<(String, Option<models::Loaded>)>) {
        self.loading_models = false;
        self.library.models.finish(results);
        self.pieces_stale = true;
        self.scene_stale = true;
    }

    pub fn models_pending(&self) -> usize {
        self.library.models.busy()
    }

    /// The ground vegetation preview for the current terrain and prop set;
    /// `None` with the layer off or without material maps, palette or prop
    /// sets.
    fn vegetation_preview(&self) -> Option<Arc<vegetation::Preview>> {
        self.vegetation_ground(|set, ground| {
            Arc::new(vegetation::scatter(set, ground, vegetation::MAX_INSTANCES))
        })
    }

    /// While a stroke or drag runs: the preview scattered again only where
    /// the island changed under `changed` (world `[x0, z0, x1, z1]`). The
    /// stroke's end scatters the whole island again, which also thins it
    /// to the bound exactly.
    fn regrow_vegetation(&self, changed: &[[f32; 4]]) -> Option<Arc<vegetation::Preview>> {
        let mut preview = vegetation::Preview::clone(self.vegetation.as_deref()?);
        self.vegetation_ground(|set, ground| {
            vegetation::rescatter(&mut preview, set, ground, changed);
        })?;
        Some(Arc::new(preview))
    }

    /// Runs `f` on the prop set and the ground the vegetation grows from.
    fn vegetation_ground<R>(
        &self,
        f: impl FnOnce(&vegetation::PropSet, &vegetation::Ground) -> R,
    ) -> Option<R> {
        if !self.layers.vegetation {
            return None;
        }
        let set = self
            .prop_sets
            .as_ref()?
            .get(&self.island.doc.session.vegetation_prop_set)?;
        let statics: Vec<([f32; 2], f32)> = self
            .island
            .doc
            .props
            .iter()
            .map(|p| {
                let m = &p.matrix;
                ([m[12], m[14]], math::length([m[0], m[1], m[2]]))
            })
            .collect();
        let ground = vegetation::Ground {
            field: &self.island.doc.field,
            materials: self.island.doc.materials.as_ref()?,
            layers: &self.palette.as_ref()?.ground_layers,
            statics: &statics,
        };
        Some(f(set, &ground))
    }

    /// Marks where a stroke or drag changed what the vegetation grows from.
    /// A dab's window overlaps the one before and joins it; props dragged
    /// apart keep a window each, so a union does not span the island.
    fn vegetation_changed(&mut self, w: [f32; 4]) {
        if !self.layers.vegetation {
            return;
        }
        let overlaps = |o: &[f32; 4]| w[0] <= o[2] && o[0] <= w[2] && w[1] <= o[3] && o[1] <= w[3];
        match self.vegetation_window.last_mut() {
            Some(o) if overlaps(o) => {
                *o = [
                    o[0].min(w[0]),
                    o[1].min(w[1]),
                    o[2].max(w[2]),
                    o[3].max(w[3]),
                ];
            }
            _ => self.vegetation_window.push(w),
        }
    }

    /// Instances in the ground vegetation preview and candidates before the
    /// bound, for the scene panel.
    pub fn vegetation_count(&self) -> Option<(usize, usize)> {
        self.vegetation
            .as_ref()
            .filter(|_| self.layers.vegetation)
            .map(|p| (p.len(), p.candidates))
    }

    /// Sets the island's vegetation prop set or ambient, as one undo step.
    pub fn edit_session(
        &mut self,
        f: impl FnOnce(&mut anno_formats::a7m::session::SessionSettings),
    ) {
        let mut next = self.island.doc.session.clone();
        f(&mut next);
        if next == self.island.doc.session {
            return;
        }
        self.checkpoint();
        self.island.doc.session = next;
        self.scene_stale = true;
    }

    pub fn triangles(&self) -> usize {
        let tiles: usize = self.tiles.iter().map(|m| m.data().indices.len() / 3).sum();
        let grid = self.island.doc.field.grid;
        // GPU terrain: what full detail would draw.
        let terrain = if self.terrain.is_some() {
            (grid.width - 1) * (grid.depth - 1) * 2
        } else {
            0
        };
        tiles + terrain + self.shipped.triangles()
    }

    /// A ground bake of the current materials, to run off the UI thread:
    /// the texture for cliff tops, map images and non-splat shading, and the
    /// GPU splat when it is missing or stale. Paint strokes keep the splat
    /// current themselves, so after one only the texture is redone.
    pub fn bake_job(&mut self) -> Option<impl FnOnce() -> Result<Ground> + Send + 'static> {
        self.ground_stale = false;
        let rebuild_splat = self.splat.is_none() || std::mem::take(&mut self.splat_stale);
        let (wanted, had_splat) = (self.bake_wanted(), self.splat.is_some() && !rebuild_splat);
        let palette = self.palette.clone()?;
        let m = self.island.doc.materials.as_ref()?;
        // The chunks shared, not copied out: two flat copies of the
        // continental island's maps held the UI thread 40-90 ms and 190 MB.
        // Only a7m paint bakes: 2070's draws through its own ground.
        let a = m.paint.a7m()?;
        let (ids, alphas) = (a.ids.clone(), a.alphas.clone());
        let grids = self.island.doc.grids.clone();
        let field = self.island.doc.field.grid;
        let size = field.size()[0];
        let bake = self.a7m_rules()?.ground_bake;
        Some(move || {
            let splat = rebuild_splat
                .then(|| {
                    let grit = grids.as_ref().and_then(|g| g.layer(Kind::Grit));
                    palette
                        .splat(bake, &ids, &alphas, grit)
                        .map_err(|e| eprintln!("splat: {e:#}"))
                        .ok()
                })
                .flatten();
            // A splat that failed leaves the terrain to the texture.
            let wanted = wanted || (splat.is_none() && !had_splat);
            Ok(Ground {
                texture: wanted
                    .then(|| palette.bake(&ids, &alphas, size))
                    .transpose()?,
                splat,
                sink: palette.sinking(&ids, &alphas, field).map(Arc::new),
            })
        })
    }

    /// Whether anything draws the baked ground: the shipped cliff tops, or
    /// the terrain when it has no splat. With the splat drawing and the
    /// cliff tops hidden (the default) the bake is skipped, 36 MB on the
    /// continental island and most of each stroke's bake job.
    fn bake_wanted(&self) -> bool {
        self.layers.shipped || self.splat.is_none()
    }

    pub fn set_ground(&mut self, ground: Ground) {
        let Ground {
            texture,
            splat,
            sink,
        } = ground;
        self.ground = texture.map(Arc::new);
        // Sinking ground (marsh) moves the drawn terrain: its heights go to
        // the GPU anew.
        let sink = sink.filter(|s| s.len() == self.island.doc.field.heights.len());
        if sink.is_some() || self.sink.is_some() {
            self.sink = sink;
            self.rebuild_tiles(None);
        }
        match splat {
            Some((mut splat, planes)) => {
                if let Some(r) = splat.relief.as_mut() {
                    r.on = self.layers.relief;
                }
                // Painted meanwhile, the maps moved on from what it was
                // built from.
                if let Some(Materials {
                    paint: Paint::A7m(a),
                    ..
                }) = &self.island.doc.materials
                {
                    planes.set(&a.ids, &a.alphas);
                }
                let splat = Arc::new(splat);
                self.splat_ground = anno_render::a7m::ground(&splat);
                self.splat = Some(splat);
                self.splat_planes = Some(planes);
                self.retexture_tiles();
            }
            // Terrain draws with the texture: every tile changes.
            None if self.splat.is_none() || self.shading != Shading::Ground => {
                self.retexture_tiles()
            }
            // Terrain draws with the live splat; only cliff tops use the
            // texture.
            None => {
                self.refresh_shipped_meshes();
                self.scene_stale = true;
            }
        }
    }

    fn extent(&self) -> Extent {
        let g = &self.island.doc.field.grid;
        let [w, d] = g.size();
        Extent {
            center: [g.origin[0] + w / 2., 0., g.origin[1] + d / 2.],
            size: w.max(d),
        }
    }

    /// What the game's ground says of how this island's paint shows in
    /// game (2070's magenta stand-ins).
    fn placeholder_note(&self) -> Option<String> {
        let paint = &self.island.doc.materials.as_ref()?.paint;
        self.own_ground.as_ref()?.note(paint)
    }

    /// Whether paint id `id` draws the developers' stand-in in game.
    pub(crate) fn placeholder_layer(&self, id: u8) -> bool {
        (self.own_ground.as_ref()).is_some_and(|g| g.stand_in(id))
    }

    /// Hands the paint changed within world `area` to what draws it: the
    /// a7m splat recomputes its control texels there in place, a game's
    /// own ground uploads its coverage there. False where neither holds
    /// it, so the splat rebuilds whole.
    pub(crate) fn show_paint(&self, area: [f32; 4]) -> bool {
        let Some(m) = &self.island.doc.materials else {
            return false;
        };
        if let Some(g) = &self.own_ground {
            g.update(&m.paint, area);
            return true;
        }
        let (Some(palette), Some(splat), Some(planes), Some(a)) = (
            &self.palette,
            &self.splat,
            &self.splat_planes,
            m.paint.a7m(),
        ) else {
            return false;
        };
        let size = self.island.doc.field.grid.size()[0];
        palette.update_splat(splat, planes, &a.ids, &a.alphas, size, area);
        true
    }

    pub fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error));
    }

    /// Records `before`, the document before an edit just made, as an
    /// undo step.
    fn checkpoint_owned(&mut self, before: Doc) {
        self.history.checkpoint_owned(before);
        self.patch_history();
    }

    /// Records the document as it is, before an edit.
    fn checkpoint(&mut self) {
        self.history.checkpoint(&self.island.doc);
        self.patch_history();
    }

    /// Stores undo steps' heights as patches against the step after each,
    /// newest first, down to the first already stored so. A step that
    /// still shares the live heights may be the snapshot before an edit
    /// not yet made: it waits for the next checkpoint.
    fn patch_history(&mut self) {
        self.trim_heap();
        let live = self.island.doc.field.clone();
        let mut next = live.clone();
        for (k, snapshot) in self.history.past_mut().enumerate() {
            if snapshot.heights_patch.is_some() {
                break;
            }
            let own = snapshot.field.clone();
            if !(k == 0 && Arc::ptr_eq(&own, &live)) {
                snapshot.patch_heights(&next);
            }
            next = own;
        }
    }

    /// Hands what edits freed back to the system, every few seconds at
    /// most: without, the process kept growing by what each stroke's
    /// temporaries and dropped undo steps had held.
    fn trim_heap(&mut self) {
        if self.trimmed.is_none_or(|t| t.elapsed().as_secs() >= 2) {
            self.trimmed = Some(std::time::Instant::now());
            gpui_editor_kit::gpu::trim_heap();
        }
    }

    pub fn undo(&mut self, redo: bool) -> bool {
        // An open river draft undoes its own edits, never the document's.
        if let Some(done) = self.river_undo(redo) {
            return done;
        }
        // A stroke still dragging becomes its own step first, so steps'
        // height patches meet the heights they were taken against.
        self.end_drags();
        // The step being left keeps land grids that match its heights.
        self.sync_land();
        let left = self.history.revision();
        // The step left, to update only what the restored one changes. All
        // copy-on-write: a few thousand reference counts.
        let before = self.island.doc.clone();
        let done = if redo {
            self.history.redo(&mut self.island.doc)
        } else {
            self.history.undo(&mut self.island.doc)
        };
        if !done {
            return false;
        }
        self.trim_heap();
        // The step restored may hold its heights as a patch against the
        // one left, which now waits on the other stack: store that one as a
        // patch against the restored heights in turn.
        self.island.doc.unpatch_heights(&before.field);
        let restored = self.island.doc.field.clone();
        let other = if redo {
            self.history.past_mut().next()
        } else {
            self.history.future_mut().next()
        };
        if let Some(other) = other {
            other.patch_heights(&restored);
        }
        let doc = &self.island.doc;
        self.selection.retain(|&item| doc.matrix(item).is_some());
        // Materials may have changed back. Like a dab, the live splat takes
        // the changed texels in place, so the GPU re-bakes only the ground
        // pages over them; a new splat re-baked every page, and the undo
        // showed only once a whole off-thread rebuild had landed. The
        // texture bake (cliff tops, sinking) still follows, as after a
        // stroke.
        let painted = match (&before.materials, &self.island.doc.materials) {
            (Some(a), Some(b)) => match a.changed(b) {
                MaterialChange::Nothing => false,
                MaterialChange::Area(area) => {
                    if !self.show_paint(area) {
                        self.splat_stale = true;
                    }
                    self.ground_stale = true;
                    true
                }
                MaterialChange::Whole => {
                    if let (Some(g), Some(m)) = (&self.own_ground, &self.island.doc.materials) {
                        let [w, d] = self.island.doc.field.grid.size();
                        g.update(&m.paint, [0., 0., w, d]);
                    }
                    self.ground_stale = true;
                    self.splat_stale = true;
                    true
                }
            },
            (None, None) => false,
            _ => {
                self.ground_stale = true;
                self.splat_stale = true;
                true
            }
        };
        fn same<T>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (a, b) => a.is_none() && b.is_none(),
            }
        }
        let same_grids = same(&before.grids, &self.island.doc.grids);
        let same_tint = same(&before.tint, &self.island.doc.tint);
        // 2070's path blockers, which the ground shows while they're painted.
        let paths = match (before.blockers(), self.island.doc.blockers()) {
            (Some(a), Some(b)) => a.changed(b).0,
            _ => None,
        };
        let grid = before.field.grid;
        let heights = (grid == self.island.doc.field.grid)
            .then(|| changed_samples(grid, &before.field.heights, &self.island.doc.field.heights));
        // Materials shading colors tiles from the material maps.
        if heights.is_none() || !same_grids || !same_tint || self.shading == Shading::Materials {
            self.rebuild_tiles(None);
        } else if let Some(Some(rect)) = heights {
            // A sculpt step: the tiles under it, as a stroke updates them.
            if let Some(gpu) = &self.gpu_heights {
                gpu.update(rect, &self.drawn_field().heights);
            }
            self.rebuild_tiles(Some(rect));
        }
        if let Some(cells) = paths.filter(|_| self.path_overlay().is_some()) {
            // Two cells a metre; the samples on both sides of each.
            let rect = Rect {
                x0: cells.x0 / 2,
                z0: cells.z0 / 2,
                x1: (cells.x1.div_ceil(2) + 1).min(grid.width),
                z1: (cells.z1.div_ceil(2) + 1).min(grid.depth),
            };
            self.rebuild_tiles(Some(rect));
        }
        let sculpted = heights != Some(None);
        // Props and objects may have moved back, and the vegetation grows
        // from the heights, grids and ground. A paint step changes only the
        // ground: it rescatters as a stroke's release does when vegetation
        // shows, and otherwise skips rebuilding every placement row (4-10
        // ms of refresh_scene on the pirate island).
        let placed_same = before.props.same(&self.island.doc.props)
            && Arc::ptr_eq(&before.objects, &self.island.doc.objects)
            && Arc::ptr_eq(&before.meshes, &self.island.doc.meshes)
            && before.session == self.island.doc.session;
        if !(placed_same && !sculpted && same_grids && same_tint)
            || (painted && self.layers.vegetation)
        {
            self.scene_stale = true;
        }
        // The selection may have lost items, a river draft may reopen.
        self.markers_stale = true;
        // So may the grit map; its mask on the GPU follows now, not at the
        // next splat rebuild. Only the cells that changed: a write re-bakes
        // the ground pages under it.
        if let Some(window) =
            changed_grit(before.grids.as_deref(), self.island.doc.grids.as_deref())
        {
            self.sync_grit(window);
        }
        self.river_written(left, redo);
        true
    }

    /// Writes the grit map's cells under `window` (island metres x0, z0,
    /// x1, z1; everything without one) into the live splat's grit mask, so
    /// a Grid stroke on the detail layer shows on the next frame.
    fn sync_grit(&mut self, window: Option<[f32; 4]>) {
        let Some(mask) = (self.splat.as_ref())
            .and_then(|s| s.grit.as_ref())
            .map(|g| g.mask.clone())
        else {
            return;
        };
        let Some(layer) = (self.island.doc.grids.as_ref()).and_then(|g| g.layer(Kind::Grit)) else {
            return;
        };
        let (xs, ys) = layer.cells_within(window);
        if xs.is_empty() || ys.is_empty() {
            return;
        }
        let texels = terrain::grit_texels(layer, (xs.clone(), ys.clone()));
        let rect = Rect {
            x0: xs.start,
            z0: ys.start,
            x1: xs.end,
            z1: ys.end,
        };
        mask.write(rect, &texels);
    }

    /// Points every tile, and the shipped meshes' tops, at the current
    /// ground texture or splat, keeping their geometry: a re-bake changes
    /// only the material, and re-meshing a 1025² island costs ~50 ms.
    /// Meshes built without UVs (height shading) are re-meshed when a
    /// texture now needs them.
    fn retexture_tiles(&mut self) {
        let splat = self
            .splat
            .clone()
            .filter(|_| self.shading == Shading::Ground);
        let texture = if splat.is_some() {
            None
        } else {
            self.texture()
        };
        let textured = texture.is_some();
        if self.terrain.is_some() {
            // GPU terrain takes the material whole (its UVs span the grid);
            // its tiles are empty, so only the shipped tops below remain.
            self.terrain = self.make_terrain();
        } else if self.tiles.is_empty()
            || self
                .tiles
                .iter()
                .any(|t| textured && t.data().uvs.is_empty())
        {
            return self.rebuild_tiles(None);
        }
        // The splat draws only as the GPU terrain's ground (render's a7m
        // ground); CPU tiles, kept for heights the GPU can't hold, show
        // the ground's colours as that terrain does while its ground loads.
        let material = gpu::Material::Lit;
        let texture = texture.map(gpui_editor_kit::texture::Image::from);
        retexture(&mut self.tiles, |data| {
            Some(MeshData {
                texture: texture.clone(),
                material: material.clone(),
                ..data.clone()
            })
        });
        // Shipped tops sample the ground texture (never the splat); walls
        // carry rock and stay.
        let ground = self.texture().map(gpui_editor_kit::texture::Image::from);
        if self
            .shipped_meshes
            .iter()
            .flatten()
            .any(|m| !m.data().ignore_alpha && ground.is_some() && m.data().uvs.is_empty())
        {
            self.refresh_shipped_meshes();
        } else {
            let mut meshes: Vec<Arc<Mesh>> =
                self.shipped_meshes.iter().flatten().cloned().collect();
            retexture(&mut meshes, |data| {
                (!data.ignore_alpha).then(|| MeshData {
                    texture: ground.clone(),
                    ..data.clone()
                })
            });
            let mut meshes = meshes.into_iter();
            for tile in &mut self.shipped_meshes {
                for mesh in tile {
                    *mesh = meshes.next().expect("same count");
                }
            }
        }
        self.scene_stale = true;
    }

    /// Writes the edited island back into its archive and stages it with
    /// every file derived from it: material levels, regenerated terrain
    /// meshes and normal pages, and re-baked tint tiles.
    pub fn stage(&mut self, install: &Install, project: &mut Project) -> Result<()> {
        self.sync_land();
        let mut warnings = Vec::new();
        if self.is_new
            && let Some(warning) = self.prepare_new_island()
        {
            warnings.push(warning);
        }
        // Saving is the only way to keep work, so what would stop the island
        // leaving the editor only warns here; the listing keeps it for
        // export, test maps and Test in game, which it blocks.
        self.listing.broken = self.validate(install, project);
        if !self.listing.broken.is_empty() {
            warnings.push(format!(
                "It can't be installed or tested in game until fixed: {}",
                said(&self.listing.broken)
            ));
        }
        // Only a warning: the game loads the island and draws them.
        warnings.extend(self.placeholder_note());
        if !warnings.is_empty() {
            self.say(warnings.join(". "), true);
        }
        let Some(Native::A7m(loaded)) = &self.island.native else {
            return self.stage_native(project);
        };
        // Every stage starts from the same tree. Differential grid/object
        // writers must also be able to restore values after undoing a save.
        let mut source = loaded.clone();
        source.unpark()?;
        if let Some(store) = &mut self.island.object_store {
            store.objects.clone_from(&self.island.doc.objects);
            store.write(source.gamedata_mut())?;
        }
        self.derive_water();
        let levels = self.island.doc.write_into(&mut source, Some(install))?;
        if let Some(grids) = &self.island.doc.grids {
            grids.write(&mut source)?;
        }
        // An island that names no set keeps naming none while it paints
        // with the game's default.
        let set = &self.island.material_set;
        let a7m = self.a7m_rules().context("a7m material set")?;
        if a7m.material_set(source.material_set_file_name()) != set {
            source.set_material_set_file_name(set);
        }
        let base = island_data_dir(&self.path)?;
        let mut current = |rel: &str| {
            let path = format!("{base}/{rel}");
            self.terrain_original
                .entry(path.clone())
                .or_insert_with(|| {
                    (project.read_file(&path).map(std::borrow::Cow::into_owned))
                        .or_else(|| install.read(&path).ok())
                })
                .clone()
        };
        let mut terrain = source.rebuild_meshes(&mut current).context("remeshing")?;
        terrain.extend(
            source
                .rebuild_height_pages(&mut current)
                .context("normal pages")?,
        );
        let coverage_changed = (self.island.a7m())
            .is_none_or(|loaded| source.active_map().data != loaded.active_map().data);
        let tint = match (&self.island.doc.tint, &self.tint_loaded) {
            (Some(now), Some(loaded)) if Arc::ptr_eq(now, loaded) && !coverage_changed => None,
            (Some(now), _) => Some(anno_formats::a7m::tint::bake(
                self.a7m_rules().context("a7m tint")?.tint_base_level,
                now,
                source.active_map(),
                source.world_size(),
            )),
            (None, _) => None,
        };
        let images = if self.is_new || !terrain.is_empty() || tint.is_some() {
            let ground = self
                .palette
                .as_ref()
                .zip(self.island.doc.materials.as_ref())
                // A 2070 island has no palette, and ships no map images.
                .and_then(|(p, m)| Some((p, m.paint.a7m()?)))
                .map(|(p, a)| p.bake(&a.ids, &a.alphas, self.world()[0] as f32))
                .transpose()?;
            new::map_images(
                ground.as_ref().or(self.ground.as_deref()),
                &self.island.doc.field,
            )
        } else {
            Vec::new()
        };
        let (gamedata, mut rd3d) = source.encode()?;
        let grids_in_rd3d = self
            .island
            .doc
            .grids
            .as_ref()
            .is_some_and(|g| g.layers.iter().any(|l| l.is_edited() && l.in_rd3d()));
        // TODO: the island already decoded rd3d when it loaded; this second
        // decode and encode goes once `Island` hands out its rd3d tree to edit.
        if grids_in_rd3d || tint.is_some() {
            let mut tree = anno_formats::adf::Adf::parse(&rd3d)?;
            if let Some(grids) = &self.island.doc.grids {
                grids.write_rd3d(&mut tree)?;
            }
            if let Some(tint) = &tint {
                tint.rd3d.write(&mut tree)?;
            }
            rd3d = tree.encode();
        }
        for tile in tint.as_ref().into_iter().flat_map(|t| &t.tiles) {
            let path = tile.path(&base);
            self.terrain_original
                .entry(path.clone())
                .or_insert_with(|| {
                    (project.read_file(&path).map(std::borrow::Cow::into_owned))
                        .or_else(|| install.read(&path).ok())
                });
        }
        for (rel, _) in &images {
            let path = format!("{base}/{rel}");
            self.terrain_original
                .entry(path.clone())
                .or_insert_with(|| {
                    (project.read_file(&path).map(std::borrow::Cow::into_owned))
                        .or_else(|| install.read(&path).ok())
                });
        }
        // A reverted node/page may no longer be emitted by a differential
        // writer. Restore its original file before overlaying this stage.
        for (path, bytes) in &self.terrain_original {
            match bytes {
                Some(bytes) => project.stage(path, bytes.clone()),
                None => {
                    project.remove_file(path);
                }
            }
        }
        for (level, (ids, alphas)) in levels.into_iter().enumerate() {
            project.stage(&format!("{base}/materialmaps/{level}/matid.dds"), ids);
            project.stage(&format!("{base}/materialmaps/{level}/matalpha.dds"), alphas);
        }
        for file in terrain {
            project.stage(&format!("{base}/{}", file.path), file.bytes);
        }
        for (rel, bytes) in images {
            project.stage(&format!("{base}/{rel}"), bytes);
        }
        for tile in tint.into_iter().flat_map(|t| t.tiles) {
            project.stage(&tile.path(&base), tile.dds);
        }
        // Presence flags suppress unused shipped tiles. Drop obsolete loose
        // copies too, so repeated expansion/shrinking does not grow the mod.
        let tree = anno_formats::adf::Adf::parse(&rd3d)?;
        let flags = anno_formats::a7m::tint::Rd3dTint::read(&tree)?.flags;
        let prefix = format!("{base}/tintmaps/");
        project.retain_files(|path| {
            let Some(tile) = path.strip_prefix(&prefix) else {
                return true;
            };
            let Some((level, name)) = tile.split_once('/') else {
                return true;
            };
            let Some((x, y)) = name.strip_suffix(".dds").and_then(|n| n.split_once('x')) else {
                return true;
            };
            let (Ok(level), Ok(x), Ok(y)) = (
                level.parse::<usize>(),
                x.parse::<usize>(),
                y.parse::<usize>(),
            ) else {
                return true;
            };
            flags
                .get(level)
                .is_some_and(|f| x < f.width && y < f.height && f.get(x, y) != 0)
        });
        if self.is_new || self.a7m_rules().is_some_and(|a| a.info_from_island) {
            self.stage_island_side_files(install, project, &source)?;
        }
        // Shipped islands store each member as its own zlib block.
        let mut packer = rda::Packer::new(rda::PackOptions::island());
        packer.add("gamedata.data", &gamedata)?;
        packer.add("rd3d.data", &rd3d)?;
        project.stage(&self.path, packer.encode()?);
        if let Some(xml) = &self.island.own_set {
            project.stage(&self.island.material_set, xml.clone().into_bytes());
        }
        self.history.mark_saved();
        self.listing_dirty = false;
        Ok(())
    }

    /// Saves an island its game's codec writes (2070): the file as loaded
    /// with the document's edits, staged at its path. Nothing else is
    /// derived from it, so nothing else is staged.
    fn stage_native(&mut self, project: &mut Project) -> Result<()> {
        let islands = (self.module.islands()).context("this game has no islands to save")?;
        let codec = (islands.codec().own()).context("an a7m island saves through `stage`")?;
        let bytes = codec.write(&self.island, &self.path, self.assets.as_deref())?;
        project.stage(&self.path, bytes);
        self.history.mark_saved();
        self.listing_dirty = false;
        if self.is_new {
            project.register_island(crate::project::IslandEntry {
                path: self.path.clone(),
                guid: project.island_guid(&self.path),
                region: self.region(),
                size: self.land_class().to_owned(),
                listing: self.listing.clone(),
            });
        }
        Ok(())
    }

    /// Rebuilds the water render bytes that follow the coast before a save.
    /// The source keeps the loaded heights that the stored bytes belong to:
    /// only cells whose derived value changed are
    /// rewritten, so shipped bytes survive where the terrain did not move.
    /// New islands rebuild everything, their bytes came from the template.
    fn derive_water(&mut self) {
        let (Some(g), Some(Native::A7m(source))) =
            (&mut self.island.doc.grids, &self.island.native)
        else {
            return;
        };
        let saved = source.heights();
        let was =
            (!self.is_new && saved.data.len() == self.island.doc.field.heights.len()).then(|| {
                anno_formats::a7m::water::Heights {
                    data: &saved.data,
                    width: saved.width,
                    spacing: self.island.doc.field.grid.spacing,
                }
            });
        if was.is_some_and(|w| w.data == self.island.doc.field.heights.as_slice()) {
            return;
        }
        anno_formats::a7m::water::derive(
            Arc::make_mut(g),
            new::heights(&self.island.doc.field),
            was,
            None,
        );
    }

    /// Side of the land's bounding square in metres: the active area's larger
    /// side. The game's size class follows it.
    pub fn land_extent(&self) -> u32 {
        let (mut lo, mut hi) = ([usize::MAX; 2], [0usize; 2]);
        let mut include = |x: usize, y: usize| {
            lo = [lo[0].min(x), lo[1].min(y)];
            hi = [hi[0].max(x + 1), hi[1].max(y + 1)];
        };
        let cell = if let Some(active) = self
            .island
            .doc
            .grids
            .as_ref()
            .and_then(|g| g.layer(Kind::ActiveMap))
        {
            for y in 0..active.height {
                for x in 0..active.width {
                    if active.get(x, y) != 0 {
                        include(x, y);
                    }
                }
            }
            active.cell_metres
        } else if let Some(source) = self.island.a7m() {
            let active = source.active_map();
            for (i, _) in active.data.iter().enumerate().filter(|(_, a)| **a) {
                include(i % active.width, i / active.width);
            }
            self.world()[0] as f32 / active.width.max(1) as f32
        } else {
            // No active map (a game with its own codec): the land is the
            // ground above the sea.
            let field = &self.island.doc.field;
            for z in 0..field.grid.depth {
                for x in 0..field.grid.width {
                    if field.at(x, z) > 0. {
                        include(x, z);
                    }
                }
            }
            field.grid.spacing
        };
        (hi[0].saturating_sub(lo[0]).max(hi[1].saturating_sub(lo[1])) as f32 * cell) as u32
    }

    /// `IslandPlaceholderSize` the land gives: `Small` … `XL`.
    pub fn land_class(&self) -> &'static str {
        self.rules().size_class(self.land_extent())
    }

    #[cfg(test)]
    pub(crate) fn renderer(&self) -> &Arc<Renderer> {
        &self.renderer
    }

    /// View `view`'s per-frame UI-thread times and camera moves since the
    /// last take (`ANNO_EDITOR_BENCH`).
    pub fn take_timing(&self, view: usize) -> gpui_editor_kit::gpu::PaneTiming {
        self.views[view].viewport.take_timing()
    }

    /// How long the UI waited for the renderer while a view's thread
    /// recorded, since the last take (`ANNO_EDITOR_BENCH`).
    pub fn take_blocked(&self) -> std::time::Duration {
        self.renderer.take_blocked()
    }

    /// The renderer's smoothed frame times, for the viewport readout.
    pub fn frame_stats(&self) -> gpui_editor_kit::gpu::Stats {
        self.renderer.stats()
    }

    /// Waves or uploads still want a present. The readout can follow them
    /// without rebuilding the window.
    pub fn wants_frames(&self) -> bool {
        self.renderer.wants_frames()
    }

    /// Draws with `graphics` from the next refresh on.
    pub fn set_graphics(&mut self, graphics: crate::graphics::Graphics) {
        // Poses ride the renderer's pieces: turning animation on or off
        // hands them over again, as a batch of models landing does.
        if graphics.animation != self.graphics.animation {
            self.pieces_stale = true;
        }
        self.graphics = graphics;
        self.scene_stale = true;
    }

    /// The game this island belongs to.
    pub fn module(&self) -> &'static dyn Module {
        self.module
    }

    /// How this island's game lays out, names and generates islands. Every
    /// island editor's game has islands: loading asks for them.
    pub fn rules(&self) -> &'static anno_core::game::IslandRules {
        (self.module.islands())
            .expect("an island's game has islands")
            .rules()
    }

    /// The a7m pipeline's rules, for an island read through it; `None`
    /// for a game with its own codec.
    pub fn a7m_rules(&self) -> Option<&'static anno_core::game::A7mRules> {
        self.module.islands()?.codec().a7m()
    }

    /// Turns view `view`'s camera about its pivot by `radians`.
    pub fn turn(&mut self, view: usize, radians: f32) {
        self.views[view].camera.yaw += radians;
    }

    /// Whether models or their GPU copies are still loading.
    pub fn loading(&self) -> bool {
        self.models_pending() > 0 || self.renderer.pending()
    }

    /// Marks the end of building the window's elements.
    pub fn mark_built(&self) {
        self.renderer.mark_built();
    }

    /// Marks the start of a window frame: CPU frame time counts from here.
    pub fn mark_frame(&self) {
        self.renderer.mark_frame();
    }

    /// The game's region, from the island path.
    /// The pointer left the viewport: nothing is under it any more, so the
    /// hovered water and the cursor readout clear.
    pub(crate) fn pointer_left(&mut self) {
        self.hover_water = None;
        self.cursor = None;
    }

    /// The hierarchy group an item is listed under.
    pub(crate) fn item_group(&self, item: Item) -> Option<Group> {
        match item {
            Item::Prop(id) => self.island.doc.prop(id).map(|p| Group::Prop(p.source.file)),
            Item::Object(id) => (self.island.doc.objects.iter())
                .find(|o| o.id == id)
                .map(|o| Group::Object(o.guid)),
        }
    }

    pub fn region(&self) -> String {
        self.rules().island_region(&self.path).to_owned()
    }

    /// The region as the assets name it (AssociatedRegions, fertility sets).
    pub fn asset_region(&self) -> String {
        let rules = self.rules();
        rules
            .asset_region(rules.island_region(&self.path))
            .to_owned()
    }

    /// What stops the island leaving the editor (level 5), its references
    /// resolved against `install` and the files `project` holds. A project
    /// island is used as its listing offers it; a shipped one as its kind
    /// says, never as a start (its role isn't the editor's to know).
    pub fn validate(&self, install: &Install, project: &Project) -> Vec<Problem> {
        let Some(islands) = self.module.islands() else {
            return Vec::new();
        };
        let rules = islands.rules();
        let region = self.region();
        let use_ = if self.is_new {
            let start = self.listing.is_start(rules);
            Use {
                region: &region,
                start,
                playable: start || self.listing.is_regular(rules),
            }
        } else {
            let asset_kind = (self.assets.as_deref())
                .and_then(|a| {
                    a.iter()
                        .find_map(|a| a.island.as_ref().filter(|i| i.path == self.path))
                })
                .map(|i| i.kind.as_str());
            Use {
                region: &region,
                start: false,
                playable: islands.kind_of(&self.path, asset_kind) == IslandKind::Playable,
            }
        };
        let has = |path: &str| project.has_file(path);
        let refs = Refs {
            install,
            project: &has,
            assets: self.assets.as_deref(),
        };
        islands.validate(&self.island, &refs, use_)
    }

    /// Whether the island is one the game does not ship (made in the editor).
    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Takes over a reopened project island: its settings, and saving it as
    /// a new island (headers, map images, every terrain tile).
    pub fn adopt(&mut self, entry: &crate::project::IslandEntry) {
        self.listing = entry.listing.clone();
        // Saved positions survive the f32 round trip to well under 1 cm.
        for p in self.island.doc.props.iter_mut() {
            let t = math::translation(&p.matrix);
            p.auto_cliff = entry
                .listing
                .auto_cliffs
                .iter()
                .any(|a| (0..3).all(|i| (a[i] - t[i]).abs() < 0.01));
        }
        self.is_new = true;
        if let Some(source) = self.island.a7m_mut() {
            source.mark_unshipped();
        }
    }

    /// Changes a setting as an unsaved edit, so Save writes the project entry.
    pub fn set_listing(&mut self, f: impl FnOnce(&mut Listing)) {
        f(&mut self.listing);
        self.listing_dirty = true;
    }

    /// Placement metadata, plus new-island map images and pool registration.
    fn stage_island_side_files(
        &self,
        install: &Install,
        project: &mut Project,
        source: &Island,
    ) -> Result<()> {
        use anno_formats::a7m::a7minfo::{
            A7mInfo, A7me, RandomSlotObject, Slots, TypedSlotGroup, TypedSlotObject,
        };
        let world = self.world();
        let stem = self.path.trim_end_matches(".a7m");
        let a7me = format!("{stem}.a7me");
        let active = source.active_map();
        let mut info = if self.a7m_rules().is_some_and(|a| a.info_from_island) {
            A7mInfo::parse(
                &source
                    .info()
                    .context("1800 island has no placement header")?
                    .encode(),
            )?
        } else {
            A7mInfo::new(world, &a7me)
        };
        info.map_size = world;
        info.source_map = a7me.replace('/', "\\");
        if self.is_new {
            info.default_session_guid = None;
        }
        info.set_active_map(active.width as u32, active.height as u32, &active.data)?;
        // The map generator only activates slots listed here (shipped Celtic
        // islands list their marsh slots too). The harbour records name
        // them; assets.xml covers slots of other kinds.
        let lib = self.island.harbour.clone();
        let assets = self.assets.clone();
        info.slots = match &info.slots {
            Slots::Random(_) => {
                Slots::Random(harbour::slot_groups(&self.island.doc.objects, |g| {
                    self.is_slot(g)
                }))
            }
            Slots::Typed(old) => {
                let mut known: std::collections::BTreeMap<_, _> = old
                    .iter()
                    .flat_map(|g| g.objects.iter().map(|o| (o.guid, g.kind)))
                    .collect();
                if let Some(lib) = &lib {
                    for group in lib.typed_slots(&self.island.doc.objects) {
                        known.extend(group.objects.iter().map(|o| (o.guid, group.kind)));
                    }
                }
                // Imported slot records may come from another island. Read
                // its small placement header for the engine's slot-kind ID.
                let mut missing: std::collections::BTreeSet<_> = self
                    .island
                    .doc
                    .objects
                    .iter()
                    .filter(|o| {
                        assets
                            .as_ref()
                            .and_then(|a| a.get(o.guid))
                            .is_some_and(|a| a.slot_type.is_some())
                    })
                    .map(|o| o.guid)
                    .filter(|g| !known.contains_key(g))
                    .collect();
                if !missing.is_empty() {
                    for id in install.search(".a7minfo", 100_000) {
                        let bytes = install.read_file(id)?;
                        let header = A7mInfo::parse(&bytes)?;
                        if let Slots::Typed(groups) = header.slots {
                            for group in groups {
                                for object in group.objects {
                                    if missing.remove(&object.guid) {
                                        known.insert(object.guid, group.kind);
                                    }
                                }
                            }
                        }
                        if missing.is_empty() {
                            break;
                        }
                    }
                    anyhow::ensure!(
                        missing.is_empty(),
                        "no shipped placement metadata for slots {missing:?}"
                    );
                }
                let mut groups = std::collections::BTreeMap::<u16, Vec<TypedSlotObject>>::new();
                for o in self.island.doc.objects.iter() {
                    if let Some(&kind) = known.get(&o.guid) {
                        groups.entry(kind).or_default().push(TypedSlotObject {
                            guid: o.guid,
                            object: RandomSlotObject {
                                object_id: o.id,
                                position: o.position,
                            },
                        });
                    }
                }
                Slots::Typed(
                    groups
                        .into_iter()
                        .map(|(kind, objects)| TypedSlotGroup { kind, objects })
                        .collect(),
                )
            }
        };
        let start_guid = lib.as_ref().and_then(|l| l.guid(Piece::StartCoast));
        let start = (self.island.doc.objects.iter())
            .find(|o| Some(o.guid) == start_guid || self.piece(o.guid) == Some(Piece::StartCoast));
        if let Some(curve) = start.and_then(|o| o.path.as_deref()) {
            let [x0, z0, x1, z1] = info.active_rect.map(|v| v as f32);
            info.start_coast_direction = Some(harbour::start_coast_direction(
                curve,
                [(x0 + x1) / 2., (z0 + z1) / 2.],
            ));
        }
        project.stage(&format!("{stem}.a7minfo"), info.encode());
        if !self.is_new {
            return Ok(());
        }
        project.stage(&a7me, A7me::new(world).encode());
        // Resources of deleted slots are dropped here, once per save.
        let mut listing = self.listing.clone();
        listing
            .slot_resources
            .retain(|id, _| self.island.doc.objects.iter().any(|o| o.id == *id));
        listing.auto_cliffs = self
            .island
            .doc
            .props
            .iter()
            .filter(|p| p.auto_cliff)
            .map(|p| math::translation(&p.matrix))
            .collect();
        project.register_island(crate::project::IslandEntry {
            path: self.path.clone(),
            guid: project.island_guid(&self.path),
            region: self.region(),
            size: self.land_class().to_owned(),
            listing,
        });
        Ok(())
    }

    /// Replaces the heights through `f` as one undo step and redraws.
    pub fn replace_heights(&mut self, f: impl FnOnce(&mut Heightfield) -> Result<String>) {
        let before = self.island.doc.clone();
        match f(Arc::make_mut(&mut self.island.doc.field)) {
            Ok(mut done) => {
                self.block_lifted_from(&before.field);
                // An island made here has nothing hand-made in its land
                // grids yet: land, active area and buildable areas follow
                // the new terrain in the same undo step.
                if self.is_new && self.island.doc.grids.is_some() {
                    let environment = self.land_environment();
                    new::derive_land(&mut self.island.doc, environment);
                    self.derive_areas_from_slope();
                    done += "; land and buildable areas follow it";
                }
                self.sync_land();
                self.checkpoint_owned(before);
                self.rebuild_tiles(None);
                self.say(done, false);
            }
            Err(e) => {
                self.island.doc = before;
                self.say(format!("Import failed: {e:#}"), true);
            }
        }
    }

    /// Island size in metres, as the tint and grid formats take it.
    pub fn world(&self) -> [u32; 2] {
        match self.island.a7m() {
            Some(source) => source.world_size(),
            None => {
                let g = self.island.doc.field.grid;
                g.size().map(|s| s.round() as u32)
            }
        }
    }

    /// Replaces the tint as one undo step and redraws.
    pub fn replace_tint(&mut self, tint: Result<TintMap>, done: String) {
        match tint {
            Ok(map) => {
                self.checkpoint();
                self.island.doc.tint = Some(Arc::new(map));
                self.layers.tint = true;
                self.rebuild_tiles(None);
                self.say(done, false);
            }
            Err(e) => self.say(format!("Tint failed: {e:#}"), true),
        }
    }

    pub fn export_heightmap(&mut self, path: &std::path::Path) {
        let mut field = (*self.island.doc.field).clone();
        flip_north(&mut field);
        match files::export_heightmap(&field, path, HEIGHT_UNITS) {
            Ok(()) => self.say(format!("Heightmap written to {}", path.display()), false),
            Err(e) => self.say(format!("Export failed: {e:#}"), true),
        }
    }

    /// A drag that loses its pane must still commit or it would be lost to undo.
    pub fn end_drags(&mut self) {
        for i in 0..self.views.len() {
            self.finish_drag(i);
        }
    }

    // --- terrain mesh -------------------------------------------------------

    /// Shipped cliff meshes, textured with the current ground bake.
    /// Rebuilds the shipped meshes while their layer is shown, on every
    /// core: they are a reference most never turn on, and a large island's
    /// 2.4M triangles took 160 ms to split by facing on one.
    fn refresh_shipped_meshes(&mut self) {
        if !self.layers.shipped {
            self.shipped_meshes.clear();
            return;
        }
        let rock = self
            .palette
            .as_ref()
            .and_then(|p| p.rock())
            .filter(|_| self.shading == Shading::Ground);
        let texture = self.texture();
        let shipped = &self.shipped;
        let tiles: Vec<usize> = (0..shipped.len()).collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        self.shipped_meshes = std::thread::scope(|scope| {
            let workers: Vec<_> = tiles
                .chunks(tiles.len().div_ceil(threads).max(1))
                .map(|part| {
                    let texture = texture.as_ref();
                    scope.spawn(move || {
                        part.iter()
                            .map(|&i| shipped.tile_meshes(i, texture, rock))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().expect("shipped mesh builder"))
                .collect()
        });
    }

    /// Rebuilds the tiles under `rect`, or all of them.
    fn rebuild_tiles(&mut self, rect: Option<Rect>) {
        if rect.is_none() {
            // Whole-terrain rebuilds follow undo, imports and new islands:
            // start the GPU copy afresh rather than diffing.
            self.stale_colors = Rect::default();
            self.gpu_heights = gpu::HeightMap::new(&self.drawn_field())
                .map_err(|e| eprintln!("GPU heights unavailable, meshing on the CPU: {e:#}"))
                .ok()
                .map(Arc::new);
        }
        self.drawn_look = Some(self.look());
        if self.gpu_heights.is_some() {
            return self.recolor_terrain(rect);
        }
        let grid = self.island.doc.field.grid;
        let [tw, td] = grid.tile_count();
        let tiles: Vec<(usize, usize)> = match rect {
            Some(rect) => grid.tiles(rect),
            None => {
                self.tiles.clear();
                (0..td).flat_map(|z| (0..tw).map(move |x| (x, z))).collect()
            }
        };
        if rect.is_none() {
            self.refresh_shipped_meshes();
        }
        // Tiles are independent; a whole 1025² island is ~110 ms on one core.
        let built = {
            let inputs = self.tile_inputs();
            let inputs = &inputs;
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            let chunk = tiles.len().div_ceil(threads).max(1);
            std::thread::scope(|scope| {
                let workers: Vec<_> = tiles
                    .chunks(chunk)
                    .map(|part| {
                        scope.spawn(move || {
                            part.iter()
                                .map(|&(x, z)| inputs.tile_mesh(x, z))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .flat_map(|w| w.join().expect("tile builder"))
                    .collect::<Vec<_>>()
            })
        };
        if self.tiles.is_empty() {
            self.tiles = built;
        } else {
            for (&(x, z), mesh) in tiles.iter().zip(built) {
                self.tiles[z * tw + x] = mesh;
            }
        }
        self.scene_stale = true;
    }

    /// Recomputes the terrain colors under `rect` (samples), or everywhere
    /// with a new color map and terrain. Writing texels is all a brush
    /// stroke or tint dab costs; the renderer meshes nothing.
    fn recolor_terrain(&mut self, rect: Option<Rect>) {
        let grid = self.island.doc.field.grid;
        let whole = Rect {
            x0: 0,
            z0: 0,
            x1: grid.width,
            z1: grid.depth,
        };
        match (rect, &self.terrain_colors) {
            (Some(rect), Some(colors)) => {
                // Sculpt rects name cells; their corner samples change too.
                let rect = Rect {
                    x0: rect.x0,
                    z0: rect.z0,
                    x1: (rect.x1 + 1).min(grid.width),
                    z1: (rect.z1 + 1).min(grid.depth),
                };
                if !rect.is_empty() {
                    colors.write(rect, &self.tile_inputs().colors(rect));
                }
            }
            _ => {
                self.refresh_shipped_meshes();
                let texels = self.tile_inputs().colors(whole);
                let colors = gpu::ControlMap::new(grid.width, grid.depth, texels)
                    .expect("one texel per sample");
                self.terrain_colors = Some(Arc::new(colors));
                self.terrain = self.make_terrain();
                self.tiles.clear();
                self.scene_stale = true;
            }
        }
    }

    /// The terrain for the current heights, colors and ground material.
    /// The heights as the terrain is drawn: the document's, with sinking
    /// ground lowered as the game's displacement lowers it.
    fn drawn_field(&self) -> std::borrow::Cow<'_, Heightfield> {
        use std::borrow::Cow;
        match &self.sink {
            Some(sink) if sink.len() == self.island.doc.field.heights.len() => {
                let heights = (self.island.doc.field.heights.iter())
                    .zip(sink.iter())
                    .map(|(h, s)| h + s)
                    .collect();
                Cow::Owned(Heightfield::new(self.island.doc.field.grid, heights))
            }
            _ => Cow::Borrowed(&*self.island.doc.field),
        }
    }

    fn make_terrain(&self) -> Option<Arc<gpu::Terrain>> {
        let inputs = self.tile_inputs();
        Some(Arc::new(gpu::Terrain {
            heights: self.gpu_heights.clone()?,
            colors: self.terrain_colors.clone()?,
            color_gain: inputs.gain,
            // Overlays and other shadings recolor the ground; models keep
            // their own color then.
            tints: inputs.shading == Shading::Ground
                && inputs.overlay.is_none()
                && inputs.paths.is_none(),
            // Under the splat's ground (`splat_ground`) this only shows,
            // in vertex colours, while the ground uploads.
            material: gpu::Material::Lit,
            texture: inputs.texture.clone().map(Into::into),
        }))
    }

    /// What terrain tiles are built from, borrowed so that tiles can be
    /// built on several threads (the editor itself is not `Sync`).
    fn tile_inputs(&self) -> TileInputs<'_> {
        // Under a splat the ground draws its own paint, with no texture.
        let splat = self.splat.is_some() && self.shading == Shading::Ground;
        TileInputs {
            field: &self.island.doc.field,
            overlay: self
                .overlay()
                .and_then(|kind| self.island.doc.grids.as_ref()?.layer(kind)),
            paths: (self.path_overlay()).and_then(|l| Some((self.island.doc.blockers()?, l))),
            // Splat ground takes the tint per pixel instead (`ground_tint`).
            tint: (self.island.doc.tint.as_deref())
                .filter(|_| self.layers.tint && !self.tint_on_gpu()),
            materials: self.island.doc.materials.as_ref(),
            palette: self.palette.as_deref(),
            shading: self.shading,
            rules: self.rules(),
            gain: self.color_gain(),
            texture: if splat { None } else { self.texture() },
            heights: self.gpu_heights.clone(),
        }
    }

    /// Whether the GPU tints the ground per pixel from the tint map, as the
    /// game does, instead of the colors carrying it per height sample:
    /// splat ground with the Tint layer on, and a map a control map holds.
    fn tint_on_gpu(&self) -> bool {
        self.layers.tint
            && self.shading == Shading::Ground
            && self.splat.is_some()
            && (self.island.doc.tint.as_ref()).is_some_and(|m| m.width <= 4096 && m.height <= 4096)
    }

    /// The preset's water when the sea is drawn as 117's ocean: the Water
    /// layer is on and the island's preset has one.
    fn sea_water(&self) -> Option<Arc<dyn ambient::Sea>> {
        self.layers.water.then(|| self.preset_water()).flatten()
    }

    fn preset_water(&self) -> Option<Arc<dyn ambient::Sea>> {
        (self.ambient_lights.iter())
            .find(|(name, _)| *name == self.island.doc.session.global_ambient)
            .and_then(|(_, a)| a.sea.clone())
    }

    /// Whether the island's sea is 117's animated ocean.
    pub fn has_ocean(&self) -> bool {
        self.preset_water().is_some()
    }

    /// The tint for the scene: the working map, copied once into a control
    /// map and kept by painting (`sync_tint`).
    fn ground_tint(&mut self) -> Option<gpu::GroundTint> {
        // Kept while not drawn: the ground's splat bakes after the first
        // scene, and dropping the copy made then would make it again on the
        // UI thread.
        if !self.tint_on_gpu() {
            return None;
        }
        let map = self.island.doc.tint.clone()?;
        let key = Arc::as_ptr(&map) as usize;
        // Made while the island loads (`tint_control`); again here only
        // after a new map replaced it (an undo).
        if self.tint_gpu.as_ref().is_none_or(|(k, ..)| *k != key) {
            self.tint_gpu = tint_control(&map);
        }
        Some(gpu::GroundTint {
            map: self.tint_gpu.as_ref()?.1.clone(),
            // Tint texels lie on island metres from 0, as tint::sample reads them.
            origin: [0., 0.],
            size: [
                map.width as f32 / map.per_metre,
                map.height as f32 / map.per_metre,
            ],
        })
    }

    /// Marks the working tint under `window` (island metres x0, z0, x1, z1)
    /// changed after a Tint dab: the GPU's copy draws it again. When painting
    /// made a new map (the first dab after an undo step shares it), the copy
    /// is made again with the scene instead.
    fn sync_tint(&mut self, window: [f32; 4]) {
        let (Some(map), Some((key, control, _))) = (&self.island.doc.tint, &self.tint_gpu) else {
            return;
        };
        if Arc::as_ptr(map) as usize != *key {
            self.scene_stale = true;
            return;
        }
        let pm = map.per_metre;
        let span = |lo: f32, hi: f32, n: usize| {
            ((lo * pm).floor().max(0.) as usize).min(n)..((hi * pm).ceil().max(0.) as usize).min(n)
        };
        let (xs, ys) = (
            span(window[0], window[2], map.width),
            span(window[1], window[3], map.height),
        );
        if xs.is_empty() || ys.is_empty() {
            return;
        }
        control.touch(Rect {
            x0: xs.start,
            z0: ys.start,
            x1: xs.end,
            z1: ys.end,
        });
    }

    fn texture(&self) -> Option<Arc<Texture>> {
        match self.shading {
            Shading::Ground => self.ground.clone(),
            Shading::Map => self.map.clone(),
            Shading::Height | Shading::Materials => None,
        }
    }

    /// What the terrain tiles' colors follow; see `drawn_look`.
    fn look(&self) -> (Option<Kind>, bool, Option<anno_core::island::PathLayer>) {
        (self.overlay(), self.layers.tint, self.path_overlay())
    }

    /// The path-blocker layer the ground shows: the one the Grid tool
    /// paints on a 2070 island.
    fn path_overlay(&self) -> Option<anno_core::island::PathLayer> {
        match self.blocker {
            grids::BlockerEdit::Paths(layer)
                if self.tool == Tool::Grid && self.edits_blockers() =>
            {
                Some(layer)
            }
            _ => None,
        }
    }

    /// The Grid tool always shows the grid it paints.
    fn overlay(&self) -> Option<Kind> {
        match self.tool {
            Tool::Grid => Some(self.grid),
            _ if self.layers.buildable => Some(Kind::AreaIds),
            // Harbour pads are construction area reaching into the sea.
            Tool::Harbour => self.layers.grid.or(Some(Kind::AreaIds)),
            Tool::River => self.layers.grid.or(Some(Kind::River)),
            _ => self.layers.grid,
        }
    }

    /// Vertex colors are divided by this and the renderer multiplies it
    /// back: Ground carries tint multipliers above 1.
    fn color_gain(&self) -> f32 {
        if self.shading == Shading::Ground && self.layers.tint && self.island.doc.tint.is_some() {
            tint::GAIN
        } else {
            1.
        }
    }

    /// What the water lines depend on besides the document; see
    /// `drawn_lines`.
    fn lines_for(&self) -> (bool, Option<u64>, Option<u64>) {
        (
            self.tool == Tool::River,
            self.hover_water,
            self.river.as_ref().and_then(|d| d.id),
        )
    }

    /// Whether the scene's water lines are current.
    pub(super) fn lines_current(&self) -> bool {
        self.drawn_lines == Some(self.lines_for())
    }

    /// Hands the renderer the selection's rows, for its outline, markers
    /// and drags, when the selection or the rows changed. True when it did.
    /// Done on the CPU once per change: 20k selected items cost nothing a
    /// frame after.
    fn sync_selection(&mut self) -> bool {
        let generation = self.placements.generation();
        if (self.selection_sent.as_ref())
            .is_some_and(|(s, g)| *g == generation && *s == self.selection)
        {
            return false;
        }
        let (rows, marks) = self.placements.selection(&self.selection);
        let placed: BTreeSet<Item> = (marks.iter())
            .filter_map(|&(row, _)| self.placements.item_at(row))
            .collect();
        self.selection_unplaced = (self.selection.iter())
            .filter(|i| !placed.contains(i))
            .copied()
            .collect();
        self.renderer
            .set_outline((!self.selection.is_empty()).then(|| gpu::Outline {
                rows: rows.clone(),
                ids: Vec::new(),
                color: crate::colors::with_alpha(crate::colors::SELECTION, 0xff),
                // 2 px and a 2 px keyline (outline.wgsl): a wider band filled a
                // sparse bush's gaps until it read as a solid orange blob.
                width: 2,
            }));
        let ring = Marker {
            position: [0.; 3],
            radius: 6.,
            color: crate::colors::PICK,
            shape: Shape::Ring,
        };
        self.renderer.set_selection(rows, &marks, &ring);
        self.selection_sent = Some((self.selection.clone(), generation));
        true
    }

    /// Whether the ground's materials ship heights the Relief setting can
    /// draw.
    pub(crate) fn has_relief(&self) -> bool {
        self.splat.as_ref().is_some_and(|s| s.relief.is_some())
    }

    /// The preset the island is lit by: its session's, else the default.
    fn ambient(&self) -> Ambient {
        (self.ambient_lights.iter())
            .find(|(name, _)| *name == self.island.doc.session.global_ambient)
            .map_or_else(Ambient::default, |(_, a)| a.clone())
    }

    /// The sky bake the scene lacks, to run off the UI thread: the sky at
    /// the current time. A bake takes ~9 ms on 117. `None` while one runs
    /// or when the scene has what it needs.
    pub(crate) fn sky_job(&mut self) -> Option<impl FnOnce() -> SkyBaked + Send + use<>> {
        if self.sky_baking.is_some() {
            return None;
        }
        let ambient = self.ambient();
        let sky = ambient.sky.clone()?;
        let time = self.time_of_day;
        let clock = gpu::clock();
        let current = !sky.drifts() || clock - self.sky_clock < SKY_DRIFT_STEP;
        let have = (self.sky.as_ref()).is_some_and(|(a, t, _)| *a == ambient && *t == time);
        if have && current {
            return None;
        }
        self.sky_baking = Some((ambient.clone(), time));
        Some(move || SkyBaked {
            ambient,
            time,
            clock,
            baked: sky.bake(time),
        })
    }

    /// How long until the sky is due to be baked again as its clouds
    /// drift; `None` for a sky that holds still or a bake that runs.
    pub(crate) fn sky_due_in(&self) -> Option<std::time::Duration> {
        let drifts = self.ambient().sky.is_some_and(|s| s.drifts());
        if !drifts || self.sky_baking.is_some() {
            return None;
        }
        let left = SKY_DRIFT_STEP - (gpu::clock() - self.sky_clock);
        Some(std::time::Duration::from_secs_f32(
            left.clamp(0., SKY_DRIFT_STEP),
        ))
    }

    /// Whether the game's sea shades rivers and lakes itself (117), else
    /// they get the translucent stand-in.
    fn shades_inland(&self) -> bool {
        self.sea_water().is_some() && self.water_pass.as_ref().is_some_and(|p| p.shades_inland())
    }

    /// The rivers and lakes' surface, built off the UI thread, when the
    /// scene asked for one and none is running: the curves are copied,
    /// so edits meanwhile ask for the next.
    pub(crate) fn inland_job(&mut self) -> Option<impl FnOnce() -> InlandBuilt + Send + use<>> {
        if !self.inland_wanted || self.inland_building {
            return None;
        }
        self.inland_wanted = false;
        self.inland_building = true;
        let shaded = self.shades_inland();
        let strips = self.water_pass.as_ref().is_some_and(|p| p.river_strips());
        // The shaded surface needs each curve's record, as stored: each game
        // reads it with its own defaults.
        type Curve = (
            Vec<anno_core::island::objects::PathPoint>,
            anno_core::water::Look,
            bool,
            u64,
        );
        let curves: Vec<Curve> = (self.island.doc.objects.iter())
            .filter_map(|o| {
                let path = o.path.as_deref()?;
                let lake = self.water_kind(o.guid)?;
                let fields = (self.island.object_store.as_ref())
                    .filter(|_| shaded)
                    .map(|s| s.component_floats(o, if lake { "Lake" } else { "River" }))
                    .unwrap_or_default();
                Some((
                    path.to_vec(),
                    anno_core::water::Look { lake, fields },
                    lake,
                    o.id,
                ))
            })
            .collect();
        // The shaded surface's flow map measures the water over the ground,
        // so a new ground asks for a new surface (`refresh_scene`).
        let ground = self.island.doc.field.clone();
        self.inland_ground = Arc::downgrade(&ground);
        Some(move || {
            let surface = if shaded {
                let curves: Vec<_> = (curves.iter())
                    .map(|(path, look, ..)| (path.as_slice(), look.clone()))
                    .collect();
                river::inland(&curves, Some(&ground), strips).map(Inland::Shaded)
            } else {
                let curves: Vec<_> = (curves.iter())
                    .map(|(path, _, lake, _)| (path.as_slice(), *lake))
                    .collect();
                river::water_mesh(&curves).map(Inland::Mesh)
            };
            let courses = river::courses(
                curves
                    .iter()
                    .map(|(path, _, lake, id)| (*id, path.as_slice(), *lake)),
            );
            InlandBuilt { surface, courses }
        })
    }

    /// The planned placement rows, built off the UI thread; one build at a
    /// time, the newest plan next.
    pub(crate) fn placement_job(
        &mut self,
    ) -> Option<impl FnOnce() -> placements::Built + Send + use<>> {
        if self.placement_building {
            return None;
        }
        let plan = self.placement_plan.take()?;
        self.placement_building = true;
        Some(move || plan.build())
    }

    /// A finished `placement_job`: its rows replace the drawn ones, unless
    /// a newer plan waits (it lands next) or the rows changed meanwhile (a
    /// drag or stroke: the scene plans again).
    pub(crate) fn placement_built(&mut self, built: placements::Built) {
        self.placement_building = false;
        if self.placement_plan.is_some() {
            return;
        }
        if self.placements.apply(built, &self.renderer) {
            self.send_owner_words();
            self.markers_stale = true;
        } else {
            self.scene_stale = true;
        }
    }

    /// Each placed object's owner colour as its rows' instance words
    /// (`SceneParts::owner_word`), from the player colour View settings
    /// picks. Placing rows zeroes the words, so this follows every
    /// placement change; a pick rebuilds the scene and lands here too. A
    /// game without owner colours sends nothing. The words ride the
    /// placements' upload and redraw no more than the placement change
    /// that comes with them already did.
    fn send_owner_words(&mut self) {
        if let Some(words) = self.owner_words() {
            self.renderer.set_instance_words(0, &words);
        }
    }

    /// Every placement row's instance word, as `send_owner_words` sends
    /// them; `None` where no object is dyed.
    fn owner_words(&self) -> Option<Vec<u32>> {
        let scene = self.module.scene()?;
        let player = self.layers.player;
        let mut owned: Vec<(u64, u32)> = (self.island.doc.objects.iter())
            .filter(|o| o.owner != 0)
            .map(|o| (o.id, scene.owner_word(o.owner, player)))
            .filter(|&(_, word)| word != 0)
            .collect();
        if owned.is_empty() {
            return None;
        }
        owned.sort_unstable_by_key(|&(id, _)| id);
        let words = (self.placements.item.iter())
            .map(|item| match item {
                Some(Item::Object(id)) => owned
                    .binary_search_by_key(id, |&(id, _)| id)
                    .map_or(0, |k| owned[k].1),
                _ => 0,
            })
            .collect();
        Some(words)
    }

    /// A finished `inland_job`, drawn from the next scene refresh.
    pub(crate) fn inland_built(&mut self, built: InlandBuilt) {
        self.inland_building = false;
        self.inland_water = built.surface;
        self.water_courses = built.courses;
        self.markers_stale = true;
    }

    /// A finished `sky_job`, for the next scene refresh.
    pub(crate) fn sky_baked(&mut self, done: SkyBaked) {
        self.sky_baking = None;
        self.sky_clock = done.clock;
        self.sky = Some((done.ambient, done.time, done.baked));
        self.markers_stale = true;
    }

    fn refresh_scene(&mut self) {
        let _span = gpui_editor_kit::profile::span("island refresh_scene");
        for view in 0..self.views.len() {
            self.apply_gizmo(view);
        }
        self.markers_stale |= self.sync_selection();
        // The Relief setting rides on the splat: a copy with it flipped
        // shares every texture, so only the tiles' material changes.
        let relief = self.layers.relief;
        if let Some(splat) = &self.splat
            && let Some(r) = splat.relief.as_ref().filter(|r| r.on != relief)
        {
            let mut flipped = (**splat).clone();
            flipped.relief = Some(anno_render::a7m::Relief {
                on: relief,
                ..r.clone()
            });
            let flipped = Arc::new(flipped);
            self.splat_ground = anno_render::a7m::ground(&flipped);
            self.splat = Some(flipped);
            self.retexture_tiles();
        }
        if Some(self.look()) != self.drawn_look {
            self.dirty = Rect::default();
            self.rebuild_tiles(None);
        }
        if !self.dirty.is_empty() {
            let rect = std::mem::take(&mut self.dirty);
            self.rebuild_tiles(Some(rect));
        }
        // Turning the shipped layer on loads and builds what it shows.
        if self.layers.shipped
            && let Some((install, path)) = self.shipped_source.take()
        {
            self.shipped =
                terrain::ShippedMeshes::load(&install, &path, self.island.doc.field.grid.size()[0]);
        }
        if self.layers.shipped && self.shipped_meshes.is_empty() && self.shipped.len() > 0 {
            self.refresh_shipped_meshes();
        }
        // The cliff tops sample the baked ground, skipped while hidden.
        if self.bake_wanted()
            && self.ground.is_none()
            && self.palette.is_some()
            && self.island.doc.materials.is_some()
        {
            self.ground_stale = true;
        }
        let lines_for = self.lines_for();
        if self.drawn_lines != Some(lines_for) {
            self.markers_stale = true;
        }
        // Mid-stroke: swap the vegetation rows alone, the props' and
        // objects' stay. A full refresh scatters everything anyway. A
        // sixteenth of the island a frame rescatters in about 4 ms on a
        // 512 m island, and each window walks every placed prop and grown
        // instance (about 0.1 ms each there), so 16 of them stay near
        // 1 ms; a drag of more props apart changes more than that every
        // move and leaves the vegetation to its release. Swapping the rows
        // adds about 3 ms, as every grown row and the renderer's bounds
        // are redone.
        let windows = std::mem::take(&mut self.vegetation_window);
        let size = self.island.doc.field.grid.size();
        let area: f32 = (windows.iter())
            .map(|w| (w[2] - w[0]) * (w[3] - w[1]))
            .sum();
        if !windows.is_empty()
            && !self.scene_stale
            && windows.len() <= 16
            && area <= size[0] * size[1] / 16.
            && let Some(preview) = self.regrow_vegetation(&windows)
        {
            let _span = gpui_editor_kit::profile::span("refresh: regrow vegetation");
            let grown = (preview.models.iter())
                .flat_map(|(cfg, ms)| ms.iter().map(move |m| (cfg.as_str(), *m)));
            self.placements.set_grown(grown, &self.renderer);
            self.send_owner_words();
            self.vegetation = Some(preview);
        }
        if std::mem::take(&mut self.rails_stale) || self.scene_stale {
            let _span = gpui_editor_kit::profile::span("refresh: rails");
            // 55-85 ms of an 1800 island's first scene on a busy machine,
            // its templates and bends: off the UI thread when a runner
            // takes them.
            if self.background_scene {
                self.rails_wanted = true;
            } else {
                self.refresh_rails();
            }
            self.markers_stale = true;
        }
        if !self.scene_stale && !self.markers_stale {
            return;
        }
        self.drawn_lines = Some(lines_for);
        let renderer = self.renderer.clone();
        let layers = self.layers;
        if self.scene_stale {
            let _span = gpui_editor_kit::profile::span("refresh: placements");
            if std::mem::take(&mut self.pieces_stale) {
                self.placements.set_pieces(
                    &self.library.models.loaded,
                    self.graphics.animation,
                    &renderer,
                );
            }
            // Every visible item with a model, then the vegetation preview.
            self.vegetation = self.vegetation_preview();
            // Each model looked up once: per mesh file for props, per plant
            // model, per object. Placements::model says why.
            let placements = &self.placements;
            let files: Vec<Option<placements::Model>> = (self.island.doc.meshes.iter())
                .map(|m| placements.model(&m.replace('\\', "/")))
                .collect();
            let hidden = &self.hidden_groups;
            let props = (self.island.doc.props.iter())
                .filter(|_| layers.props)
                .filter(|p| !hidden.contains(&Group::Prop(p.source.file)))
                .filter_map(|p| {
                    let model = *files.get(p.source.file as usize)?;
                    let allow = p.source.adapt_terrain_height.is_some();
                    Some((Some(Item::Prop(p.id)), model, p.matrix, allow))
                });
            let assets = self.assets.clone();
            let objects = (self.island.doc.objects.iter())
                .filter(|_| layers.objects)
                .filter(|o| !hidden.contains(&Group::Object(o.guid)))
                .filter_map(|o| {
                    let asset = assets.as_ref().and_then(|a| a.get(o.guid));
                    let cfgs = asset.map(|a| a.cfgs.as_slice()).unwrap_or(&[]);
                    let cfg = o.model(cfgs)?;
                    // Owned only where the record uses backslashes. What
                    // the game draws only in its own editor has no model
                    // here unless Layers::editor_only, so it shows as a
                    // marker.
                    let model = match cfg.contains('\\') {
                        _ if !layers.editor_only
                            && asset.is_some_and(|a| a.editor_only == Some(true)) =>
                        {
                            None
                        }
                        true => placements.model(&cfg.replace('\\', "/")),
                        false => placements.model(cfg),
                    };
                    Some((
                        Some(Item::Object(o.id)),
                        model,
                        doc::trs(o.position, o.rotation, o.scale),
                        true,
                    ))
                });
            let vegetation = self.vegetation.clone();
            let grown = vegetation.iter().flat_map(|p| {
                p.models.iter().flat_map(|(cfg, ms)| {
                    let model = placements.model(cfg);
                    ms.iter().map(move |m| (None, model, *m, true))
                })
            });
            let (plan, missing) = placements.plan_models(props.chain(objects).chain(grown));
            self.undrawn = missing;
            // The rows' matrices and bounds take tens of milliseconds on a
            // busy machine: off the UI thread when a runner takes them, the
            // old rows drawn until the new land.
            if self.background_scene {
                self.placement_plan = Some(plan);
            } else {
                let built = gpui_editor_kit::task::interactive(|| plan.build());
                if self.placements.apply(built, &renderer) {
                    self.send_owner_words();
                }
            }
        }
        // A game's sea replaces the flat sea quad, and a sea that shades
        // rivers and lakes gets their records to shade them by. The rivers'
        // surface builds off the UI thread (`inland_job`): 22 ms of an 1800
        // island's rebuild was its mesh.
        let water = self.sea_water();
        let built_shaded = matches!(self.inland_water, Some(Inland::Shaded(_)));
        if std::mem::take(&mut self.inland_stale)
            || self.scene_stale
            || (self.inland_water.is_some() && built_shaded != self.shades_inland())
            || (built_shaded && self.inland_ground.as_ptr() != Arc::as_ptr(&self.island.doc.field))
        {
            self.inland_wanted = true;
        }
        // `set` lays out new rows: the selection names them again.
        self.sync_selection();
        self.scene_stale = false;
        self.markers_stale = false;
        let inland = match &self.inland_water {
            Some(Inland::Mesh(mesh)) => Some(mesh),
            _ => None,
        };
        let identity = self.identity.clone();
        let quad = water.is_none().then_some(&self.water);
        let batches: Vec<Batch> = self
            .tiles
            .iter()
            .chain(
                self.shipped_meshes
                    .iter()
                    .flatten()
                    .filter(|_| layers.shipped),
            )
            .chain(
                [quad, inland]
                    .into_iter()
                    .flatten()
                    .filter(|_| layers.water),
            )
            .chain(self.rails.meshes())
            .map(|mesh| Batch::new(mesh.clone(), identity.clone()))
            .collect();
        // Markers for items no model draws, and rings on the selected ones
        // among them; the GPU rings the rest (`sync_selection`).
        // One pass over the document: before models load every item is
        // marked, and looking each up by id was quadratic (~90 ms at 16k).
        let selection = &self.selection;
        let marker = |item: Item, position: Vec3| {
            let selected = selection.contains(&item);
            Marker {
                position,
                radius: if selected { 6. } else { 3. },
                color: match (selected, item) {
                    (true, _) => crate::colors::PICK,
                    (false, Item::Prop(_)) => crate::colors::PROP_MARKER,
                    (false, Item::Object(_)) => crate::colors::OBJECT_MARKER,
                },
                shape: match (selected, item) {
                    (true, _) => Shape::Ring,
                    (false, Item::Prop(_)) => Shape::Circle,
                    (false, Item::Object(_)) => Shape::Square,
                },
            }
        };
        let doc = || {
            let props = (self.island.doc.props.iter())
                .map(|p| (Item::Prop(p.id), math::translation(&p.matrix)));
            let objects =
                (self.island.doc.objects.iter()).map(|o| (Item::Object(o.id), o.position));
            props.chain(objects)
        };
        // The plan lists `undrawn` in the document's order (props, then
        // objects), so walking both together finds them without a lookup:
        // while models load all 68k of an 1800 island's items are marked,
        // and a set of them took 10-24 ms a refresh. Out of step (never
        // expected), the set again.
        let unplaced: foldhash::HashSet<Item> = self.selection_unplaced.iter().copied().collect();
        let undrawn = &self.undrawn;
        let mut next = 0;
        let mut markers = Vec::with_capacity(undrawn.len() + unplaced.len());
        for (item, position) in doc() {
            let listed = undrawn.get(next) == Some(&item);
            next += usize::from(listed);
            if listed || unplaced.contains(&item) {
                markers.push(marker(item, position));
            }
        }
        if next < undrawn.len() {
            let marked: foldhash::HashSet<Item> =
                undrawn.iter().chain(&unplaced).copied().collect();
            markers = (doc())
                .filter(|(item, _)| marked.contains(item))
                .map(|(item, position)| marker(item, position))
                .collect();
        }
        let ambient = self.ambient();
        let sky = ambient.sky.clone();
        let at = match &sky {
            Some(sky) => sky.sun_moon(self.time_of_day),
            None => SunMoon::at(self.time_of_day, ambient.elevation, ambient.min_elevation),
        };
        // The sky bakes off the UI thread (`sky_job`). Until the preset's
        // first bake lands its fixed colours light the island, and a new
        // time keeps the last bake's sky for the frames in between.
        let baked = (self.sky.as_ref())
            .filter(|(a, ..)| *a == ambient)
            .map(|(_, _, b)| b.clone());
        // Presets lit by fixed colors keep them; time only moves the sun,
        // which stays a little above the horizon for them.
        let sun = baked
            .as_ref()
            .map_or([at.sun[0], at.sun[1].max(0.05), at.sun[2]], |b| b.toward);
        let horizon = baked.as_ref().map_or(ambient.horizon, Baked::horizon);
        // Held waves stand at time 0; moving ones follow the renderer's
        // clock. Mid-stroke the coast stays as it was: remeasuring takes
        // tens of milliseconds, and the stroke's end brings a new one.
        let stroking = (self.views.iter()).any(|v| matches!(v.drag, Some(Drag::Brush { .. })));
        let mut passes: Vec<Arc<dyn gpu::Pass>> = Vec::new();
        // The game's sky from the camera, and its haze in place of the
        // editor's fog.
        let mut hazed = false;
        let graphics = self.graphics;
        self.renderer.set_detail(graphics.kit(&self.view_distances));
        (self.renderer).set_render_scale(self.render_scale_env.unwrap_or(graphics.render_scale));
        // A view of the game's own stands in for its sky and water and
        // lights the scene itself (2070 under water), so no editor fog.
        let view = self.view.clone().filter(|_| layers.under_water);
        if let Some(sky_pass) = &self.sky_pass {
            sky_pass.detail(graphics.game());
            sky_pass.show(sky.as_ref(), self.time_of_day);
            hazed = sky_pass.hazes();
            if view.is_none() {
                passes.extend(sky_pass.passes());
            }
        }
        if let Some(view) = &view {
            hazed = true;
            passes.extend(view.passes.iter().cloned());
        }
        if let Some(pass) = &self.water_pass {
            let inland = match &self.inland_water {
                Some(Inland::Shaded(inland)) if layers.water => Some(inland.clone()),
                _ => None,
            };
            pass.show(
                water.as_ref(),
                &self.island.doc.field,
                stroking,
                inland,
                (!layers.waves).then_some(0.),
            );
            pass.island_maps(self.island.doc.grids.as_ref());
            pass.island_file(self.island.native.as_ref().and_then(Native::file));
            pass.rain(if layers.rain { 1. } else { 0. });
            let seasons =
                (self.module().scene()).and_then(|s| s.seasons(&self.path, self.assets.as_deref()));
            pass.season(&self.path, seasons, if layers.flood { 1. } else { 0. });
            pass.detail(graphics.game());
            if water.is_some() && view.is_none() {
                passes.push(pass.clone());
            }
        }
        let scene = Scene {
            batches,
            terrain: self.terrain.clone(),
            lines: Arc::new(self.water_lines()),
            markers: Arc::new(markers),
            // Placed models' own lights are not drawn on islands.
            local_lights: Arc::default(),
            light: Light {
                sun,
                sun_color: baked.as_ref().map_or(ambient.sun, |b| b.light),
                // Only lights without a sky map; with one it lights.
                sky: ambient.sky_light,
                ground: ambient.ground,
                // Both games fit four shadow cascades to what the camera
                // sees (functions_shadow.fxh, SDSM). The island's 4096² map
                // (64 MB, shared by viewports) is not redrawn per camera
                // move and spreads a quarter metre a texel over a 768 m
                // island, so it shades only past three view cascades of
                // 2048² (48 MB a viewport), which cover the game's 500 m
                // shadow distance (1800's MAX_SHADOW_DISTANCE_WS; 117 sets
                // its own at run time). Filtering is the game's: six taps
                // 1.5 texels out (BRDF_SHADOW_RADIUS, BRDF_SHADOW_SAMPLES).
                // High, Graphics' default; the View settings' Shadows
                // switch still turns them off for this island alone.
                shadows: graphics.shadow_maps().filter(|_| layers.shadows),
                // Into the horizon's haze, so the island and the sea
                // beyond the water quad meet the sky without a line.
                fog: (!hazed).then(|| Fog {
                    color: horizon,
                    start: self.extent().size * 1.5,
                    end: self.extent().size * 6.,
                }),
            },
            tone: Some(match sky.as_ref().and_then(|sky| sky.curve()) {
                Some(curve) => Tone {
                    exposure: ambient.exposure,
                    sun_occlusion: ambient.sun_occlusion,
                    ambient: 1.,
                    curve,
                    auto: None,
                },
                None => Tone {
                    exposure: ambient.exposure,
                    sun_occlusion: ambient.sun_occlusion,
                    ambient: ambient.ambient * vegetation::NO_RT_AMBIENT,
                    curve: (self.module.scene()).map_or_else(Default::default, |s| s.tone_curve()),
                    auto: (self.module.scene()).and_then(|s| s.auto_exposure()),
                },
            }),
            // Anno's HBAO+ (ambient_ssao.fx) at the preset's AOIntensity.
            // The engine sets its radius, bias, power, scales and screen
            // radius bounds at run time; these are HBAO+'s defaults but for
            // a radius reaching about a tree's foot and a rock's crevices,
            // and a 32 px floor that keeps it on the ground seen from the
            // strategy camera (estimates).
            occlusion: graphics.occlusion.then(|| Occlusion {
                radius: 2.5,
                strength: ambient.occlusion,
                min_pixels: 32.,
                ..Default::default()
            }),
            ground_tint: self.ground_tint(),
            cloud_shadows: sky.as_ref().and_then(|s| s.cloud_shadows(self.time_of_day)),
            // Lit windows, fires and lamps by the view's hour; dyed parts
            // in the player colour picked in View settings.
            glow: {
                let look = self.module.model_look();
                let colors = look.player_colors;
                let tint =
                    (colors.get(layers.player).or(colors.first())).map_or([1.; 3], |c| c.dye());
                let glow =
                    anno_core::ambient::glow(&ambient, self.time_of_day, tint, look.deferred_glow);
                let mut glow = look.glow(&glow);
                // The preset's own numbers for the scene's lighting (2070's
                // scattering): a preset switch rewrites them, no pipeline.
                if let Some(sky) = &sky {
                    sky.host_words(&mut glow.host);
                }
                glow
            },
            // What models and water reflect: the sky pass's own cube.
            environment: self.sky_pass.as_ref().and_then(|p| p.environment()),
            // The sea's mirror, where the game's sea reads one and draws.
            reflection: (self.water_pass.as_ref())
                .filter(|_| water.is_some() && view.is_none())
                .and_then(|p| p.reflection(graphics.game())),
            view_mode: view.as_ref().map_or(0, |v| v.mode),
            host_texture: view.and_then(|v| v.host_texture),
            passes,
            // A game's own ground draws its paint wherever the ground
            // shows, and the a7m splat's where there is one. The kit's draws Map
            // and Height in the terrain's material.
            ground: match &self.own_ground {
                Some(g) if self.shading == Shading::Ground => g.draw(),
                _ if self.shading == Shading::Ground && self.splat.is_some() => {
                    self.splat_ground.clone()
                }
                _ => gpu::GroundDraw::Kit,
            },
            sky: Some(Sky {
                zenith: baked.as_ref().map_or(ambient.zenith, Baked::zenith),
                horizon,
                // Past the water quad (terrain::water): about what the
                // quad shows over deep floor.
                sea: Some(Sea {
                    level: 0.,
                    color: terrain::SEA_BODY,
                }),
                sun_disc: baked.as_ref().map_or([0.; 3], |b| b.disc),
                ambient: baked.as_ref().and_then(|b| b.ambient),
                radiance: baked.map(|b| b.map),
            }),
        };
        self.renderer.set_scene(&scene);
    }

    // --- input ---------------------------------------------------------------

    fn ground(&self, view: usize, cursor: Point<Pixels>) -> Option<Vec3> {
        let v = &self.views[view];
        let ray = v.camera.ray(cursor, v.bounds, self.extent());
        self.island.doc.field.raycast(ray)
    }

    /// The selection's mean position, where the gizmo sits. Every paint
    /// asks: a few items look themselves up; more walk the props beside
    /// the selection, both sorted by id, as 20k lookups took 1.7 ms.
    fn pivot(&self) -> Option<Vec3> {
        let add = |(s, n): (Vec3, usize), p: Vec3| (math::add(s, p), n + 1);
        let props = &self.island.doc.props;
        let (sum, n) =
            if self.selection.len() < 64 || !props.iter().is_sorted_by(|a, b| a.id < b.id) {
                (self.selection.iter())
                    .filter_map(|&item| self.island.doc.matrix(item))
                    .map(|m| math::translation(&m))
                    .fold(([0.; 3], 0), add)
            } else {
                let mut selected = self.selection.range(..=Item::Prop(u64::MAX)).peekable();
                let mut acc = ([0.; 3], 0);
                for p in props.iter() {
                    while selected.next_if(|&&i| i < Item::Prop(p.id)).is_some() {}
                    if selected.next_if_eq(&&Item::Prop(p.id)).is_some() {
                        acc = add(acc, math::translation(&p.matrix));
                    }
                }
                (self.island.doc.objects.iter())
                    .filter(|o| self.selection.contains(&Item::Object(o.id)))
                    .fold(acc, |acc, o| add(acc, o.position))
            };
        (n > 0).then(|| math::scale(sum, 1. / n as f32))
    }

    fn gizmo_frame(&self, view: usize) -> Option<gizmo::Frame> {
        let v = &self.views[view];
        // The path edit mode's points, and the River tool's picked point,
        // move only.
        if let Some(pivot) = self.road_pivot().or_else(|| self.river_pivot()) {
            return gizmo::Frame::new(
                gizmo::Mode::Move,
                pivot,
                None,
                v.camera,
                v.bounds,
                self.extent(),
            );
        }
        if self.tool != Tool::Select {
            return None;
        }
        // A GPU drag left the document behind: the pivot it began at, moved.
        let pivot = match &v.drag {
            Some(Drag::Gizmo {
                gesture,
                gpu: Some(m),
                ..
            }) => math::transform(m, gesture.frame.pivot()),
            _ => self.pivot()?,
        };
        gizmo::Frame::new(self.gizmo, pivot, None, v.camera, v.bounds, self.extent())
    }

    fn dab(&mut self, at: Vec3, level: f32, invert: bool, baseline: Option<&[f32]>) -> bool {
        let center = [at[0], at[2]];
        match self.tool {
            Tool::Paint => {
                let Some(m) = &mut self.island.doc.materials else {
                    return false;
                };
                if !m.paint(center, &self.brush, self.paint) {
                    return false;
                }
                // The ground shows the paint on the next frame.
                let r = self.brush.radius + 1.;
                self.show_paint([center[0] - r, center[1] - r, center[0] + r, center[1] + r]);
                // The ground type picks what grows.
                self.vegetation_changed([
                    center[0] - r,
                    center[1] - r,
                    center[0] + r,
                    center[1] + r,
                ]);
                // Vertex colors show materials only in the Ids shading; the
                // cliff-top bake catches up after the stroke.
                if self.shading == Shading::Materials || self.splat.is_none() {
                    let rect = self
                        .island
                        .doc
                        .field
                        .grid
                        .footprint(center, self.brush.radius);
                    self.dirty = self.dirty.union(rect);
                }
                true
            }
            Tool::Grid if self.edits_blockers() => match self.blocker {
                grids::BlockerEdit::Paths(layer) => self.path_dab(center, layer, invert),
                // Points, not a brush: `shape_press`.
                grids::BlockerEdit::Shapes => false,
            },
            Tool::Grid => {
                let (kind, mut value) = (self.grid, self.grid_value);
                let mut invert = invert;
                // Areas paint buildable or not; 0 would leave land in no area.
                if kind == Kind::AreaIds {
                    value = if invert {
                        harbour::BASE
                    } else {
                        harbour::CONSTRUCTION
                    };
                    invert = false;
                }
                // The comb: each dab pushes the current along the drag.
                let comb = (kind == Kind::WaterFlow).then(|| {
                    let from = self.flow_from.replace(center);
                    let (dx, dz) = from.map_or((0., 0.), |f| (center[0] - f[0], center[1] - f[1]));
                    let l = dx.hypot(dz);
                    (l > 1e-3).then(|| [dx / l, dz / l])
                });
                if comb == Some(None) && !invert {
                    return false;
                }
                let Some(layer) = self
                    .island
                    .doc
                    .grids
                    .as_mut()
                    .and_then(|g| Arc::make_mut(g).layer_mut(kind))
                else {
                    return false;
                };
                let painted = match comb {
                    Some(dir) => {
                        grids::paint_flow(layer, center, &self.brush, dir.filter(|_| !invert))
                    }
                    None => grids::paint(layer, center, &self.brush, value, invert),
                };
                if !painted {
                    return false;
                }
                let rect = self
                    .island
                    .doc
                    .field
                    .grid
                    .footprint(center, self.brush.radius);
                self.dirty = self.dirty.union(rect);
                if kind == Kind::Grit {
                    // A cell past the brush each way: paint rounds outward.
                    let r = self.brush.radius + 1.;
                    self.sync_grit(Some([
                        center[0] - r,
                        center[1] - r,
                        center[0] + r,
                        center[1] + r,
                    ]));
                }
                if kind == Kind::Marsh && self.marsh_shape && !invert {
                    self.shape_marsh(center, baseline);
                }
                true
            }
            Tool::Tint => {
                // The tint control draws from this map: let go of it while
                // painting, so the map is not copied on write for it.
                let source = self.tint_gpu.as_ref().map(|(.., s)| s.clone());
                let mut lent = source.as_ref().map(|s| s.0.lock().unwrap());
                if let Some(lent) = lent.as_mut() {
                    lent.take();
                }
                let Some(map) = &mut self.island.doc.tint else {
                    return false;
                };
                let painted = tint::paint(
                    Arc::make_mut(map),
                    center,
                    &self.brush,
                    self.tint_color,
                    invert,
                );
                if let Some(lent) = lent.as_mut() {
                    **lent = Some(map.clone());
                }
                drop(lent);
                if !painted {
                    return false;
                }
                let rect = self
                    .island
                    .doc
                    .field
                    .grid
                    .footprint(center, self.brush.radius);
                self.dirty = self.dirty.union(rect);
                let r = self.brush.radius + 1.;
                self.sync_tint([center[0] - r, center[1] - r, center[0] + r, center[1] + r]);
                true
            }
            Tool::Scatter => {
                if self.scatter.files.is_empty()
                    && let Some(file) = self.place
                {
                    self.scatter.files.push(file);
                }
                let n = if invert {
                    self.scatter.thin(&mut self.island.doc, center, &self.brush)
                } else {
                    self.scatter.fill(
                        &mut self.island.doc,
                        center,
                        &self.brush,
                        &mut self.island.next_id,
                    )
                };
                if n == 0 {
                    return false;
                }
                // Props changed mid-stroke; the model batches must notice
                // before the checkpoint bumps the revision.
                self.moves += 1;
                self.scene_stale = true;
                if self.scatter.mark_trees && self.overlay() == Some(Kind::Trees) {
                    let rect = self
                        .island
                        .doc
                        .field
                        .grid
                        .footprint(center, self.brush.radius);
                    self.dirty = self.dirty.union(rect);
                }
                true
            }
            tool => {
                let op = match (tool, invert) {
                    (Tool::Raise, false) | (Tool::Lower, true) => Sculpt::Raise,
                    (Tool::Lower, false) | (Tool::Raise, true) => Sculpt::Lower,
                    (Tool::Smooth, _) => Sculpt::Smooth,
                    (Tool::Flatten, _) => Sculpt::Flatten(self.level.unwrap_or(level)),
                    (Tool::Fill, _) => Sculpt::Fill(self.level.unwrap_or(level)),
                    (Tool::Scrape, _) => Sculpt::Scrape(self.level.unwrap_or(level)),
                    (Tool::Noise, _) => Sculpt::Noise(self.brush.radius * 0.4),
                    (
                        Tool::Select
                        | Tool::Paint
                        | Tool::Place
                        | Tool::Grid
                        | Tool::Scatter
                        | Tool::Tint
                        | Tool::Harbour
                        | Tool::Slot
                        | Tool::River,
                        _,
                    ) => return false,
                };
                self.sculpt_dab(center, op, baseline)
            }
        }
    }

    /// The rest of a marsh under a Marsh grid dab, as every shipped Albion
    /// marsh has it: ground flat at `MARSH_LEVEL`, the Marsh environment,
    /// and the set's marsh floor with reeds over it (reeds are the `Marsh`
    /// ground type the game grows marsh plants on). Sets without marsh
    /// textures keep their ground.
    fn shape_marsh(&mut self, center: [f32; 2], baseline: Option<&[f32]>) {
        if let Some(env) = (self.island.doc.grids.as_mut())
            .and_then(|g| Arc::make_mut(g).layer_mut(Kind::Environment))
        {
            grids::paint(env, center, &self.brush, harbour::MARSH_ENVIRONMENT, false);
        }
        self.sculpt_dab(center, Sculpt::Flatten(MARSH_LEVEL), baseline);
        let Some(palette) = self.palette.clone() else {
            return;
        };
        let stem = |id: &u8| {
            let file = &palette.materials[id].file;
            file.rsplit('/').next().unwrap_or(file).to_ascii_lowercase()
        };
        // TODO(alex): shipped marshes also stripe the floor with
        // marshislands_01 patches; a noise mask over the brush would match.
        let floor = palette
            .materials
            .keys()
            .find(|id| stem(id).starts_with("marshfloor"));
        let reed = (palette.ground_layers.iter())
            .find(|(_, ground)| ground == "Marsh")
            .map(|(id, _)| id);
        let Some(m) = &mut self.island.doc.materials else {
            return;
        };
        // Full strength whatever the grid brush says: the cells are on or
        // off after one pass, and half-laid ground left grass showing
        // through a finished marsh.
        let mut painted = false;
        if let Some(&floor) = floor {
            let brush = Brush {
                strength: 1.,
                ..self.brush
            };
            painted |= m.paint(center, &brush, floor);
        }
        if let Some(&reed) = reed {
            // Shipped marshes are about half reed over the floor.
            let brush = Brush {
                strength: 0.55,
                ..self.brush
            };
            painted |= m.paint(center, &brush, reed);
        }
        if !painted {
            return;
        }
        let r = self.brush.radius + 1.;
        let area = [center[0] - r, center[1] - r, center[0] + r, center[1] + r];
        self.show_paint(area);
        self.vegetation_changed(area);
    }

    /// One sculpt dab of `op` at `center`: heights, the props that follow
    /// the ground, the GPU heights and what reads them.
    fn sculpt_dab(&mut self, center: [f32; 2], op: Sculpt, baseline: Option<&[f32]>) -> bool {
        // Props with AdaptTerrainHeight keep their height above the
        // ground, as the game's own editor keeps them: note the
        // ground under those the brush may reach, lift them by its
        // change. Bilinear heights reach a sample past the brush.
        let reach = self.brush.radius + 2. * self.island.doc.field.grid.spacing;
        let seated: Vec<(usize, f32)> = (self.island.doc.props.iter().enumerate())
            .filter(|(_, p)| p.source.adapt_terrain_height.is_some())
            .map(|(i, p)| (i, [p.matrix[12], p.matrix[14]]))
            .filter(|(_, at)| {
                (at[0] - center[0]).abs() <= reach && (at[1] - center[1]).abs() <= reach
            })
            .map(|(i, at)| (i, self.island.doc.field.height(at)))
            .collect();
        // 2070 blocks the ground a sculpt lifts off buildable land; the
        // path blockers show it as the stroke goes.
        let lift = self.lift_baseline(center, reach);
        let rect = Arc::make_mut(&mut self.island.doc.field).sculpt_from(
            center,
            &self.brush,
            op,
            baseline,
        );
        if let Some(lift) = lift.filter(|_| !rect.is_empty()) {
            self.block_lifted(&lift, rect);
        }
        if !rect.is_empty() && !seated.is_empty() {
            let doc = &mut self.island.doc;
            let props = &mut doc.props;
            let mut moved = Vec::new();
            for (i, was) in seated {
                let p = &mut props[i];
                let rise = doc.field.height([p.matrix[12], p.matrix[14]]) - was;
                if rise != 0. {
                    p.matrix[13] += rise;
                    moved.push((Item::Prop(p.id), p.matrix));
                }
            }
            if !moved.is_empty() {
                let renderer = self.renderer.clone();
                self.placements.move_items(&mut moved, &renderer);
                self.moves += 1;
            }
        }
        match &self.gpu_heights {
            Some(heights) if !rect.is_empty() => {
                heights.update(rect, &self.drawn_field().heights);
                self.stale_colors = self.stale_colors.union(rect);
            }
            _ => self.dirty = self.dirty.union(rect),
        }
        if !rect.is_empty() {
            let g = self.island.doc.field.grid;
            let at = |x: usize, z: usize| {
                [
                    g.origin[0] + x as f32 * g.spacing,
                    g.origin[1] + z as f32 * g.spacing,
                ]
            };
            let (lo, hi) = (at(rect.x0, rect.z0), at(rect.x1, rect.z1));
            self.vegetation_changed([lo[0], lo[1], hi[0], hi[1]]);
        }
        // Bank outlines drape on the ground and show on a selected
        // or hovered river; redrawing them costs little more than
        // the markers.
        let lit =
            |id: u64| self.hover_water == Some(id) || self.selection.contains(&Item::Object(id));
        self.markers_stale |= !rect.is_empty()
            && (self.island.doc.objects.iter())
                .any(|o| o.path.is_some() && lit(o.id) && self.water_kind(o.guid) == Some(false));
        !rect.is_empty()
    }

    fn mouse_down(&mut self, view: usize, event: &MouseDownEvent) {
        // Sculpting on keeps the land job going; every other edit reads or
        // writes the land grids, so they catch up first.
        let sculpting = matches!(
            self.tool,
            Tool::Raise
                | Tool::Lower
                | Tool::Smooth
                | Tool::Flatten
                | Tool::Fill
                | Tool::Scrape
                | Tool::Noise
        );
        if event.button == MouseButton::Left && !sculpting {
            self.sync_land();
        }
        let ground = self.ground(view, event.position);
        let gesture = (event.button == MouseButton::Left)
            .then(|| self.gizmo_frame(view))
            .flatten()
            .and_then(|f| Gesture::begin(f, event.position));
        let extent = self.extent();
        let drag = match event.button {
            MouseButton::Right if self.views[view].camera.tilt > 0. && !event.modifiers.shift => {
                let v = &mut self.views[view];
                v.camera.begin_orbit(ground, v.bounds, extent);
                Some(Drag::Orbit(event.position))
            }
            MouseButton::Right | MouseButton::Middle => Some(Drag::Pan(event.position, ground)),
            MouseButton::Left if event.modifiers.alt => {
                // Alt-click is the pipette: material while painting, else the
                // flatten target height, like the JC3 editor.
                if let Some(at) = ground {
                    self.sample(at);
                }
                None
            }
            MouseButton::Left if self.tool == Tool::Place => {
                if let Some(at) = ground {
                    self.place_at(at);
                }
                None
            }
            MouseButton::Left
                if self.tool == Tool::Grid
                    && self.blocker == grids::BlockerEdit::Shapes
                    && self.edits_blockers() =>
            {
                ground.and_then(|at| {
                    let double = event.click_count >= 2;
                    self.shape_press([at[0], at[2]], event.modifiers.shift, double)
                })
            }
            MouseButton::Left if self.tool == Tool::Harbour => {
                ground.and_then(|at| self.harbour_press(view, event, at))
            }
            MouseButton::Left if self.tool == Tool::River && gesture.is_some() => {
                gesture.and_then(|g| self.river_gizmo_drag(g))
            }
            MouseButton::Left if self.tool == Tool::River => {
                ground.and_then(|at| self.river_press(view, event, at))
            }
            MouseButton::Left if self.tool == Tool::Slot && gesture.is_some() => {
                gesture.and_then(|g| self.path_gizmo_drag(g))
            }
            MouseButton::Left if self.tool == Tool::Slot => {
                ground.and_then(|at| self.slot_press(view, event, at))
            }
            // Double-clicking a river or lake opens its curve for editing.
            MouseButton::Left
                if self.tool == Tool::Select
                    && gesture.is_none()
                    && event.click_count >= 2
                    && let Some(id) = self.water_at(view, event.position) =>
            {
                self.edit_water(id);
                None
            }
            MouseButton::Left if self.tool == Tool::Select => match gesture {
                Some(gesture) => Some(Drag::Gizmo {
                    targets: self
                        .selection
                        .iter()
                        .filter_map(|&item| Some((item, self.island.doc.matrix(item)?)))
                        .collect(),
                    gesture,
                    before: Box::new(self.island.doc.clone()),
                    changed: false,
                    pending: None,
                    gpu: None,
                    turn: self.selection_turn(),
                }),
                None => Some(Drag::Marquee(Marquee::new(
                    event.position,
                    event.modifiers.shift,
                ))),
            },
            MouseButton::Left => ground.map(|at| {
                self.flow_from = None;
                let mut drag = Drag::Brush {
                    stroke: Stroke::default(),
                    before: Box::new(self.island.doc.clone()),
                    level: at[1],
                    changed: false,
                    hold: None,
                };
                self.continue_brush(&mut drag, at, event.modifiers.shift);
                drag
            }),
            _ => None,
        };
        self.views[view].drag = drag;
    }

    /// Wheel zoom toward the ground under the cursor.
    pub fn zoom(&mut self, view: usize, delta: f32, cursor: Point<Pixels>) {
        let target = self.ground(view, cursor);
        let extent = self.extent();
        let v = &mut self.views[view];
        v.camera
            .zoom_toward(delta, cursor, target, v.bounds, extent);
    }

    fn sample(&mut self, at: Vec3) {
        if self.tool == Tool::Paint {
            if let Some(m) = &self.island.doc.materials {
                self.paint = doc::shown(m, [at[0], at[2]], self.palette.as_deref());
                self.say(format!("Picked material {}", self.paint), false);
            }
        } else if self.tool == Tool::Grid {
            if let Some(layer) = self
                .island
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(self.grid))
            {
                self.grid_value = grids::value_at(layer, at[0], at[2]);
                self.say(
                    format!(
                        "Picked {} value {}",
                        grids::label(self.grid),
                        self.grid_value
                    ),
                    false,
                );
            }
        } else if self.tool == Tool::Tint {
            if let Some(map) = &self.island.doc.tint {
                self.tint_color = tint::sample(map, at[0], at[2]);
                let [r, g, b] = self.tint_color;
                self.say(format!("Picked tint ×{r:.2} ×{g:.2} ×{b:.2}"), false);
            }
        } else {
            self.level = Some(at[1]);
            self.say(format!("Target height {:.2} m", at[1]), false);
        }
    }

    /// The turn objects of asset `guid` stand on (`IslandCodec::turn_step`);
    /// 0 for free, `None` while the game still reads it.
    fn turn_step(&self, guid: u32) -> Option<f32> {
        let codec = self.module.islands().and_then(|i| i.codec().own());
        match (codec, &self.install) {
            (Some(codec), Some(install)) => codec.turn_step(install, guid),
            _ => Some(0.),
        }
    }

    /// The game's shared harbour and slot pieces (`Islands::harbours`).
    fn harbours(&self) -> Option<&'static dyn anno_core::game::Harbours> {
        self.module.islands()?.harbours()
    }

    /// The coarsest turn the selected objects stand on, which the rotate
    /// gizmo snaps to; 0 where all turn freely, `None` while any is still
    /// being read.
    pub(crate) fn selection_turn(&self) -> Option<f32> {
        let objects = &self.island.doc.objects;
        (self.selection.iter())
            .filter_map(|item| match item {
                Item::Object(id) => objects.iter().find(|o| o.id == *id),
                _ => None,
            })
            .try_fold(0., |turn: f32, o| Some(turn.max(self.turn_step(o.guid)?)))
    }

    /// Adds the Place tool's prop at `at` with a random yaw, copying tint and
    /// flags from an existing instance of the same file.
    fn place_at(&mut self, at: Vec3) {
        // Cheap spread of yaws; reproducible per placement count.
        let yaw = (self.island.next_id as f32 * 2.399_963).rem_euclid(std::f32::consts::TAU);
        if let Some((template, config)) = self.place_object.clone() {
            // 2070's records: a copy its codec numbers when it saves. An
            // asset that ships only on quarter turns (its buildings) takes
            // one; the game can't be run to say whether it refuses others.
            if template.object.record().is_some() {
                // The template came from the same index, so it is read.
                let step = self.turn_step(template.object.guid).unwrap_or(0.);
                let yaw = match step > 0. {
                    true => (yaw / step).round() * step,
                    false => yaw,
                };
                let id = (self.island.doc.objects.iter())
                    .map(|o| o.id)
                    .max()
                    .unwrap_or(0)
                    + 1;
                let mut o = template.object.copied(id);
                o.position = at;
                o.set_direction(yaw);
                self.checkpoint();
                Arc::make_mut(&mut self.island.doc.objects).push(o);
                self.selection = BTreeSet::from([Item::Object(id)]);
                self.scene_stale = true;
                return;
            }
            let Some(store) = &mut self.island.object_store else {
                return self.say("This island has no object areas", true);
            };
            store.objects.clone_from(&self.island.doc.objects);
            let area = self
                .island
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(Kind::AreaIds))
                .and_then(|a| a.cell_at(at[0], at[2]).map(|(x, y)| a.get(x, y)))
                .filter(|&a| a == harbour::CONSTRUCTION || a == harbour::BASE)
                .unwrap_or(harbour::BASE) as u16;
            let mut o = store.add_template(&template, area, at);
            if config.is_some() {
                o.config = config;
            }
            o.set_direction(yaw);
            self.checkpoint();
            let id = o.id;
            Arc::make_mut(&mut self.island.doc.objects).push(o);
            self.selection = BTreeSet::from([Item::Object(id)]);
            self.scene_stale = true;
            return;
        }
        let Some(file) = self.place else {
            return self.say("Choose something in the Library first", true);
        };
        // A prop new to the island takes default settings.
        let mut source = scatter::template(&self.island.doc, file);
        source.position = at;
        source.rotation = [0., (yaw / 2.).sin(), 0., (yaw / 2.).cos()];
        self.checkpoint();
        let id = self.island.next_id;
        self.island.next_id += 1;
        self.island.doc.props.push(Prop {
            id,
            matrix: doc::trs(source.position, source.rotation, source.scale),
            source,
            auto_cliff: false,
        });
        self.selection = BTreeSet::from([Item::Prop(id)]);
        self.scene_stale = true;
    }

    /// Places props of the `.cfg` at `path` with the Place tool, adding the
    /// model to the island's prop files if it is new.
    /// Adds the prop model `path` to the scatter set, or takes it out.
    /// True when it is in the set afterwards.
    pub fn toggle_scatter(&mut self, path: &str) -> bool {
        let file = self.island.doc.mesh_index(path);
        if let Some(k) = self.scatter.files.iter().position(|&f| f == file) {
            self.scatter.files.remove(k);
            false
        } else {
            self.scatter.files.push(file);
            true
        }
    }

    /// Replaces the scatter set with `paths`.
    pub fn set_scatter(&mut self, paths: &[String]) {
        self.scatter.files = paths
            .iter()
            .map(|p| self.island.doc.mesh_index(p))
            .collect();
    }

    /// Whether the prop model `path` is in the scatter set.
    pub fn in_scatter(&self, path: &str) -> bool {
        self.scatter.files.iter().any(|&f| {
            self.island
                .doc
                .meshes
                .get(f as usize)
                .is_some_and(|m| m.replace('\\', "/").eq_ignore_ascii_case(path))
        })
    }

    /// Dresses every face steeper than 35° with the region's cliff pieces
    /// and big rocks, as one undo step. With `cliffs.replace` the pieces of
    /// earlier runs go first; existing cliffs and rocks are kept clear of
    /// either way, so reruns do not stack.
    pub fn auto_cliffs(&mut self, install: &Install) {
        let kit = match cliffs::Kit::load(self.rules(), install, &self.region()) {
            Ok(kit) => kit,
            Err(e) => return self.say(format!("Auto cliffs: {e:#}"), true),
        };
        let before = self.island.doc.clone();
        let old = self.island.doc.props.len();
        if self.cliffs.replace {
            self.island.doc.props.retain(|p| !p.auto_cliff);
        }
        let removed = old - self.island.doc.props.len();
        let shape_of = |doc: &Doc, p: &Prop| {
            let cfg = doc.meshes.get(p.source.file as usize)?;
            kit.shapes
                .iter()
                .find(|s| s.cfg.eq_ignore_ascii_case(&cfg.replace('\\', "/")))
        };
        // Rocks of unknown size count as 4 m: the small end of the big
        // rocks, so hand-placed scatter does not block a whole face.
        let obstacles: Vec<cliffs::Obstacle> = self
            .island
            .doc
            .props
            .iter()
            .filter_map(|p| match shape_of(&self.island.doc, p) {
                Some(shape) => Some(cliffs::Obstacle::of(shape, &p.matrix)),
                None => {
                    let cfg = self.island.doc.meshes.get(p.source.file as usize)?;
                    let t = math::translation(&p.matrix);
                    cliffs::is_rock(cfg).then_some(cliffs::Obstacle {
                        at: [t[0], t[2]],
                        width: 4.,
                    })
                }
            })
            .collect();
        let placed = cliffs::plan(&self.island.doc.field, &kit, &self.cliffs, &obstacles);
        if placed.is_empty() && removed == 0 {
            self.island.doc = before;
            return self.say("No new spot steeper than 35° above sea for cliffs", true);
        }
        for p in &placed {
            let file = self.island.doc.mesh_index(&kit.shapes[p.shape].cfg);
            // Retail cliffs and rocks never carry AdaptTerrainHeight and
            // always the neutral color.
            let source = anno_formats::a7m::island::Prop {
                file,
                position: p.position,
                rotation: p.rotation,
                scale: [p.scale; 3],
                color: [1.; 4],
                adapt_terrain_height: None,
            };
            self.island.doc.props.push(Prop {
                id: self.island.next_id,
                matrix: doc::trs(source.position, source.rotation, source.scale),
                source,
                auto_cliff: true,
            });
            self.island.next_id += 1;
        }
        let painted = self.paint_cliff_ground(&kit, &placed);
        let share = self.auto_cliff_coverage(&kit);
        self.checkpoint_owned(before);
        self.selection
            .retain(|item| self.island.doc.matrix(*item).is_some());
        self.scene_stale = true;
        self.say(
            format!(
                "Auto cliffs: {} placed{}; {:.0} % of steep ground under them{}",
                placed.len(),
                if removed > 0 {
                    format!(", {removed} earlier ones replaced")
                } else {
                    String::new()
                },
                share * 100.,
                if painted {
                    "; cliff ground painted beneath"
                } else {
                    ""
                }
            ),
            false,
        );
    }

    /// Paints the region's cliff ground material on the steep ground under
    /// new cliff pieces, so faces between and around them read as rock
    /// rather than stretched grass. False when the palette has none.
    fn paint_cliff_ground(&mut self, kit: &cliffs::Kit, placed: &[cliffs::Placement]) -> bool {
        let rules = self.rules();
        let (Some(palette), Some(m)) = (self.palette.clone(), &mut self.island.doc.materials)
        else {
            return false;
        };
        let names: Vec<(u8, String)> = palette
            .materials
            .iter()
            .map(|(&id, mat)| (id, mat.file.clone()))
            .collect();
        let Some(id) = cliffs::ground_material(rules, &names) else {
            return false;
        };
        let dab = gpui_editor_kit::heightfield::Brush {
            radius: 1.5,
            inner: 0.3,
            strength: 0.85,
            ..Default::default()
        };
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for p in placed {
            let center = [p.position[0], p.position[2]];
            let radius = cliffs::width(&kit.shapes[p.shape], p.scale) / 2. + cliffs::PAINT_MARGIN;
            for at in cliffs::paint_spots(&self.island.doc.field, center, radius) {
                m.paint(at, &dab, id);
            }
            lo = [lo[0].min(center[0] - radius), lo[1].min(center[1] - radius)];
            hi = [hi[0].max(center[0] + radius), hi[1].max(center[1] + radius)];
        }
        if placed.is_empty() {
            return false;
        }
        self.show_paint([lo[0], lo[1], hi[0], hi[1]]);
        // The cliff-top bake and map shading follow the new paint.
        self.ground_stale = true;
        true
    }

    /// Share of ground over 35° under the auto-placed pieces.
    fn auto_cliff_coverage(&self, kit: &cliffs::Kit) -> f32 {
        let boxes: Vec<_> = self
            .island
            .doc
            .props
            .iter()
            .filter(|p| p.auto_cliff)
            .filter_map(|p| {
                let cfg = self.island.doc.meshes.get(p.source.file as usize)?;
                let shape = kit
                    .shapes
                    .iter()
                    .find(|s| s.cfg.eq_ignore_ascii_case(cfg))?;
                Some((p.matrix, shape.center, shape.extent))
            })
            .collect();
        cliffs::coverage(&self.island.doc.field, &boxes)
    }

    pub fn choose_prop(&mut self, path: &str) {
        // A game whose own codec saves objects places models as objects of
        // their asset (2070). Its trees, rocks and grass are props whose
        // file names their record's GUID and variation, which the codec
        // looks up when it saves a kind the island never held.
        let mut path = path.to_owned();
        if self.island.saves_through_codec() {
            let codec = self.module.islands().and_then(|i| i.codec().own());
            let (Some(islands), Some(install), Some(assets)) = (codec, &self.install, &self.assets)
            else {
                return self.say("The asset database is still loading", true);
            };
            if !islands.places_as_prop(assets, &path) {
                match islands.object_for(install, assets, &self.island, &path) {
                    Some(Ok(o)) => {
                        self.choose_object(anno_core::island::objects::Template::of_record(o), None)
                    }
                    Some(Err(e)) => self.say(format!("{e:#}"), true),
                    None => self.say("This game places no models on its islands", true),
                }
                return;
            }
            // Its prop files are spelled with forward slashes.
            path = path.replace('\\', "/");
            // A kind new to the island saves as a copy of a shipped record
            // of its asset, which the island keeps for its save.
            if !self.island.doc.meshes.contains(&path) {
                match islands.object_for(install, assets, &self.island, &path) {
                    Some(Ok(o)) => keep_foreign(&mut self.island, &o),
                    Some(Err(e)) => return self.say(format!("{e:#}"), true),
                    None => {}
                }
            }
        }
        let index = self.island.doc.mesh_index(&path);
        self.place = Some(index);
        self.place_object = None;
        self.tool = Tool::Place;
    }

    /// Places copies of a shipped object record with the Place tool.
    pub fn choose_object(
        &mut self,
        template: anno_core::island::objects::Template,
        config: Option<String>,
    ) {
        self.place_object = Some((template, config));
        self.place = None;
        self.tool = Tool::Place;
    }

    /// The Library search field is being typed into.
    /// The Hierarchy's search. Starting a name search folds every group, so
    /// the matching groups line up from the top: an open 549-row table kept
    /// the list scrolled inside one group, with no match on screen. Ending
    /// it opens the selection's group again and scrolls to it. An id search
    /// ("#518") opens what it finds on its own.
    pub fn set_filter(&mut self, text: String) {
        let (was, is) = (!self.filter.is_empty(), !text.is_empty());
        self.filter = text;
        if is && !was && !self.filter.starts_with('#') {
            self.open_groups.collapse_all();
            self.open_groups.scroll_to(0);
        } else if was
            && !is
            && let Some(&item) = self.selection.iter().next()
        {
            if let Some(group) = self.item_group(item) {
                self.open_groups.expand(group);
            }
            self.reveal.set(Some(item));
        }
    }

    pub fn library_editing(&self) -> Option<String> {
        self.library_edit.as_ref().map(|e| e.display())
    }

    pub fn edit_library_filter(&mut self) {
        self.library_edit = Some(TextEdit::new((), self.library_filter.clone(), Filter::Text));
    }

    /// The tools need shipped records to copy; the editor loads them.
    pub fn needs_harbour_library(&self) -> bool {
        // An island whose game lays its own harbours needs no coast pieces.
        let lays_own = self.tool == Tool::Harbour && self.island.saves_through_codec();
        self.island.harbour.is_none()
            && !lays_own
            && matches!(self.tool, Tool::Harbour | Tool::Slot | Tool::River)
    }

    /// Says why a tool has no shipped records yet. A failed load is not
    /// retried, so it must not read as "still loading" forever.
    /// Says why a harbour or slot was not placed; `no_record` when the
    /// province ships none of the piece.
    fn say_unplaced(&mut self, why: anno_core::island::Unplaced, what: &str, no_record: &str) {
        use anno_core::island::Unplaced;
        match why {
            Unplaced::NoRecords => self.say_no_records(what),
            Unplaced::NoRecord => self.say(no_record, true),
            Unplaced::NoObjectAreas => self.say("This island has no object areas", true),
        }
    }

    fn say_no_records(&mut self, what: &str) {
        if self.harbour_failed {
            self.say(
                format!("{what} records failed to load; reopen the island to retry"),
                true,
            );
        } else {
            self.say(format!("{what} records are still loading"), true);
        }
    }

    /// Adds a harbour spot on the shore nearest `at`, or removes the one
    /// there.
    /// Adds a harbour spot on the shore nearest `at`, or removes the one
    /// there.
    fn harbour_at(&mut self, at: Vec3, remove: bool) {
        let coasts = self.coast_guids();
        if remove {
            let near = self.island.doc.objects.iter().position(|o| {
                coasts.contains(&o.guid)
                    && o.path.as_deref().is_some_and(|pts| {
                        pts.iter()
                            .any(|p| (p.p[0] - at[0]).hypot(p.p[2] - at[2]) < 20.)
                    })
            });
            let Some(i) = near else {
                return self.say("No harbour spot there", true);
            };
            self.checkpoint();
            let gone = Arc::make_mut(&mut self.island.doc.objects).remove(i);
            self.selection.remove(&Item::Object(gone.id));
            self.curve_point = None;
            self.refresh_areas();
            return self.say("Harbour spot removed", false);
        }
        let Some(curve) = harbour::trace_coast(&self.island.doc.field, [at[0], at[2]]) else {
            return self.say("Click closer to the shore: no coast within 24 m", true);
        };
        let before = self.island.doc.clone();
        if self.push_harbour(curve) {
            self.checkpoint_owned(before);
        }
    }

    /// Adds a harbour (or the start coast) along `curve`, without an undo
    /// step of its own. False when nothing was added.
    fn push_harbour(&mut self, curve: Vec<anno_core::island::objects::PathPoint>) -> bool {
        if self.island.saves_through_codec() {
            return self.lay_harbour(&curve);
        }
        let length = curve_length(&curve);
        let id = match self.island.add_harbour(curve) {
            Ok(id) => id,
            Err(why) => {
                self.say_unplaced(why, "Harbour", "This province ships no start coast to copy");
                return false;
            }
        };
        self.refresh_areas();
        self.selection = BTreeSet::from([Item::Object(id)]);
        self.curve_point = None;
        self.scene_stale = true;
        let coasts = self.coast_guids();
        let count = self
            .island
            .doc
            .objects
            .iter()
            .filter(|o| coasts.contains(&o.guid))
            .count();
        self.say(
            format!(
                "Harbour spot added ({length:.0} m); the island has {count}. Its pad reaches 29 m into the sea"
            ),
            false,
        );
        true
    }

    /// The Harbour tool on an island whose game lays its harbours as
    /// objects by its own rules (2070's 1 m tiles): the tiles along
    /// `curve`'s shore, selected. The caller makes the undo step.
    fn lay_harbour(&mut self, curve: &[anno_core::island::objects::PathPoint]) -> bool {
        let coast: Vec<[f32; 2]> = curve.iter().map(|p| [p.p[0], p.p[2]]).collect();
        let codec = self.module.islands().and_then(|i| i.codec().own());
        let (Some(codec), Some(install)) = (codec, &self.install) else {
            self.say("The install is still loading", true);
            return false;
        };
        match codec.harbour_along(install, &self.island, &coast) {
            Some(Ok(tiles)) if !tiles.is_empty() => {
                let count = tiles.len();
                self.selection = tiles.iter().map(|o| Item::Object(o.id)).collect();
                Arc::make_mut(&mut self.island.doc.objects).extend(tiles);
                self.scene_stale = true;
                self.say(format!("Harbour laid: {count} tiles of 1 m"), false);
                true
            }
            Some(Ok(_)) => {
                self.say("No water along that shore for a harbour to face", true);
                false
            }
            Some(Err(e)) => {
                self.say(format!("{e:#}"), true);
                false
            }
            None => {
                self.say("This game lays no harbours", true);
                false
            }
        }
    }

    /// Press with the Harbour tool: pick a point handle, select a harbour or
    /// add a point to it, start drawing one, or start a region.
    fn harbour_press(&mut self, view: usize, event: &MouseDownEvent, at: Vec3) -> Option<Drag> {
        if event.modifiers.shift {
            self.harbour_at(at, true);
            return None;
        }
        if self.auto_region {
            return Some(self.region_drag(at));
        }
        if let Some((id, index)) = self.curve_handle_at(view, event.position) {
            self.curve_point = Some((id, index));
            self.selection = BTreeSet::from([Item::Object(id)]);
            return Some(Drag::CoastPoint {
                id,
                index,
                before: Box::new(self.island.doc.clone()),
                changed: false,
            });
        }
        self.curve_point = None;
        if let Some((id, segment)) = self.curve_segment_at(view, event.position) {
            // A click on a harbour selects it; a double click adds a point.
            self.selection = BTreeSet::from([Item::Object(id)]);
            if event.click_count >= 2 {
                self.insert_coast_point(id, segment, at);
            }
            return None;
        }
        Some(Drag::Shore {
            from: at,
            to: at,
            preview: None,
        })
    }

    /// Screen positions of every harbour curve point: object, index, point.
    /// With the Slot tool, the points of the path being edited instead.
    pub(super) fn curve_handles(&self, view: usize) -> Vec<(u64, usize, Point<Pixels>)> {
        let v = &self.views[view];
        let extent = self.extent();
        if self.tool == Tool::Slot {
            return match self.road_editing() {
                Some(e) => self.road_handles(view, Some(e.work)),
                None => Vec::new(),
            };
        }
        let coasts = self.coast_guids();
        self.island
            .doc
            .objects
            .iter()
            .filter(|o| coasts.contains(&o.guid))
            .flat_map(|o| {
                o.path
                    .iter()
                    .flatten()
                    .enumerate()
                    .map(move |(i, p)| (o.id, i, v.camera.project(p.p, v.bounds, extent)))
            })
            .collect()
    }

    fn curve_handle_at(&self, view: usize, at: Point<Pixels>) -> Option<(u64, usize)> {
        // Handles are drawn 6 px across; give the cursor a little more.
        const HIT: f32 = 9.;
        self.curve_handles(view)
            .into_iter()
            .map(|(id, i, p)| (id, i, f32::from(p.x - at.x).hypot(f32::from(p.y - at.y))))
            .filter(|&(.., d)| d < HIT)
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(id, i, _)| (id, i))
    }

    /// The harbour segment under the cursor: object, and the index of the
    /// segment's first point.
    fn curve_segment_at(&self, view: usize, at: Point<Pixels>) -> Option<(u64, usize)> {
        segment_at(&self.curve_handles(view), at)
    }

    /// Screen positions of the points of every work area's path, or of
    /// `only`'s: object, index, point.
    fn road_handles(&self, view: usize, only: Option<u64>) -> Vec<(u64, usize, Point<Pixels>)> {
        let v = &self.views[view];
        let extent = self.extent();
        let Some(assets) = self.assets.as_deref() else {
            return Vec::new();
        };
        (self.island.doc.objects.iter())
            .filter(|o| only.is_none_or(|id| o.id == id) && o.path.is_some())
            .filter(|o| harbour::link_end(assets, o.guid) == Some(harbour::LinkEnd::WorkArea))
            .flat_map(|o| {
                (o.path.iter().flatten())
                    .enumerate()
                    .map(move |(i, p)| (o.id, i, v.camera.project(p.p, v.bounds, extent)))
            })
            .collect()
    }
}

/// The segment of `handles` (object, index, screen point, in path order)
/// under the cursor: object, and the index of the segment's first point.
fn segment_at(handles: &[(u64, usize, Point<Pixels>)], at: Point<Pixels>) -> Option<(u64, usize)> {
    const HIT: f32 = 8.;
    let (px, py) = (f32::from(at.x), f32::from(at.y));
    handles
        .windows(2)
        .filter(|w| w[0].0 == w[1].0)
        .filter_map(|w| {
            let (ax, ay) = (f32::from(w[0].2.x), f32::from(w[0].2.y));
            let (bx, by) = (f32::from(w[1].2.x), f32::from(w[1].2.y));
            let (dx, dy) = (bx - ax, by - ay);
            let l2 = dx * dx + dy * dy;
            let t = if l2 > 0. {
                (((px - ax) * dx + (py - ay) * dy) / l2).clamp(0., 1.)
            } else {
                0.
            };
            let d = (px - ax - t * dx).hypot(py - ay - t * dy);
            (d < HIT).then_some((w[0].0, w[0].1, d))
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(id, i, _)| (id, i))
}

impl IslandEditor {
    /// Rewrites one harbour's points through `f` on their ground positions,
    /// keeping heights on the waterline and `d` toward the sea.
    fn edit_coast(&mut self, id: u64, f: impl FnOnce(&mut Vec<[f32; 2]>)) {
        let Some(o) = Arc::make_mut(&mut self.island.doc.objects)
            .iter_mut()
            .find(|o| o.id == id)
        else {
            return;
        };
        let Some(path) = &o.path else { return };
        let mut ground: Vec<[f32; 2]> = path.iter().map(|p| [p.p[0], p.p[2]]).collect();
        f(&mut ground);
        let curve = harbour::with_directions(&self.island.doc.field, &ground);
        o.position = curve[0].p;
        o.path = Some(curve);
        self.scene_stale = true;
    }

    fn insert_coast_point(&mut self, id: u64, segment: usize, at: Vec3) {
        let Some(p) = harbour::snap_to_shore(&self.island.doc.field, [at[0], at[2]]) else {
            return self.say("No shore there to add a point on", true);
        };
        self.checkpoint();
        self.edit_coast(id, |pts| pts.insert(segment + 1, p));
        self.curve_point = Some((id, segment + 1));
        self.refresh_areas();
        self.say("Point added; drag it along the shore", false);
    }

    fn remove_coast_point(&mut self) {
        let Some((id, index)) = self.curve_point else {
            return;
        };
        let len = self
            .island
            .doc
            .objects
            .iter()
            .find(|o| o.id == id)
            .and_then(|o| o.path.as_ref())
            .map_or(0, Vec::len);
        // Shipped harbours have at least 3 points.
        if len <= 3 {
            return self.say(
                "A harbour needs 3 points; Shift-click removes the whole harbour",
                true,
            );
        }
        self.checkpoint();
        self.edit_coast(id, |pts| {
            pts.remove(index.min(pts.len() - 1));
        });
        self.curve_point = None;
        self.refresh_areas();
    }

    /// Turns the seaward side of the selected harbours around, for curves
    /// where the shore test picked the wrong side.
    pub fn flip_selected_coasts(&mut self) {
        let coasts = self.coast_guids();
        let selection = self.selection.clone();
        self.checkpoint();
        for o in Arc::make_mut(&mut self.island.doc.objects)
            .iter_mut()
            .filter(|o| coasts.contains(&o.guid) && selection.contains(&Item::Object(o.id)))
        {
            for p in o.path.iter_mut().flatten() {
                p.d = p.d.map(|[x, z]| [-x, -z]);
            }
        }
        self.scene_stale = true;
    }

    /// "Buildable · slope 12°" under `at`, while buildable areas are shown.
    pub fn buildable_readout(&self, at: Vec3) -> Option<String> {
        if self.overlay() != Some(Kind::AreaIds) {
            return None;
        }
        let layer = self.island.doc.grids.as_ref()?.layer(Kind::AreaIds)?;
        let h = |x: f32, z: f32| self.island.doc.field.height([x, z]);
        let (dx, dz) = (
            (h(at[0] + 1., at[2]) - h(at[0] - 1., at[2])) / 2.,
            (h(at[0], at[2] + 1.) - h(at[0], at[2] - 1.)) / 2.,
        );
        let slope = dx.hypot(dz).atan().to_degrees();
        let what = match grids::value_at(layer, at[0], at[2]) {
            harbour::CONSTRUCTION if at[1] <= 0. => "Harbour pad",
            harbour::CONSTRUCTION => "Buildable",
            _ if at[1] <= 0. => "Sea",
            _ => "Not buildable",
        };
        Some(format!("{what} · slope {slope:.0}° · "))
    }

    /// Whether any selected item is a harbour.
    pub fn harbour_selected(&self) -> bool {
        let coasts = self.coast_guids();
        self.island
            .doc
            .objects
            .iter()
            .any(|o| coasts.contains(&o.guid) && self.selection.contains(&Item::Object(o.id)))
    }

    /// The region drag a press starts in auto mode.
    fn region_drag(&self, at: Vec3) -> Drag {
        Drag::Region {
            from: at,
            to: at,
            lasso: self.auto_lasso.then(|| vec![[at[0], at[2]]]),
        }
    }

    /// Auto placement in a dragged region, for the active tool; one undo
    /// step.
    fn auto_place(&mut self, region: harbour::Region) {
        let count = self.auto_count as usize;
        let before = self.island.doc.clone();
        let placed = if self.tool == Tool::Harbour {
            let coasts = self.coast_guids();
            let existing: Vec<Vec<anno_core::island::objects::PathPoint>> = self
                .island
                .doc
                .objects
                .iter()
                .filter(|o| coasts.contains(&o.guid))
                .filter_map(|o| o.path.clone())
                .collect();
            let refs: Vec<&[anno_core::island::objects::PathPoint]> =
                existing.iter().map(Vec::as_slice).collect();
            let curves = harbour::auto_harbours(&self.island.doc.field, &region, &refs, count);
            if curves.is_empty() {
                return self.say(
                    "No room for a harbour there: it needs shore with water 2 m deep in front, 70 m from other harbours",
                    true,
                );
            }
            curves
                .into_iter()
                .filter(|c| self.push_harbour(c.clone()))
                .count()
        } else {
            let Some(lib) = self.island.harbour.clone() else {
                return self.say_no_records("Slot");
            };
            let Some(slot_guid) = lib.guid(self.island.resource.pieces(self.island.fixed_slots).0)
            else {
                return self.say("This province ships no slot of that kind", true);
            };
            // Random and fixed slots of the kind alike: a new random mine
            // on a shipped fixed one would be two mines on one spot.
            let existing: Vec<[f32; 2]> = self
                .island
                .doc
                .objects
                .iter()
                .filter(|o| {
                    o.guid == slot_guid || lib.resource_of(o.guid) == Some(self.island.resource)
                })
                .map(|o| [o.position[0], o.position[2]])
                .collect();
            let river = self
                .island
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(Kind::River));
            let environment = self
                .island
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(Kind::Environment));
            let spots = harbour::auto_slots(
                &self.island.doc.field,
                river,
                environment,
                &region,
                self.island.resource,
                &existing,
                count,
            );
            if spots.is_empty() {
                return self.say(
                    match self.island.resource {
                        harbour::Resource::Mine => {
                            "No room for a mine there: it needs land 8 m from the sea, 30 m from other mines"
                        }
                        harbour::Resource::NewWorldMine => "No room for a DLC mine there: choose inland ground away from other slots",
                        harbour::Resource::Clay | harbour::Resource::Oil | harbour::Resource::Gas => "No room for a resource slot there: choose flat inland ground away from other slots",
                        harbour::Resource::River => {
                            "No river in that region: paint one with the Grid tool (River)"
                        }
                        harbour::Resource::Marsh => {
                            "No marsh ground there: it needs flat land 0.2–0.8 m above the sea within 24 m of the coast"
                        }
                    },
                    true,
                );
            }
            spots.iter().filter(|p| self.push_slot(**p)).count()
        };
        if placed > 0 {
            self.checkpoint_owned(before);
        }
        let what = if self.tool == Tool::Harbour {
            "harbour spot"
        } else {
            "slot"
        };
        self.say(format!("Placed {placed} {what}(s) in the region"), false);
    }

    /// Swaps the generated objects of `guids` for copies of `placed`
    /// (record, position, variation), as one undo step. Hand-placed and
    /// shipped objects of those assets stay. Returns how many were placed.
    fn replace_generated(
        &mut self,
        guids: &[u32],
        placed: Vec<(&anno_core::island::objects::Template, [f32; 3], u32)>,
    ) -> usize {
        let Some(store) = &mut self.island.object_store else {
            self.say("This island has no object areas", true);
            return 0;
        };
        let before = self.island.doc.clone();
        Arc::make_mut(&mut self.island.doc.objects)
            .retain(|o| !(guids.contains(&o.guid) && is_generated(o.id)));
        store.objects.clone_from(&self.island.doc.objects);
        let areas = self
            .island
            .doc
            .grids
            .as_ref()
            .and_then(|g| g.layer(Kind::AreaIds));
        let mut low = store
            .objects
            .iter()
            .filter(|o| is_generated(o.id))
            .map(|o| o.id as u32)
            .max()
            .unwrap_or(0);
        let n = placed.len();
        for (record, position, variation) in placed {
            let area = areas
                .and_then(|a| {
                    a.cell_at(position[0], position[2])
                        .map(|(x, y)| a.get(x, y))
                })
                .filter(|&a| a == harbour::CONSTRUCTION)
                .unwrap_or(harbour::BASE) as u16;
            let mut o = store.add_template(record, area, position);
            low += 1;
            o.id = GENERATED_ID_HIGH | u64::from(low);
            o.variation = variation;
            o.skin = 0;
            o.scale = [1.; 3];
            o.direction = 0.;
            o.rotation = objects::yaw_rotation(0.);
            Arc::make_mut(&mut self.island.doc.objects).push(o);
        }
        let doc = &self.island.doc;
        self.selection.retain(|&item| doc.matrix(item).is_some());
        if self.island.doc.objects != before.objects {
            self.checkpoint_owned(before);
        }
        self.scene_stale = true;
        n
    }

    /// Sound spots and build blockers for the overlay: position, blocker
    /// side in metres (0 for a spot), and whether the editor generated it.
    pub(super) fn spot_marks(&self) -> Vec<([f32; 3], f32, bool)> {
        self.island
            .doc
            .objects
            .iter()
            .filter_map(|o| {
                let blocker = self.rules().build_blocker;
                let side = if blocker != 0 && o.guid == blocker {
                    *blockers::SIZES.get(o.variation as usize)? as f32
                } else if sound::guids(self.rules()).contains(&o.guid)
                    || self
                        .assets
                        .as_ref()
                        .is_some_and(|a| harbour::template_of(a, o.guid) == "AudioSpots")
                {
                    0.
                } else {
                    return None;
                };
                Some((o.position, side, is_generated(o.id)))
            })
            .collect()
    }

    /// What the island has for settling, for the inspector: text, and
    /// whether it is a problem.
    pub fn harbour_report(&self) -> Vec<(String, bool)> {
        let coasts = self.coast_guids();
        let curves: Vec<_> = self
            .island
            .doc
            .objects
            .iter()
            .filter(|o| coasts.contains(&o.guid))
            .collect();
        let starts = (curves.iter())
            .filter(|o| self.piece(o.guid) == Some(Piece::StartCoast))
            .count();
        let harbours = curves.len() - starts;
        let mut out = vec![if harbours == 0 {
            (
                "No harbour spot: players cannot found a settlement".to_owned(),
                true,
            )
        } else {
            (format!("{harbours} harbour spot(s)"), false)
        }];
        if self.listing.is_start(self.rules()) {
            out.push(if starts == 0 {
                (
                    "No start coast: a start island needs one (Harbour tool → Start coast)".into(),
                    true,
                )
            } else {
                ("Start coast placed".into(), false)
            });
        }
        let short = curves
            .iter()
            .filter(|o| curve_length(o.path.as_deref().unwrap_or(&[])) < 20.)
            .count();
        if short > 0 {
            out.push((
                format!("{short} harbour(s) shorter than 20 m; shipped ones are 25–75 m"),
                true,
            ));
        }
        let slots: Vec<u64> = self
            .island
            .doc
            .objects
            .iter()
            .filter(|o| self.is_slot(o.guid))
            .map(|o| o.id)
            .collect();
        let assigned = slots
            .iter()
            .filter(|id| self.listing.slot_resources.contains_key(id))
            .count();
        out.push((
            format!(
                "{} resource slot(s), {assigned} with a test-map resource",
                slots.len()
            ),
            false,
        ));
        out
    }

    /// Whether `guid` is a resource slot (mine, river or marsh, random or
    /// fixed), from the library or assets.xml.
    pub(super) fn is_slot(&self, guid: u32) -> bool {
        self.island
            .harbour
            .as_ref()
            .is_some_and(|l| l.is_slot(guid))
            || self.assets.as_ref().is_some_and(|a| {
                matches!(
                    harbour::template_of(a, guid).as_str(),
                    "Slot" | "Slot_Marsh"
                )
            })
    }

    /// The single selected object when it is a slot: its ID and kind.
    pub fn selected_slot(&self) -> Option<(u64, harbour::Resource)> {
        let [Item::Object(id)] = self.selection.iter().copied().collect::<Vec<_>>()[..] else {
            return None;
        };
        let o = self.island.doc.objects.iter().find(|o| o.id == id)?;
        let slots = self.rules().slots;
        if slots == anno_core::game::ResourceSlots::None {
            return None;
        }
        if slots == anno_core::game::ResourceSlots::Random {
            let asset = self.assets.as_ref()?.get(o.guid)?;
            if asset.template != "Slot" || asset.slot_type.as_deref() != Some("Random") {
                return None;
            }
            // The library's records, else the piece the game names it; a
            // random slot of no resource it names is a mine.
            let kind = (self.island.harbour.as_ref())
                .and_then(|lib| lib.resource_of(o.guid))
                .or_else(|| self.piece(o.guid).and_then(harbour::Resource::of_slot))
                .unwrap_or(harbour::Resource::Mine);
            return Some((id, kind));
        }
        Some((id, self.island.harbour.as_ref()?.resource_of(o.guid)?))
    }

    /// Gives the selected slot a deposit for the test map, or clears it.
    /// Sets `AdaptTerrainHeight` on every selected prop: whether it keeps
    /// its height above the ground through sculpting and moves, and may
    /// bend to the ground where its model asks to.
    pub fn set_follows_terrain(&mut self, on: bool) {
        let ids: Vec<u64> = (self.selection.iter())
            .filter_map(|i| match i {
                Item::Prop(id) => Some(*id),
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        self.checkpoint();
        for id in ids {
            if let Some(p) = self.island.doc.prop_mut(id) {
                p.source.adapt_terrain_height = on.then_some(1);
            }
        }
        // Rows carry whether they may bend.
        self.scene_stale = true;
    }

    pub fn set_slot_resource(&mut self, deposit: Option<u32>) {
        let Some((id, _)) = self.selected_slot() else {
            return;
        };
        self.set_listing(|l| match deposit {
            Some(g) => {
                l.slot_resources.insert(id, g);
            }
            None => {
                l.slot_resources.remove(&id);
            }
        });
        // The resource picks the building, and so the model along its
        // work areas' paths.
        self.rails_stale = true;
        self.markers_stale = true;
    }

    /// Every resource slot, for the markers the slot tool keeps over them:
    /// its kind, any test-map deposit, and whether it is the kind being
    /// placed.
    pub(super) fn slot_marks(&self) -> Vec<SlotMark> {
        let Some(lib) = &self.island.harbour else {
            return Vec::new();
        };
        self.island
            .doc
            .objects
            .iter()
            .filter_map(|o| {
                let resource = lib.resource_of(o.guid)?;
                let fixed = lib.guid(resource.pieces(true).0) == Some(o.guid)
                    && lib.guid(resource.pieces(false).0) != Some(o.guid);
                let name = (harbour::Resource::ALL.iter())
                    .find(|(r, _)| *r == resource)
                    .map_or("Slot", |(_, n)| n);
                let mut label = name.to_owned();
                if let (Some(assets), Some(g)) =
                    (&self.assets, self.listing.slot_resources.get(&o.id))
                {
                    label = format!("{label} · {}", fertility::label(assets, *g));
                }
                Some(SlotMark {
                    at: o.position,
                    label,
                    resource,
                    fixed,
                    yaw: o.direction,
                    current: resource == self.island.resource,
                })
            })
            .collect()
    }

    /// Places a slot and its work areas at `at`, or removes the ones there.
    fn slot_at(&mut self, at: Vec3, remove: bool) {
        let Some(lib) = self.island.harbour.clone() else {
            return self.say_no_records("Slot");
        };
        // Removal takes random and fixed slots of the resource alike.
        let ((slot, work), (fixed, _)) = (
            self.island.resource.pieces(false),
            self.island.resource.pieces(true),
        );
        let guids: Vec<u32> = ([slot, fixed].iter().chain(work))
            .filter_map(|&p| lib.guid(p))
            .collect();
        // A click on a slot selects it, for the resource picker, instead of
        // stacking a second one there.
        if !remove
            && let Some(o) = self.island.doc.objects.iter().find(|o| {
                lib.is_slot(o.guid) && (o.position[0] - at[0]).hypot(o.position[2] - at[2]) < 4.
            })
        {
            self.selection = BTreeSet::from([Item::Object(o.id)]);
            return self.say(
                "Slot selected; pick its test-map resource above. Ctrl-click adds a work area",
                false,
            );
        }
        if remove {
            let mut gone: BTreeSet<Item> = (self.island.doc.objects.iter())
                .filter(|o| {
                    guids.contains(&o.guid)
                        && (o.position[0] - at[0]).hypot(o.position[2] - at[2]) < 6.
                })
                .map(|o| Item::Object(o.id))
                .collect();
            if gone.is_empty() {
                return self.say("No slot of that kind there", true);
            }
            gone.extend(self.served_work_areas(&gone));
            let snapshot = self.island.doc.clone();
            Arc::make_mut(&mut self.island.doc.objects)
                .retain(|o| !gone.contains(&Item::Object(o.id)));
            let doc = &self.island.doc;
            self.selection.retain(|&item| doc.matrix(item).is_some());
            self.checkpoint_owned(snapshot);
            self.scene_stale = true;
            return self.say("Slot removed", false);
        }
        let before = self.island.doc.clone();
        if self.push_slot(at) {
            self.checkpoint_owned(before);
            self.say(
                "Slot placed; pick a test-map resource above, or let the map generator decide",
                false,
            );
        }
    }

    /// Adds a slot of the chosen resource and its work areas at `at`,
    /// without an undo step of its own. False when nothing was added.
    fn push_slot(&mut self, at: Vec3) -> bool {
        let islands = self.module.islands().expect("island workspace");
        // Turned the way shipped slots of the kind face: into the mountain,
        // with the river alongside, towards the sea.
        let river = (self.island.doc.grids.as_ref()).and_then(|g| g.layer(Kind::River));
        let yaw = harbour::slot_yaw(
            &self.island.doc.field,
            river,
            self.island.resource,
            [at[0], at[2]],
            islands.rules().slot_yaw_step,
        );
        let Some(harbours) = islands.harbours() else {
            self.say("This game lays no slots", true);
            return false;
        };
        let id = match self.island.add_slot(at, yaw, harbours) {
            Ok(id) => id,
            Err(why) => {
                self.say_unplaced(why, "Slot", "This province ships no slot of that kind");
                return false;
            }
        };
        self.selection = BTreeSet::from([Item::Object(id)]);
        self.scene_stale = true;
        true
    }

    /// The selected slots (the Slot tool's subject): id, GUID, position,
    /// yaw.
    fn selected_slots(&self) -> Vec<(u64, u32, Vec3, f32)> {
        let Some(assets) = self.assets.as_deref() else {
            return Vec::new();
        };
        (self.island.doc.objects.iter())
            .filter(|o| self.selection.contains(&Item::Object(o.id)))
            .filter(|o| harbour::link_end(assets, o.guid) == Some(harbour::LinkEnd::Slot))
            .map(|o| (o.id, o.guid, o.position, o.direction))
            .collect()
    }

    /// The Inspector's path preview: for the selected slot, or the slot a
    /// selected work area serves.
    pub(super) fn path_preview(&self) -> Option<PathPreview> {
        let assets = self.assets.as_deref()?;
        let [Item::Object(id)] = self.selection.iter().copied().collect::<Vec<_>>()[..] else {
            return None;
        };
        let o = self.island.doc.objects.iter().find(|o| o.id == id)?;
        let slot = match harbour::link_end(assets, o.guid)? {
            harbour::LinkEnd::Slot => id,
            harbour::LinkEnd::WorkArea => {
                (self.slot_links([Item::Object(id)]).into_iter())
                    .find(|l| l.work == id)?
                    .slot
            }
        };
        let guid = self.island.doc.objects.iter().find(|o| o.id == slot)?.guid;
        // Only buildings that lay something on one of the slot's work areas
        // are worth a choice.
        let works: Vec<u32> = (self.slot_links([Item::Object(slot)]).into_iter())
            .filter(|l| l.slot == slot)
            .filter_map(|l| {
                self.island
                    .doc
                    .objects
                    .iter()
                    .find(|o| o.id == l.work)
                    .map(|o| o.guid)
            })
            .collect();
        // Scenario copies of a mine share its name and models: one choice.
        let mut seen: Vec<(String, Vec<Option<&str>>)> = Vec::new();
        let candidates = assets.slot_buildings(guid, &self.asset_region(), None);
        let buildings: Vec<(u32, String)> = (candidates.into_iter())
            .filter_map(|b| {
                let lays: Vec<Option<&str>> = (works.iter())
                    .map(|&w| assets.building_work_area_path(b.guid, w))
                    .collect();
                let name = b.text.clone().unwrap_or_else(|| b.name.clone());
                let key = (name.clone(), lays);
                if key.1.iter().all(Option::is_none) || seen.contains(&key) {
                    return None;
                }
                seen.push(key);
                Some((b.guid, name))
            })
            .collect();
        (!buildings.is_empty()).then(|| PathPreview {
            slot,
            buildings,
            current: self.path_previews.get(&slot).copied(),
        })
    }

    /// Sets the path preview of `slot`: a building, 0 for none, None for
    /// Auto.
    pub fn set_path_preview(&mut self, slot: u64, choice: Option<u32>) {
        match choice {
            Some(c) => self.path_previews.insert(slot, c),
            None => self.path_previews.remove(&slot),
        };
        self.rails_stale = true;
        self.markers_stale = true;
    }

    /// Work areas whose path carries a model, and the model's cfg: what
    /// the assets say a building on their slot lays along it
    /// (`Assets::work_area_path`), narrowed by the slot's test-map resource
    /// if one is picked ("Empty" lays nothing). With the objects layer on
    /// and the work area's group shown.
    fn rail_works(&self) -> Vec<(u64, String)> {
        let Some(assets) = self.assets.as_deref() else {
            return Vec::new();
        };
        if !self.layers.objects {
            return Vec::new();
        }
        let region = self.asset_region();
        let objects = &self.island.doc.objects;
        let guid = |id: u64| objects.iter().find(|o| o.id == id).map(|o| o.guid);
        harbour::links(objects, |g| harbour::link_end(assets, g), |_| true)
            .into_iter()
            .filter_map(|l| {
                let work = guid(l.work)?;
                if self.hidden_groups.contains(&Group::Object(work)) {
                    return None;
                }
                let cfg = match self.path_previews.get(&l.slot) {
                    Some(0) => return None,
                    Some(&building) => assets.building_work_area_path(building, work)?,
                    None => {
                        let resource = self.listing.slot_resources.get(&l.slot).copied();
                        if resource == Some(0) {
                            return None;
                        }
                        assets.work_area_path(guid(l.slot)?, work, &region, resource)?
                    }
                };
                Some((l.work, cfg.replace('\\', "/")))
            })
            .collect()
    }

    /// Bends each work area's path model along its path where either
    /// changed. Models load with the island's; until one has, its paths
    /// show none.
    fn refresh_rails(&mut self) {
        let job = self.rails_work();
        self.rails_built(job());
    }

    /// The rails due a bend (`rails_wanted`), to bend off the UI thread;
    /// one bend at a time, the newest request next.
    pub(crate) fn rails_job(&mut self) -> Option<impl FnOnce() -> RailsBuilt + Send + use<>> {
        if !self.rails_wanted || self.rails_building {
            return None;
        }
        self.rails_wanted = false;
        self.rails_building = true;
        Some(self.rails_work())
    }

    /// Each work area's path and model, with the template it bends (or
    /// the pieces to make one from) and the rails drawn now, whose
    /// unchanged paths keep their meshes.
    fn rails_work(&self) -> impl FnOnce() -> RailsBuilt + Send + use<> {
        type Wanted = (
            u64,
            String,
            Vec<anno_core::island::objects::PathPoint>,
            Option<Arc<rail::Template>>,
        );
        let mut wanted: Vec<Wanted> = Vec::new();
        let mut pieces: HashMap<String, Vec<(Mat4, Arc<Mesh>)>> = HashMap::new();
        let objects = &self.island.doc.objects;
        for (id, cfg) in self.rail_works() {
            let Some(path) = (objects.iter().find(|o| o.id == id)).and_then(|o| o.path.clone())
            else {
                continue;
            };
            let template = self.rail_templates.get(&cfg).cloned();
            if template.is_none()
                && !pieces.contains_key(&cfg)
                && let Some(loaded) = self.library.models.loaded.get(&cfg)
            {
                let loaded = loaded.iter().map(|p| (p.local, p.mesh.clone()));
                pieces.insert(cfg.clone(), loaded.collect());
            }
            wanted.push((id, cfg, path, template));
        }
        let mut rails = self.rails.clone();
        move || {
            let made: HashMap<String, Arc<rail::Template>> = (pieces.into_iter())
                .filter_map(|(cfg, pieces)| {
                    let pieces = pieces.iter().map(|(local, mesh)| (*local, mesh.geometry()));
                    Some((cfg, Arc::new(rail::Template::new(pieces)?)))
                })
                .collect();
            let paths = wanted.iter().filter_map(|(id, cfg, path, template)| {
                let template = template.as_ref().or_else(|| made.get(cfg))?;
                Some((*id, cfg.as_str(), template.as_ref(), path.as_slice()))
            });
            rails.update(paths);
            RailsBuilt {
                rails,
                templates: made,
            }
        }
    }

    /// A finished `rails_job`, drawn from the next scene refresh.
    pub(crate) fn rails_built(&mut self, built: RailsBuilt) {
        self.rails_building = false;
        self.rail_templates.extend(built.templates);
        self.rails = built.rails;
        self.markers_stale = true;
    }

    /// Which piece an object of `guid` plays on this game's islands
    /// (`Islands::piece_of`): a work area's for its road's shape, a slot's
    /// resource, a start coast.
    fn piece(&self, guid: u32) -> Option<Piece> {
        harbour::piece_of(self.module, self.assets.as_deref()?, guid)
    }

    /// The Slot tool's Road button: lays each selected slot's work areas
    /// and their roads anew the way shipped islands of this game do, and
    /// adds the work areas its resource ships but the slot lacks (an 1800
    /// mine's mountain and cave ones). One undo step.
    pub fn rebuild_slot_roads(&mut self) {
        let Some(lib) = self.island.harbour.clone() else {
            return self.say_no_records("Slot");
        };
        let slots = self.selected_slots();
        if slots.is_empty() {
            return self.say("Select a mine or river slot first", true);
        }
        let before = self.island.doc.clone();
        let links = self.slot_links(slots.iter().map(|&(id, ..)| Item::Object(id)));
        let field = self.island.doc.field.clone();
        let Some(harbours) = self.harbours() else {
            return self.say("This game lays no work areas", true);
        };
        let (mut laid, mut added) = (0, 0);
        for (id, guid, at, yaw) in slots {
            let mut kinds = Vec::new();
            for link in links.iter().filter(|l| l.slot == id) {
                let Some(w) = (self.island.doc.objects.iter()).find(|o| o.id == link.work) else {
                    continue;
                };
                let kind = self.piece(w.guid);
                kinds.push(kind);
                let shape = harbours.road_shape(kind);
                let objects = Arc::make_mut(&mut self.island.doc.objects);
                let Some(w) = objects.iter_mut().find(|o| o.id == link.work) else {
                    continue;
                };
                w.position = shape.work_area_position(&field, at, yaw);
                shape.link(w, &field, at);
                laid += 1;
            }
            // Missing kinds; marsh, clay and oil slots ship none.
            let missing: Vec<Piece> = (lib.resource_of(guid))
                .map_or(&[][..], |r| r.pieces(false).1)
                .iter()
                .copied()
                .filter(|p| !kinds.contains(&Some(*p)))
                .collect();
            let Some(slot) = (self.island.doc.objects.iter())
                .find(|o| o.id == id)
                .cloned()
            else {
                continue;
            };
            let new = self
                .island
                .with_store(|store, areas| {
                    lib.place_work_areas(store, areas, &field, harbours, &missing, &slot)
                })
                .unwrap_or_default();
            added += new.len();
            Arc::make_mut(&mut self.island.doc.objects).extend(new);
        }
        if laid + added == 0 {
            self.island.doc = before;
            return self.say("These slots take no work area", true);
        }
        self.checkpoint_owned(before);
        self.scene_stale = true;
        let roads = crate::project::plural(laid + added, "road", "roads");
        match added {
            0 => self.say(format!("Laid {roads}"), false),
            n => self.say(format!("Laid {roads}, {n} with a new work area"), false),
        }
    }

    /// Ctrl-click with the Slot tool: a work area at `at` for the selected
    /// slot, of the first kind its resource ships that the slot lacks, else
    /// a second of its first kind. Shipped 1800 slots serve up to two (one
    /// serves three); the game reads each path on its own.
    fn add_work_area(&mut self, at: Vec3) {
        let Some(lib) = self.island.harbour.clone() else {
            return self.say_no_records("Slot");
        };
        let [(id, guid, slot, _)] = self.selected_slots()[..] else {
            return self.say(
                "Select one slot, then Ctrl-click where its work area goes",
                true,
            );
        };
        let have: Vec<Option<Piece>> = (self.slot_links([Item::Object(id)]).iter())
            .filter(|l| l.slot == id)
            .filter_map(|l| (self.island.doc.objects.iter()).find(|o| o.id == l.work))
            .map(|o| self.piece(o.guid))
            .collect();
        let kinds = lib.resource_of(guid).map_or(&[][..], |r| r.pieces(false).1);
        let kinds: Vec<Piece> = kinds
            .iter()
            .copied()
            .filter(|p| lib.guid(*p).is_some())
            .collect();
        let Some(&kind) = (kinds.iter())
            .find(|p| !have.contains(&Some(**p)))
            .or(kinds.first())
        else {
            return self.say("This slot takes no work area", true);
        };
        let Some(harbours) = self.harbours() else {
            return self.say("This game lays no work areas", true);
        };
        let shape = harbours.road_shape(Some(kind));
        let field = self.island.doc.field.clone();
        let placed = self
            .island
            .with_store(|store, areas| {
                let mut w = lib.place(
                    store,
                    kind,
                    areas,
                    [at[0], field.height([at[0], at[2]]), at[2]],
                )?;
                shape.link(&mut w, &field, slot);
                Some(w)
            })
            .flatten();
        let Some(w) = placed else {
            return self.say("No work area record to place here", true);
        };
        self.checkpoint();
        let wid = w.id;
        Arc::make_mut(&mut self.island.doc.objects).push(w);
        self.selection = BTreeSet::from([Item::Object(wid)]);
        self.scene_stale = true;
        self.say("Work area added; drag its first point to move it", false);
    }

    /// Press with the Slot tool. In the path edit mode: pick points
    /// (Shift adds), add one by double-clicking the path, switch paths by
    /// double-clicking another, nothing else. Otherwise: add a work area
    /// (Ctrl), start a region, select a path (double-click edits it), or
    /// select, place or remove (Shift) a slot.
    fn slot_press(&mut self, view: usize, event: &MouseDownEvent, at: Vec3) -> Option<Drag> {
        let shift = event.modifiers.shift;
        if let Some(edit) = self.road_editing().cloned() {
            let len = self.road_points(edit.work).len();
            if let Some((_, index)) = self.curve_handle_at(view, event.position)
                && index + 1 < len
            {
                let picked = &mut self.road_edit.as_mut()?.picked;
                if !shift {
                    picked.clear();
                }
                if !picked.remove(&index) {
                    picked.insert(index);
                }
                return None;
            }
            if event.click_count >= 2
                && let Some((id, segment)) =
                    segment_at(&self.road_handles(view, None), event.position)
            {
                if id == edit.work {
                    self.insert_road_point(segment, Some(at));
                } else {
                    self.start_road_edit(id);
                }
                return None;
            }
            if !shift && let Some(e) = &mut self.road_edit {
                e.picked.clear();
            }
            return None;
        }
        if event.modifiers.control {
            self.add_work_area(at);
            return None;
        }
        if self.auto_region && !shift {
            return Some(self.region_drag(at));
        }
        // A click on a slot is for the slot, though its paths end there.
        let near_slot = (self.island.harbour.as_ref()).is_some_and(|lib| {
            (self.island.doc.objects.iter()).any(|o| {
                lib.is_slot(o.guid) && (o.position[0] - at[0]).hypot(o.position[2] - at[2]) < 4.
            })
        });
        if !shift
            && !near_slot
            && let Some((id, _)) = segment_at(&self.road_handles(view, None), event.position)
        {
            self.selection = BTreeSet::from([Item::Object(id)]);
            if event.click_count >= 2 {
                self.start_road_edit(id);
            } else {
                self.say(
                    "Path selected; double-click it or press Edit path to edit its points",
                    false,
                );
            }
            return None;
        }
        self.slot_at(at, shift);
        None
    }

    /// The path edit mode, while the Slot tool is on and its work area and
    /// path still exist (undo can take them).
    pub(super) fn road_editing(&self) -> Option<&RoadEdit> {
        let e = self
            .road_edit
            .as_ref()
            .filter(|_| self.tool == Tool::Slot)?;
        (self.island.doc.objects.iter())
            .any(|o| o.id == e.work && o.path.as_ref().is_some_and(|p| p.len() > 1))
            .then_some(e)
    }

    /// Points of work area `id`'s path.
    fn road_points(&self, id: u64) -> Vec<Vec3> {
        (self.island.doc.objects.iter())
            .find(|o| o.id == id)
            .and_then(|o| o.path.as_ref())
            .map(|p| p.iter().map(|p| p.p).collect())
            .unwrap_or_default()
    }

    /// The edit mode's picked points that still exist, ascending.
    fn picked_road_points(&self) -> Vec<usize> {
        let Some(e) = self.road_editing() else {
            return Vec::new();
        };
        let len = self.road_points(e.work).len();
        e.picked.iter().copied().filter(|&i| i + 1 < len).collect()
    }

    /// Where the path gizmo stands: the picked points' centre.
    fn road_pivot(&self) -> Option<Vec3> {
        let picked = self.picked_road_points();
        let points = self.road_points(self.road_editing()?.work);
        let n = picked.len() as f32;
        (n > 0.).then(|| {
            let sum = (picked.iter()).fold([0.; 3], |acc, &i| math::add(acc, points[i]));
            math::scale(sum, 1. / n)
        })
    }

    /// Opens work area `id`'s path in the edit mode.
    pub fn start_road_edit(&mut self, id: u64) {
        self.tool = Tool::Slot;
        self.road_edit = Some(RoadEdit {
            work: id,
            picked: BTreeSet::new(),
        });
        self.selection = BTreeSet::from([Item::Object(id)]);
        self.say(
            "Editing the path: click points (Shift adds) and move them with the gizmo; double-click the path to add a point; Enter when done",
            false,
        );
    }

    /// The Edit path button: the selected work area's path, or the first
    /// of the selected slot's.
    pub fn edit_selected_path(&mut self) {
        let Some(assets) = self.assets.clone() else {
            return;
        };
        let end = |o: &anno_core::island::objects::Object| harbour::link_end(&assets, o.guid);
        let selected = (self.island.doc.objects.iter())
            .filter(|o| self.selection.contains(&Item::Object(o.id)));
        let work = selected
            .clone()
            .find(|o| end(o) == Some(harbour::LinkEnd::WorkArea))
            .map(|o| o.id);
        let work = work.or_else(|| {
            let slots: Vec<u64> = (selected.filter(|o| end(o) == Some(harbour::LinkEnd::Slot)))
                .map(|o| o.id)
                .collect();
            let links = self.slot_links(slots.iter().map(|&id| Item::Object(id)));
            links
                .into_iter()
                .find(|l| slots.contains(&l.slot))
                .map(|l| l.work)
        });
        match work {
            Some(id) => self.start_road_edit(id),
            None => self.say("Select a slot with a work area, or a work area", true),
        }
    }

    /// Rewrites work area `id`'s path through `f` on its points in 3D,
    /// then its Bezier handles by its kind's shape. The last point stays on
    /// the slot.
    fn set_road_points(&mut self, id: u64, f: impl FnOnce(&mut Vec<Vec3>)) {
        let Some(guid) = (self.island.doc.objects.iter())
            .find(|o| o.id == id)
            .map(|o| o.guid)
        else {
            return;
        };
        let Some(harbours) = self.harbours() else {
            return;
        };
        let shape = harbours.road_shape(self.piece(guid));
        let mut points = self.road_points(id);
        let Some(&last) = points.last() else { return };
        f(&mut points);
        if let Some(end) = points.last_mut() {
            *end = last;
        }
        if let Some(o) = Arc::make_mut(&mut self.island.doc.objects)
            .iter_mut()
            .find(|o| o.id == id)
        {
            o.path = Some(shape.handles(&points));
        }
        self.markers_stale = true;
        self.rails_stale = true;
    }

    /// Adds a point to the edited path after `segment`'s first point: at
    /// `at` (its height read off the segment), else halfway. Picks it.
    fn insert_road_point(&mut self, segment: usize, at: Option<Vec3>) {
        let Some(work) = self.road_editing().map(|e| e.work) else {
            return;
        };
        self.checkpoint();
        self.set_road_points(work, |pts| {
            let (a, b) = (pts[segment], pts[segment + 1]);
            let p = match at {
                Some(at) => {
                    let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
                    let l2 = (dx * dx + dz * dz).max(1e-6);
                    let t = (((at[0] - a[0]) * dx + (at[2] - a[2]) * dz) / l2).clamp(0., 1.);
                    [at[0], a[1] + (b[1] - a[1]) * t, at[2]]
                }
                None => std::array::from_fn(|k| (a[k] + b[k]) / 2.),
            };
            pts.insert(segment + 1, p);
        });
        if let Some(e) = &mut self.road_edit {
            e.picked = BTreeSet::from([segment + 1]);
        }
    }

    /// The Add point button: a point halfway along the segment after each
    /// picked point, or along the longest segment with none picked. Picks
    /// the new points. One undo step.
    pub fn add_road_points(&mut self) {
        let Some(work) = self.road_editing().map(|e| e.work) else {
            return;
        };
        let points = self.road_points(work);
        let mut after = self.picked_road_points();
        if after.is_empty() {
            let len = |k: usize| {
                let (a, b) = (points[k], points[k + 1]);
                (b[0] - a[0]).hypot(b[2] - a[2])
            };
            after = (0..points.len() - 1)
                .max_by(|&a, &b| len(a).total_cmp(&len(b)))
                .into_iter()
                .collect();
        }
        self.checkpoint();
        // Back to front, so earlier indices hold.
        self.set_road_points(work, |pts| {
            for &k in after.iter().rev() {
                let mid = std::array::from_fn(|x| (pts[k][x] + pts[k + 1][x]) / 2.);
                pts.insert(k + 1, mid);
            }
        });
        if let Some(e) = &mut self.road_edit {
            // The n-th new point (ascending) lands at k + 1, shifted by
            // the n inserted before it.
            e.picked = (after.iter().enumerate())
                .map(|(n, &k)| k + 1 + n)
                .collect();
        }
    }

    /// Deletes the picked points; the first and last stay, as every path
    /// keeps its two ends. One undo step.
    pub fn remove_road_points(&mut self) {
        let Some(work) = self.road_editing().map(|e| e.work) else {
            return;
        };
        let doomed: Vec<usize> = (self.picked_road_points().into_iter())
            .filter(|&i| i > 0)
            .collect();
        if doomed.is_empty() {
            return self.say(
                "Pick an inner point to delete; a path keeps its first and last points",
                true,
            );
        }
        self.checkpoint();
        self.set_road_points(work, |pts| {
            for &i in doomed.iter().rev() {
                pts.remove(i);
            }
        });
        if let Some(e) = &mut self.road_edit {
            e.picked.clear();
        }
    }

    /// A press on the path gizmo: drags the picked points.
    fn path_gizmo_drag(&self, gesture: Gesture) -> Option<Drag> {
        let points = self.road_points(self.road_editing()?.work);
        let start: Vec<(usize, Vec3)> = (self.picked_road_points().into_iter())
            .map(|i| (i, points[i]))
            .collect();
        (!start.is_empty()).then(|| Drag::PathGizmo {
            gesture,
            start,
            before: Box::new(self.island.doc.clone()),
            changed: false,
        })
    }

    /// Slot links touching objects among `items`, taken before an edit
    /// moves or removes them.
    fn slot_links(&self, items: impl IntoIterator<Item = Item>) -> Vec<harbour::Link> {
        let Some(assets) = self.assets.as_deref() else {
            return Vec::new();
        };
        let ids: BTreeSet<u64> = (items.into_iter())
            .filter_map(|i| match i {
                Item::Object(id) => Some(id),
                Item::Prop(_) => None,
            })
            .collect();
        let end = |guid| harbour::link_end(assets, guid);
        // Most edits move no slot; skip the island-wide scan for them.
        if !(self.island.doc.objects.iter()).any(|o| ids.contains(&o.id) && end(o.guid).is_some()) {
            return Vec::new();
        }
        harbour::links(&self.island.doc.objects, end, |id| ids.contains(&id))
    }

    /// After a move: each work area's path runs from it to its slot again.
    fn follow_slot_links(&mut self, links: &[harbour::Link]) {
        if links.is_empty() {
            return;
        }
        let objects = Arc::make_mut(&mut self.island.doc.objects);
        let at = |id: u64| objects.iter().find(|o| o.id == id).map(|o| o.position);
        let paths: Vec<_> = (links.iter())
            .filter_map(|l| Some((l.work, l.follow(at(l.work)?, at(l.slot)?)?)))
            .collect();
        for (work, path) in paths {
            if let Some(o) = objects.iter_mut().find(|o| o.id == work) {
                o.path = Some(path);
                self.rails_stale = true;
            }
        }
    }

    /// The work areas serving slots among `items`: they go when their slot
    /// does. 1800 work areas stand about 12 m away, past any click radius.
    fn served_work_areas(&self, items: &BTreeSet<Item>) -> Vec<Item> {
        (self.slot_links(items.iter().copied()).into_iter())
            .filter(|l| items.contains(&Item::Object(l.slot)))
            .map(|l| Item::Object(l.work))
            .collect()
    }

    /// GUIDs of coastline assets, from the library or assets.xml.
    pub(super) fn coast_guids(&self) -> Vec<u32> {
        if let Some(lib) = &self.island.harbour {
            return [Piece::Coast, Piece::StartCoast]
                .iter()
                .filter_map(|&p| lib.guid(p))
                .collect();
        }
        self.island
            .doc
            .objects
            .iter()
            .map(|o| o.guid)
            .filter(|&g| matches!(self.piece(g), Some(Piece::Coast | Piece::StartCoast)))
            .collect()
    }

    /// Synchronizes terrain-derived cells after sculpting/import, preserving
    /// manually painted cells whose terrain classification did not change.
    /// The land grids follow the heights now, taking over from a land job
    /// still running (its result is dropped).
    pub(super) fn sync_land(&mut self) {
        if let Some((_, was)) = self.land_job.take() {
            self.island.doc.land_heights = was;
        }
        self.land_stale = false;
        let coasts = self.coast_paths();
        let environment = self.land_environment();
        if new::sync_land(&mut self.island.doc, &coasts, environment) {
            self.land_redrawn();
        }
    }

    /// The land grids following the heights, to run off the UI thread
    /// after a stroke: the dab already shows, and the grids feed overlays,
    /// saves and other tools. `land_synced` takes the result; anything
    /// that needs the grids current first calls `sync_land`.
    pub fn land_job(
        &mut self,
    ) -> Option<(
        u64,
        impl FnOnce() -> Option<new::LandChanges> + Send + 'static,
    )> {
        if !self.land_stale || self.land_job.is_some() {
            return None;
        }
        self.land_stale = false;
        // Paint strokes leave the heights shared with the last sync.
        if Arc::ptr_eq(&self.island.doc.land_heights, &self.island.doc.field) {
            return None;
        }
        let grids = self.island.doc.grids.clone()?;
        let was = std::mem::replace(
            &mut self.island.doc.land_heights,
            self.island.doc.field.clone(),
        );
        let now = self.island.doc.land_heights.clone();
        let (grid, coasts) = (self.island.doc.field.grid, self.coast_paths());
        let environment = self.land_environment();
        self.land_jobs += 1;
        self.land_job = Some((self.land_jobs, was.clone()));
        Some((self.land_jobs, move || {
            new::LandChanges::between(
                grid,
                &was.heights,
                &now.heights,
                &grids,
                &coasts,
                environment,
            )
        }))
    }

    /// Applies land job `job`'s changes, unless `sync_land` took over.
    pub fn land_synced(&mut self, job: u64, changes: Option<new::LandChanges>) {
        if self.land_job.as_ref().is_none_or(|(j, _)| *j != job) {
            return;
        }
        self.land_job = None;
        if let (Some(changes), Some(grids)) = (changes, &mut self.island.doc.grids) {
            changes.apply(Arc::make_mut(grids));
            self.land_redrawn();
        }
    }

    /// Ground colors show the land grids only under an overlay; a whole
    /// redraw otherwise added 50 ms to a stroke's release.
    fn land_redrawn(&mut self) {
        if self.overlay().is_some() {
            self.drawn_look = None;
        }
    }

    /// Paths of the coast objects, which harbour pads follow.
    fn coast_paths(&self) -> Vec<Vec<anno_core::island::objects::PathPoint>> {
        let guids = self.coast_guids();
        (self.island.doc.objects.iter())
            .filter(|o| guids.contains(&o.guid))
            .filter_map(|o| o.path.clone())
            .collect()
    }

    /// Rederives `AreaIDs` along the coast from land and harbour spots,
    /// keeping the interior as painted.
    pub fn refresh_areas(&mut self) {
        self.derive_areas(false);
    }

    /// Rederives `AreaIDs` everywhere: buildable land from the slope, plus
    /// harbour pads and the coastal rim. Replaces hand-painted areas.
    pub fn derive_areas_from_slope(&mut self) {
        self.derive_areas(true);
    }

    fn derive_areas(&mut self, from_slope: bool) {
        // Areas follow the water grid, which follows the heights.
        self.sync_land();
        let coasts = self.coast_guids();
        harbour::refresh_areas(&mut self.island.doc, &coasts, from_slope);
        if self.overlay() == Some(Kind::AreaIds) {
            // Ground colors carry the overlay; redraw them.
            self.drawn_look = None;
        }
    }

    /// Rederives a gameplay grid from the landscape, or all of them with
    /// `None`: tree cells (with their forest chunks) from the tree props,
    /// chunks alone from the tree cells, environment, herds. Replaces what
    /// was painted. Returns what changed for the status line.
    pub fn derive_grids(&mut self, kind: Option<Kind>) -> String {
        let rules = self.rules();
        let marsh = self
            .palette
            .as_ref()
            .is_some_and(|p| p.materials.values().any(|m| m.file.contains("marsh")));
        let doc = &mut self.island.doc;
        let Some(grids) = &mut doc.grids else {
            return "This island has no gameplay grids".into();
        };
        let field = &doc.field;
        let land = Landscape {
            heights: &field.heights,
            width: field.grid.width,
            depth: field.grid.depth,
            spacing: field.grid.spacing,
            trees: doc
                .props
                .iter()
                .filter(|p| {
                    doc.meshes
                        .get(p.source.file as usize)
                        .is_some_and(|m| landscape::is_harvestable_tree(rules.tree_exclusions, m))
                })
                .map(|p| {
                    let t = math::translation(&p.matrix);
                    [t[0], t[2]]
                })
                .collect(),
            marsh,
            environments: rules.environment_values(),
        };
        let grids = Arc::make_mut(grids);
        let text = match kind {
            Some(Kind::Trees) => {
                land.derive_trees(grids);
                "Tree cells follow the trees on buildable land; forest chunks renumbered"
            }
            Some(Kind::ForestChunks) => {
                landscape::derive_chunks(grids);
                "Forest chunks renumbered from the tree cells"
            }
            Some(Kind::Environment) => {
                land.derive_environment(grids);
                "Environment derived from coast, slope, height and trees"
            }
            Some(Kind::Herds) => {
                land.derive_herds(grids);
                "Herds roam buildable land outside trees and rivers"
            }
            _ => {
                land.derive_all(grids);
                "Tree cells, forest chunks, environment and herds derived"
            }
        };
        let trees = grids.layer(Kind::Trees).map_or(0, |l| {
            (0..l.width * l.height)
                .filter(|&k| l.get(k % l.width, k / l.width) != 0)
                .count()
        });
        if self.overlay().is_some() {
            self.drawn_look = None;
        }
        format!("{text} ({trees} tree cells)")
    }

    /// What a new island needs before it can be played: areas that follow
    /// the land and harbour spots, and one player-island marker by the
    /// first harbour. Returns a warning when it cannot be settled.
    fn prepare_new_island(&mut self) -> Option<String> {
        // A game without the shared coast pieces (2070's harbours are
        // tiles) has no harbour spot or player marker to check.
        if (self.harbours()).is_none_or(|h| h.required_pieces(&self.region()).is_empty()) {
            return None;
        }
        self.refresh_areas();
        let coasts = self.coast_guids();
        let first = self
            .island
            .doc
            .objects
            .iter()
            .find(|o| coasts.contains(&o.guid))
            .and_then(|o| o.path.clone());
        let Some(curve) = first else {
            return Some(
                "No harbour spot yet: players cannot found a settlement here. Add one with the Harbour tool (H)".into(),
            );
        };
        let lib = self.island.harbour.clone()?;
        let marker = lib.guid(Piece::Marker)?;
        if self.island.doc.objects.iter().any(|o| o.guid == marker) {
            return None;
        }
        // Shipped markers sit just off a harbour, at sea level.
        let mid = curve[curve.len() / 2];
        let d = mid.d.unwrap_or([0., -1.]);
        let at = [mid.p[0] + d[0] * 6., 0.3, mid.p[2] + d[1] * 6.];
        let store = self.island.object_store.as_mut()?;
        store.objects.clone_from(&self.island.doc.objects);
        let areas = self
            .island
            .doc
            .grids
            .as_ref()
            .and_then(|g| g.layer(Kind::AreaIds));
        let o = lib.place(store, Piece::Marker, areas, at)?;
        Arc::make_mut(&mut self.island.doc.objects).push(o);
        None
    }

    fn continue_brush(&mut self, drag: &mut Drag, at: Vec3, invert: bool) {
        let Drag::Brush {
            stroke,
            level,
            changed,
            before,
            hold,
        } = drag
        else {
            return;
        };
        // From the press on, the stroke's dabs leave the shadows as they are,
        // and the release draws them once over the whole stroke; a click
        // shades on the frame after its release. With wgpu's indirect-draw
        // checks on, a dab that redraws the shadows costs about 2 ms more.
        // Sculpt brushes hold as paint does; for live sculpt shadows, take
        // the hold only for `Tool::Paint`.
        if hold.is_none() {
            *hold = Some(self.renderer.hold_shadows());
        }
        // Quarter-radius spacing keeps strokes even without over-applying.
        // Absolute mode measures against the heights from stroke start.
        for dab in stroke.to([at[0], at[2]], self.brush.radius * 0.25) {
            let y = self.island.doc.field.height(dab);
            *changed |= self.dab(
                [dab[0], y, dab[1]],
                *level,
                invert,
                Some(&before.field.heights),
            );
        }
    }

    fn mouse_move(&mut self, view: usize, event: &MouseMoveEvent) {
        if event.pressed_button.is_none() && self.views[view].drag.is_some() {
            self.finish_drag(view);
        }
        self.cursor = self.ground(view, event.position);
        // The GPU reads back what is under the cursor a frame later; asking
        // on hover has the answer waiting for a click.
        if event.pressed_button.is_none() && self.tool == Tool::Select {
            self.views[view].viewport.pick(event.position);
        }
        let hover = (event.pressed_button.is_none()
            && (self.tool == Tool::Select || self.tool == Tool::River && self.river.is_none()))
        .then(|| self.water_at(view, event.position))
        .flatten();
        if hover != self.hover_water {
            self.hover_water = hover;
        }
        let snap = event.modifiers.control || event.modifiers.platform;
        let Some(mut drag) = self.views[view].drag.take() else {
            return;
        };
        match &mut drag {
            Drag::Orbit(last) => {
                self.views[view].camera.orbit_by(event.position - *last);
                *last = event.position;
            }
            Drag::Pan(last, grab) => {
                let extent = self.extent();
                let camera = &mut self.views[view].camera;
                match grab {
                    Some(grab) if camera.is_perspective() => {
                        camera.pan_at(event.position - *last, *grab, extent)
                    }
                    _ => camera.pan_by(event.position - *last),
                }
                *last = event.position;
            }
            Drag::Marquee(m) => m.cursor = event.position,
            Drag::Shore { from, to, preview } => {
                if let Some(at) = self.cursor {
                    *to = at;
                    *preview = harbour::trace_between(
                        &self.island.doc.field,
                        [from[0], from[2]],
                        [at[0], at[2]],
                    );
                }
            }
            Drag::Region { to, lasso, .. } => {
                if let Some(at) = self.cursor {
                    *to = at;
                    // A point a metre on keeps the outline smooth without
                    // one per motion event.
                    if let Some(points) = lasso
                        && points
                            .last()
                            .is_none_or(|l| (l[0] - at[0]).hypot(l[1] - at[2]) >= 1.)
                    {
                        points.push([at[0], at[2]]);
                    }
                }
            }
            Drag::RiverKnot {
                index,
                handle,
                start,
            } => {
                if let Some(at) = self.cursor {
                    self.river_drag(*index, *handle, start, at);
                }
            }
            Drag::RiverGizmo {
                gesture,
                index,
                start,
                surfaces,
            } => {
                if let Some(gizmo::Delta::Move(d)) =
                    gesture.delta(event.position, snap.then_some(self.snap_step))
                {
                    let at = start[*index];
                    self.river_move(*index, start, [at[0] + d[0], at[1] + d[2]]);
                    if d[1] != 0. {
                        self.river_raise(*index, start, surfaces, d[1]);
                    }
                }
            }
            Drag::ShapePoint {
                shape,
                point,
                changed,
                ..
            } => {
                if let Some(at) = self.cursor
                    && self.move_shape_point(*shape, *point, [at[0], at[2]])
                {
                    *changed = true;
                }
            }
            Drag::CoastPoint {
                id, index, changed, ..
            } => {
                if let Some(at) = self.cursor
                    && let Some(p) = harbour::snap_to_shore(&self.island.doc.field, [at[0], at[2]])
                {
                    let i = *index;
                    self.edit_coast(*id, |pts| pts[i] = p);
                    *changed = true;
                }
            }
            Drag::PathGizmo {
                gesture,
                start,
                changed,
                ..
            } => {
                if let Some(gizmo::Delta::Move(d)) =
                    gesture.delta(event.position, snap.then_some(self.snap_step))
                    && let Some(work) = self.road_editing().map(|e| e.work)
                {
                    self.set_road_points(work, |pts| {
                        for &(i, p) in start.iter() {
                            pts[i] = math::add(p, d);
                        }
                    });
                    *changed = true;
                }
            }
            Drag::Brush { .. } => {
                if let Some(at) = self.cursor {
                    self.continue_brush(&mut drag, at, event.modifiers.shift);
                }
            }
            Drag::Gizmo { pending, .. } => *pending = Some((event.position, snap)),
        }
        self.views[view].drag = Some(drag);
    }

    /// Moves a gizmo drag's targets to the newest cursor. Motion events
    /// arrive one by one, up to a thousand a second from a fast mouse, and
    /// moving 20k items takes about 5 ms: moving them for every event fell
    /// seconds behind. The frame (`refresh_scene`) and the release apply the
    /// newest one only.
    fn apply_gizmo(&mut self, view: usize) {
        // A turn still being read when the drag began: ask again, so the
        // drag snaps from the move it arrives on.
        if let Some(Drag::Gizmo { turn: None, .. }) = &self.views[view].drag {
            let now = self.selection_turn();
            if let Some(Drag::Gizmo { turn, .. }) = &mut self.views[view].drag {
                *turn = now;
            }
        }
        let Some(Drag::Gizmo {
            gesture,
            targets,
            changed,
            pending,
            gpu,
            turn,
            ..
        }) = &mut self.views[view].drag
        else {
            return;
        };
        let Some((position, snap)) = pending.take() else {
            return;
        };
        let Some(mut delta) = gesture.delta(position, snap.then_some(self.snap_step)) else {
            return;
        };
        delta = whole_turns(delta, turn.unwrap_or(0.));
        *changed = true;
        let pivot = gesture.frame.pivot();
        // The delta is affine: what it does to the identity it does to any
        // start, from the left.
        let on_gpu = targets.len() > GPU_DRAG;
        if on_gpu {
            let m = delta.apply(&math::IDENTITY, pivot);
            *gpu = Some(m);
            self.renderer.drag_selection(Some(m));
        }
        let targets = std::mem::take(targets);
        let renderer = self.renderer.clone();
        let mut moved = Vec::with_capacity(targets.len());
        // On the GPU only items without placement rows (rivers, models not
        // loaded) move here, so their lines and markers follow.
        let unplaced = |item: &Item| self.selection_unplaced.binary_search(item).is_ok();
        let cpu: Vec<(Item, Mat4)> = (targets.iter())
            .filter(|(item, _)| !on_gpu || unplaced(item))
            .copied()
            .collect();
        let links = self.slot_links(cpu.iter().map(|(item, _)| *item));
        for (item, start) in &cpu {
            let m = self.keep_above_ground(*item, start, delta.apply(start, pivot));
            // Props thin the vegetation around them, where they were and
            // where they land.
            if let Item::Prop(_) = item
                && let Some(was) = self.island.doc.matrix(*item)
            {
                for p in [was, m] {
                    let r = vegetation::prop_reach(math::length([p[0], p[1], p[2]]));
                    self.vegetation_changed([p[12] - r, p[14] - r, p[12] + r, p[14] + r]);
                }
            }
            self.island.doc.set_matrix(*item, m);
            // Objects store TRS: read back what was kept.
            if let Some(m) = self.island.doc.matrix(*item) {
                moved.push((*item, m));
            }
        }
        self.follow_slot_links(&links);
        if let Some(Drag::Gizmo { targets: t, .. }) = &mut self.views[view].drag {
            *t = targets;
        }
        if moved.is_empty() {
            return;
        }
        self.placements.move_items(&mut moved, &renderer);
        // Curves move with their object (`set_matrix`). `moved` is sorted
        // now.
        self.inland_stale |= (self.island.doc.objects.iter()).any(|o| {
            o.path.is_some()
                && moved
                    .binary_search_by_key(&Item::Object(o.id), |(i, _)| *i)
                    .is_ok()
        });
        self.moves += 1;
        self.markers_stale = true;
    }

    /// `to`, for a prop with AdaptTerrainHeight moved from `from`, lifted by
    /// the ground's rise between the two: the game keeps such props at
    /// their height above the ground. Other items move as dragged.
    fn keep_above_ground(&self, item: Item, from: &Mat4, mut to: Mat4) -> Mat4 {
        let Item::Prop(id) = item else { return to };
        if self
            .island
            .doc
            .prop(id)
            .is_some_and(|p| p.source.adapt_terrain_height.is_some())
        {
            let field = &self.island.doc.field;
            to[13] += field.height([to[12], to[14]]) - field.height([from[12], from[14]]);
        }
        to
    }

    fn finish_drag(&mut self, view: usize) {
        self.apply_gizmo(view);
        // A GPU drag lands in the document now, once, and the rows take
        // where it landed before the GPU lets them go.
        if let Some(Drag::Gizmo {
            targets,
            gpu: Some(m),
            ..
        }) = &self.views[view].drag
        {
            let (targets, m) = (targets.clone(), *m);
            let mut moved = Vec::with_capacity(targets.len());
            let placed = |item: &Item| self.selection_unplaced.binary_search(item).is_err();
            let links = self.slot_links(targets.iter().map(|(i, _)| *i).filter(placed));
            for (item, start) in &targets {
                if self.selection_unplaced.binary_search(item).is_ok() {
                    continue;
                }
                let to = self.keep_above_ground(*item, start, math::multiply(&m, start));
                self.island.doc.set_matrix(*item, to);
                if let Some(m) = self.island.doc.matrix(*item) {
                    moved.push((*item, m));
                }
            }
            self.follow_slot_links(&links);
            let renderer = self.renderer.clone();
            self.placements.move_items(&mut moved, &renderer);
            self.renderer.drag_selection(None);
            self.moves += 1;
            self.markers_stale = true;
        }
        match self.views[view].drag.take() {
            Some(Drag::Brush {
                before,
                changed: true,
                ..
            }) => {
                self.land_stale = true;
                // Painted areas keep harbour pads and the coastal rim valid.
                if self.tool == Tool::Grid && self.grid == Kind::AreaIds {
                    self.refresh_areas();
                }
                self.ground_stale |= self.tool == Tool::Paint
                    || (self.tool == Tool::Grid && self.grid == Kind::Marsh && self.marsh_shape);
                let skipped = (self.island.doc.materials.as_mut()).map_or(0, |m| m.take_skipped());
                if skipped > 0 {
                    self.say(
                        format!(
                            "Left {} as they were: a chunk there already holds 24 layers",
                            crate::project::plural(skipped, "vertex", "vertices")
                        ),
                        false,
                    );
                }
                let stale = std::mem::take(&mut self.stale_colors);
                self.dirty = self.dirty.union(stale);
                // Dabs regrew the vegetation under the brush; a full scatter
                // (50-60 ms on a 512 m island) thins it to the bound again.
                let feeds_vegetation = !matches!(self.tool, Tool::Grid | Tool::Tint);
                self.scene_stale |= self.layers.vegetation && feeds_vegetation;
                self.checkpoint_owned(*before);
            }
            Some(Drag::Gizmo {
                before,
                changed: true,
                targets,
                ..
            }) => {
                // As after a stroke: the drag regrew the vegetation around
                // the props it moved.
                self.scene_stale |= self.layers.vegetation
                    && targets.iter().any(|(i, _)| matches!(i, Item::Prop(_)));
                self.checkpoint_owned(*before);
                self.refresh_areas();
            }
            Some(Drag::Shore { from, to, preview }) => {
                if (from[0] - to[0]).hypot(from[2] - to[2]) < 6. {
                    self.harbour_at(from, false);
                } else if let Some(curve) = preview {
                    let before = self.island.doc.clone();
                    if self.push_harbour(curve) {
                        self.checkpoint_owned(before);
                    }
                } else {
                    self.say(
                        "Could not follow the shore between those points; drag along one shore",
                        true,
                    );
                }
            }
            Some(Drag::Region { from, to, lasso }) => match lasso {
                Some(points) => match harbour::Region::lasso(&points) {
                    Some(region) => self.auto_place(region),
                    None => self.say("Trace a loop around the ground to fill", true),
                },
                None => self.auto_place(harbour::Region::spanning(
                    [from[0], from[2]],
                    [to[0], to[2]],
                )),
            },
            Some(Drag::CoastPoint {
                before,
                changed: true,
                ..
            }) => {
                self.checkpoint_owned(*before);
                self.refresh_areas();
            }
            Some(Drag::PathGizmo {
                before,
                changed: true,
                ..
            })
            | Some(Drag::ShapePoint {
                before,
                changed: true,
                ..
            }) => self.checkpoint_owned(*before),
            Some(Drag::RiverKnot { .. } | Drag::RiverGizmo { .. }) => self.river_moved(),
            Some(Drag::Marquee(m)) => {
                let v = &self.views[view];
                let extent = self.extent();
                let layers = self.layers;
                // Hidden layers are not selectable.
                let props = self.island.doc.items().filter(|(item, _)| {
                    let shown = match item {
                        Item::Prop(_) => layers.props,
                        Item::Object(_) => layers.objects,
                    };
                    // Lookups only when something is hidden.
                    shown
                        && (self.hidden_groups.is_empty()
                            || !self
                                .item_group(*item)
                                .is_some_and(|g| self.hidden_groups.contains(&g)))
                });
                // Models pick on the GPU; unloaded items fall back to markers.
                let picked = m
                    .is_click()
                    .then(|| v.viewport.pick(m.start))
                    .flatten()
                    .and_then(|h| h.placement)
                    .and_then(|row| self.placements.item_at(row));
                let v = &self.views[view];
                let ids: Vec<Item> = if let Some(item) = picked {
                    vec![item]
                } else if m.is_click() {
                    // A model or marker within 8 px of the click comes before
                    // the water around it: a click beside a thin bush, inside
                    // a lake's outline, picked the lake 60 m away. Markers
                    // are not meshes, so the nearest one on screen.
                    let near = props
                        .map(|(id, p)| {
                            let s = v.camera.project(p, v.bounds, extent);
                            let d = f32::from(s.x - m.start.x).hypot(f32::from(s.y - m.start.y));
                            (id, d)
                        })
                        .filter(|&(_, d)| d < 8.)
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(id, _)| id);
                    // Rivers and lakes have no model: their water is the
                    // target when nothing is that close.
                    near.or_else(|| self.water_at(view, m.start).map(Item::Object))
                        .into_iter()
                        .collect()
                } else {
                    m.select(props, v.camera, v.bounds, extent)
                };
                if !m.additive {
                    self.selection.clear();
                }
                // One model picked in the viewport: the Hierarchy opens its
                // group and scrolls to it, as an outliner follows a pick.
                if let [item] = ids.as_slice() {
                    if let Some(group) = self.item_group(*item) {
                        self.open_groups.expand(group);
                    }
                    self.reveal.set(Some(*item));
                }
                self.selection.extend(ids);
                self.scene_stale = true;
            }
            _ => {}
        }
    }

    pub fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut App) -> bool {
        if let Some(edit) = &mut self.library_edit {
            match edit.key_down_in(event, cx) {
                EditEvent::Commit | EditEvent::Cancel => self.library_edit = None,
                EditEvent::Changed => self.library_filter = edit.text.clone(),
                // Keys the field does not use (Ctrl 1, Ctrl S) are the
                // editor's shortcuts, not swallowed by an open field.
                EditEvent::Ignored => return false,
            }
            return true;
        }
        if let Some(edit) = &mut self.filter_edit {
            match edit.key_down_in(event, cx) {
                EditEvent::Commit | EditEvent::Cancel => self.filter_edit = None,
                EditEvent::Changed => {
                    let text = edit.text.clone();
                    self.set_filter(text);
                }
                EditEvent::Ignored => return false,
            }
            return true;
        }
        if let Some(edit) = &mut self.editing {
            match edit.key_down_in(event, cx) {
                EditEvent::Commit => {
                    let edit = self.editing.take().expect("active edit");
                    match edit.number() {
                        Some(v) => self.set_field(edit.field, v),
                        None => self.say("Enter a finite number", true),
                    }
                }
                EditEvent::Cancel => self.editing = None,
                EditEvent::Changed | EditEvent::Ignored => return false,
            }
            return true;
        }
        let mods = event.keystroke.modifiers;
        if mods.control || mods.platform {
            if event.keystroke.key == "d" && !self.selection.is_empty() {
                self.duplicate_selected();
                return true;
            }
            return false;
        }
        match event.keystroke.key.as_str() {
            "q" => self.tool = Tool::Select,
            "w" => (self.tool, self.gizmo) = (Tool::Select, gizmo::Mode::Move),
            "e" => (self.tool, self.gizmo) = (Tool::Select, gizmo::Mode::Rotate),
            "r" => (self.tool, self.gizmo) = (Tool::Select, gizmo::Mode::Scale),
            "g" => self.tool = Tool::Grid,
            "s" => self.tool = Tool::Scatter,
            "t" => self.tool = Tool::Tint,
            "h" => self.tool = Tool::Harbour,
            "m" => self.tool = Tool::Slot,
            "v" => self.tool = Tool::River,
            "enter" if self.tool == Tool::River && self.river.is_some() => self.finish_river(),
            "enter" if self.tool == Tool::Grid && !self.outline.is_empty() => self.close_outline(),
            "i" => {
                if let Some(at) = self.cursor {
                    self.sample(at);
                }
            }
            key @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                // Digits follow the toolbar order after Select.
                let i = key.parse::<usize>().unwrap_or(1);
                self.tool = Tool::ALL[i.min(Tool::ALL.len() - 1)].0;
            }
            "[" => self.brush.radius = (self.brush.radius / 1.25).max(1.),
            "]" => self.brush.radius = (self.brush.radius * 1.25).min(256.),
            "f" => self.frame(),
            "delete" | "backspace" if self.tool == Tool::Harbour && self.curve_point.is_some() => {
                self.remove_coast_point()
            }
            "delete" | "backspace" if self.road_editing().is_some() => self.remove_road_points(),
            "enter" if self.road_editing().is_some() => self.road_edit = None,
            "delete" | "backspace" if self.tool == Tool::River && self.river.is_some() => {
                self.remove_river_knot()
            }
            "delete" | "backspace" => self.delete_selected(),
            "escape" => {
                // Cancel restores the document from before the drag.
                let befores: Vec<Doc> = self
                    .views
                    .iter_mut()
                    .filter_map(|v| match v.drag.take() {
                        Some(
                            Drag::Brush { before, .. }
                            | Drag::Gizmo { before, .. }
                            | Drag::PathGizmo { before, .. }
                            | Drag::ShapePoint { before, .. },
                        ) => Some(*before),
                        _ => None,
                    })
                    .collect();
                // A GPU drag's rows go back to the table, which still holds
                // the starts.
                self.renderer.drag_selection(None);
                if let Some(before) = befores.into_iter().next() {
                    self.island.doc = before;
                    self.dirty = Rect::default();
                    self.rebuild_tiles(None);
                }
                // Drops a river draft and a blocker outline; nothing of
                // either is in the document yet.
                self.river = None;
                self.outline.clear();
                // Leaves the path edit mode; its edits stay.
                self.road_edit = None;
                self.river_history.close();
                self.selection.clear();
                self.scene_stale = true;
            }
            _ => return false,
        }
        true
    }

    fn set_field(&mut self, field: Field, value: f32) {
        match field {
            Field::Radius => self.brush.radius = value.clamp(1., 256.),
            Field::Strength => self.brush.strength = value.clamp(0.01, 20.),
            Field::Inner => self.brush.inner = (value / 100.).clamp(0., 0.95),
            Field::Level => self.level = Some(value),
            Field::Density => self.scatter.spread.density = value.clamp(0.05, 400.),
            Field::Jitter => self.scatter.spread.scale_jitter = (value / 100.).clamp(0., 0.9),
            Field::GridValue => {
                self.grid_value =
                    (value.max(0.) as u32).min(grids::max_value(self.rules(), self.grid))
            }
            Field::TimeOfDay => {
                // Wraps, so scrolling past midnight carries on into the day.
                // Only the light changes: the scene keeps its placements.
                self.time_of_day = value.rem_euclid(24.);
                self.markers_stale = true;
            }
            Field::Flow(bank) => {
                let Some(i) = self.river.as_ref().and_then(|d| d.picked) else {
                    return;
                };
                if self.flow_step != Some(i) {
                    self.river_step();
                    self.flow_step = Some(i);
                }
                if let Some(knot) = self.river.as_mut().and_then(|d| d.knots.get_mut(i)) {
                    let mut flow = knot.flow.unwrap_or(river::U0);
                    flow[bank] = value.clamp(0., anno_core::water::MAX_FLOW_SPEED);
                    knot.flow = Some(flow);
                }
            }
            Field::Falloff => self.river_falloff = value.clamp(0., 200.),
            Field::Bed(f) => {
                let b = &mut self.river_bed;
                match f {
                    BedField::Depth => b.depth = value.clamp(0.05, 3.),
                    BedField::Bank => b.bank = value.clamp(0.1, 5.),
                    BedField::Reach => b.reach = value.clamp(0.5, 20.),
                    BedField::Bends => b.bends = (value / 100.).clamp(0., 1.),
                    BedField::Pools => b.pools = (value / 100.).clamp(0., 1.),
                }
            }
            Field::Position(_) | Field::Yaw | Field::Scale => {
                if self.selection.is_empty() {
                    return;
                }
                self.checkpoint();
                let links = self.slot_links(self.selection.iter().copied());
                for &item in &self.selection {
                    let Some(mut m) = self.island.doc.matrix(item) else {
                        continue;
                    };
                    match field {
                        Field::Position(axis) => m[12 + axis] = value,
                        Field::Yaw | Field::Scale => {
                            let (t, mut q, mut s) = doc::decompose(&m);
                            if field == Field::Yaw {
                                // Replaces any tilt with a pure yaw.
                                let half = value.to_radians() / 2.;
                                q = [0., half.sin(), 0., half.cos()];
                            } else {
                                s = [value.max(0.01); 3];
                            }
                            m = doc::trs(t, q, s);
                        }
                        _ => {}
                    }
                    self.island.doc.set_matrix(item, m);
                }
                self.follow_slot_links(&links);
                self.moves += 1;
                self.scene_stale = true;
            }
        }
    }

    /// The span a tool-bar slider covers, and whether it is logarithmic
    /// (sizes and rates, where small values need the precision). `None`
    /// for fields typed only. Typing can still go past the span, up to
    /// `set_field`'s own limits.
    fn field_range(&self, field: Field) -> Option<(f32, f32, bool)> {
        Some(match field {
            Field::Radius => (1., 128., true),
            Field::Inner => (0., 95., false),
            Field::Strength => (0.01, 4., true),
            Field::Level => (-20., 80., false),
            Field::Density => (0.1, 50., true),
            Field::Jitter => (0., 90., false),
            Field::GridValue => (0., grids::max_value(self.rules(), self.grid) as f32, false),
            Field::TimeOfDay => (0., 23.99, false),
            Field::Flow(_) => (0., anno_core::water::MAX_FLOW_SPEED, false),
            Field::Falloff => (0., 60., false),
            Field::Bed(BedField::Depth) => (0.05, 1.5, false),
            Field::Bed(BedField::Bank) => (0.1, 3., false),
            Field::Bed(BedField::Reach) => (0.5, 12., false),
            Field::Bed(BedField::Bends | BedField::Pools) => (0., 100., false),
            Field::Position(_) | Field::Yaw | Field::Scale => return None,
        })
    }

    /// Where `field`'s value sits in its slider, 0..1.
    fn field_fraction(&self, field: Field) -> Option<f32> {
        let (lo, hi, log) = self.field_range(field)?;
        let v = self.field_value(field).clamp(lo, hi);
        Some(if log {
            (v / lo).ln() / (hi / lo).ln()
        } else {
            (v - lo) / (hi - lo).max(1e-6)
        })
    }

    /// Sets `field` from a slider position, 0..1.
    fn set_field_fraction(&mut self, field: Field, f: f32) {
        let Some((lo, hi, log)) = self.field_range(field) else {
            return;
        };
        let f = f.clamp(0., 1.);
        let v = if log {
            lo * (hi / lo).powf(f)
        } else {
            lo + (hi - lo) * f
        };
        // Whole numbers where the field is a count or a class.
        let v = if matches!(
            field,
            Field::GridValue
                | Field::Jitter
                | Field::Inner
                | Field::Bed(BedField::Bends | BedField::Pools)
        ) {
            v.round()
        } else {
            v
        };
        self.set_field(field, v);
    }

    fn field_value(&self, field: Field) -> f32 {
        match field {
            Field::Radius => self.brush.radius,
            Field::Strength => self.brush.strength,
            Field::Inner => self.brush.inner * 100.,
            Field::Level => self.level.unwrap_or(0.),
            Field::Density => self.scatter.spread.density,
            Field::Jitter => self.scatter.spread.scale_jitter * 100.,
            Field::GridValue => self.grid_value as f32,
            Field::TimeOfDay => self.time_of_day,
            Field::Flow(bank) => self
                .river
                .as_ref()
                .and_then(|d| d.knots.get(d.picked?))
                .map_or(river::U0[bank], |k| k.flow.unwrap_or(river::U0)[bank]),
            Field::Falloff => self.river_falloff,
            Field::Bed(f) => {
                let b = &self.river_bed;
                match f {
                    BedField::Depth => b.depth,
                    BedField::Bank => b.bank,
                    BedField::Reach => b.reach,
                    BedField::Bends => b.bends * 100.,
                    BedField::Pools => b.pools * 100.,
                }
            }
            Field::Position(_) | Field::Yaw | Field::Scale => {
                let Some(m) = self
                    .selection
                    .iter()
                    .find_map(|&i| self.island.doc.matrix(i))
                else {
                    return 0.;
                };
                match field {
                    Field::Position(axis) => m[12 + axis],
                    // Yaw of the local X axis about world Y.
                    Field::Yaw => (-m[2]).atan2(m[0]).to_degrees(),
                    _ => math::length([m[0], m[1], m[2]]),
                }
            }
        }
    }

    /// Frames the selection, or the island when nothing is selected.
    pub(crate) fn frame(&mut self) {
        match self.pivot() {
            Some(p) => {
                // As big as what is selected: a tree fills the view, a group
                // of houses fits in it. Items without a box count as 10 m.
                let boxes = self.selection_boxes();
                let size = if boxes.is_empty() {
                    10.
                } else {
                    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                    for corner in boxes.iter().flatten() {
                        for a in 0..3 {
                            lo[a] = lo[a].min(corner[a]);
                            hi[a] = hi[a].max(corner[a]);
                        }
                    }
                    (0..3).map(|a| hi[a] - lo[a]).fold(0., f32::max)
                };
                let extent = self.extent();
                for v in &mut self.views {
                    v.camera.frame(p, (size * 1.5).max(6.), extent);
                    // Centred in what the floating tool panel leaves in
                    // view, as Fit is: on the whole view a pick sat high,
                    // its gizmo reaching the panel's edge.
                    v.camera.pan[1] = 56.;
                }
            }
            None => self.fit_island(),
        }
    }

    /// Frames the land, not the whole grid: the grid's margin is sea, and
    /// fitting it leaves the island small in the middle of the view.
    pub(crate) fn fit_island(&mut self) {
        let extent = self.extent();
        let (center, size) = match self.island.doc.field.bounds_above(0.5) {
            // About half the land's size: Camera::frame leaves a wide margin
            // and the tilted view is shorter than it is wide, so this is
            // what fills roughly three quarters of the height.
            Some([x0, z0, x1, z1]) => (
                [(x0 + x1) / 2., 0., (z0 + z1) / 2.],
                ((x1 - x0).max(z1 - z0)).max(30.),
            ),
            None => (extent.center, extent.size),
        };
        for v in &mut self.views {
            // The top-down view sees the whole square; the tilted one
            // foreshortens it and can come closer.
            let fill = if v.camera.tilt > 0. { 0.72 } else { 0.92 };
            v.camera.frame(center, size * fill, extent);
            // Down past the floating tool panel; the tilted view's near
            // coast needs the room at the bottom more.
            // Less in the tilted view: at 40 the island sat low with a band
            // of sky-blue sea above it.
            v.camera.pan[1] = if v.camera.tilt > 0. { 8. } else { 80. };
        }
    }

    fn delete_selected(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.checkpoint();
        let served = self.served_work_areas(&self.selection);
        self.selection.extend(served);
        let selection = &self.selection;
        self.island
            .doc
            .props
            .retain(|p| !selection.contains(&Item::Prop(p.id)));
        Arc::make_mut(&mut self.island.doc.objects)
            .retain(|o| !selection.contains(&Item::Object(o.id)));
        self.selection.clear();
        self.scene_stale = true;
        self.refresh_areas();
    }

    fn duplicate_selected(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.checkpoint();
        let selection = std::mem::take(&mut self.selection);
        let copies: Vec<Prop> = self
            .island
            .doc
            .props
            .iter()
            .filter(|p| selection.contains(&Item::Prop(p.id)))
            .cloned()
            .collect();
        for mut p in copies {
            p.id = self.island.next_id;
            self.island.next_id += 1;
            // A copy is the user's own; a rerun of Auto cliffs keeps it.
            p.auto_cliff = false;
            // Offset copies so they are visible and grabbable.
            p.matrix[12] += 4.;
            self.selection.insert(Item::Prop(p.id));
            self.island.doc.props.push(p);
        }
        // 2070's objects copy their records; its codec numbers the copies
        // when it saves them, so these ids only tell them apart until then.
        let records: Vec<_> = (self.island.doc.objects.iter())
            .filter(|o| o.record().is_some() && selection.contains(&Item::Object(o.id)))
            .cloned()
            .collect();
        let mut next = (self.island.doc.objects.iter())
            .map(|o| o.id)
            .max()
            .unwrap_or(0)
            + 1;
        for o in records {
            let mut copy = o.copied(next);
            next += 1;
            copy.position[0] += 4.;
            self.selection.insert(Item::Object(copy.id));
            Arc::make_mut(&mut self.island.doc.objects).push(copy);
        }
        // Objects get fresh ids from the store so they stay unique on save.
        if let Some(store) = &mut self.island.object_store {
            store.objects.clone_from(&self.island.doc.objects);
            let picked: Vec<usize> = (0..store.objects.len())
                .filter(|&i| selection.contains(&Item::Object(store.objects[i].id)))
                .collect();
            for i in picked {
                let mut copy = store.duplicate(i);
                copy.position[0] += 4.;
                self.selection.insert(Item::Object(copy.id));
                store.objects.push(copy.clone());
                Arc::make_mut(&mut self.island.doc.objects).push(copy);
            }
        }
        self.scene_stale = true;
    }
}

/// The island's top-down color render, `_gamedata/<name>/mapimage_0.dds`,
/// covering the height grid exactly. Its rows run north to south, opposite to
/// the grid, so it is flipped here; alpha is forced opaque because the lit
/// shader cuts out below 0.5. Missing images just disable the mode.
/// Swaps a heightfield's rows end for end. Images put north in row 0, but
/// Anno's +z is north, so grid row 0 is the island's south edge: a field
/// read from an image row by row is flipped once to sit as the game shows
/// it, and once more before it is written out as one.
pub(super) fn flip_north(field: &mut gpui_editor_kit::heightfield::Heightfield) {
    let w = field.grid.width;
    let rows: Vec<f32> = field.heights.chunks(w).rev().flatten().copied().collect();
    field.heights = rows;
}

fn load_map_image(install: &Install, path: &str) -> Option<Arc<Texture>> {
    let bytes = install
        .read(&format!("{}/mapimage_0.dds", island_data_dir(path).ok()?))
        .ok()?;
    let dds = Dds::parse(&bytes).ok()?;
    let image = dds.decode(&bytes, 0).ok()?;
    let row = image.width as usize * 4;
    let mut rgba: Vec<u8> = image.rgba.chunks(row).rev().flatten().copied().collect();
    for texel in rgba.as_chunks_mut::<4>().0 {
        if texel[3] < 128 {
            // Empty sea: neutral so the vertex water tint shows through.
            *texel = [150, 160, 170, 255];
        }
        texel[3] = 255;
    }
    Texture::from_rgba(image.width, image.height, rgba)
        .ok()
        .map(Arc::new)
}

fn height_color(h: f32) -> u32 {
    // Beach, grass, rock, peak; Anno 117 islands top out around 20-30 m.
    const STOPS: [(f32, u32); 5] = [
        (0., 0xc9b98a),
        (1., 0x8fae62),
        (6., 0x5f8a45),
        (14., 0x8a8273),
        (26., 0xd8d6d0),
    ];
    if h <= STOPS[0].0 {
        return STOPS[0].1;
    }
    for pair in STOPS.windows(2) {
        let ((h0, c0), (h1, c1)) = (pair[0], pair[1]);
        if h <= h1 {
            return mix(c0, c1, (h - h0) / (h1 - h0));
        }
    }
    STOPS[STOPS.len() - 1].1
}

/// A stable, distinct color per material id.
fn material_color(id: u8) -> u32 {
    let hue = (f32::from(id) * 0.618_034).fract();
    let [r, g, b] = [0., 1. / 3., 2. / 3.].map(|o| {
        let v = ((hue + o) * std::f32::consts::TAU).cos() * 0.5 + 0.5;
        (60. + v * 160.) as u32
    });
    r << 16 | g << 8 | b
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0., 1.);
    [16, 8, 0]
        .map(|s| {
            let (x, y) = (((a >> s) & 255) as f32, ((b >> s) & 255) as f32);
            (x + (y - x) * t).round() as u32
        })
        .into_iter()
        .fold(0, |c, v| c << 8 | v)
}

/// See `IslandEditor::tile_inputs`.
struct TileInputs<'a> {
    field: &'a Heightfield,
    overlay: Option<&'a anno_formats::a7m::grids::Layer>,
    /// 2070's path blockers, shown over the ground in place of a grid.
    paths: Option<(
        &'a anno_core::island::Blockers,
        anno_core::island::PathLayer,
    )>,
    /// The working tint, when shown.
    tint: Option<&'a anno_formats::a7m::tint::TintMap>,
    materials: Option<&'a Materials>,
    palette: Option<&'a crate::terrain::Palette>,
    shading: Shading,
    rules: &'static anno_core::game::IslandRules,
    gain: f32,
    texture: Option<Arc<Texture>>,
    heights: Option<Arc<gpu::HeightMap>>,
}

impl TileInputs<'_> {
    /// RGBA texels of `rect`'s samples, rows across every core: a whole
    /// 1537² island is 2.4M colors.
    fn colors(&self, rect: Rect) -> Vec<u8> {
        let g = &self.field.grid;
        let width = rect.x1 - rect.x0;
        let mut out = vec![255u8; width * (rect.z1 - rect.z0) * 4];
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let band = (rect.z1 - rect.z0).div_ceil(threads).max(1) * width * 4;
        std::thread::scope(|scope| {
            for (n, rows) in out.chunks_mut(band.max(1)).enumerate() {
                scope.spawn(move || {
                    let z0 = rect.z0 + n * band / (width * 4).max(1);
                    for (i, texel) in rows.chunks_mut(4).enumerate() {
                        let (x, z) = (rect.x0 + i % width, z0 + i / width);
                        let p = [
                            g.origin[0] + x as f32 * g.spacing,
                            self.field.at(x, z),
                            g.origin[1] + z as f32 * g.spacing,
                        ];
                        let (c, cover) = self.ground_color(p, self.overlay, self.gain, true);
                        // Alpha 255 is no cover, so a map written before
                        // overlays existed still multiplies everywhere.
                        texel.copy_from_slice(&[
                            (c >> 16) as u8,
                            (c >> 8) as u8,
                            c as u8,
                            255 - (cover * 255.).round() as u8,
                        ]);
                    }
                });
            }
        });
        out
    }

    fn tile_mesh(&self, tx: usize, tz: usize) -> Arc<Mesh> {
        let field = self.field;
        // 1025-sample islands stay under ~1M vertices at full detail.
        let tile = field.tile(tx, tz, 1);
        let (overlay, gain) = (self.overlay, self.gain);
        let colors = tile
            .positions
            .iter()
            .map(|&p| self.terrain_color(p, overlay, gain))
            .collect();
        let texture = self.texture.clone();
        // Displaced tiles take height and normals from the GPU map.
        let normals = if self.heights.is_some() {
            Vec::new()
        } else {
            tile.samples.iter().map(|&i| self.normal_at(i)).collect()
        };
        Arc::new(
            Mesh::new(MeshData {
                positions: tile.positions,
                uvs: if texture.is_some() {
                    tile.uvs
                } else {
                    Vec::new()
                },
                colors,
                color_gain: gain,
                normals,
                heights: self.heights.clone(),
                indices: tile.indices,
                texture: texture.map(Into::into),
                // CPU tiles draw no splat (retexture_tiles).
                material: gpu::Material::Lit,
                ..Default::default()
            })
            .expect("heightfield tiles are valid meshes"),
        )
    }

    /// Smooth normal at a height sample from central differences.
    fn normal_at(&self, sample: usize) -> Vec3 {
        let g = &self.field.grid;
        let (x, z) = (sample % g.width, sample / g.width);
        let h = |x: usize, z: usize| self.field.at(x.min(g.width - 1), z.min(g.depth - 1));
        let dx = (h(x + 1, z) - h(x.saturating_sub(1), z)) / (2. * g.spacing);
        let dz = (h(x, z + 1) - h(x, z.saturating_sub(1))) / (2. * g.spacing);
        math::normalize([-dx, 1., -dz]).unwrap_or([0., 1., 0.])
    }

    fn terrain_color(
        &self,
        p: Vec3,
        overlay: Option<&anno_formats::a7m::grids::Layer>,
        gain: f32,
    ) -> u32 {
        self.ground_color(p, overlay, gain, false).0
    }

    /// The vertex color and, with `cover`, how far an overlay color covers
    /// the ground texture (the GPU color map's alpha). Without it the
    /// overlay is blended into the color the texture multiplies, as the
    /// CPU-meshed fallback draws it.
    fn ground_color(
        &self,
        p: Vec3,
        overlay: Option<&anno_formats::a7m::grids::Layer>,
        gain: f32,
        cover: bool,
    ) -> (u32, f32) {
        let rgb = |c: u32| [(c >> 16) & 255, (c >> 8) & 255, c & 255].map(|v| v as f32 / 255.);
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| -> [f32; 3] {
            std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
        };
        let h = p[1];
        let mut c = match self.shading {
            Shading::Ground => match self.tint {
                Some(map) => tint::sample(map, p[0], p[2]),
                None => [1.; 3],
            },
            Shading::Map => [1.; 3],
            Shading::Height => rgb(height_color(h)),
            Shading::Materials => match self.materials {
                Some(m) => rgb(material_color(doc::shown(m, [p[0], p[2]], self.palette))),
                None => [0.5; 3],
            },
        };
        if h < 0. {
            // Water tint deepens with depth so coastlines read from above.
            c = lerp(
                c,
                rgb(crate::colors::WATER_TINT),
                (-h / 8.).clamp(0.25, 0.8),
            );
        }
        let paths = (self.paths).map(|(b, layer)| {
            grids::path_at(b, layer, p[0], p[2]).then(|| grids::path_overlay(layer))
        });
        let shown = match overlay {
            _ if paths.is_some() => paths.flatten(),
            Some(layer) if layer.kind == Kind::AreaIds => buildable_overlay(layer, p),
            Some(layer) => {
                grids::overlay(self.rules, layer.kind, grids::value_at(layer, p[0], p[2]))
            }
            None => None,
        };
        let mut covered = 0.;
        if let Some((color, alpha)) = shown {
            let textured = matches!(self.shading, Shading::Ground | Shading::Map);
            if cover && textured {
                // The shader paints the color over the texture by `alpha`
                // and multiplies the rest, which keeps the ground's detail.
                c = color;
                covered = alpha;
            } else if overlay.is_some_and(|l| l.kind == Kind::AreaIds) {
                c = lerp(c, color, alpha);
            } else {
                // Textured ground multiplies the color; lift it so it reads.
                let lift = if textured { 1.6 } else { 1. };
                c = lerp(c, color.map(|v| v * lift), alpha);
            }
        }
        let [r, g, b] = c.map(|v| ((v / gain).clamp(0., 1.) * 255.).round() as u32);
        (r << 16 | g << 8 | b, covered)
    }
}

/// Replaces each mesh `f` returns new data for, spread over the cores:
/// copying a large island's terrain geometry is ~10 ms on one.
fn retexture(meshes: &mut [Arc<Mesh>], f: impl Fn(&MeshData) -> Option<MeshData> + Sync) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = meshes.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for part in meshes.chunks_mut(chunk) {
            let f = &f;
            scope.spawn(move || {
                for mesh in part {
                    if let Some(data) = f(mesh.data()) {
                        *mesh = Arc::new(Mesh::new(data).expect("retextured meshes stay valid"));
                    }
                }
            });
        }
    });
}

/// Where the grit map differs between two steps' grids: `None` when it
/// does not, `Some(None)` for all of it (no grit before, or another size),
/// else island metres x0, z0, x1, z1 over the changed cells.
fn changed_grit(a: Option<&Grids>, b: Option<&Grids>) -> Option<Option<[f32; 4]>> {
    let b = b?.layer(Kind::Grit)?;
    let a = a.and_then(|a| a.layer(Kind::Grit));
    let Some(a) = a.filter(|a| a.width == b.width && a.height == b.height) else {
        return Some(None);
    };
    if Arc::ptr_eq(&a.values, &b.values) {
        return None;
    }
    let mut rect: Option<Rect> = None;
    for z in 0..b.height {
        let differs = |x: &usize| a.get(*x, z) != b.get(*x, z);
        let Some(x0) = (0..b.width).find(differs) else {
            continue;
        };
        let x1 = (0..b.width).rfind(differs).unwrap() + 1;
        let row = Rect {
            x0,
            z0: z,
            x1,
            z1: z + 1,
        };
        rect = Some(rect.map_or(row, |r| r.union(row)));
    }
    let r = rect?;
    let cell = b.cell_metres;
    Some(Some([
        r.x0 as f32 * cell,
        r.z0 as f32 * cell,
        r.x1 as f32 * cell,
        r.z1 as f32 * cell,
    ]))
}

/// `delta` with a turn about the vertical rounded to whole steps of `turn`
/// (2070's buildings stand on quarter turns); 0 leaves it free.
fn whole_turns(delta: gizmo::Delta, turn: f32) -> gizmo::Delta {
    match delta {
        gizmo::Delta::Rotate(axis, angle) if turn > 0. && axis[1].abs() > 0.99 => {
            gizmo::Delta::Rotate(axis, (angle / turn).round() * turn)
        }
        other => other,
    }
}

/// Keeps the shipped record `o` carries (2070's `object_for` for a tree,
/// rock or grass) on `island` for its save to copy, once.
fn keep_foreign(island: &mut anno_core::island::Island, o: &anno_core::island::objects::Object) {
    if let Some(anno_core::island::objects::Record::Foreign(bytes)) = o.record()
        && !island.foreign.iter().any(|b| **b == *bytes)
    {
        island.foreign.push(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        MODERATE_M_01, PIRATE, SMALL, assets, create, harbour_library, install, install_1800,
        layer, new_island, objects, open, reopen, saved, staged,
    };
    use super::{
        Grids, IslandEditor, Item, Kind, Project, Start, Tool, grids, height_color,
        island_data_dir, material_color, mix, tint,
    };
    use anno_core::harbour::Piece;
    use anno_formats::{a7m::island, vfs::Install};
    use gpui_editor_kit::heightfield::{Brush, Falloff};
    use std::sync::Arc;

    // Needs Anno 2070: n_m02 opens with its objects placed as their records
    // store them, each drawn with the model its GUID names in the asset
    // database: its trees and rocks as props, its handles and decoration as
    // objects. The ones drawn are exactly those whose model reads with a
    // mesh to draw, each at its record's matrix, and a save with them in
    // place still writes the file as shipped.
    #[test]
    fn a_2070_island_draws_its_objects_with_their_models() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        let module = &anno2070::MODULE;
        let install = Install::open(dir, module.profile().archive_families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let shipped = install.read(path).unwrap();
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let objects = anno_formats::isd::objects::read(
            &anno_formats::isd::Doc::parse(&shipped).unwrap(),
            path,
        )
        .unwrap();
        assert_eq!(objects.len(), 5_368);
        // Records and the document's items, in file order.
        let items: Vec<Item> = {
            let (mut prop, mut object) = (
                editor.island.doc.props.iter(),
                editor.island.doc.objects.iter(),
            );
            // Where the record stands.
            let matrix = |o: &anno_formats::isd::objects::Object| {
                anno_core::island::trs(o.position, o.rotation, [o.scale; 3])
            };
            (objects.iter())
                .map(|o| match o.section {
                    anno_formats::isd::objects::Section::Nature
                    | anno_formats::isd::objects::Section::Grass => {
                        let p = prop.next().unwrap();
                        assert_eq!(p.matrix, matrix(o));
                        Item::Prop(p.id)
                    }
                    _ => {
                        let n = object.next().unwrap();
                        assert_eq!(n.id, o.id);
                        assert_eq!(
                            editor.island.doc.matrix(Item::Object(n.id)),
                            Some(matrix(o))
                        );
                        Item::Object(n.id)
                    }
                })
                .collect()
        };
        assert_eq!(
            editor.island.doc.props.len() + editor.island.doc.objects.len(),
            objects.len()
        );

        let assets = Arc::new(
            anno_formats::assets::Assets::load(&install, module.database().unwrap()).unwrap(),
        );
        editor.assets = Some(assets.clone());
        let install = Arc::new(install);
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
        }
        editor.refresh_scene();
        let drawn: std::collections::HashSet<Item> =
            editor.placements.item.iter().flatten().copied().collect();

        // Independently: the cfg its asset names, read by formats, has a
        // part with a mesh, a decal or a nested model.
        let mut reads = std::collections::HashMap::<String, bool>::new();
        let mut with_models = std::collections::HashSet::new();
        for (k, o) in objects.iter().enumerate() {
            let Some(cfg) = anno2070::objects::model(&assets, o) else {
                continue;
            };
            let cfg = cfg.replace('\\', "/");
            let draws = *reads.entry(cfg.clone()).or_insert_with(|| {
                anno_formats::model::Model::load(&install, &cfg).is_ok_and(|m| {
                    m.parts.iter().any(|p| p.mesh.is_some() && !p.bone_linked)
                        || !m.decals.is_empty()
                        || !m.props.is_empty()
                })
            });
            if draws {
                with_models.insert(items[k]);
            }
        }
        let mut only_drawn: Vec<_> = drawn.difference(&with_models).collect();
        let mut only_models: Vec<_> = with_models.difference(&drawn).collect();
        only_drawn.sort();
        only_models.sort();
        eprintln!(
            "{} of {} objects drawn; {} models",
            drawn.len(),
            objects.len(),
            reads.len()
        );
        assert!(
            only_drawn.is_empty() && only_models.is_empty(),
            "drawn without a model: {only_drawn:?}; with a model, not drawn: {only_models:?}"
        );
        // The one left is GUID 352, `rainbow`, an effect with no mesh.
        assert_eq!(drawn.len(), 5_367);

        // Its buildings draw in 2070's building family, its leaves (cut
        // out, no glow) in the kit's own.
        let family = anno_render::a2070_buildings::families()
            .id(anno_render::a2070_buildings::NAME)
            .unwrap();
        let (mut buildings, mut kit) = (0, 0);
        for piece in editor.library.models.loaded.values().flat_map(|p| p.iter()) {
            match piece.mesh.data().family == family {
                true => buildings += 1,
                false => kit += 1,
            }
        }
        eprintln!("{buildings} pieces in the building family, {kit} in the kit's");
        assert!(buildings > 0 && kit > 0);

        let (_config, project) = staged(&install, &mut editor);
        assert!(project.files[path] == shipped, "it saves as shipped");
    }

    // Needs Anno 2070: n_m02's objects take the editor's own tools. The
    // inspector moves the rock 17617 four metres east and nothing else;
    // Delete takes the simple record 18304 and nothing else; one undo brings
    // it back where it stood, a second puts the rock back, and the document's
    // objects are again the records as loaded.
    #[test]
    fn a_2070_object_moves_deletes_and_undoes() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        let install = Install::open(dir, anno2070::MODULE.profile().archive_families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let loaded = editor.island.doc.objects.clone();
        let get =
            |e: &IslandEditor, id: u64| e.island.doc.objects.iter().find(|o| o.id == id).cloned();
        let rock = get(&editor, 17617).unwrap();

        editor.selection = [Item::Object(17617)].into();
        editor.set_field(super::Field::Position(0), rock.position[0] + 4.);
        let moved = get(&editor, 17617).unwrap();
        assert_eq!(
            moved.position,
            [rock.position[0] + 4., rock.position[1], rock.position[2]]
        );
        assert_eq!(
            (moved.guid, moved.variation, moved.record()),
            (rock.guid, rock.variation, rock.record())
        );
        for (a, b) in editor.island.doc.objects.iter().zip(loaded.iter()) {
            if a.id != 17617 {
                assert_eq!(a, b);
            }
        }

        editor.selection = [Item::Object(18304)].into();
        editor.delete_selected();
        assert_eq!(editor.island.doc.objects.len(), loaded.len() - 1);
        assert!(get(&editor, 18304).is_none());
        assert_eq!(get(&editor, 17617), Some(moved.clone()));

        assert!(editor.undo(false));
        assert_eq!(
            get(&editor, 18304).as_ref(),
            loaded.iter().find(|o| o.id == 18304)
        );
        assert_eq!(get(&editor, 17617), Some(moved));
        assert!(editor.undo(false));
        assert_eq!(*editor.island.doc.objects, *loaded);
    }

    // Needs Anno 2070: n_m02's objects edited with the editor's own tools
    // save into its file. The inspector moves the rock 17617 four metres
    // east, Ctrl-D copies the stone 17630, and Delete takes the simple record
    // 18304; the staged file reopens with the rock moved, the copy beside
    // its original under a fresh id, 18304 gone, and every other record
    // untouched.
    #[test]
    fn a_2070_object_move_copy_and_delete_save_and_reopen() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        let install = Install::open(dir, anno2070::MODULE.profile().archive_families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let loaded = editor.island.doc.objects.clone();
        let find = |id: u64| loaded.iter().find(|o| o.id == id).unwrap().clone();
        let (rock, stone) = (find(17617), find(17630));

        editor.selection = [Item::Object(17617)].into();
        editor.set_field(super::Field::Position(0), rock.position[0] + 4.);
        editor.selection = [Item::Object(17630)].into();
        editor.duplicate_selected();
        assert_eq!(editor.island.doc.objects.len(), loaded.len() + 1);
        editor.selection = [Item::Object(18304)].into();
        editor.delete_selected();
        let (_config, project) = staged(&install, &mut editor);
        assert!(!editor.dirty(), "the save is the document's");

        let registry = anno2070::island::registry(&install).unwrap();
        let saved: Arc<[u8]> = project.files[path].clone().into();
        let back = anno2070::island::from_bytes(saved, path, registry, None).unwrap();
        let objects = &back.doc.objects;
        assert_eq!(objects.len(), loaded.len());
        assert!(objects.iter().all(|o| o.id != 18304));
        let moved = objects.iter().find(|o| o.id == 17617).unwrap();
        assert_eq!(
            moved.position,
            [rock.position[0] + 4., rock.position[1], rock.position[2]]
        );
        let largest = loaded.iter().map(|o| o.id).max().unwrap();
        let copy = objects.iter().find(|o| o.id > largest).unwrap();
        assert_eq!(
            (copy.guid, copy.rotation, copy.section),
            (stone.guid, stone.rotation, stone.section)
        );
        assert_eq!(
            copy.position,
            [stone.position[0] + 4., stone.position[1], stone.position[2]]
        );
        for o in loaded.iter().filter(|o| ![17617, 18304].contains(&o.id)) {
            let b = objects.iter().find(|b| b.id == o.id).unwrap();
            assert_eq!(
                (b.guid, b.position, b.rotation, b.scale),
                (o.guid, o.position, o.rotation, o.scale)
            );
        }
        assert_eq!(back.doc.props.len(), editor.island.doc.props.len());
    }

    // Needs Anno 2070: n_d11's trees, rocks and grass take the editor's own
    // tools and save into its file. The Library places a tree n_d11 never
    // had, Ctrl-D copies a tree, the inspector moves a rock four metres
    // east, and Delete takes a grass tuft. The staged file reopens with a
    // record of the new tree's GUID where it was put, a tree record of the
    // copied one's GUID, variation, turn and scale four metres east of it,
    // both under ids no record had; the rock moved; the grass record gone;
    // and every other record as shipped.
    #[test]
    fn a_2070_tree_copy_rock_move_and_grass_delete_save_and_reopen() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        use anno_formats::isd::objects::{Object, Section};
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let path = "data/levels/islands/normal/n_d11.isd";
        let read = |bytes: &[u8]| -> Vec<Object> {
            let doc = anno_formats::isd::Doc::parse(bytes).unwrap();
            let all = anno_formats::isd::objects::read(&doc, path).unwrap();
            (all.into_iter())
                .filter(|o| matches!(o.section, Section::Nature | Section::Grass))
                .collect()
        };
        let shipped = read(&install.read(path).unwrap());
        let assets = Arc::new(
            anno_formats::assets::Assets::load(&install, module.database().unwrap()).unwrap(),
        );
        let template = |o: &Object| assets.get(o.guid).map(|a| a.template.as_str());
        // A prop's id is its record's index among these, plus one.
        let tree = (shipped.iter())
            .position(|o| template(o) == Some("Tree"))
            .unwrap();
        let rock = (shipped.iter())
            .position(|o| {
                matches!(
                    template(o),
                    Some("NatureMountainObject" | "SimpleStoneObject")
                )
            })
            .unwrap();
        let grass = (shipped.iter())
            .position(|o| o.section == Section::Grass)
            .unwrap();

        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets.clone())),
        )
        .unwrap();
        assert_eq!(editor.island.doc.props.len(), shipped.len());
        // A tree n_d11 never had, from the Library.
        let (cfg, new) = (assets.iter())
            .filter(|a| a.template == "Tree" && !a.cfgs.is_empty())
            .map(|a| {
                (
                    &a.cfgs[0],
                    anno2070::objects::prop_kind(&assets, &a.cfgs[0]).unwrap(),
                )
            })
            .find(|(_, (guid, _))| shipped.iter().all(|o| o.guid != *guid))
            .unwrap();
        editor.choose_prop(cfg);
        assert_eq!(editor.tool, Tool::Place);
        editor.place_at([40., 0.75, 40.]);
        editor.selection = [Item::Prop(tree as u64 + 1)].into();
        editor.duplicate_selected();
        editor.selection = [Item::Prop(rock as u64 + 1)].into();
        editor.set_field(super::Field::Position(0), shipped[rock].position[0] + 4.);
        editor.selection = [Item::Prop(grass as u64 + 1)].into();
        editor.delete_selected();
        let (_config, project) = staged(&install, &mut editor);
        assert!(!editor.dirty(), "the save is the document's");

        let back = read(&project.files[path]);
        assert_eq!(back.len(), shipped.len() + 1);
        let near = |a: [f32; 3], b: [f32; 3]| (0..3).all(|k| (a[k] - b[k]).abs() <= 0.5 / 4096.);
        let largest = shipped.iter().map(|o| o.id).max().unwrap();
        let added: Vec<&Object> = back.iter().filter(|o| o.id > largest).collect();
        assert_eq!(added.len(), 2);
        let placed = added.iter().find(|o| o.guid == new.0).unwrap();
        assert_eq!((placed.section, placed.variation), (Section::Nature, new.1));
        assert_eq!(placed.position, [40., 0.75, 40.]);
        let (copy, was) = (
            *added.iter().find(|o| o.guid != new.0).unwrap(),
            &shipped[tree],
        );
        assert_eq!(
            (copy.section, copy.guid, copy.variation),
            (was.section, was.guid, was.variation)
        );
        // Through the prop's matrix: the same turn and scale, to float noise.
        let dot: f32 = (0..4).map(|k| copy.rotation[k] * was.rotation[k]).sum();
        assert!(dot.abs() > 1. - 1e-6, "{:?}", copy.rotation);
        assert!((copy.scale - was.scale).abs() < 1e-5);
        let [x, y, z] = was.position;
        assert!(near(copy.position, [x + 4., y, z]), "{:?}", copy.position);
        assert!(back.iter().all(|o| o.id != shipped[grass].id));
        for o in shipped.iter().filter(|o| o.id != shipped[grass].id) {
            let b = back.iter().find(|b| b.id == o.id).unwrap();
            assert_eq!(
                (b.guid, b.variation, b.rotation),
                (o.guid, o.variation, o.rotation)
            );
            if o.id == shipped[rock].id {
                let [x, y, z] = o.position;
                assert!(near(b.position, [x + 4., y, z]), "{:?}", b.position);
            } else {
                assert_eq!(b.position, o.position);
            }
        }
    }

    // Needs Anno 2070: the browser's "Place on an island" and the Place
    // tool put the Ecos bakery's model on n_m02 as a bakery object, by its
    // asset's GUID; one click places it, undo takes it back,
    // a second click places it again, and the staged file reopens with it
    // there, its GUID and transform as placed.
    #[test]
    fn a_2070_building_places_saves_and_reopens() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let assets = Arc::new(
            anno_formats::assets::Assets::load(&install, module.database().unwrap()).unwrap(),
        );
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets.clone())),
        )
        .unwrap();
        let cfg = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
        let guid = (assets.iter())
            .find(|a| {
                a.cfgs
                    .iter()
                    .any(|c| c.replace('\\', "/").eq_ignore_ascii_case(cfg))
            })
            .unwrap()
            .guid;
        let count = editor.island.doc.objects.len();
        editor.choose_prop(cfg);
        assert_eq!(editor.tool, Tool::Place);
        assert_eq!(
            editor.place_object.as_ref().map(|(t, _)| t.object.guid),
            Some(guid)
        );
        let at = [96.5, 0.75, 52.25];
        editor.place_at(at);
        assert_eq!(editor.island.doc.objects.len(), count + 1);
        assert!(editor.undo(false));
        assert_eq!(editor.island.doc.objects.len(), count);
        editor.place_at(at);
        let placed = editor.island.doc.objects.last().unwrap().clone();
        assert_eq!((placed.guid, placed.position), (guid, at));
        assert_eq!(editor.selection, [Item::Object(placed.id)].into());

        let (_config, project) = staged(&install, &mut editor);
        let registry = anno2070::island::registry(&install).unwrap();
        let saved: Arc<[u8]> = project.files[path].clone().into();
        let back = anno2070::island::from_bytes(saved, path, registry, None).unwrap();
        assert_eq!(back.doc.objects.len(), count + 1);
        let b = back.doc.objects.iter().find(|o| o.guid == guid).unwrap();
        assert_eq!(b.position, at);
        assert!(
            (0..4).all(|k| (b.rotation[k] - placed.rotation[k]).abs() < 1e-6),
            "{:?} for {:?}",
            b.rotation,
            placed.rotation
        );
        assert_eq!(b.variation, placed.variation);
    }

    // Needs Anno 2070: on n_m02 the Ecos bakery, which ships only on quarter
    // turns, places on one wherever the Place tool's spread of turns falls,
    // and saves a `m_Direction` of whole quarter turns; a tree placed
    // between them keeps its free turn. The rotate gizmo snaps a turn about
    // the vertical to quarters for a selection holding the bakery, and
    // leaves a tree's, a mine's and a tilt free.
    #[test]
    fn a_2070_building_places_and_turns_by_quarter_turns_and_a_tree_freely() {
        use super::whole_turns;
        use gpui_editor_kit::gizmo::Delta;
        use std::f32::consts::FRAC_PI_2;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let assets = Arc::new(
            anno_formats::assets::Assets::load(&install, module.database().unwrap()).unwrap(),
        );
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets.clone())),
        )
        .unwrap();
        let bakery = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
        let tree = (assets.iter())
            .find(|a| a.template == "Tree" && !a.cfgs.is_empty())
            .unwrap();
        let (tree_guid, tree) = (tree.guid, tree.cfgs[0].replace('\\', "/"));
        let mine = (assets.iter())
            .find(|a| a.name == "coal_mine")
            .unwrap()
            .guid;
        // The index this test reads, built here rather than polled for.
        anno2070::objects::Shipped::of(&install).unwrap();
        assert_eq!(
            editor.turn_step(mine),
            Some(0.),
            "mines turn to their deposit"
        );
        assert_eq!(editor.turn_step(tree_guid), Some(0.));

        let quarter = |yaw: f32| {
            let q = yaw / FRAC_PI_2;
            (q - q.round()).abs() < 1e-4
        };
        let mut placed = Vec::new();
        for k in 0..4 {
            editor.choose_prop(bakery);
            editor.place_at([90. + 12. * k as f32, 0.75, 52.]);
            let o = editor.island.doc.objects.last().unwrap().clone();
            assert!(quarter(o.direction), "{}", o.direction);
            assert!(editor.turn_step(o.guid) == Some(FRAC_PI_2));
            placed.push(o.id);
            // A tree between them: the spread moves on, freely.
            editor.choose_prop(&tree);
            editor.place_at([90. + 12. * k as f32, 0.75, 70.]);
            let p = editor.island.doc.props.last().unwrap();
            let yaw = 2. * p.source.rotation[1].atan2(p.source.rotation[3]);
            let spread = ((p.id as f32) * 2.399_963).rem_euclid(std::f32::consts::TAU);
            assert!((yaw.rem_euclid(std::f32::consts::TAU) - spread).abs() < 1e-4);
        }
        let turns: std::collections::BTreeSet<i64> = (editor.island.doc.objects.iter())
            .filter(|o| placed.contains(&o.id))
            .map(|o| (o.direction / FRAC_PI_2).round() as i64)
            .collect();
        assert!(
            turns.len() > 1,
            "the spread lands on several quarters: {turns:?}"
        );

        let (_config, project) = staged(&install, &mut editor);
        let doc = anno_formats::isd::Doc::parse(&project.files[path]).unwrap();
        let back = anno_formats::isd::objects::read(&doc, path).unwrap();
        let guid = editor.island.doc.objects.last().unwrap().guid;
        let saved: Vec<i64> = (back.iter())
            .filter(|o| o.guid == guid)
            .map(|o| o.direction)
            .collect();
        assert_eq!(saved.len(), placed.len());
        assert!(saved.iter().all(|d| d % 368_640 == 0), "{saved:?}");

        editor.selection = [Item::Object(placed[0])].into();
        assert_eq!(editor.selection_turn(), Some(FRAC_PI_2));
        let up = [0., 1., 0.];
        let snapped = |d: Delta, turn: f32| match whole_turns(d, turn) {
            Delta::Rotate(_, a) => a,
            _ => unreachable!(),
        };
        let turn = editor.selection_turn().unwrap();
        assert_eq!(snapped(Delta::Rotate(up, 0.9), turn), FRAC_PI_2);
        assert_eq!(snapped(Delta::Rotate(up, -2.5), turn), -2. * FRAC_PI_2);
        assert_eq!(snapped(Delta::Rotate(up, 0.3), turn), 0.);
        assert_eq!(
            snapped(Delta::Rotate([1., 0., 0.], 0.3), turn),
            0.3,
            "a tilt"
        );
        let tree_id = editor.island.doc.props.last().unwrap().id;
        editor.selection = [Item::Prop(tree_id)].into();
        let turn = editor.selection_turn().unwrap();
        assert_eq!(turn, 0.);
        assert_eq!(snapped(Delta::Rotate(up, 0.3), turn), 0.3);
    }

    // Needs Anno 2070: the buildings of mission 4's n_m02_tilda, owned by
    // players 1 and 2 (2,806 records), are dyed by their owner. Once its placements are drawn, each owned building's
    // rows carry the owner's profile colour as their instance word, the
    // first player's the colour View settings picks and the next players'
    // the colours after it; trees and unowned rows stay 0. Picking another
    // colour moves every owner's.
    #[test]
    fn a_2070_island_dyes_each_building_by_its_owner() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let path = "addondata/levels/islands/scenarios/mission04/n_m02_tilda.isd";
        let mut editor = IslandEditor::load_with(
            crate::games::module(anno_core::game::Game::Anno2070),
            &install,
            path,
            Some((
                install.clone(),
                crate::island::test_support::assets(&install),
            )),
        )
        .unwrap();
        super::gpu_bench::settle(&mut editor, &install);
        let word = |owner: u32, player: usize| {
            let rgb = module.model_look().player_colors
                [(player + owner as usize - 1) % module.model_look().player_colors.len()]
            .rgb;
            anno_render::a2070_buildings::owner_word(rgb)
        };
        let check = |editor: &IslandEditor, player: usize| {
            let owner = |id: u64| {
                (editor.island.doc.objects.iter())
                    .find(|o| o.id == id)
                    .unwrap()
                    .owner
            };
            let words = editor
                .owner_words()
                .expect("the island has owned buildings");
            assert_eq!(words.len(), editor.placements.item.len());
            let mut dyed = 0;
            for (item, &w) in editor.placements.item.iter().zip(&words) {
                match item {
                    Some(Item::Object(id)) if (1..15).contains(&owner(*id)) => {
                        assert_eq!(w, word(owner(*id), player), "object {id}");
                        dyed += 1;
                    }
                    _ => assert_eq!(w, 0, "{item:?}"),
                }
            }
            assert!(dyed > 0, "no owned building drawn");
            words
        };
        let first = check(&editor, 0);
        editor.layers.player = 4;
        editor.scene_stale = true;
        super::gpu_bench::settle(&mut editor, &install);
        let then = check(&editor, 4);
        assert_ne!(first, then);
    }

    // Needs Anno 2070: the rotate gizmo's turn never waits on the UI thread.
    // Opened from an install folder no index was built for (a link to the
    // install, which keys a fresh one), a selected building's turn is
    // unknown at once, in well under a frame, where it took 1.2 s (release)
    // on the first drag; once the index lands off the thread it is a quarter
    // turn.
    // Unix only: it links the install folder, which Windows allows only
    // with developer mode or admin rights.
    #[cfg(unix)]
    #[test]
    fn a_2070_building_turn_answers_at_once_and_snaps_once_read() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        let link = tempfile::tempdir().unwrap();
        let fresh = link.path().join("anno2070");
        std::os::unix::fs::symlink(&dir, &fresh).unwrap();
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(&fresh, module.profile().archive_families).unwrap());
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut editor = IslandEditor::load_with(
            crate::games::module(anno_core::game::Game::Anno2070),
            &install,
            path,
            Some((
                install.clone(),
                crate::island::test_support::assets(&install),
            )),
        )
        .unwrap();
        // n_m02's objects that stand on quarter turns: its buildings among
        // them.
        let quarters = std::f32::consts::FRAC_PI_2;
        editor.selection = (editor.island.doc.objects.iter())
            .filter(|o| (o.direction / quarters - (o.direction / quarters).round()).abs() < 1e-3)
            .map(|o| Item::Object(o.id))
            .collect();
        assert!(!editor.selection.is_empty());
        let asked = std::time::Instant::now();
        let first = editor.selection_turn();
        let took = asked.elapsed();
        assert!(took < std::time::Duration::from_millis(16), "{took:?}");
        let started = std::time::Instant::now();
        let turn = loop {
            if let Some(turn) = editor.selection_turn() {
                break turn;
            }
            assert!(started.elapsed().as_secs() < 120, "the index never landed");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(turn, std::f32::consts::FRAC_PI_2, "first answer {first:?}");
    }

    // Needs Anno 2070: on n_m02 the Harbour tool lays a harbour along the
    // best shore it finds: 1 m tiles of the harbour asset copied from a
    // shipped one, on cell middles, a sea edge facing the water, ground
    // behind and a land edge at the back. One undo step takes them all
    // back, after which the island saves as shipped; redone, it saves and
    // reopens to the same tiles: variation, place and turn.
    #[test]
    fn a_2070_harbour_lays_along_the_shore_with_undo_and_reopens() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        use anno_core::game::Module;
        use anno2070::harbour::{HARBOUR, kind};
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let assets = Arc::new(
            anno_formats::assets::Assets::load(&install, module.database().unwrap()).unwrap(),
        );
        let path = "data/levels/islands/normal/n_m02.isd";
        let shipped = install.read(path).unwrap();
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets.clone())),
        )
        .unwrap();
        let count = editor.island.doc.objects.len();
        editor.tool = Tool::Harbour;
        assert!(!editor.needs_harbour_library(), "2070 lays its own");
        let side = editor.island.doc.field.grid.width as f32;
        let region = super::harbour::Region::spanning([0., 0.], [side, side]);
        let curve = super::harbour::auto_harbours(&editor.island.doc.field, &region, &[], 1)
            .pop()
            .unwrap();
        let before = editor.island.doc.clone();
        assert!(editor.push_harbour(curve));
        editor.history.checkpoint_owned(before);

        let tiles: Vec<anno_core::island::objects::Object> =
            editor.island.doc.objects[count..].to_vec();
        assert!(tiles.len() > 8, "{} tiles", tiles.len());
        let mut kinds = std::collections::BTreeMap::new();
        for t in &tiles {
            assert_eq!(t.guid, HARBOUR);
            assert_eq!(t.position[0].fract(), 0.5);
            assert_eq!(t.position[2].fract(), 0.5);
            let q = t.direction / std::f32::consts::FRAC_PI_2;
            assert!((q - q.round()).abs() < 1e-4, "{}", t.direction);
            *kinds.entry(kind(t.variation)).or_insert(0) += 1;
            if kind(t.variation) == "sea edge" {
                // Ahead along local +z, the sea side, the ground falls more
                // than a quarter metre below the tile within 4 m.
                let a = t.direction;
                let ahead = |k: f32| [t.position[0] - k * a.sin(), t.position[2] + k * a.cos()];
                let f = &editor.island.doc.field;
                let low = (1..=4)
                    .map(|k| f.height(ahead(k as f32)))
                    .fold(f32::MAX, f32::min);
                assert!(low < t.position[1] - 0.25, "{:?}: {low}", t.position);
            }
        }
        eprintln!("{kinds:?}");
        for k in ["sea edge", "ground", "land edge"] {
            assert!(kinds.contains_key(k), "{k}: {kinds:?}");
        }
        let laid = |objects: &[anno_core::island::objects::Object]| {
            let mut v: Vec<(u32, [i32; 3], i64)> = (objects.iter())
                .filter(|o| o.guid == HARBOUR)
                .map(|o| {
                    let p = o.position.map(|v| (v * 4096.).round() as i32);
                    let q = (o.direction / std::f32::consts::FRAC_PI_2).round() as i64;
                    (o.variation, p, q.rem_euclid(4))
                })
                .collect();
            v.sort_unstable();
            v
        };
        let want = laid(&tiles);

        assert!(editor.undo(false));
        assert_eq!(editor.island.doc.objects.len(), count);
        let (_config, project) = staged(&install, &mut editor);
        assert!(
            project.files[path] == shipped,
            "undone, it saves as shipped"
        );
        assert!(editor.undo(true));
        let (_config, mut project) = staged(&install, &mut editor);
        let back = reopen(&install, &mut project, path);
        assert_eq!(back.island.doc.objects.len(), count + tiles.len());
        assert_eq!(laid(&back.island.doc.objects), want);
    }

    // Needs Anno 2070 and a Vulkan device: 2070 islands with their objects,
    // shot from the island's own camera and from closer in, into the folder
    // SHOTS_2070 names. SHOTS_2070_ISLANDS: a comma list of island paths
    // instead of n_m02 and the add-on's settled mission-4 island.
    // `ANNO2070_DIR=… SHOTS_2070=… cargo test island_shots_2070 -- --ignored`
    #[test]
    #[ignore]
    fn island_shots_2070() {
        use anno_core::game::Module;
        let (Some(dir), Some(out)) = (
            anno_formats::vfs::install_dir("ANNO2070_DIR"),
            std::env::var_os("SHOTS_2070"),
        ) else {
            eprintln!("skip: ANNO2070_DIR or SHOTS_2070 unset");
            return;
        };
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let islands = std::env::var("SHOTS_2070_ISLANDS").unwrap_or_else(|_| {
            "data/levels/islands/normal/n_m02.isd,addondata/levels/islands/scenarios/mission04/n_m02_tilda.isd".into()
        });
        let (w, h) = (1600u32, 900u32);
        let mut target = gpui_editor_kit::gpu::Offscreen::new(w, h).unwrap();
        let assets = assets(&install);
        for path in islands.split(',') {
            // As island_shots opens an island: with the asset database, which
            // names the models the island's buildings draw, then every
            // background job run to the end. Without the database the
            // objects never resolve and the shot shows bare ground.
            let mut editor = IslandEditor::load_with(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
                Some((install.clone(), assets.clone())),
            )
            .unwrap();
            super::gpu_bench::settle(&mut editor, &install);
            let renderer = editor.renderer.clone();
            let extent = editor.extent();
            let stem = path.rsplit('/').next().unwrap().trim_end_matches(".isd");
            // The island's own camera, then 60 m up looking 0.6 down.
            for (name, height, pitch) in [("fit", 0., 0.), ("close", 60., 0.6f32)] {
                let mut camera = editor.views[0].camera;
                if height > 0. {
                    camera.tilt = std::f32::consts::FRAC_PI_2 - pitch;
                    let at = camera.distance(extent).expect("perspective");
                    camera.zoom *= at / (height / pitch.sin());
                }
                // Pipelines, model textures and ground pages come up over
                // the first frames; draw until the pages are baked and the
                // count of drawn models holds still, at most 10 s.
                let mut shot = target.render(&renderer, camera, extent).unwrap();
                let mut last = usize::MAX;
                for _ in 0..64 {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    shot = target.frame(&renderer).unwrap();
                    let drawn = renderer.drawn();
                    let placed = drawn.instances + drawn.placed.iter().sum::<usize>();
                    if drawn.pages.baked >= drawn.pages.wanted && placed == last && placed > 0 {
                        break;
                    }
                    last = placed;
                }
                let file = std::path::Path::new(&out).join(format!("{stem}_{name}.png"));
                image::RgbaImage::from_raw(w, h, shot)
                    .unwrap()
                    .save(&file)
                    .unwrap();
                eprintln!("{}", file.display());
            }
        }
    }

    // Needs Anno 2070: a Deep Ocean island opens in the game's view from
    // under the water and a surface island in the plain one; the under-water
    // view draws the whole frame otherwise (its light, its water, no sea or
    // sky), and turning it off and on again in View settings gives back the
    // view from the air. Frames drift a little with the sky's clouds, so the
    // way back is held to what still moves, not to the byte (the 2070 crate's
    // test holds the mode switch itself to the byte).
    #[test]
    fn deep_ocean_islands_open_under_water_and_switch_back_to_the_air() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Arc::new(Install::open(dir, families).unwrap());
        let module = crate::games::module(crate::games::of(&install));
        let surface =
            IslandEditor::load(module, &install, "data/levels/islands/normal/n_m02.isd").unwrap();
        assert!(
            surface.view.is_none(),
            "a surface island opened in a view of its own"
        );
        let mut editor =
            IslandEditor::load(module, &install, "data/levels/islands/underwater/u_m02.isd")
                .unwrap();
        let view = editor
            .view
            .as_ref()
            .expect("a Deep Ocean island opens under water");
        assert_eq!(view.mode, anno2070::underwater::MODE);
        assert!(view.host_texture.is_some(), "the caustics");
        assert!(editor.layers.under_water);
        editor.layers.waves = false;
        let (w, h) = (320u32, 180u32);
        let mut target = gpui_editor_kit::gpu::Offscreen::new(w, h).unwrap();
        let mut frame = |editor: &mut IslandEditor, under: bool| {
            editor.layers.under_water = under;
            editor.scene_stale = true;
            editor.refresh_scene();
            let (camera, extent) = (editor.views[0].camera, editor.extent());
            let mut shot = Vec::new();
            for _ in 0..4 {
                shot = target.render(&editor.renderer, camera, extent).unwrap();
            }
            shot
        };
        // Share of pixels more than 8 levels apart.
        let apart = |a: &[u8], b: &[u8]| {
            let n = (a.chunks(4).zip(b.chunks(4)))
                .filter(|(p, q)| (0..3).any(|c| p[c].abs_diff(q[c]) > 8))
                .count();
            n as f64 / f64::from(w * h)
        };
        let air = frame(&mut editor, false);
        let under = frame(&mut editor, true);
        let back = frame(&mut editor, false);
        let (changed, restored) = (apart(&air, &under), apart(&air, &back));
        eprintln!("under water moved {changed:.3} of the frame; back {restored:.4}");
        assert!(
            changed > 0.3,
            "the under-water view moved {changed:.3} of the frame"
        );
        assert!(
            restored < 0.005,
            "the view from the air came back {restored:.4} apart"
        );
    }

    // Needs the games: an open island drawn on a device that is then lost,
    // as a driver reset loses one, draws the same frame on a new device
    // once its frames settle. The kit's renderer, the island's ground and
    // the game's passes (sky, sea, the under-water view's backdrop and
    // plankton) make their GPU state again from what the editor holds.
    #[test]
    fn an_open_island_draws_the_same_after_its_device_is_lost() {
        use anno_core::game::Game;
        use gpui_editor_kit::gpu::{HELD_CLOCK, Offscreen, Renderer};
        let islands = [
            (Game::Anno117, "ANNO117_DIR", PIRATE),
            (Game::Anno1800, "ANNO1800_DIR", MODERATE_M_01),
            (
                Game::Anno2070,
                "ANNO2070_DIR",
                "data/levels/islands/normal/n_m02.isd",
            ),
            (
                Game::Anno2070,
                "ANNO2070_DIR",
                "data/levels/islands/underwater/u_m02.isd",
            ),
        ];
        let (w, h) = (480, 270);
        // Held at 4 s, as the other island tests hold it, and not let go:
        // the clock is the process's, and letting it run under a parallel
        // test moved that test's held waves.
        HELD_CLOCK.store(4f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
        for (game, var, path) in islands {
            let Some(dir) = anno_formats::vfs::install_dir(var) else {
                eprintln!("skip {path}: {var} unset");
                continue;
            };
            let families = crate::games::profile(game).archive_families;
            let install = Arc::new(Install::open(dir, families).unwrap());
            let mut editor = IslandEditor::load_with(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
                Some((install.clone(), assets(&install))),
            )
            .unwrap();
            editor.layers.waves = false;
            super::gpu_bench::settle(&mut editor, &install);
            super::water_shots::show_water_at(&editor, 4.);
            let (camera, extent) = (editor.views[0].camera, editor.extent());
            let renderer = editor.renderer.clone();
            let sky = editor.sky_pass.clone();
            // As each_graphics_option_changes_the_view_and_high_draws_it_back
            // settles a view: until nothing is left to bring in or let go,
            // the ground pages baked and the clouds marched, then a pause
            // for the exposure. A few frames first: until a frame comes on
            // the new device the renderer reports the lost one's state.
            let draw = |target: &mut Offscreen, renderer: &Renderer| {
                target.render(renderer, camera, extent).unwrap();
                let started = std::time::Instant::now();
                for k in 0.. {
                    let pages = renderer.drawn().pages;
                    let still = renderer.settled() && sky.as_ref().is_none_or(|s| s.settled());
                    if k >= 3 && still && pages.baked >= pages.wanted {
                        break;
                    }
                    assert!(
                        started.elapsed().as_secs() < 90,
                        "{path}: still settling after 90 s: {:?}, pages {pages:?}",
                        renderer.waiting_on()
                    );
                    target.frame(renderer).unwrap();
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
                target.frame(renderer).unwrap()
            };
            let mut target = Offscreen::on_new_device(w, h).unwrap();
            let before = draw(&mut target, &renderer);
            target.lose();
            drop(target);
            let mut target = Offscreen::on_new_device(w, h).unwrap();
            let after = draw(&mut target, &renderer);
            // An island, not a blank or a backdrop alone.
            let colours = (before.chunks(4))
                .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
                .collect::<std::collections::HashSet<_>>()
                .len();
            assert!(colours > 2000, "{path}: drew {colours} colours");
            let off = before.iter().zip(&after).filter(|(a, b)| a != b).count();
            let most = before.iter().zip(&after).map(|(a, b)| a.abs_diff(*b)).max();
            assert!(
                off == 0,
                "{path}: {off} of {} bytes off after the loss, by at most {most:?}",
                before.len()
            );
        }
    }

    // Needs Anno 2070: an island painted with the developers' magenta
    // stand-in layers says so on opening and again on saving, naming the
    // layers and the ground they cover; the Paint pane marks them, and they
    // still paint. One without them says nothing.
    #[test]
    fn a_2070_island_with_placeholder_layers_warns_on_open_and_save() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Install::open(dir, families).unwrap();
        let open = |path: &str| {
            IslandEditor::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
            )
            .unwrap()
        };
        let mut editor = open("data/levels/testworlds/merged_press/tycooncity_n_l21.isd");
        let (said, warning) = editor.message.take().expect("a warning on opening");
        assert!(warning, "{said}");
        assert!(said.contains("draw magenta in game"), "{said}");
        // Ids 9 and 88 are the two it paints, each with some ground.
        assert!(said.contains("9 >>> test >>> n_earth_01 ("), "{said}");
        assert!(said.contains("88 wiki_dummy ("), "{said}");
        assert!(!said.contains(" 11 ") && !said.contains(" 35 "), "{said}");
        assert!(editor.placeholder_layer(9) && editor.placeholder_layer(88));
        assert!(!editor.placeholder_layer(52), "gras_01");
        let (_config, _project) = staged(&install, &mut editor);
        let (said, _) = editor.message.take().expect("a warning on saving");
        assert!(said.contains("draw magenta in game"), "{said}");

        let mut plain = open("data/levels/islands/normal/n_m02.isd");
        let said = plain.message.take().map(|m| m.0).unwrap_or_default();
        assert!(!said.contains("magenta"), "{said}");
        // Painting with one works, and the save then warns about it.
        let side = plain.island.doc.field.grid.width as f32;
        let at = [side / 2., side / 2.];
        plain.brush = Brush {
            radius: 4.,
            strength: 1.,
            ..Brush::default()
        };
        plain.tool = Tool::Paint;
        plain.paint = 9;
        let ground = [at[0], plain.island.doc.field.height(at), at[1]];
        assert!(plain.dab(ground, 0., false, None));
        let (_config, _project) = staged(&install, &mut plain);
        let (said, _) = plain.message.take().expect("a warning on saving");
        assert!(said.contains("9 >>> test >>> n_earth_01 ("), "{said}");
    }

    // Needs Anno 2070: every scatter layer n_m02 paints (grass, bushes or
    // kelp objects, with no diffuse texture) gets a Paint pane thumbnail of
    // the texture its objects wear: a picture with visible, coloured pixels.
    #[test]
    fn every_scatter_layer_on_a_2070_island_has_a_thumbnail() {
        use anno_core::island::Paint;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Install::open(dir, families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let Some(Paint::Layers(layers)) = editor.island.doc.materials.as_ref().map(|m| &m.paint)
        else {
            panic!("2070 paint");
        };
        let [w, h] = layers.chunks();
        let scatter: std::collections::BTreeSet<u8> = (0..w * h)
            .flat_map(|k| layers.ids(k).iter().copied())
            .filter(|&id| layers.registry().scatter(id))
            .collect();
        eprintln!("scatter layers on n_m02: {scatter:?}");
        assert!(!scatter.is_empty());
        for id in scatter {
            let (_, image) = (editor.thumbs.iter())
                .find(|t| t.0 == id)
                .unwrap_or_else(|| panic!("no thumbnail for scatter layer {id}"));
            let size = image.size(0);
            assert!(size.width.0 >= 16 && size.height.0 >= 16, "{id}: {size:?}");
            let pixels = image.as_bytes(0).unwrap();
            let shown = (pixels.chunks(4))
                .filter(|p| p[3] > 0 && p[..3].iter().any(|&c| c > 16))
                .count();
            assert!(shown * 10 > pixels.len() / 4, "{id}: {shown} pixels show");
        }
    }

    // Needs Anno 2070: a Raise dab on n_m02's buildable plateau lifts ground
    // over the band's top edge (gameplay height 768/1024 m). Every 1 m cell
    // it lifts from the band to above it is blocked in the document's path
    // blockers at once, all four 0.5 m bits, and no other bit of either
    // layer changes. The save writes the same blockers, since 2070's codec
    // blocks lifted ground by the same rule; one undo clears them with the
    // heights.
    #[test]
    fn a_2070_sculpt_blocks_the_ground_it_lifts_at_once_and_undo_clears_it() {
        use anno_core::island::PathLayer;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Install::open(dir, families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let loaded = editor.island.doc.clone();
        let paths = |d: &super::Doc, layer, x, z| d.blockers().unwrap().get(layer, x, z);
        // The shipped rule, worked out here: the mean of a cell's corners
        // in 1/1024 m, truncated.
        let gameplay = |f: &gpui_editor_kit::heightfield::Heightfield, x: usize, z: usize| {
            let sum = f.at(x, z) as f64 + f.at(x + 1, z) as f64;
            ((sum + f.at(x, z + 1) as f64 + f.at(x + 1, z + 1) as f64) / 4. * 1024.).trunc()
        };
        let open = |x: usize, z: usize| {
            (0..4).all(|q| !paths(&loaded, PathLayer::Blocked, 2 * x + q % 2, 2 * z + q / 2))
        };
        let band = |x: usize, z: usize| (676. ..=768.).contains(&gameplay(&loaded.field, x, z));
        // A cell of open buildable land with 5 m of it all round.
        let cols = loaded.field.grid.width - 1;
        let (x, z) = (6..cols - 6)
            .flat_map(|z| (6..cols - 6).map(move |x| (x, z)))
            .find(|&(x, z)| {
                (x - 5..=x + 5).all(|i| (z - 5..=z + 5).all(|j| band(i, j) && open(i, j)))
            })
            .unwrap();
        editor.brush = Brush {
            radius: 3.,
            strength: 1.,
            ..Brush::default()
        };
        editor.tool = Tool::Raise;
        let at = [x as f32 + 0.5, loaded.field.at(x, z), z as f32 + 0.5];
        let before = editor.island.doc.clone();
        assert!(editor.dab(at, 0., false, None));
        editor.history.checkpoint_owned(before);

        let now = editor.island.doc.clone();
        let mut lifted = 0;
        for j in z - 8..=z + 8 {
            for i in x - 8..=x + 8 {
                let lifts =
                    gameplay(&loaded.field, i, j) <= 768. && gameplay(&now.field, i, j) > 768.;
                lifted += usize::from(lifts);
                for q in 0..4 {
                    let (a, b) = (2 * i + q % 2, 2 * j + q / 2);
                    let was = paths(&loaded, PathLayer::Blocked, a, b);
                    assert_eq!(
                        paths(&now, PathLayer::Blocked, a, b),
                        was || lifts,
                        "{i},{j}"
                    );
                    let high = PathLayer::HighGround;
                    assert_eq!(paths(&now, high, a, b), paths(&loaded, high, a, b));
                }
            }
        }
        assert!(lifted > 4, "{lifted} cells lifted");

        // What the save blocks is what the document showed.
        let (_config, mut project) = staged(&install, &mut editor);
        let back = reopen(&install, &mut project, path);
        let (cols, rows) = now.blockers().unwrap().size();
        for b in 0..rows {
            for a in 0..cols {
                for layer in [PathLayer::Blocked, PathLayer::HighGround] {
                    assert_eq!(
                        paths(&back.island.doc, layer, a, b),
                        paths(&now, layer, a, b),
                        "{a},{b}"
                    );
                }
            }
        }

        assert!(editor.undo(false));
        assert_eq!(editor.island.doc.field.heights, loaded.field.heights);
        for b in 0..rows {
            for a in 0..cols {
                let layer = PathLayer::Blocked;
                assert_eq!(
                    paths(&editor.island.doc, layer, a, b),
                    paths(&loaded, layer, a, b)
                );
            }
        }
    }

    // A game whose islands, paint and ground are its own (the skeleton's
    // `.ski`, `Dots` and `Sand`) opens, sculpts, paints, undoes, saves and
    // reopens through its module alone: its codec reads and writes the
    // file, its paint takes the brush, and its ground mounts and follows
    // every dab and undo.
    #[test]
    fn a_skeleton_island_of_its_own_format_edits_saves_and_reopens() {
        use crate::games::fixtures::{Dots, SAND_UPDATES, SKELETON, SKELETON_FILES, install};
        use std::sync::atomic::Ordering;
        let path = "content/isles/reef/made/atoll/atoll.ski";
        let side = 33;
        let heights: Vec<String> = (0..side * side)
            .map(|i| {
                let (x, z) = ((i % side) as f32 - 16., (i / side) as f32 - 16.);
                format!("{}", 6. - 0.02 * (x * x + z * z))
            })
            .collect();
        let ski = format!(
            "ski 1\nsize {side} {side}\nh {}\np {}\n",
            heights.join(" "),
            vec!["1"; side * side].join(" ")
        );
        let mut files = SKELETON_FILES.to_vec();
        files.push((path, ski.as_bytes()));
        let dir = tempfile::tempdir().unwrap();
        let install = install(&dir.path().join("game"), &files);
        let mut editor = IslandEditor::load(&SKELETON, &install, path).unwrap();
        assert!(editor.island.saves_through_codec());
        let ground = editor.own_ground.as_ref().expect("draws its own ground");
        assert!(matches!(ground.draw(), super::gpu::GroundDraw::Surface(_)));
        assert_eq!(editor.shading, super::Shading::Ground);
        let ids: Vec<u8> = editor.thumbs.iter().map(|t| t.0).collect();
        assert_eq!(ids, [1, 2, 3]);
        let dots = |d: &super::Doc| match &d.materials.as_ref().unwrap().paint {
            super::Paint::Own(own) => own.get::<Dots>().unwrap().clone(),
            _ => panic!("skeleton paint"),
        };
        let loaded = editor.island.doc.clone();

        let (x, z) = (16, 16);
        let height = loaded.field.at(x, z);
        editor.brush = Brush {
            radius: 3.,
            strength: 1.,
            ..Brush::default()
        };
        editor.tool = Tool::Raise;
        let before = editor.island.doc.clone();
        assert!(editor.dab([16., height, 16.], 0., false, None));
        editor.history.checkpoint_owned(before);
        let raised = editor.island.doc.field.at(x, z);
        assert!(raised > height + 0.5, "{height} -> {raised}");

        let updates = SAND_UPDATES.load(Ordering::Relaxed);
        editor.tool = Tool::Paint;
        editor.paint = 2;
        let before = editor.island.doc.clone();
        assert!(editor.dab([16., raised, 16.], 0., false, None));
        editor.history.checkpoint_owned(before);
        assert!(
            SAND_UPDATES.load(Ordering::Relaxed) > updates,
            "the ground took the dab"
        );
        let m = editor.island.doc.materials.as_ref().unwrap();
        assert_eq!(m.dominant([16., 16.]), 2);
        assert_eq!(m.dominant([2., 2.]), 1, "away from the dab");
        let edited = editor.island.doc.clone();

        // Undone, the file saves as it was read.
        assert!(editor.undo(false) && editor.undo(false));
        assert_eq!(editor.island.doc.field.heights, loaded.field.heights);
        assert_eq!(dots(&editor.island.doc).id(x, z), 1);
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), &SKELETON).unwrap();
        editor.stage(&install, &mut project).unwrap();
        assert_eq!(
            project.files[path],
            ski.as_bytes(),
            "undone, it saves as read"
        );

        assert!(editor.undo(true) && editor.undo(true));
        editor.stage(&install, &mut project).unwrap();
        project.write().unwrap();
        (install.mount_mods(
            &[project.folder().as_path()],
            anno_core::game::Module::project_roots(&SKELETON),
        ))
        .unwrap();
        let back = IslandEditor::load(&SKELETON, &install, path).unwrap();
        assert_eq!(back.island.doc.field.heights, edited.field.heights);
        let (shown, made) = (dots(&back.island.doc), dots(&edited));
        assert!(
            (0..side * side).all(|i| shown.id(i % side, i / side) == made.id(i % side, i / side))
        );
        assert_eq!(shown.id(x, z), 2);
        assert!(back.own_ground.is_some());
    }

    // Needs Anno 2070: n_m02 opens in the island editor through 2070's own
    // codec, drawing its own ground with a thumbnail per colour layer. A
    // sculpt and a paint stroke through the shared tools undo exactly; a
    // save then gives back the shipped file byte for byte. Redone, they
    // save and reopen with the heights and paint they made, and every chunk
    // away from them as shipped.
    // Chunk `k`'s height samples as the file stores them; `None` for an
    // empty ocean chunk.
    fn chunk_heights(doc: &anno_formats::isd::Doc, k: usize) -> Option<&[u8]> {
        let map = doc.child(doc.root("Terrain")?, "ChunkMap")?;
        let element = (doc.children(map))
            .filter(|&c| doc.name(c) == b"Element")
            .nth(k)?;
        let data = doc.child(doc.child(element, "HeightMap")?, "Data")?;
        match doc.value(data) {
            anno_formats::isd::Value::Blob(b) => Some(b),
            _ => None,
        }
    }

    #[test]
    fn a_2070_island_sculpts_paints_undoes_saves_and_reopens() {
        use anno_formats::isd::terrain::Terrain;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Install::open(dir, families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let shipped = install.read(path).unwrap();
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        assert!(editor.island.saves_through_codec());
        assert!(editor.own_ground.is_some(), "draws its own ground");
        assert_eq!(editor.shading, super::Shading::Ground);
        assert!(
            editor.thumbs.len() > 20,
            "{} thumbnails",
            editor.thumbs.len()
        );
        let loaded = editor.island.doc.clone();

        // The middle of the island, on land, at a metre corner.
        let side = loaded.field.grid.width;
        let (x, z) = (side / 2, side / 2);
        let at = [x as f32, z as f32];
        let height = loaded.field.at(x, z);
        assert!(height > 0., "{height}");
        editor.brush = Brush {
            radius: 4.,
            strength: 1.,
            ..Brush::default()
        };
        editor.tool = Tool::Raise;
        let before = editor.island.doc.clone();
        assert!(editor.dab([at[0], height, at[1]], 0., false, None));
        editor.history.checkpoint_owned(before);
        let raised = editor.island.doc.field.at(x, z);
        assert!(raised > height + 0.5, "{height} -> {raised}");

        let shown = editor.island.doc.materials.as_ref().unwrap().dominant(at);
        let paint = (editor.thumbs.iter().map(|t| t.0))
            .find(|&id| id != shown && id != 0)
            .unwrap();
        editor.tool = Tool::Paint;
        editor.paint = paint;
        let before = editor.island.doc.clone();
        assert!(editor.dab([at[0], raised, at[1]], 0., false, None));
        editor.history.checkpoint_owned(before);
        let m = editor.island.doc.materials.as_ref().unwrap();
        assert_eq!(m.dominant(at), paint, "the dab's layer shows at its centre");
        let edited = editor.island.doc.clone();

        assert!(editor.undo(false) && editor.undo(false));
        assert!(!editor.undo(false), "two steps");
        assert_eq!(editor.island.doc.field.heights, loaded.field.heights);
        let paint_of = |d: &super::Doc| d.materials.as_ref().unwrap().paint.clone();
        assert!(paint_of(&editor.island.doc) == paint_of(&loaded));
        let (_config, project) = staged(&install, &mut editor);
        assert!(
            project.files[path] == shipped,
            "undone, it saves as shipped"
        );

        assert!(editor.undo(true) && editor.undo(true));
        assert_eq!(editor.island.doc.field.heights, edited.field.heights);
        assert!(paint_of(&editor.island.doc) == paint_of(&edited));
        let (_config, mut project) = staged(&install, &mut editor);
        let saved = project.files[path].clone();
        assert!(saved != shipped);
        let back = reopen(&install, &mut project, path);
        assert_eq!(back.island.doc.field.heights, edited.field.heights);
        assert!(paint_of(&back.island.doc) == paint_of(&edited));
        assert_eq!(
            back.island.doc.materials.as_ref().unwrap().dominant(at),
            paint
        );

        // Chunks away from the edits keep their heights and paint as shipped.
        let (da, db) = (
            anno_formats::isd::Doc::parse(&shipped).unwrap(),
            anno_formats::isd::Doc::parse(&saved).unwrap(),
        );
        let (a, b) = (
            Terrain::read(&da, path).unwrap(),
            Terrain::read(&db, path).unwrap(),
        );
        let (w, h) = a.size();
        let mut kept = 0;
        for k in 0..w * h {
            let (cx, cz) = ((k % w * 16) as f32, (k / w * 16) as f32);
            let near =
                cx - 6. <= at[0] && at[0] <= cx + 22. && cz - 6. <= at[1] && at[1] <= cz + 22.;
            if near {
                continue;
            }
            kept += 1;
            assert_eq!(chunk_heights(&da, k), chunk_heights(&db, k));
            assert_eq!(a.ids(k), b.ids(k), "chunk {k}");
            for l in 0..a.ids(k).len() {
                assert!(a.plane(k, l) == b.plane(k, l), "chunk {k} layer {l}");
            }
        }
        assert!(kept > w * h / 2);
    }

    // Needs Anno 2070: a Deep Ocean island takes the editor's tools and
    // reopens with what they did. On u_m02's plateau a Raise dab lifts open
    // ground off the plateau's buildable band (-27.10 to -26.90 m), and the
    // cells it lifts past the band are blocked at once, as on land; a paint
    // dab, a path-blocker stroke and an object moved 2 m east join it. The
    // staged file reopens with those heights, that paint, those blockers and
    // that object, still passes 2070's checks, and keeps every chunk away
    // from the edits as shipped. The game has no harbours under water: no
    // shipped Deep Ocean island holds a harbour tile, and the Harbour tool
    // finds no shore on one.
    #[test]
    fn a_deep_ocean_island_sculpts_paints_blocks_moves_saves_and_reopens() {
        use anno_core::game::{Module, Refs, Use};
        use anno_core::island::PathLayer;
        use anno_formats::isd::grids::gameplay_height;
        use anno_formats::isd::terrain::Terrain;
        use anno2070::island::UNDERWATER_BUILDABLE;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let module = &anno2070::MODULE;
        let install = Arc::new(Install::open(dir, module.profile().archive_families).unwrap());
        let assets = Arc::new(
            anno_formats::assets::Assets::load(&install, module.database().unwrap()).unwrap(),
        );
        // No shipped Deep Ocean island has a harbour tile.
        let deep: Vec<String> = (install.search(".isd", 100_000).into_iter())
            .map(|id| install.file_path(id).to_owned())
            .filter(|p| p.contains("underwater/"))
            .collect();
        assert!(deep.len() > 30, "{} Deep Ocean islands", deep.len());
        for p in &deep {
            let codec = module.islands().unwrap().codec().own().unwrap();
            let island = codec.read(&install, p, Some(&assets)).unwrap();
            let tiles = (island.doc.objects.iter())
                .filter(|o| o.guid == anno2070::harbour::HARBOUR)
                .count();
            assert_eq!(tiles, 0, "{p} holds harbour tiles");
        }

        let path = "data/levels/islands/underwater/u_m02.isd";
        let shipped = install.read(path).unwrap();
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets.clone())),
        )
        .unwrap();
        let side = editor.island.doc.field.grid.width as f32;
        let region = super::harbour::Region::spanning([0., 0.], [side, side]);
        let shores = super::harbour::auto_harbours(&editor.island.doc.field, &region, &[], 1);
        assert!(shores.is_empty(), "a shore under water");

        let loaded = editor.island.doc.clone();
        let blocked = |d: &super::Doc, x: usize, z: usize| {
            (0..4).any(|q| {
                (d.blockers().unwrap()).get(PathLayer::Blocked, 2 * x + q % 2, 2 * z + q / 2)
            })
        };
        let gameplay = |f: &gpui_editor_kit::heightfield::Heightfield, x: usize, z: usize| {
            gameplay_height([
                f.at(x, z),
                f.at(x + 1, z),
                f.at(x, z + 1),
                f.at(x + 1, z + 1),
            ])
        };
        let band =
            |x: usize, z: usize| UNDERWATER_BUILDABLE.contains(&gameplay(&loaded.field, x, z));
        // Open plateau with 4 m of it all round.
        let cols = loaded.field.grid.width - 1;
        let (x, z) = (6..cols - 6)
            .flat_map(|z| (6..cols - 6).map(move |x| (x, z)))
            .find(|&(x, z)| {
                (x - 4..=x + 4)
                    .all(|i| (z - 4..=z + 4).all(|j| band(i, j) && !blocked(&loaded, i, j)))
            })
            .expect("open plateau");
        editor.brush = Brush {
            radius: 3.,
            strength: 1.,
            ..Brush::default()
        };
        editor.tool = Tool::Raise;
        let at = [x as f32 + 0.5, loaded.field.at(x, z), z as f32 + 0.5];
        let before = editor.island.doc.clone();
        assert!(editor.dab(at, 0., false, None));
        editor.history.checkpoint_owned(before);
        let top = *UNDERWATER_BUILDABLE.end();
        let mut lifted = 0;
        for j in z - 6..=z + 6 {
            for i in x - 6..=x + 6 {
                let lifts = gameplay(&loaded.field, i, j) <= top
                    && gameplay(&editor.island.doc.field, i, j) > top;
                lifted += usize::from(lifts);
                assert_eq!(
                    blocked(&editor.island.doc, i, j),
                    blocked(&loaded, i, j) || lifts,
                    "{i},{j}"
                );
            }
        }
        assert!(lifted > 4, "{lifted} cells lifted off the plateau");

        // Paint 20 m away, a blocker stroke on open plateau 12 m or more
        // the other way.
        let paint_at = [x as f32 + 20., z as f32];
        let shown = editor
            .island
            .doc
            .materials
            .as_ref()
            .unwrap()
            .dominant(paint_at);
        let paint = (editor.thumbs.iter().map(|t| t.0))
            .find(|&id| id != shown && id != 0)
            .unwrap();
        editor.tool = Tool::Paint;
        editor.paint = paint;
        let before = editor.island.doc.clone();
        let h = editor.island.doc.field.height(paint_at);
        assert!(editor.dab([paint_at[0], h, paint_at[1]], 0., false, None));
        editor.history.checkpoint_owned(before);
        let before = editor.island.doc.clone();
        let (sx, sz) = (2..cols - 2)
            .flat_map(|j| (2..x.saturating_sub(12)).map(move |i| (i, j)))
            .find(|&(i, j)| band(i, j) && !blocked(&loaded, i, j))
            .expect("open plateau to block");
        let stroke = [sx as f32 + 0.5, sz as f32 + 0.5];
        let set = (editor.island.doc.blockers_mut().unwrap()).brush(
            PathLayer::Blocked,
            stroke[0],
            stroke[1],
            1.5,
            true,
        );
        assert!(set > 0);
        editor.history.checkpoint_owned(before);
        // An object 2 m east.
        let object = loaded.objects[0].clone();
        editor.selection = [Item::Object(object.id)].into();
        editor.set_field(super::Field::Position(0), object.position[0] + 2.);
        let edited = editor.island.doc.clone();

        let (_config, mut project) = staged(&install, &mut editor);
        let saved = project.files[path].clone();
        assert!(saved != shipped);
        let back = reopen(&install, &mut project, path);
        let doc = &back.island.doc;
        assert_eq!(doc.field.heights, edited.field.heights);
        let paint_of = |d: &super::Doc| d.materials.as_ref().unwrap().paint.clone();
        assert!(paint_of(doc) == paint_of(&edited));
        assert_eq!(doc.materials.as_ref().unwrap().dominant(paint_at), paint);
        let (a, b) = (doc.blockers().unwrap(), edited.blockers().unwrap());
        let (bc, br) = b.size();
        for layer in [PathLayer::Blocked, PathLayer::HighGround] {
            for j in 0..br {
                for i in 0..bc {
                    assert_eq!(a.get(layer, i, j), b.get(layer, i, j), "{i},{j}");
                }
            }
        }
        let moved = doc.objects.iter().find(|o| o.id == object.id).unwrap();
        assert_eq!(
            moved.position,
            [
                object.position[0] + 2.,
                object.position[1],
                object.position[2]
            ]
        );
        let none = |_: &str| false;
        let refs = Refs {
            install: &install,
            project: &none,
            assets: Some(&assets),
        };
        let use_ = Use {
            region: module.islands().unwrap().rules().island_region(path),
            start: false,
            playable: true,
        };
        let problems = module
            .islands()
            .unwrap()
            .validate(&back.island, &refs, use_);
        assert!(
            problems.is_empty(),
            "{:?}",
            problems.iter().map(|p| p.said()).collect::<Vec<_>>()
        );

        // Chunks away from the edits keep their heights and paint as shipped.
        let (da, db) = (
            anno_formats::isd::Doc::parse(&shipped).unwrap(),
            anno_formats::isd::Doc::parse(&saved).unwrap(),
        );
        let (a, b) = (
            Terrain::read(&da, path).unwrap(),
            Terrain::read(&db, path).unwrap(),
        );
        let (w, h) = a.size();
        let mut kept = 0;
        for k in 0..w * h {
            let (cx, cz) = ((k % w * 16) as f32, (k / w * 16) as f32);
            let near = |p: [f32; 2]| {
                cx - 6. <= p[0] && p[0] <= cx + 22. && cz - 6. <= p[1] && p[1] <= cz + 22.
            };
            if near([at[0], at[2]]) || near(paint_at) || near(stroke) {
                continue;
            }
            kept += 1;
            assert_eq!(chunk_heights(&da, k), chunk_heights(&db, k));
            assert_eq!(a.ids(k), b.ids(k), "chunk {k}");
        }
        assert!(kept > w * h / 2);
    }

    // Needs Anno 2070: the editor's database, read once, serves an island
    // open: n_m02 opened with it parses no assets again, and its 5,368
    // records (props and objects) name the models the database lists, as
    // an open that reads its own does. That one reads the database once.
    #[test]
    fn a_2070_island_opens_on_the_editors_database_without_reading_it_again() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let module = &anno2070::MODULE;
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Arc::new(Install::open(dir, families).unwrap());
        let path = "data/levels/islands/normal/n_m02.isd";
        let assets = Arc::new(crate::games::assets(module, &install).unwrap());
        let game = crate::games::of(&install);
        let before = anno2070::assets::parses();
        let shared = Some((install.clone(), assets.clone()));
        let editor =
            IslandEditor::load_with(crate::games::module(game), &install, path, shared).unwrap();
        assert_eq!(
            anno2070::assets::parses(),
            before,
            "the open read no assets"
        );
        let alone = IslandEditor::load(crate::games::module(game), &install, path).unwrap();
        assert_eq!(
            anno2070::assets::parses(),
            before + 1,
            "alone, it reads them once"
        );
        let doc = &editor.island.doc;
        let placed = |e: &IslandEditor| {
            let d = &e.island.doc;
            (d.meshes.clone(), d.props.len(), d.objects.len())
        };
        assert_eq!(placed(&editor), placed(&alone));
        // Trees, rocks and grass whose file names a model, not a marker,
        // and objects whose asset lists one.
        let props = (doc.props.iter())
            .filter(|p| doc.meshes[p.source.file as usize] != anno2070::objects::NO_MODEL)
            .count();
        let objects = (doc.objects.iter())
            .filter(|o| assets.get(o.guid).is_some_and(|a| !a.cfgs.is_empty()))
            .count();
        // Recorded 2026-11-19 on the Steam install.
        assert_eq!(assets.iter().count(), 1_359);
        assert_eq!((props, doc.props.len()), (5_338, 5_338));
        assert_eq!((objects, doc.objects.len()), (30, 30));
    }

    // Needs Anno 2070: on n_m02 a Raise dab lifts buildable ground and the
    // document blocks it; a Grid stroke with Shift then clears the middle of
    // it. The save writes the cleared cells open, as the editor shows them,
    // where it used to block them again from the file's heights. Undoing the
    // clear saves them blocked, undoing the sculpt saves the island as
    // shipped, and redoing both saves the cleared cells open again.
    #[test]
    fn a_2070_cell_a_sculpt_lifts_and_the_grid_brush_clears_saves_open() {
        use anno_core::island::PathLayer;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Install::open(dir, families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let shipped = install.read(path).unwrap();
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let loaded = editor.island.doc.clone();
        let blocked = |d: &super::Doc, x: usize, z: usize| {
            d.blockers().unwrap().get(PathLayer::Blocked, x, z)
        };
        // Open band ground 5 m all round (the shipped rule, worked out here).
        let gameplay = |f: &gpui_editor_kit::heightfield::Heightfield, x: usize, z: usize| {
            let sum = f.at(x, z) as f64 + f.at(x + 1, z) as f64;
            ((sum + f.at(x, z + 1) as f64 + f.at(x + 1, z + 1) as f64) / 4. * 1024.).trunc()
        };
        let open =
            |x: usize, z: usize| (0..4).all(|q| !blocked(&loaded, 2 * x + q % 2, 2 * z + q / 2));
        let band = |x: usize, z: usize| (676. ..=768.).contains(&gameplay(&loaded.field, x, z));
        let cols = loaded.field.grid.width - 1;
        let (x, z) = (6..cols - 6)
            .flat_map(|z| (6..cols - 6).map(move |x| (x, z)))
            .find(|&(x, z)| {
                (x - 5..=x + 5).all(|i| (z - 5..=z + 5).all(|j| band(i, j) && open(i, j)))
            })
            .unwrap();
        let at = [x as f32 + 0.5, z as f32 + 0.5];
        let ground = |e: &IslandEditor| [at[0], e.island.doc.field.height(at), at[1]];

        editor.brush = Brush {
            radius: 3.,
            strength: 1.,
            ..Brush::default()
        };
        editor.tool = Tool::Raise;
        let before = editor.island.doc.clone();
        assert!(editor.dab(ground(&editor), 0., false, None));
        editor.history.checkpoint_owned(before);
        // The 0.5 m bits of the dab's middle metre.
        let middle: Vec<(usize, usize)> = (0..4).map(|q| (2 * x + q % 2, 2 * z + q / 2)).collect();
        assert!(
            middle
                .iter()
                .all(|&(a, b)| blocked(&editor.island.doc, a, b))
        );
        let lifted = editor.island.doc.clone();

        editor.tool = Tool::Grid;
        editor.blocker = grids::BlockerEdit::Paths(PathLayer::Blocked);
        editor.brush.radius = 1.;
        let before = editor.island.doc.clone();
        assert!(editor.dab(ground(&editor), 0., true, None));
        editor.history.checkpoint_owned(before);
        assert!(
            middle
                .iter()
                .all(|&(a, b)| !blocked(&editor.island.doc, a, b))
        );
        let cleared = editor.island.doc.clone();

        // Saved and reopened, every bit of both layers is the document's.
        let saves_as = |editor: &mut IslandEditor, want: &super::Doc| {
            let (_config, mut project) = staged(&install, editor);
            let back = reopen(&install, &mut project, path);
            let (a, b) = (
                back.island.doc.blockers().unwrap(),
                want.blockers().unwrap(),
            );
            let (cols, rows) = a.size();
            for layer in [PathLayer::Blocked, PathLayer::HighGround] {
                for j in 0..rows {
                    for i in 0..cols {
                        assert_eq!(a.get(layer, i, j), b.get(layer, i, j), "{layer:?} {i},{j}");
                    }
                }
            }
        };
        saves_as(&mut editor, &cleared);

        assert!(editor.undo(false));
        assert!(
            middle
                .iter()
                .all(|&(a, b)| blocked(&editor.island.doc, a, b))
        );
        saves_as(&mut editor, &lifted);
        assert!(editor.undo(false) && !editor.undo(false));
        let (_config, project) = staged(&install, &mut editor);
        assert!(
            project.files[path] == shipped,
            "undone, it saves as shipped"
        );

        assert!(editor.undo(true) && editor.undo(true));
        assert_eq!(editor.island.doc.field.heights, cleared.field.heights);
        assert!(
            middle
                .iter()
                .all(|&(a, b)| !blocked(&editor.island.doc, a, b))
        );
        saves_as(&mut editor, &cleared);
    }

    // Needs Anno 2070: on every shipped island with open buildable land, the
    // rule that blocks lifted ground runs in the document, and the save
    // writes the document's path blockers bit for bit: 2 m of ground raised
    // there is blocked, the middle metre cleared again saves open, and
    // reopened, both layers equal the document's.
    #[test]
    fn every_2070_island_saves_its_path_blockers_as_the_document_holds_them() {
        use anno_core::island::PathLayer;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Install::open(dir, families).unwrap();
        let registry = anno2070::island::registry(&install).unwrap();
        let (mut files, mut lifted_on) = (0, 0);
        for i in 0..install.file_count() {
            let id = anno_formats::vfs::FileId(i as u32);
            let path = install.file_path(id).to_owned();
            if !path.ends_with(".isd") {
                continue;
            }
            files += 1;
            let bytes: Arc<[u8]> = install.read_file(id).unwrap().into();
            let mut island =
                anno2070::island::from_bytes(bytes.clone(), &path, registry.clone(), None).unwrap();
            let doc = &island.doc;
            let blocked = |d: &super::Doc, a: usize, b: usize| {
                d.blockers().unwrap().get(PathLayer::Blocked, a, b)
            };
            let f = &doc.field;
            let gameplay = |x: usize, z: usize| {
                anno_formats::isd::grids::gameplay_height([
                    f.at(x, z),
                    f.at(x + 1, z),
                    f.at(x, z + 1),
                    f.at(x + 1, z + 1),
                ])
            };
            let fits = |x: usize, z: usize| {
                (x - 1..=x + 2).all(|i| {
                    (z - 1..=z + 2).all(|j| {
                        (676..=768).contains(&gameplay(i, j))
                            && (0..4).all(|q| !blocked(doc, 2 * i + q % 2, 2 * j + q / 2))
                    })
                })
            };
            let cols = f.grid.width - 1;
            let Some((x, z)) = (2..cols - 3)
                .flat_map(|z| (2..cols - 3).map(move |x| (x, z)))
                .find(|&(x, z)| fits(x, z))
            else {
                continue;
            };
            lifted_on += 1;
            // Corners x..=x+2 rise 2 m: cells x-1..=x+2 lift.
            let was = doc.field.clone();
            let width = f.grid.width;
            let field = Arc::make_mut(&mut island.doc.field);
            for j in z..=z + 2 {
                for i in x..=x + 2 {
                    field.heights[j * width + i] += 2.;
                }
            }
            let cells = gpui_editor_kit::heightfield::Rect {
                x0: x - 1,
                z0: z - 1,
                x1: x + 3,
                z1: z + 3,
            };
            assert_eq!(
                grids::block_lifted(&mut island.doc, |i, j| was.at(i, j), cells),
                16,
                "{path}"
            );
            let middle = [x as f32 + 1., z as f32 + 1.];
            let b = island.doc.blockers_mut().unwrap();
            assert!(b.brush(PathLayer::Blocked, middle[0], middle[1], 0.6, false) > 0);
            assert!(!blocked(&island.doc, 2 * x + 2, 2 * z + 2), "{path}");

            let saved = anno2070::island::write(&island, &path, None).unwrap();
            let back =
                anno2070::island::from_bytes(saved.into(), &path, registry.clone(), None).unwrap();
            let (a, b) = (back.doc.blockers().unwrap(), island.doc.blockers().unwrap());
            let (cols, rows) = a.size();
            for layer in [PathLayer::Blocked, PathLayer::HighGround] {
                for j in 0..rows {
                    for i in 0..cols {
                        assert_eq!(
                            a.get(layer, i, j),
                            b.get(layer, i, j),
                            "{path} {layer:?} {i},{j}"
                        );
                    }
                }
            }
        }
        assert_eq!(files, 384);
        eprintln!("{lifted_on} islands with open buildable land");
        assert!(lifted_on > 200, "{lifted_on}");
    }

    // Needs Anno 2070: on n_m02 the Grid tool edits 2070's blocker grids.
    // A stroke on each path layer, a build blocker drawn by clicks, one of
    // its points dragged and a shipped polygon removed are five undo steps.
    // A sculpt before them that lifts no ground over the band (a Lower dab)
    // leaves the grids shared with its step, so it copies none of them.
    // Undone, the island saves as shipped; redone, it
    // saves and reopens with the cells and polygons the tools made, and the
    // ground shows the painted layer.
    #[test]
    fn a_2070_island_edits_its_blockers_with_undo() {
        use anno_core::island::PathLayer;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno2070).archive_families;
        let install = Install::open(dir, families).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let shipped = install.read(path).unwrap();
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        assert!(editor.edits_blockers());
        let side = editor.island.doc.field.grid.width as f32;
        let at = [side / 2., side / 2.];
        let ground = |e: &IslandEditor, p: [f32; 2]| [p[0], e.island.doc.field.height(p), p[1]];
        editor.brush = Brush {
            radius: 2.,
            strength: 1.,
            ..Brush::default()
        };

        editor.tool = Tool::Lower;
        let before = editor.island.doc.clone();
        assert!(editor.dab(ground(&editor, at), 0., false, None));
        editor.history.checkpoint_owned(before);
        let step = editor.history.snapshots().next().unwrap();
        assert!(
            std::ptr::eq(
                step.blockers().unwrap(),
                editor.island.doc.blockers().unwrap()
            ),
            "a sculpt shares the blocker grids with its undo step"
        );
        let unblocked = editor.island.doc.clone();

        editor.tool = Tool::Grid;
        let cell = |p: [f32; 2]| ((p[0] * 2.) as usize, (p[1] * 2.) as usize);
        let strokes = [
            (PathLayer::Blocked, [at[0] - 20., at[1]], false),
            (PathLayer::HighGround, [at[0] + 20., at[1]], false),
        ];
        let mut want = Vec::new();
        for (layer, p, _) in strokes {
            editor.blocker = grids::BlockerEdit::Paths(layer);
            let was = editor
                .island
                .doc
                .blockers()
                .unwrap()
                .get(layer, cell(p).0, cell(p).1);
            let before = editor.island.doc.clone();
            // Blocked where it was open, cleared where it was blocked.
            assert!(editor.dab(ground(&editor, p), 0., was, None));
            editor.history.checkpoint_owned(before);
            want.push((layer, p, !was));
        }
        assert_eq!(editor.path_overlay(), Some(PathLayer::HighGround));

        editor.blocker = grids::BlockerEdit::Shapes;
        let shapes = |e: &IslandEditor| {
            let s = e.island.doc.blockers().unwrap().shapes.clone().unwrap();
            (0..s.len())
                .map(|n| s.points(n).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        let shipped_shapes = shapes(&editor);
        assert!(shipped_shapes.len() >= 2, "n_m02 lists build blockers");
        let corners = [[10., 10.], [18., 10.], [18., 18.], [10., 18.]];
        for c in corners {
            assert!(editor.shape_press(c, false, false).is_none());
        }
        // Clicking the first point again closes it.
        assert!(editor.shape_press([10.5, 10.], false, false).is_none());
        assert_eq!(shapes(&editor).len(), shipped_shapes.len() + 1);
        let added = shipped_shapes.len();
        let drag = editor.shape_press([18., 18.], false, false);
        assert!(
            matches!(drag, Some(super::Drag::ShapePoint { shape, point: 2, .. }) if shape == added)
        );
        let before = editor.island.doc.clone();
        assert!(editor.move_shape_point(added, 2, [20., 21.]));
        assert!(
            !editor.move_shape_point(added, 2, [5., 5.]),
            "a move that turns it inside out is refused"
        );
        editor.history.checkpoint_owned(before);
        // A metre point inside the first shipped polygon and no other.
        let inside = (0..side as usize * side as usize)
            .map(|i| [(i % side as usize) as f64, (i / side as usize) as f64])
            .find(|&p| {
                (shipped_shapes.iter().enumerate()).all(|(n, s)| grids::inside(s, p) == (n == 0))
            })
            .unwrap();
        assert!(
            editor
                .shape_press(inside.map(|v| v as f32), true, false)
                .is_none()
        );
        let edited_shapes = shapes(&editor);
        assert_eq!(edited_shapes.len(), shipped_shapes.len());
        assert_eq!(edited_shapes.last().unwrap()[2], [20., 21.]);
        let edited = editor.island.doc.clone();

        for _ in 0..5 {
            assert!(editor.undo(false));
        }
        assert_eq!(shapes(&editor), shipped_shapes);
        for (layer, p, on) in &want {
            let (x, z) = cell(*p);
            assert_eq!(editor.island.doc.blockers().unwrap().get(*layer, x, z), !on);
        }
        assert_eq!(editor.island.doc.field.heights, unblocked.field.heights);
        assert!(editor.undo(false) && !editor.undo(false));
        let (_config, project) = staged(&install, &mut editor);
        assert!(
            project.files[path] == shipped,
            "undone, it saves as shipped"
        );

        for _ in 0..6 {
            assert!(editor.undo(true));
        }
        assert_eq!(shapes(&editor), edited_shapes);
        let (_config, mut project) = staged(&install, &mut editor);
        let back = reopen(&install, &mut project, path);
        assert_eq!(shapes(&back), edited_shapes);
        let (a, b) = (
            back.island.doc.blockers().unwrap(),
            edited.blockers().unwrap(),
        );
        let (cols, rows) = a.size();
        for layer in [PathLayer::Blocked, PathLayer::HighGround] {
            for z in 0..rows {
                for x in 0..cols {
                    assert_eq!(a.get(layer, x, z), b.get(layer, x, z), "{layer:?} {x},{z}");
                }
            }
        }
        for (layer, p, on) in &want {
            let (x, z) = cell(*p);
            assert_eq!(a.get(*layer, x, z), *on, "{layer:?} at {p:?}");
        }
    }

    /// The editor's ground materials by palette id and file.
    fn material_names(editor: &IslandEditor) -> Vec<(u8, String)> {
        (editor.palette.as_ref().unwrap().materials.iter())
            .map(|(&id, m)| (id, m.file.clone()))
            .collect()
    }

    // Needs the game: with the splat drawing the ground and the cliff tops
    // hidden nothing bakes it; showing the cliff tops bakes it for them.
    #[test]
    fn the_ground_bakes_once_cliff_tops_show() {
        let Some((install, mut editor)) = open(PIRATE) else {
            return;
        };
        let install = Arc::new(install);
        super::gpu_bench::settle(&mut editor, &install);
        assert!(editor.splat.is_some() && editor.ground.is_none());
        editor.layers.shipped = true;
        editor.refresh_scene();
        assert!(editor.ground_stale, "a bake is due");
        super::gpu_bench::settle(&mut editor, &install);
        assert!(editor.ground.is_some());
        let ground = editor.texture().map(gpui_editor_kit::texture::Image::from);
        assert!(ground.is_some(), "the cliff tops take it");
    }

    // Needs the game: a prop with AdaptTerrainHeight keeps its height above
    // the ground through a raise stroke and a sideways move, another
    // stays where it is, and the inspector's toggle sets the flag with undo.
    #[test]
    fn props_that_follow_the_terrain_keep_their_height_above_it() {
        let Some((_, mut editor)) = open(PIRATE) else {
            return;
        };
        let (x, z) = (160., 160.);
        let grounds = [0., 1.].map(|dx| editor.island.doc.field.height([x + dx, z]));
        let props = &mut editor.island.doc.props;
        for (k, follows) in [(0, true), (1, false)] {
            let p = &mut props[k];
            p.source.adapt_terrain_height = follows.then_some(1);
            p.matrix[12] = x + k as f32;
            p.matrix[13] = grounds[k] + 0.3;
            p.matrix[14] = z;
        }
        let (a, b) = (props[0].id, props[1].id);
        let y = |e: &IslandEditor, id| e.island.doc.prop(id).unwrap().matrix[13];
        let above = |e: &IslandEditor, id| {
            let m = e.island.doc.prop(id).unwrap().matrix;
            m[13] - e.island.doc.field.height([m[12], m[14]])
        };
        editor.tool = Tool::Raise;
        editor.brush.radius = 6.;
        editor.brush.strength = 1.;
        for _ in 0..4 {
            assert!(editor.dab([x, grounds[0], z], 0., false, None));
        }
        assert!(editor.island.doc.field.height([x, z]) > grounds[0] + 0.5);
        assert!(
            (above(&editor, a) - 0.3).abs() < 1e-3,
            "{}",
            above(&editor, a)
        );
        assert_eq!(y(&editor, b), grounds[1] + 0.3);

        // Sideways across the slope the brush left.
        let from = editor.island.doc.prop(a).unwrap().matrix;
        let mut to = from;
        to[12] += 5.;
        let moved = editor.keep_above_ground(Item::Prop(a), &from, to);
        let ground = editor.island.doc.field.height([to[12], to[14]]);
        assert!((moved[13] - ground - 0.3).abs() < 1e-3);
        assert_eq!(editor.keep_above_ground(Item::Prop(b), &from, to), to);

        editor.selection = [Item::Prop(b)].into();
        editor.set_follows_terrain(true);
        assert_eq!(
            editor
                .island
                .doc
                .prop(b)
                .unwrap()
                .source
                .adapt_terrain_height,
            Some(1)
        );
        assert!(editor.undo(false));
        assert_eq!(
            editor
                .island
                .doc
                .prop(b)
                .unwrap()
                .source
                .adapt_terrain_height,
            None
        );
    }

    #[test]
    fn sculpted_coast_updates_gameplay_without_erasing_other_grid_edits() {
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = new_island(&install, "moderate", 192, Start::Sea, "CoastSync");
        let grids = Arc::make_mut(editor.island.doc.grids.as_mut().unwrap());
        grids.layer_mut(Kind::Environment).unwrap().set(0, 0, 7);
        grids.layer_mut(Kind::River).unwrap().set(5, 5, 1);
        let before = editor.island.doc.clone();
        let width = editor.island.doc.field.grid.width;
        let heights = &mut Arc::make_mut(&mut editor.island.doc.field).heights;
        for z in 144..240 {
            for x in 144..240 {
                heights[z * width + x] = 4.;
            }
        }
        editor.history.checkpoint_owned(before);
        let (_config, mut project) = staged(&install, &mut editor);
        let at_centre =
            |e: &IslandEditor, kind| grids::value_at(layer(&e.island.doc, kind), 96., 96.);
        assert_eq!(at_centre(&editor, Kind::Water), 1);
        assert_eq!(
            at_centre(&editor, Kind::AreaIds),
            super::harbour::CONSTRUCTION
        );
        assert_eq!(layer(&editor.island.doc, Kind::Environment).get(0, 0), 7);
        assert_eq!(layer(&editor.island.doc, Kind::River).get(5, 5), 1);
        assert!(editor.undo(false));
        editor.stage(&install, &mut project).unwrap();
        assert_eq!(at_centre(&editor, Kind::Water), 0);
    }

    #[test]
    fn saves_and_undo_keep_terrain_objects_and_grids_consistent() {
        for (install, path) in [(install_1800(), MODERATE_M_01), (install(), PIRATE)] {
            let Some(install) = install else {
                continue;
            };
            let game = crate::games::of(&install);
            let mut editor = IslandEditor::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
            )
            .unwrap();
            let before = editor.island.doc.clone();
            let at = [160., 160.];
            let width = editor.island.doc.field.grid.width;
            let heights = &mut Arc::make_mut(&mut editor.island.doc.field).heights;
            for (i, h) in heights.iter_mut().enumerate() {
                if (318..323).contains(&(i % width)) && (318..323).contains(&(i / width)) {
                    *h += 1.5;
                }
            }
            Arc::make_mut(&mut editor.island.doc.objects)[0].position[1] += 1.;
            let active = Arc::make_mut(editor.island.doc.grids.as_mut().unwrap())
                .layer_mut(Kind::ActiveMap)
                .unwrap();
            active.set(0, 0, 1 - active.get(0, 0));
            editor.brush.radius = 3.;
            editor.brush.strength = 1.;
            editor.tool = Tool::Paint;
            editor.paint = 31;
            editor.dab(
                [at[0], editor.island.doc.field.height(at), at[1]],
                0.,
                false,
                None,
            );
            editor.tool = Tool::Tint;
            editor.tint_color = tint::PRESETS[1].1;
            editor.dab(
                [at[0], editor.island.doc.field.height(at), at[1]],
                0.,
                false,
                None,
            );
            editor.history.checkpoint_owned(before.clone());
            let (_config, mut project) = staged(&install, &mut editor);
            let pages: std::collections::BTreeMap<_, _> = (project.files.iter())
                .filter(|(p, _)| {
                    [
                        "/normalmaps/",
                        "/meshes/",
                        "/tintmaps/",
                        "/materialmaps/",
                        "mapimage",
                    ]
                    .iter()
                    .any(|d| p.contains(d))
                })
                .map(|(p, b)| (p.clone(), b.clone()))
                .collect();
            assert!(!pages.is_empty(), "{game:?}");
            let originals: std::collections::BTreeMap<_, _> = (pages.keys())
                .map(|p| (p.clone(), install.read(p).ok()))
                .collect();
            project.write().unwrap();
            // Save mounts the mod before the next edit or save.
            install
                .mount_mods(&[project.folder().as_path()], &["data"])
                .unwrap();
            editor.stage(&install, &mut project).unwrap();
            for (p, bytes) in &pages {
                assert!(&project.files[p] == bytes, "{game:?}: repeated save {p}");
            }
            assert!(editor.undo(false));
            editor.stage(&install, &mut project).unwrap();
            for (path, bytes) in &originals {
                match bytes {
                    Some(bytes) => assert!(
                        project.files.get(path) == Some(bytes),
                        "{game:?}: undo changed original {path}"
                    ),
                    None => assert!(
                        !project.files.contains_key(path),
                        "{game:?}: undo left generated {path}"
                    ),
                }
            }
            let back = reopen(&install, &mut project, path);
            assert_eq!(
                back.island.doc.field.height(at),
                before.field.height(at),
                "{game:?}: height undo"
            );
            assert_eq!(
                back.island.doc.objects[0].position, before.objects[0].position,
                "{game:?}: object undo"
            );
            assert_eq!(
                layer(&back.island.doc, Kind::ActiveMap).get(0, 0),
                layer(&before, Kind::ActiveMap).get(0, 0),
                "{game:?}: grid undo"
            );
        }
    }

    #[test]
    fn installed_1800_world_machine_import_saves_embedded_maps() {
        use super::worldmachine::{Link, default_channels};
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            MODERATE_M_01,
        )
        .unwrap();
        let exports = tempfile::tempdir().unwrap();
        image::ImageBuffer::from_pixel(8, 8, image::Luma([32768u16]))
            .save(exports.path().join("Height.png"))
            .unwrap();
        image::ImageBuffer::from_pixel(8, 8, image::Rgba([0u8, 0, 0, 255]))
            .save(exports.path().join("Splatmap.png"))
            .unwrap();
        image::RgbImage::from_fn(8, 8, |x, _| image::Rgb([80 + x as u8 * 16, 128, 128]))
            .save(exports.path().join("Colormap.png"))
            .unwrap();
        let channels = default_channels(&editor.rules().ground_names, &material_names(&editor));
        assert!(channels.iter().all(Option::is_some));
        let grass = channels[3].unwrap();
        let mut link = Link::new(exports.path().to_owned());
        link.channels = vec![channels];
        editor.set_listing(|l| l.world_machine = Some(link));
        let old_height = editor.island.doc.field.height([160., 160.]);
        let imported = editor.wm_job(true).unwrap()().unwrap();
        assert!(
            imported.heights.is_some() && imported.materials.is_some() && imported.tint.is_some()
        );
        editor.apply_wm(Ok(imported));
        let height = editor.island.doc.field.height([160., 160.]);
        assert!((height - 40.).abs() < 0.01);
        let dominant = |e: &IslandEditor| {
            e.island
                .doc
                .materials
                .as_ref()
                .unwrap()
                .dominant([160., 160.])
        };
        assert_eq!(dominant(&editor), grass);
        assert!(editor.undo(false));
        assert_eq!(editor.island.doc.field.height([160., 160.]), old_height);
        assert!(editor.undo(true));
        let (_config, mut project) = staged(&install, &mut editor);
        let back = reopen(&install, &mut project, MODERATE_M_01);
        assert!((back.island.doc.field.height([160., 160.]) - height).abs() < 1. / 128.);
        assert_eq!(back.island.doc.materials.as_ref().unwrap().per_metre, 2.);
        assert_eq!(dominant(&back), grass);
        assert!(back.island.doc.tint.is_some());
        assert!(!project.files.keys().any(|p| p.contains("materialmaps/")));
    }

    #[test]
    fn installed_1800_land_class_follows_unsaved_active_area() {
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            MODERATE_M_01,
        )
        .unwrap();
        let layer = Arc::make_mut(editor.island.doc.grids.as_mut().unwrap())
            .layer_mut(Kind::ActiveMap)
            .unwrap();
        for y in 0..layer.height {
            for x in 0..layer.width {
                layer.set(x, y, u32::from(x < 20 && y < 20));
            }
        }
        assert_eq!(editor.land_extent(), 160);
        assert_eq!(editor.land_class(), "Small");
    }

    // New Hills islands in every 1800 region: the template's textures and
    // areas, a harbour, river, lake and each resource slot the region has,
    // staged with slot metadata and fertilities onto test maps, reloaded.
    #[test]
    fn installed_1800_new_islands_save_with_resources_in_all_four_regions() {
        use super::{fertility, harbour};
        let Some(install) = install_1800() else {
            return;
        };
        let assets = assets(&install);
        let skeletons = super::skeletons(
            crate::games::module(crate::games::of(&install)),
            &install,
            Some(&assets),
        );
        let mut qa = std::env::var_os("ANNO_EDITOR_QA_EXPORT").map(|dir| {
            let mut project = Project::load(
                std::path::Path::new(&dir),
                crate::games::module(crate::games::of(&install)),
            )
            .unwrap();
            project.info.name = "Anno Editor 1800 QA".into();
            project.set_mod_id("anno-editor-1800-qa").unwrap();
            project
        });
        for region in ["moderate", "colony01", "arctic", "africa"] {
            let skeleton = (skeletons.iter())
                .find(|s| s.region == region && s.world[0] <= 384)
                .unwrap()
                .clone();
            let name = format!("resource_qa_{region}");
            let mut editor = create(&install, skeleton, Start::Hills, &name);
            assert!(
                (editor.palette.as_ref()).is_some_and(|p| !p.materials.is_empty()),
                "{region} textures"
            );
            assert!(
                (editor.island.doc.grids.as_ref().unwrap())
                    .layer(Kind::AreaIds)
                    .is_some(),
                "{region} areas"
            );
            editor.assets = Some(assets.clone());
            let lib = harbour_library(&install, &assets, &editor);
            editor.island.harbour = Some(lib.clone());
            let size = editor.world()[0] as f32;
            let c = size * 0.5;
            editor.harbour_at([c, 0., size * 0.16], false);
            let coast = editor.coast_guids();
            assert!(
                (editor.island.doc.objects.iter()).any(|o| coast.contains(&o.guid)),
                "{region}: harbour"
            );
            for (lake, piece) in [(false, Piece::River), (true, Piece::Lake)] {
                if lib.guid(piece).is_none() {
                    continue;
                }
                editor.lake_mode = lake;
                let points = if lake {
                    vec![
                        [c + 10., c + 15.],
                        [c + 18., c + 10.],
                        [c + 25., c + 18.],
                        [c + 17., c + 25.],
                    ]
                } else {
                    vec![[c - 20., c - 12.], [c, c - 10.], [c + 20., c - 12.]]
                };
                for p in points {
                    editor.river_add(p);
                }
                editor.finish_river();
                assert!(
                    (editor.island.doc.objects.iter()).any(|o| Some(o.guid) == lib.guid(piece)),
                    "{region}: {piece:?}"
                );
            }
            let mut slots = Vec::new();
            for (i, (resource, _)) in harbour::Resource::ALL.into_iter().enumerate() {
                if lib.guid(resource.pieces(false).0).is_none() {
                    continue;
                }
                editor.island.resource = resource;
                let x = c - 18. + i as f32 * 6.;
                assert!(
                    editor.push_slot([x, editor.island.doc.field.height([x, c]), c]),
                    "{region}: {resource:?}"
                );
                let &Item::Object(id) = editor.selection.first().unwrap() else {
                    unreachable!()
                };
                let slot = editor
                    .island
                    .doc
                    .objects
                    .iter()
                    .find(|o| o.id == id)
                    .unwrap();
                slots.push((id, slot.guid));
                if let Some(replacement) =
                    (assets.slot_replacements(slot.guid, &editor.asset_region())).first()
                {
                    editor.set_slot_resource(Some(replacement.guid));
                }
            }
            assert!(!slots.is_empty(), "{region}: resources");
            let fertilities = fertility::choices(&assets, &editor.asset_region());
            if region != "arctic" {
                assert!(!fertilities.is_empty(), "{region}: fertility sets");
            }
            editor.listing.fertilities = Some(
                fertilities
                    .first()
                    .map(|f| vec![f.guid])
                    .unwrap_or_default(),
            );
            let (_config, mut project) = staged(&install, &mut editor);
            assert_eq!(project.islands[0].region, editor.region());
            assert_eq!(saved(&project, &editor.path).world_size(), editor.world());
            let info = anno_formats::a7m::a7minfo::A7mInfo::parse(
                &project.files[&format!("{}info", editor.path)],
            )
            .unwrap();
            let metadata = info.random_slots();
            for (id, guid) in &slots {
                assert!(
                    (metadata.iter())
                        .any(|g| g.guid == *guid && g.objects.iter().any(|o| o.object_id == *id)),
                    "{region}: missing slot metadata"
                );
            }
            let plans = crate::playtest::plan(&install, Some(&assets), &project).unwrap();
            assert!(!plans.is_empty(), "{region} test maps");
            assert!(
                plans.iter().any(|p| p
                    .slots
                    .iter()
                    .any(|s| matches!(s.fill, crate::playtest::Fill::Ours(0)))),
                "{region}: no test-map slot"
            );
            crate::playtest::apply(plans, &mut project);
            for map in &project.maps {
                let bytes = &project.files[&map.path.replace(".a7t", ".a7tinfo")];
                use anno_formats::a7m::template::{MapTemplate, SlotKind};
                let template = MapTemplate::parse(bytes).unwrap();
                for slot in template.slots.iter().filter(
                    |s| matches!(&s.kind, SlotKind::Fixed { path, .. } if *path == editor.path),
                ) {
                    assert_eq!(
                        slot.fertilities,
                        editor.listing.fertilities.clone().unwrap(),
                        "{region}: fertilities"
                    );
                }
                let tree = anno_formats::adf::Adf::parse(bytes).unwrap();
                let root = tree.child_node(tree.root(), "MapTemplate").unwrap();
                // Only the TemplateElements under the root hold an Element.
                for element in tree.children(root) {
                    let Some(e) = tree.child_node(element, "Element") else {
                        continue;
                    };
                    if !tree
                        .child_value(e, "MapFilePath")
                        .is_some_and(|v| tree.read_utf16(v).unwrap() == editor.path)
                    {
                        continue;
                    }
                    let random = tree.child_value(e, "RandomizeFertilities").unwrap();
                    assert_eq!(tree.payload(random), &[0]);
                    let mapping = tree.child_node(e, "MineSlotMapping").unwrap();
                    let values: Vec<_> = tree.children(mapping).collect();
                    for (&id, &guid) in &editor.listing.slot_resources {
                        assert!(
                            values
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .any(|p| tree.payload(p[0]) == id.to_le_bytes()
                                    && tree.read_u32(p[1]).unwrap() == guid),
                            "{region}: slot assignment"
                        );
                    }
                }
            }
            let back = reopen(&install, &mut project, &editor.path);
            for (id, guid) in &slots {
                assert!(
                    (back.island.doc.objects.iter()).any(|o| o.id == *id && o.guid == *guid),
                    "{region}: slot reload"
                );
            }
            assert!(
                (back.island.doc.objects.iter()).any(|o| coast.contains(&o.guid)),
                "{region}: harbour reload"
            );
            if let Some(qa) = &mut qa {
                editor.stage(&install, qa).unwrap();
            }
        }
        if let Some(mut qa) = qa {
            let plans = crate::playtest::plan(&install, Some(&assets), &qa).unwrap();
            crate::playtest::apply(plans, &mut qa);
            eprintln!("1800 game QA mod: {}", qa.write().unwrap().display());
        }
    }

    #[test]
    fn installed_1800_island_edits_save_and_reload() {
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            MODERATE_M_01,
        )
        .unwrap();
        let before = editor.island.doc.clone();
        let at = [160., 160.];
        let old_height = editor.island.doc.field.height(at);
        let prop_count = editor.island.doc.props.len();
        editor.brush.radius = 4.;
        editor.brush.strength = 1.;
        let paint = if editor.island.doc.materials.as_ref().unwrap().dominant(at) == 1 {
            2
        } else {
            1
        };
        editor.paint = paint;
        editor.grid = Kind::River;
        editor.place = Some(editor.island.doc.props[0].source.file);
        editor.tint_color = tint::PRESETS[1].1;
        for tool in [
            Tool::Raise,
            Tool::Paint,
            Tool::Grid,
            Tool::Scatter,
            Tool::Tint,
        ] {
            editor.tool = tool;
            let y = editor.island.doc.field.height(at);
            assert!(editor.dab([at[0], y, at[1]], 0., false, None), "{tool:?}");
        }
        editor.history.checkpoint_owned(before);
        let new_height = editor.island.doc.field.height(at);
        assert!(new_height > old_height);
        assert!(editor.undo(false));
        assert_eq!(editor.island.doc.field.height(at), old_height);
        assert!(editor.undo(true));
        assert_eq!(editor.island.doc.field.height(at), new_height);
        let (_config, mut project) = staged(&install, &mut editor);
        let base = island_data_dir(MODERATE_M_01).unwrap();
        for dir in ["meshes", "normalmaps", "tintmaps/0"] {
            let dir = format!("{base}/{dir}/");
            assert!(project.files.keys().any(|p| p.starts_with(&dir)), "{dir}");
        }
        assert!(!project.files.keys().any(|p| p.contains("materialmaps/")));
        let info = anno_formats::a7m::a7minfo::A7mInfo::parse(
            &project.files[&format!("{MODERATE_M_01}info")],
        )
        .unwrap();
        assert!(matches!(
            info.slots,
            anno_formats::a7m::a7minfo::Slots::Typed(_)
        ));
        let back = reopen(&install, &mut project, MODERATE_M_01);
        assert!((back.island.doc.field.height(at) - new_height).abs() < 1. / 128.);
        assert!(back.island.doc.props.len() > prop_count);
        assert_eq!(
            back.island.doc.materials.as_ref().unwrap().dominant(at),
            paint
        );
        assert_eq!(
            grids::value_at(layer(&back.island.doc, Kind::River), at[0], at[1]),
            1
        );
    }

    // Needs Anno 1800: a new mine slot gets its mountain and cave work
    // areas; Road relays a shipped slot's rails; road points can be added,
    // dragged and removed while the end stays on the slot; the first point
    // moves the work area; Ctrl-click adds another work area.
    #[test]
    fn installed_1800_slot_roads_rebuild_and_edit() {
        use super::harbour;
        use anno_core::harbour::Piece;
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            MODERATE_M_01,
        )
        .unwrap();
        let assets = assets(&install);
        let lib = harbour_library(&install, &assets, &editor);
        editor.island.harbour = Some(lib.clone());
        editor.assets = Some(assets.clone());
        let ground = |a: [f32; 3], b: [f32; 3]| (a[0] - b[0]).hypot(a[2] - b[2]);
        let get = |e: &IslandEditor, id: u64| {
            (e.island.doc.objects.iter())
                .find(|o| o.id == id)
                .cloned()
                .unwrap()
        };
        let served = |e: &IslandEditor, slot: u64| {
            let mut kinds: Vec<_> = harbour::links(
                &e.island.doc.objects,
                |g| harbour::link_end(&assets, g),
                |id| id == slot,
            )
            .into_iter()
            .filter(|l| l.slot == slot)
            .map(|l| {
                (
                    harbour::piece_of(e.module(), &assets, get(e, l.work).guid),
                    l.work,
                )
            })
            .collect();
            kinds.sort_by_key(|k| format!("{:?}", k.0));
            kinds
        };

        // A new slot on the hilltop the shipped one stands on.
        let shipped = harbour::links(
            &editor.island.doc.objects,
            |g| harbour::link_end(&assets, g),
            |_| true,
        )
        .into_iter()
        .find(|l| {
            harbour::piece_of(editor.module(), &assets, get(&editor, l.work).guid)
                == Some(Piece::MineWorkArea)
        })
        .unwrap();
        let slot = get(&editor, shipped.slot).position;
        editor.island.resource = harbour::Resource::Mine;
        editor.island.fixed_slots = false;
        assert!(editor.push_slot([
            slot[0] + 30.,
            editor.island.doc.field.height([slot[0] + 30., slot[2]]),
            slot[2]
        ]));
        let (new_slot, _) = editor.selected_slot().unwrap();
        let kinds = served(&editor, new_slot);
        assert_eq!(
            kinds.iter().map(|k| k.0).collect::<Vec<_>>(),
            [Some(Piece::CaveWorkArea), Some(Piece::MineWorkArea)],
            "a new mine slot gets both work areas"
        );

        // Road on the shipped slot.
        editor.selection = [Item::Object(shipped.slot)].into();
        let count = editor.island.doc.objects.len();
        editor.rebuild_slot_roads();
        let kinds = served(&editor, shipped.slot);
        assert_eq!(kinds.len(), 2, "both kinds, the missing one added");
        assert!(editor.island.doc.objects.len() <= count + 1);
        for (kind, work) in &kinds {
            let w = get(&editor, *work);
            let span = if *kind == Some(Piece::CaveWorkArea) {
                5.3
            } else {
                11.4
            };
            assert!((ground(w.position, slot) - span).abs() < 1e-3, "{kind:?}");
            assert_eq!(w.path.unwrap().last().unwrap().p, slot);
        }

        let work = kinds
            .iter()
            .find(|k| k.0 == Some(Piece::MineWorkArea))
            .unwrap()
            .1;
        let points = |e: &IslandEditor| e.road_points(work);
        let n = points(&editor).len();

        // The edit mode opens on the selected work area's path; clicks on
        // open ground place nothing while it is on.
        editor.tool = super::Tool::Slot;
        editor.selection = [Item::Object(work)].into();
        editor.edit_selected_path();
        assert_eq!(editor.road_editing().map(|e| e.work), Some(work));
        editor.road_edit.as_mut().unwrap().picked = [1].into();
        assert_eq!(editor.road_pivot(), Some(points(&editor)[1]));

        // Add point splits the segment after the picked one and picks it.
        editor.add_road_points();
        let p = points(&editor);
        assert_eq!(p.len(), n + 1);
        assert_eq!(editor.road_editing().unwrap().picked, [2].into());
        let mid: [f32; 3] = std::array::from_fn(|k| (p[1][k] + p[3][k]) / 2.);
        assert_eq!(p[2], mid);

        // The gizmo moves picked points in 3D; heights stay as moved,
        // handles follow, and the last point stays on the slot.
        editor.set_road_points(work, |pts| {
            pts[2][1] += 3.;
            *pts.last_mut().unwrap() = [0.; 3];
        });
        let bent = get(&editor, work).path.unwrap();
        assert!((bent[2].p[1] - mid[1] - 3.).abs() < 1e-4);
        assert_eq!(bent.last().unwrap().p, slot, "the end stays on the slot");

        // Delete removes picked points but never the ends.
        editor.road_edit.as_mut().unwrap().picked = [0, 2].into();
        editor.remove_road_points();
        assert_eq!(points(&editor).len(), n, "the first point stays");
        assert!(editor.undo(false));
        assert_eq!(points(&editor).len(), n + 1);
        editor.road_edit = None;

        // Ctrl-click: a third work area for the shipped slot, linked to it.
        editor.selection = [Item::Object(shipped.slot)].into();
        editor.add_work_area([slot[0] + 6., 0., slot[2] - 6.]);
        assert_eq!(served(&editor, shipped.slot).len(), 3);
        // And deleting the slot takes all of them.
        editor.selection = [Item::Object(shipped.slot)].into();
        editor.delete_selected();
        assert!(served(&editor, shipped.slot).is_empty());
        assert!(
            kinds
                .iter()
                .all(|(_, w)| !editor.island.doc.objects.iter().any(|o| o.id == *w))
        );
    }

    // Needs Anno 1800: the Inspector's path preview offers the buildings
    // that lay a model on a mine slot's paths; Coal puts the rail on the
    // cave work area alone, None on neither, Auto on both.
    #[test]
    fn installed_1800_path_preview_picks_the_building() {
        use super::harbour;
        use anno_core::harbour::Piece;
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            MODERATE_M_01,
        )
        .unwrap();
        let assets = assets(&install);
        editor.assets = Some(assets.clone());
        let links = harbour::links(
            &editor.island.doc.objects,
            |g| harbour::link_end(&assets, g),
            |_| true,
        );
        let kind = |e: &IslandEditor, id: u64| {
            let o = e.island.doc.objects.iter().find(|o| o.id == id).unwrap();
            harbour::piece_of(e.module(), &assets, o.guid)
        };
        let slot = (links.iter())
            .find(|l| kind(&editor, l.work) == Some(Piece::CaveWorkArea))
            .unwrap()
            .slot;
        let (cave, mountain): (Vec<u64>, Vec<u64>) = (links.iter())
            .filter(|l| l.slot == slot)
            .map(|l| l.work)
            .partition(|&w| kind(&editor, w) == Some(Piece::CaveWorkArea));
        assert_eq!((cave.len(), mountain.len()), (1, 1));
        let railed = |e: &IslandEditor| -> Vec<u64> {
            let mut r: Vec<u64> = (e.rail_works().into_iter())
                .filter(|(w, cfg)| {
                    (cave[0] == *w || mountain[0] == *w) && cfg.contains("mining_rail_01")
                })
                .map(|(w, _)| w)
                .collect();
            r.sort();
            r
        };
        let mut both = vec![cave[0], mountain[0]];
        both.sort();
        assert_eq!(railed(&editor), both, "Auto: both paths");

        editor.selection = [Item::Object(slot)].into();
        let preview = editor.path_preview().unwrap();
        assert_eq!((preview.slot, preview.current), (slot, None));
        let buildings = preview.buildings;
        let name = |g: u32| {
            buildings
                .iter()
                .find(|(b, _)| *b == g)
                .map(|(_, n)| n.clone())
        };
        assert!(
            name(1010304).is_some() && name(1010305).is_some(),
            "{buildings:?}"
        );
        assert!(
            name(1010309).is_none(),
            "cement lays nothing: {buildings:?}"
        );

        editor.set_path_preview(slot, Some(1010304)); // Coal Mine
        assert_eq!(railed(&editor), cave);
        editor.set_path_preview(slot, Some(0));
        assert!(railed(&editor).is_empty());
        // A work area's selection reaches its slot's preview too.
        editor.selection = [Item::Object(mountain[0])].into();
        assert_eq!(editor.path_preview().unwrap().current, Some(0));
        editor.set_path_preview(slot, None);
        assert_eq!(railed(&editor), both);
    }

    // Needs the games: a work area's path keeps running from it to its slot
    // when either end moves (two-point 117 links and winding 1800 roads),
    // and deleting the slot takes its work area along.
    #[test]
    fn work_area_paths_follow_their_slot() {
        use super::{Field, harbour};
        let ground = |a: [f32; 3], b: [f32; 3]| (a[0] - b[0]).hypot(a[2] - b[2]);
        for (install, path) in [(install(), SMALL), (install_1800(), MODERATE_M_01)] {
            let Some(install) = install else { continue };
            let mut editor = IslandEditor::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                path,
            )
            .unwrap();
            let assets = assets(&install);
            editor.assets = Some(assets.clone());
            let all = harbour::links(
                &editor.island.doc.objects,
                |g| harbour::link_end(&assets, g),
                |_| true,
            );
            assert!(!all.is_empty(), "{path}: no work area linked to a slot");
            let (work, slot) = (all[0].work, all[0].slot);
            let find = |e: &IslandEditor, id: u64| {
                (e.island.doc.objects.iter()).find(|o| o.id == id).cloned()
            };
            let points = find(&editor, work).unwrap().path.unwrap().len();
            let start = find(&editor, work).unwrap().path.unwrap()[0].p;
            let offset = ground(start, find(&editor, work).unwrap().position);

            for moved in [slot, work] {
                editor.selection = [Item::Object(moved)].into();
                let x = find(&editor, moved).unwrap().position[0];
                editor.set_field(Field::Position(0), x + 4.);
                let (w, s) = (find(&editor, work).unwrap(), find(&editor, slot).unwrap());
                let curve = w.path.as_deref().unwrap();
                assert_eq!(curve.len(), points, "{path}: the road keeps its points");
                assert!(
                    ground(curve[points - 1].p, s.position) < 1e-3,
                    "{path}: ends on the slot"
                );
                // 1800 roads start ~0.6 m off their work area; that stays.
                assert!(
                    (ground(curve[0].p, w.position) - offset).abs() < 1e-3,
                    "{path}"
                );
            }

            editor.selection = [Item::Object(slot)].into();
            editor.delete_selected();
            assert!(
                find(&editor, slot).is_none() && find(&editor, work).is_none(),
                "{path}"
            );
        }
    }

    #[test]
    fn installed_1800_new_island_has_its_own_headers_and_pool_registration() {
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = new_island(&install, "moderate", 192, Start::Plateau, "Steamstead");
        let assets = assets(&install);
        editor.island.harbour = Some(harbour_library(&install, &assets, &editor));
        editor.assets = Some(assets);
        editor.harbour_at([96., 0., 25.], false);
        let coast = editor.coast_guids();
        assert!(
            editor
                .island
                .doc
                .objects
                .iter()
                .any(|o| coast.contains(&o.guid))
        );
        assert!(editor.push_slot([65., editor.island.doc.field.height([65., 70.]), 70.]));
        let slot = (editor.island.doc.objects.iter())
            .find(|o| editor.is_slot(o.guid))
            .unwrap();
        let slot_id = slot.id;
        let resource = editor
            .assets
            .as_ref()
            .unwrap()
            .get(slot.guid)
            .unwrap()
            .replacements[0];
        editor.selection = std::collections::BTreeSet::from([Item::Object(slot_id)]);
        editor.set_slot_resource(Some(resource));
        let river_guid = (editor.island.harbour.as_ref().unwrap())
            .guid(Piece::River)
            .unwrap();
        for at in [[72., 110.], [96., 110.], [120., 112.]] {
            editor.river_add(at);
        }
        editor.finish_river();
        assert!(
            !editor.message.as_ref().is_some_and(|(_, error)| *error),
            "{:?}",
            editor.message
        );
        assert!(
            editor
                .island
                .doc
                .objects
                .iter()
                .any(|o| o.guid == river_guid)
        );
        let (_config, mut project) = staged(&install, &mut editor);
        let folder = project.write().unwrap();
        let entry = project.islands[0].clone();
        assert_eq!(entry.region, "Moderate");
        assert!(
            entry
                .path
                .starts_with("data/sessions/islands/custom/moderate/")
        );
        let info = anno_formats::a7m::a7minfo::A7mInfo::parse(
            &project.files[&format!("{}info", entry.path)],
        )
        .unwrap();
        assert!(matches!(
            info.slots,
            anno_formats::a7m::a7minfo::Slots::Typed(_)
        ));
        assert!(info.source_map.contains("steamstead"));
        assert_eq!(info.default_session_guid, None);
        let assets_xml = |folder: &std::path::Path| {
            let schema = crate::games::module(anno_core::game::Game::Anno1800).database();
            std::fs::read_to_string(folder.join(schema.unwrap().assets_path)).unwrap()
        };
        let xml = assets_xml(&folder);
        assert!(xml.contains("GUID=\"140363\""));
        assert!(!xml.contains("IslandPlaceholderSize"));
        install.mount_mods(&[folder.as_path()], &["data"]).unwrap();
        assert!(
            IslandEditor::load(
                crate::games::module(crate::games::of(&install)),
                &install,
                &entry.path
            )
            .is_ok()
        );
        let plans = crate::playtest::plan(&install, editor.assets.as_deref(), &project).unwrap();
        assert_eq!(plans.len(), 3);
        assert!(plans.iter().all(|p| {
            p.slots
                .iter()
                .any(|s| matches!(s.fill, crate::playtest::Fill::Ours(0)))
        }));
        crate::playtest::apply(plans, &mut project);
        let xml = assets_xml(&project.write().unwrap());
        assert!(xml.contains("EnlargedTemplateFilename"));
        assert!(xml.contains("Archipelago/ValueName"));
        assert!(!xml.contains("<Template>MapTemplateType</Template>"));
        for map in &project.maps {
            let info = anno_formats::adf::Adf::parse(
                &project.files[&map.path.replace(".a7t", ".a7tinfo")],
            )
            .unwrap();
            let root = info.child_node(info.root(), "MapTemplate").unwrap();
            // Only the TemplateElements under the root hold an Element.
            let fixed = info
                .children(root)
                .find(|&n| {
                    info.child_node(n, "Element")
                        .and_then(|e| info.child_value(e, "MapFilePath"))
                        .is_some_and(|v| info.read_utf16(v).unwrap() == entry.path)
                })
                .unwrap();
            let element = info.child_node(fixed, "Element").unwrap();
            assert!(info.child_node(element, "MineSlotMapping").is_some());
            assert!(info.child_node(element, "MineSlotActivation").is_none());
        }
    }

    // Needs the game: one stroke each of Raise, Grid (river), Scatter and
    // Tint, a material paint through the level-0 maps and a moved object,
    // then a save. Re-reading the staged archive must show the river cells,
    // the new props and the object where it moved to, and the save must
    // carry regenerated terrain meshes, tint tiles and all four DDS levels
    // of the material maps, readable.
    #[test]
    fn island_edits_of_every_kind_survive_saving() {
        let Some((install, mut editor)) = open(PIRATE) else {
            return;
        };
        // Land in the middle of the island, away from the coast.
        let at = [160., 160.];
        let ground = |e: &IslandEditor| [at[0], e.island.doc.field.height(at), at[1]];
        let props_before = editor.island.doc.props.len();
        let objects_before = editor.island.doc.objects.len();
        assert!(objects_before > 1000, "{objects_before}");
        let before = editor.island.doc.clone();
        editor.brush.radius = 8.;
        editor.brush.strength = 1.;
        for tool in [Tool::Raise, Tool::Grid, Tool::Scatter, Tool::Tint] {
            editor.tool = tool;
            editor.grid = Kind::River;
            editor.place = Some(editor.island.doc.props[0].source.file);
            editor.tint_color = tint::PRESETS[1].1;
            assert!(
                editor.dab(ground(&editor), 0., false, None),
                "{tool:?} changed nothing"
            );
        }
        let m = (editor.island.doc.materials.as_mut()).expect("pirate_01 ships material maps");
        let brush = Brush {
            radius: 6.,
            strength: 1.,
            falloff: Falloff::Constant,
            ..Brush::default()
        };
        assert!(m.paint(at, &brush, 5));
        assert_eq!(m.dominant(at), 5);
        let item = Item::Object(editor.island.doc.objects[0].id);
        let mut m = editor.island.doc.matrix(item).unwrap();
        m[12] += 3.;
        editor.island.doc.set_matrix(item, m);
        let moved = editor.island.doc.objects[0].position;
        editor.history.checkpoint_owned(before);
        let added = editor.island.doc.props.len() - props_before;
        assert!(added > 0, "scatter added no props");
        let river_at = |g: &Grids| grids::value_at(g.layer(Kind::River).unwrap(), at[0], at[1]);
        assert_eq!(river_at(editor.island.doc.grids.as_ref().unwrap()), 1);

        let (_config, project) = staged(&install, &mut editor);
        let base = island_data_dir(PIRATE).unwrap();
        let staged_in = |dir: &str| {
            let dir = format!("{base}/{dir}/");
            project.files.keys().filter(|k| k.starts_with(&dir)).count()
        };
        assert!(staged_in("meshes") > 0, "no remeshed terrain tiles");
        assert!(staged_in("tintmaps") > 0, "no tint tiles");
        for level in 0..4 {
            for name in ["matid", "matalpha"] {
                let key = format!("{base}/materialmaps/{level}/{name}.dds");
                island::MaterialMap::parse(&project.files[&key]).unwrap();
            }
        }
        let island = saved(&project, PIRATE);
        assert_eq!(island.props().len(), props_before + added);
        assert_eq!(river_at(&Grids::read(&island).unwrap()), 1);
        let back = objects(&island);
        let o = back.iter().find(|o| Item::Object(o.id) == item).unwrap();
        assert!(
            (0..3).all(|i| (o.position[i] - moved[i]).abs() < 1e-3),
            "{:?} vs {moved:?}",
            o.position
        );
    }

    /// A plateau with a 28 m mesa, scaled to the nearest shipped template
    /// to 256 m. 117's templates at that size give the original 50° sides.
    fn mesa_island(install: &Install, region: &str, name: &str) -> IslandEditor {
        let skeleton = super::skeletons(
            crate::games::module(crate::games::of(&install)),
            install,
            None,
        )
        .into_iter()
        .filter(|s| s.region == region && (128..=384).contains(&s.world[0]))
        .min_by_key(|s| s.world[0].abs_diff(256))
        .unwrap();
        let mut editor = create(install, skeleton, Start::Plateau, name);
        let scale = editor.world()[0] as f32 / 256.;
        editor.replace_heights(|f| {
            for z in 0..f.grid.depth {
                for x in 0..f.grid.width {
                    let [wx, wz] = f.grid.world(x, z).map(|v| v / scale);
                    let inside = (wx - 72.).min(184. - wx).min(wz - 88.).min(168. - wz);
                    if inside > 0. {
                        let i = f.grid.index(x, z);
                        f.heights[i] = f.heights[i].max(4. + (inside * 1.2).min(24.));
                    }
                }
            }
            Ok("mesa".into())
        });
        editor
    }

    // Not a check: writes a mesa island with auto cliffs into the project
    // under $CLIFF_SAMPLE_DIR/anno-editor, for screenshots
    // (XDG_CONFIG_HOME=$CLIFF_SAMPLE_DIR anno-editor --open <path>).
    #[test]
    #[ignore]
    fn write_auto_cliff_sample() {
        let (Some(out), Some(install)) = (std::env::var_os("CLIFF_SAMPLE_DIR"), install()) else {
            eprintln!("skipping: ANNO117_DIR or CLIFF_SAMPLE_DIR unset");
            return;
        };
        let region = std::env::var("CLIFF_SAMPLE_REGION").unwrap_or("roman".into());
        let mut editor = mesa_island(&install, &region, "Cliff Sample");
        editor.auto_cliffs(&install);
        let mut project = Project::load(
            &std::path::Path::new(&out).join("anno-editor"),
            &anno117::MODULE,
        )
        .unwrap();
        editor.stage(&install, &mut project).unwrap();
        project.write().unwrap();
        println!("{}", editor.path);
    }

    #[test]
    fn installed_1800_automatic_tools_save_and_undo_in_each_region() {
        use super::{cliffs, harbour, sound};
        let Some(install) = install_1800() else {
            return;
        };
        let assets = assets(&install);
        let mut wanted = sound::guids(crate::games::rules(crate::games::of(&install))).to_vec();
        wanted.extend([
            crate::games::rules(crate::games::of(&install)).build_blocker,
            100961,
        ]);
        let records = Arc::new(
            harbour::find_records(
                crate::games::rules(crate::games::of(&install)),
                &install,
                &wanted,
            )
            .unwrap(),
        );
        for guid in &wanted {
            assert!(records.contains_key(guid), "missing record {guid}");
        }
        let mut export = std::env::var_os("ANNO_EDITOR_QA_EXPORT").map(|dir| {
            let mut p = Project::load(
                std::path::Path::new(&dir),
                crate::games::module(crate::games::of(&install)),
            )
            .unwrap();
            p.info.name = "Anno 1800 Automatic Tools QA".into();
            p.set_mod_id("anno-1800-automatic-tools-qa").unwrap();
            p
        });
        let blocker_count = |e: &IslandEditor| {
            (e.island.doc.objects.iter())
                .filter(|o| o.guid == e.rules().build_blocker)
                .count()
        };
        for region in ["moderate", "colony01", "arctic", "africa"] {
            let mut editor = mesa_island(&install, region, &format!("auto_tools_{region}"));
            assert_eq!(
                editor.island.doc.session.vegetation_prop_set,
                match region {
                    "moderate" => "Moderate",
                    "colony01" => "Colony",
                    "arctic" => "Arctic",
                    "africa" => "Colony02",
                    _ => unreachable!(),
                }
            );
            editor.assets = Some(assets.clone());
            editor.spot_records = Some(records.clone());
            let kit = cliffs::Kit::load(
                crate::games::rules(crate::games::of(&install)),
                &install,
                &editor.region(),
            )
            .unwrap();
            assert!((kit.shapes.iter()).all(|s| s.extent.iter().all(|e| *e > 0. && e.is_finite())));
            let rock = cliffs::ground_material(
                crate::games::rules(crate::games::of(&install)),
                &material_names(&editor),
            )
            .expect("regional rock material");
            editor.auto_cliffs(&install);
            let count = editor.island.doc.props.len();
            assert!(count > 5, "{region}: {count} cliffs");
            assert!(
                editor.auto_cliff_coverage(&kit) > 0.15,
                "{region}: cliff coverage"
            );
            let materials = editor.island.doc.materials.as_ref().unwrap();
            assert!(
                (editor.island.doc.props.iter())
                    .any(|p| materials.dominant([p.matrix[12], p.matrix[14]]) == rock)
            );
            editor.auto_cliffs(&install);
            assert_eq!(
                editor.island.doc.props.len(),
                count,
                "{region}: cliffs stacked"
            );
            assert!(editor.undo(false));
            assert_eq!(editor.island.doc.props.len(), count);

            // A rock on the flat plateau requires blockers; the automatic
            // cliff pieces lie on unbuildable slopes.
            let file = editor.island.doc.mesh_index(&kit.shapes[2].cfg);
            editor.place = Some(file);
            let scale = editor.world()[0] as f32 / 256.;
            let x = 128. * scale;
            let z = 70. * scale;
            editor.place_at([x, editor.island.doc.field.height([x, z]), z]);
            editor.choose_object(records[&100961].clone(), None);
            let z = 82. * scale;
            editor.place_at([x, editor.island.doc.field.height([x, z]), z]);
            let before_sound = editor.island.doc.objects.len();
            editor.auto_sound_spots();
            let sounds = editor.island.doc.objects.len() - before_sound;
            assert!(sounds >= 3, "{region}: sound emitters");
            let placed_sound: Vec<_> = (editor.island.doc.objects.iter())
                .filter(|o| {
                    sound::guids(crate::games::rules(crate::games::of(&install))).contains(&o.guid)
                })
                .map(|o| (o.guid, o.position))
                .collect();
            let revision = editor.history.revision();
            editor.auto_sound_spots();
            assert_eq!(
                editor.island.doc.objects.len(),
                before_sound + sounds,
                "{region}: sounds stacked"
            );
            assert_eq!(
                editor.history.revision(),
                revision,
                "{region}: identical sounds added undo"
            );
            assert!(editor.undo(false));
            assert_eq!(editor.island.doc.objects.len(), before_sound);
            assert!(editor.undo(true));
            editor.auto_blockers(&install);
            let blockers = blocker_count(&editor);
            assert!(blockers > 0, "{region}: blockers");
            let revision = editor.history.revision();
            editor.auto_blockers(&install);
            assert_eq!(blocker_count(&editor), blockers);
            assert_eq!(
                editor.history.revision(),
                revision,
                "{region}: identical blockers added undo"
            );
            assert!(editor.undo(false));
            assert_eq!(blocker_count(&editor), 0);
            assert!(editor.undo(true));
            assert!(editor.spot_marks().iter().any(|m| m.1 > 0.));
            let (_config, mut project) = staged(&install, &mut editor);
            let mut reopened = reopen(&install, &mut project, &editor.path);
            reopened.adopt(&project.islands[0]);
            assert_eq!(
                reopened
                    .island
                    .doc
                    .props
                    .iter()
                    .filter(|p| p.auto_cliff)
                    .count(),
                count
            );
            assert_eq!(blocker_count(&reopened), blockers);
            for (guid, position) in placed_sound {
                assert!(
                    (reopened.island.doc.objects.iter())
                        .any(|o| o.guid == guid && o.position == position),
                    "{region}: sound reload"
                );
            }
            assert!(editor.undo(false));
            assert_eq!(blocker_count(&editor), 0);
            if let Some(p) = &mut export {
                assert!(editor.undo(true));
                editor.stage(&install, p).unwrap();
            }
        }
        if let Some(p) = export {
            p.write().unwrap();
        }
    }

    // Needs the game: on a new island with a sculpted mesa (50° sides),
    // Auto cliffs puts the region's cliff set on the sides only, covers a
    // shipped share of the steep ground, reruns without stacking, saves
    // (the listing remembers which props it placed) and undoes in one step.
    #[test]
    fn auto_cliffs_dress_steep_faces_only() {
        use super::cliffs;
        let Some(install) = install() else {
            return;
        };
        for region in ["roman", "celtic"] {
            let mut editor = mesa_island(&install, region, "Cliff Test");
            let kit = cliffs::Kit::load(
                crate::games::rules(crate::games::of(&install)),
                &install,
                &editor.region(),
            )
            .unwrap();
            let rock = cliffs::ground_material(editor.rules(), &material_names(&editor))
                .expect("no cliff material");
            let ground = |e: &IslandEditor, p: [f32; 2]| {
                e.island.doc.materials.as_ref().unwrap().dominant(p)
            };
            // Plateau top and the shore below the mesa are flat.
            let flat = [[128., 128.], [128., 60.]];
            let before = flat.map(|p| ground(&editor, p));
            editor.auto_cliffs(&install);
            let auto: Vec<_> = editor
                .island
                .doc
                .props
                .iter()
                .filter(|p| p.auto_cliff)
                .collect();
            assert!(auto.len() >= 8, "{region}: {} pieces", auto.len());
            let centers: Vec<[f32; 2]> = (auto.iter())
                .map(|p| {
                    let [x, _, z] = super::math::translation(&p.matrix);
                    [x, z]
                })
                .collect();
            let on_rock =
                |e: &IslandEditor| centers.iter().filter(|&&c| ground(e, c) == rock).count();
            let rocky = on_rock(&editor);
            assert!(
                rocky * 10 >= centers.len() * 8,
                "{region}: {rocky} of {} on rock",
                centers.len()
            );
            assert_eq!(
                flat.map(|p| ground(&editor, p)),
                before,
                "{region}: flat ground painted"
            );
            // Paint and pieces are one undo step.
            assert!(editor.undo(false));
            assert!(!editor.island.doc.props.iter().any(|p| p.auto_cliff));
            let still = on_rock(&editor);
            assert!(
                still * 10 < centers.len(),
                "{region}: undo kept {still} rock"
            );
            editor.auto_cliffs(&install);
            let auto: Vec<_> = editor
                .island
                .doc
                .props
                .iter()
                .filter(|p| p.auto_cliff)
                .collect();
            let field = &editor.island.doc.field;
            for p in &auto {
                let [x, y, z] = super::math::translation(&p.matrix);
                let h = |dx: f32, dz: f32| field.height([x + dx, z + dz]);
                let slope = ((h(1., 0.) - h(-1., 0.)) / 2.)
                    .hypot((h(0., 1.) - h(0., -1.)) / 2.)
                    .atan()
                    .to_degrees();
                assert!(slope > 34., "{region}: piece on {slope:.0}° ground");
                assert!(h(0., 0.) >= 0.5, "{region}: piece under water");
                assert!(y < h(0., 0.), "{region}: origin above the ground");
                let cfg = &editor.island.doc.meshes[p.source.file as usize];
                assert!(cfg.contains(&format!("/{region}/")), "{region}: {cfg}");
            }
            let share = editor.auto_cliff_coverage(&kit);
            eprintln!("{region}: {} pieces cover {share:.2}", auto.len());
            assert!((0.33..=0.8).contains(&share), "{region}: covers {share}");

            // Rerun: replacing gives the same set, filling gaps adds little.
            let count = auto.len();
            editor.auto_cliffs(&install);
            let now = editor
                .island
                .doc
                .props
                .iter()
                .filter(|p| p.auto_cliff)
                .count();
            assert_eq!(now, count, "{region}: replace changed the count");
            editor.cliffs.replace = false;
            editor.auto_cliffs(&install);
            let more = editor.island.doc.props.len() - count;
            assert!(more * 4 <= count, "{region}: {more} stacked on {count}");
            // Nothing to fill makes no undo step.
            if more > 0 {
                assert!(editor.undo(false));
            }
            editor.cliffs.replace = true;

            if region == "roman" {
                let (_config, project) = staged(&install, &mut editor);
                let island = saved(&project, &editor.path);
                let walls = (island.props().iter())
                    .filter(|p| island.prop_files()[p.file as usize].contains("roman_cliff_01"))
                    .count();
                assert!(walls > 0, "no cliff walls saved");
                assert_eq!(island.props().len(), count);
                let entry = &project.islands[0];
                assert_eq!(entry.listing.auto_cliffs.len(), count);
                // Reopening a project island takes its listing back.
                for p in editor.island.doc.props.iter_mut() {
                    p.auto_cliff = false;
                }
                editor.adopt(entry);
                let known = editor
                    .island
                    .doc
                    .props
                    .iter()
                    .filter(|p| p.auto_cliff)
                    .count();
                assert_eq!(known, count, "listing lost track of auto pieces");
            }

            // Undo steps back over the replacing rerun, then takes the
            // pieces away; the mesa stays.
            assert!(editor.undo(false));
            assert_eq!(editor.island.doc.props.len(), count);
            assert!(editor.undo(false));
            assert!(editor.island.doc.props.iter().all(|p| !p.auto_cliff));
            assert!(editor.island.doc.field.height([128., 128.]) > 27.);
        }
    }

    // Needs the game: a flow brush stroke eastward combs WaterFlow east, and
    // the saved rd3d carries it; Shift calms it again.
    #[test]
    fn flow_brush_combs_along_the_stroke_and_saves() {
        let Some((install, mut editor)) = open(SMALL) else {
            return;
        };
        editor.tool = Tool::Grid;
        editor.grid = Kind::WaterFlow;
        editor.brush = Brush {
            radius: 6.,
            strength: 1.,
            falloff: Falloff::Constant,
            ..Brush::default()
        };
        let before = editor.island.doc.clone();
        let (a, b) = ([100., 0., 120.], [104., 0., 120.]);
        assert!(
            !editor.dab(a, 0., false, None),
            "a lone dab has no direction"
        );
        assert!(editor.dab(b, 0., false, None));
        editor.history.checkpoint_owned(before);
        let flow = |g: &Grids| grids::value_at(g.layer(Kind::WaterFlow).unwrap(), b[0], b[2]);
        let [x, z] = grids::flow(flow(editor.island.doc.grids.as_ref().unwrap()));
        assert!(x > 15. && z.abs() < 1., "east: {x} {z}");

        let (_config, project) = staged(&install, &mut editor);
        let word = flow(&Grids::read(&saved(&project, SMALL)).unwrap());
        assert!(grids::flow(word)[0] > 15., "saved {word:#x}");

        editor.flow_from = None;
        assert!(
            editor.dab(b, 0., true, None),
            "Shift calms without a direction"
        );
        assert_eq!(
            grids::flow(flow(editor.island.doc.grids.as_ref().unwrap())),
            [0., 0.]
        );
    }

    // Every shipped island of both games, with props, objects and the
    // vegetation preview, must pack a visible entry's row and material into
    // 32 bits; past that the renderer draws pieces one by one, several times
    // slower. Loads every island's models, so ignored: `ANNO117_DIR=…
    // ANNO1800_DIR=… cargo test --release placement_entries_pack --
    // --ignored --nocapture`. ISLAND_FILTER narrows to paths containing it.
    #[test]
    #[ignore]
    fn placement_entries_pack_on_every_shipped_island() {
        let filter = std::env::var("ISLAND_FILTER").unwrap_or_default();
        let mut worst = Vec::new();
        for install in [install(), install_1800()].into_iter().flatten() {
            let install = Arc::new(install);
            let assets = assets(&install);
            let mut islands: Vec<String> = (install.search(".a7m", 100_000).into_iter())
                .map(|id| install.file_path(id).to_owned())
                .filter(|p| p.ends_with(".a7m") && p.contains("/islands/") && p.contains(&filter))
                .collect();
            islands.sort();
            for path in islands {
                let shared = Some((install.clone(), assets.clone()));
                let mut editor = match IslandEditor::load_with(
                    crate::games::module(crate::games::of(&install)),
                    &install,
                    &path,
                    shared,
                ) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("skip {path}: {e:#}");
                        continue;
                    }
                };
                editor.layers.vegetation = true;
                editor.refresh_scene();
                while let Some(job) = editor.models_job(install.clone()) {
                    editor.models_loaded(job());
                }
                editor.refresh_scene();
                let c = editor.renderer.placement_counts();
                eprintln!(
                    "{:2} bits  {:6} rows {:5} materials {:6} draws  {path}",
                    c.bits(),
                    c.rows,
                    c.materials,
                    c.draws
                );
                worst.push((c.bits(), c, path));
            }
        }
        worst.sort_by_key(|(bits, c, _)| std::cmp::Reverse((*bits, c.rows)));
        eprintln!("worst:");
        for (bits, c, path) in worst.iter().take(10) {
            eprintln!("{bits:2} bits  {c:?}  {path}");
        }
        let over: Vec<_> = worst.iter().filter(|(bits, _, _)| *bits > 32).collect();
        assert!(over.is_empty(), "past 32 bits: {over:#?}");
    }

    // Timing harness, not a pass/fail test: the UI thread's cost of an
    // island's first scene and of a full rebuild after, fastest of five, so
    // a loaded machine still compares. ANNO117_DIR and ANNO1800_DIR.
    #[test]
    #[ignore]
    fn scene_build_timing() {
        use std::time::{Duration, Instant};
        let islands = [
            (super::test_support::install(), PIRATE),
            (
                super::test_support::install_1800(),
                "data/dlc01/sessions/islands/pool/moderate_c_01/moderate_c_01.a7m",
            ),
        ];
        for (install, path) in islands {
            let Some(install) = install.map(Arc::new) else {
                continue;
            };
            let assets = assets(&install);
            let (mut first, mut again) = (Duration::MAX, Duration::MAX);
            let (mut off, mut land) = (Duration::MAX, Duration::MAX);
            for _ in 0..5 {
                let mut editor = IslandEditor::load_with(
                    crate::games::module(crate::games::of(&install)),
                    &install,
                    path,
                    Some((install.clone(), assets.clone())),
                )
                .unwrap();
                while let Some(job) = editor.models_job(install.clone()) {
                    editor.models_loaded(job());
                }
                // As the app runs it: rows and river surfaces build off
                // the UI thread, timed apart.
                editor.background_scene = true;
                let jobs = |editor: &mut IslandEditor| {
                    let t = Instant::now();
                    while let Some(job) = editor.placement_job() {
                        editor.placement_built(job());
                    }
                    while let Some(job) = editor.inland_job() {
                        editor.inland_built(job());
                    }
                    while let Some(job) = editor.rails_job() {
                        editor.rails_built(job());
                    }
                    let took = t.elapsed();
                    let t = Instant::now();
                    editor.refresh_scene();
                    (took, t.elapsed())
                };
                let t = Instant::now();
                editor.refresh_scene();
                first = first.min(t.elapsed());
                let (built, landed) = jobs(&mut editor);
                (off, land) = (off.min(built), land.min(landed));
                editor.scene_stale = true;
                let t = Instant::now();
                editor.refresh_scene();
                again = again.min(t.elapsed());
                jobs(&mut editor);
            }
            eprintln!(
                "{path}: UI thread: first scene {first:?}, full rebuild {again:?}, \
                 taking the built rows {land:?}; off it: rows, rivers and rails {off:?}"
            );
        }
    }

    // Sky, cloud and haze views to compare with the game, not a pass/fail
    // test: `ANNO117_DIR=… SKY_SHOTS=DIR cargo test --release sky_shots --
    // --ignored --nocapture`. Heights and pitch are the game's camera.xml
    // presets: 60 is the default, 175 the highest before the world map.
    // SKY_MOTION=1 (or a comma list of its sequences) shoots the camera
    // moving instead, and the clouds' march time (sky_motion).
    #[test]
    #[ignore]
    fn sky_shots() {
        let Some(dir) = std::env::var_os("SKY_SHOTS") else {
            eprintln!("skipping: SKY_SHOTS unset");
            return;
        };
        let install = Arc::new(install().expect("ANNO117_DIR"));
        // SKY_MOD: a mod folder to mount, for an island a mod ships.
        if let Some(dir) = std::env::var_os("SKY_MOD") {
            install
                .mount_mods(&[std::path::Path::new(&dir)], &["data"])
                .expect("SKY_MOD");
        }
        let path = std::env::var("PROFILE_ISLAND").unwrap_or_else(|_| PIRATE.into());
        let assets = assets(&install);
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            &path,
            Some((install.clone(), assets)),
        )
        .unwrap();
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
        }
        // SKY_RELIEF=0: the ground without its material relief.
        if std::env::var("SKY_RELIEF").is_ok_and(|v| v == "0") {
            editor.layers.relief = false;
        }
        // SKY_AMBIENT: another preset than the island's own.
        if let Ok(name) = std::env::var("SKY_AMBIENT") {
            editor.edit_session(|s| s.global_ambient = name);
        }
        eprintln!("ambient {}", editor.island.doc.session.global_ambient);
        let (w, h) = (1920u32, 1080u32);
        let mut target = gpui_editor_kit::gpu::Offscreen::new(w, h).unwrap();
        let renderer = editor.renderer.clone();
        // SKY_MOTION=1: sequences instead, the camera panning, turning and
        // zooming in small steps, to see that clouds and haze hold still in
        // the world (motion_<kind>_<n>.png).
        if std::env::var_os("SKY_MOTION").is_some() {
            sky_motion(&mut editor, &mut target, std::path::Path::new(&dir));
            return;
        }
        // SKY_VIEWS: a comma list of the views below to shoot.
        let only = std::env::var("SKY_VIEWS").unwrap_or_default();
        // (name, hours, eye height, pitch below the horizon); "fit" keeps
        // the camera the island opens with.
        for (name, hours, height, pitch) in [
            ("fit", 12., 0., 0.),
            ("noon_60", 12., 60., 0.8),
            // The game's widest zoom with the pitch lowered to see the
            // whole island.
            ("game_max", 12., 150., 0.4),
            // As retail screenshots frame a large island: the game's lens
            // (camera.xml's Fov 0.7), 115 up, looking 0.45 down.
            ("retail", 12., 115., 0.45),
            ("noon_175", 12., 175., 0.8),
            ("noon_600", 12., 600., 0.8),
            ("noon_2000", 12., 2000., 0.8),
            ("noon_horizon", 12., 40., 0.15),
            ("evening_horizon", 17.3, 40., 0.15),
            ("evening_175", 17.3, 175., 0.8),
            ("night_horizon", 0., 40., 0.15),
            // Zoomed out over the game's ceiling toward the horizon: under
            // the clouds' base (190), inside the layer, and above it.
            ("under_clouds", 12., 185., 0.2),
            ("in_clouds", 12., 280., 0.2),
            ("over_clouds", 12., 550., 0.35),
            // Raised over the island at dusk, the far sea's edge in view.
            ("dusk_high", 18., 400., 0.3),
            // SKY_CAMERA's own view, set below.
            ("camera", 12., 0., 0.),
        ] {
            if !only.is_empty() && !only.split(',').any(|k| k == name) {
                continue;
            }
            // SKY_CAMERA="height,pitch,turn,fov[,hours]" (radians; turn is
            // added to the island's own yaw, fov 0 keeps the editor's): to
            // line a view up with a game screenshot.
            let custom: Vec<f32> = std::env::var("SKY_CAMERA")
                .unwrap_or_default()
                .split(',')
                .filter_map(|v| v.trim().parse().ok())
                .collect();
            if name == "camera" && custom.len() < 4 {
                continue;
            }
            let (hours, height, pitch) = if name == "camera" {
                (
                    custom.get(4).copied().unwrap_or(hours),
                    custom[0],
                    custom[1],
                )
            } else {
                (hours, height, pitch)
            };
            editor.time_of_day = hours;
            editor.sky = None;
            while let Some(job) = editor.sky_job() {
                editor.sky_baked(job());
            }
            editor.scene_stale = true;
            editor.refresh_scene();
            // Rivers and lakes, as the view's background work builds them
            // after the first scene asks.
            while let Some(job) = editor.inland_job() {
                editor.inland_built(job());
                editor.refresh_scene();
            }
            let extent = editor.extent();
            let mut camera = editor.views[0].camera;
            if name == "retail" {
                camera.fov = 0.7;
            }
            if name == "camera" {
                camera.yaw += custom[2];
                if custom[3] > 0. {
                    camera.fov = custom[3];
                }
            }
            if height > 0. {
                camera.tilt = std::f32::consts::FRAC_PI_2 - pitch;
                let at = camera.distance(extent).expect("perspective");
                camera.zoom *= at / (height / pitch.sin());
            }
            // Pipelines and tables come up over the first frames.
            let mut shot = Vec::new();
            for _ in 0..8 {
                shot = target.render(&renderer, camera, extent).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            for _ in 0..10 {
                target.time_frame(&renderer).unwrap();
            }
            let stats = renderer.stats();
            let passes: Vec<String> = (stats.passes.iter())
                .filter(|(n, _)| !n.is_empty())
                .map(|(n, ms)| format!("{n} {ms:.2}"))
                .collect();
            eprintln!("{name}: [{}]", passes.join(", "));
            image::RgbaImage::from_raw(w, h, shot)
                .unwrap()
                .save(std::path::Path::new(&dir).join(format!("{name}.png")))
                .unwrap();
            // What a moving eye costs: 117 bakes its clouds again for each
            // new eye. Read back alike, so the difference is the bake.
            if name == "noon_horizon" {
                let mut timed = |turn: f32| {
                    let mut c = camera;
                    let mut best = std::time::Duration::MAX;
                    for i in 0..12 {
                        c.yaw = camera.yaw + turn * i as f32;
                        let t = std::time::Instant::now();
                        target.render(&renderer, c, extent).unwrap();
                        best = best.min(t.elapsed());
                    }
                    best
                };
                let (still, moving) = (timed(0.), timed(0.05));
                eprintln!("frame still {still:?}, eye moving {moving:?}");
            }
        }
    }

    /// sky_shots' SKY_MOTION sequences at noon: a pan along x, a turn about
    /// the pivot and a zoom out, eight frames each.
    fn sky_motion(
        editor: &mut IslandEditor,
        target: &mut gpui_editor_kit::gpu::Offscreen,
        dir: &std::path::Path,
    ) {
        editor.time_of_day = 12.;
        editor.sky = None;
        while let Some(job) = editor.sky_job() {
            editor.sky_baked(job());
        }
        editor.scene_stale = true;
        editor.refresh_scene();
        let renderer = editor.renderer.clone();
        let extent = editor.extent();
        let base = editor.views[0].camera;
        let pivot = extent.center;
        // The camera `height` up, `pitch` below the horizon, its pivot moved
        // `along` x and turned `yaw`.
        let at = |height: f32, pitch: f32, along: f32, yaw: f32| {
            let mut c = base;
            c.center = Some([pivot[0] + along, 0., pivot[2]]);
            c.pan = [0.; 2];
            c.tilt = std::f32::consts::FRAC_PI_2 - pitch;
            c.yaw = base.yaw + yaw;
            let d = c.distance(extent).expect("perspective");
            c.zoom *= d / (height / pitch.sin());
            c
        };
        let shot = |target: &mut gpui_editor_kit::gpu::Offscreen, c, name: String| {
            let mut pixels = Vec::new();
            for _ in 0..4 {
                pixels = target.render(&renderer, c, extent).unwrap();
            }
            let (w, h) = (1920, 1080);
            image::RgbaImage::from_raw(w, h, pixels)
                .unwrap()
                .save(dir.join(format!("{name}.png")))
                .unwrap();
        };
        // Pipelines and tables come up over the first frames.
        for _ in 0..8 {
            target
                .render(&renderer, at(60., 0.1, 0., 0.), extent)
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        let steps = |f: &dyn Fn(f32) -> gpui_editor_kit::camera::Camera| -> Vec<_> {
            (0..8).map(|i| f(i as f32)).collect()
        };
        let sequences = [
            // Low, looking along the sea: the game's own views.
            ("pan", steps(&|i| at(60., 0.1, i * 12., 0.))),
            ("yaw", steps(&|i| at(60., 0.1, 0., i * 0.08))),
            // Zoomed out past the game's camera, as the editor usually is.
            ("high_pan", steps(&|i| at(600., 0.5, i * 40., 0.))),
            ("high_yaw", steps(&|i| at(600., 0.5, 0., i * 0.15))),
            // Zoomed out and tilted up enough to see the sky.
            ("orbit", steps(&|i| at(400., 0.3, 0., i * 0.2))),
            (
                "out",
                [100., 175., 250., 350., 500., 700., 1000., 1500.]
                    .map(|h| at(h, 0.25, 0., 0.))
                    .to_vec(),
            ),
            ("tilt", steps(&|i| at(300., 0.2 + i * 0.12, 0., 0.))),
            (
                "zoom",
                [40., 60., 100., 140., 175., 250., 400., 800.]
                    .map(|h| at(h, 0.35, 0., 0.))
                    .to_vec(),
            ),
        ];
        let only = std::env::var("SKY_MOTION").unwrap_or_default();
        for (kind, cameras) in sequences {
            if !only.is_empty() && only != "1" && !only.split(',').any(|k| k == kind) {
                continue;
            }
            for (i, c) in cameras.into_iter().enumerate() {
                shot(target, c, format!("motion_{kind}_{i}"));
            }
        }
        // A steady climb through the cloud layer, one frame a step as a
        // zoom draws it: every eighth frame is shot, to see the clouds hold
        // smooth while the view moves (glide_<n>.png).
        if only.is_empty() || only == "1" || only.split(',').any(|k| k == "glide") {
            for i in 0..64 {
                let c = at(150. + i as f32 * 5., 0.15, 0., i as f32 * 0.004);
                let pixels = target.render(&renderer, c, extent).unwrap();
                if i % 8 == 7 {
                    image::RgbaImage::from_raw(1920, 1080, pixels)
                        .unwrap()
                        .save(dir.join(format!("glide_{}.png", i / 8)))
                        .unwrap();
                }
            }
        }
        // What a moving view costs: the clouds march again for each, timed
        // on the GPU (the "clouds" pass). A turn too small to see still
        // moves the view.
        for (name, height, pitch) in [("low", 60., 0.1), ("high", 400., 0.3)] {
            let mut ms = Vec::new();
            for i in 0..24 {
                let c = at(height, pitch, 0., 1e-4 * i as f32);
                target.render(&renderer, c, extent).unwrap();
                let stats = renderer.stats();
                ms.extend(
                    (stats.passes.iter())
                        .filter(|(n, _)| *n == "clouds")
                        .map(|p| p.1),
                );
            }
            ms.sort_by(f32::total_cmp);
            let median = ms.get(ms.len() / 2).copied().unwrap_or(0.);
            eprintln!(
                "{name}: cloud march {median:.3} ms (median of {})",
                ms.len()
            );
        }
        eprintln!("{:?}", renderer.stats().passes);
    }

    // Timing harness, not a pass/fail test: `ANNO117_DIR=… cargo test --release
    // profile_island -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn profile_island() {
        use std::time::Instant;
        let t = Instant::now();
        let install = Arc::new(install().expect("ANNO117_DIR"));
        eprintln!("install index {:?}", t.elapsed());
        // PROFILE_ISLAND picks another island, e.g. a 512 m one.
        let path = std::env::var("PROFILE_ISLAND").unwrap_or_else(|_| PIRATE.into());
        let path = path.as_str();
        // Objects (buildings) draw from the asset registry, which the app
        // loads at startup; the models load alongside the island, as there.
        let t = Instant::now();
        let assets = assets(&install);
        eprintln!("assets {:?}", t.elapsed());
        let t = Instant::now();
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
            Some((install.clone(), assets)),
        )
        .unwrap();
        eprintln!("load {:?}", t.elapsed());
        for line in &editor.load_log {
            eprintln!("   {line}");
        }
        let t = Instant::now();
        editor.refresh_scene();
        eprintln!("first refresh_scene {:?}", t.elapsed());
        // Models, as the editor streams them.
        let t = Instant::now();
        let mut batches = 0;
        while let Some(job) = editor.models_job(install.clone()) {
            editor.models_loaded(job());
            batches += 1;
        }
        eprintln!(
            "models: {batches} batches in {:?}, {} loaded",
            t.elapsed(),
            editor.library.models.loaded.len()
        );
        // The sky, as the editor bakes it in the background.
        let t = Instant::now();
        while let Some(job) = editor.sky_job() {
            editor.sky_baked(job());
        }
        eprintln!("sky {:?}", t.elapsed());
        // Rivers and lakes, as the editor builds them in the background.
        let t = Instant::now();
        while let Some(job) = editor.inland_job() {
            editor.inland_built(job());
        }
        eprintln!("rivers and lakes {:?}", t.elapsed());
        let t = Instant::now();
        editor.refresh_scene();
        eprintln!("refresh_scene with models {:?}", t.elapsed());
        // What the GPU is asked to draw, and how long frames take.
        let tris = |m: &super::Mesh| m.triangles() as u64;
        let terrain: u64 = editor.tiles.iter().map(|m| tris(m)).sum();
        eprintln!(
            "scene: {}; {} placement rows of {} models",
            if editor.terrain.is_some() {
                "GPU terrain".to_owned()
            } else {
                format!("terrain {terrain} tris in {} CPU tiles", editor.tiles.len())
            },
            editor.placements.rows(),
            editor.placements.models()
        );
        // PROFILE_SIZE=WxH, e.g. the viewport of a window on a 5K screen
        // (3960x2065); 1080p by default.
        let [w, h] = std::env::var("PROFILE_SIZE")
            .ok()
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some([w.parse().ok()?, h.parse().ok()?])
            })
            .unwrap_or([1920u32, 1080]);
        // PROFILE_SCALE: the window's scale factor (2 on a 5K screen), which
        // the renderer's screen-size choices go by.
        let scale = std::env::var("PROFILE_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.);
        let mut target = gpui_editor_kit::gpu::Offscreen::new(w, h)
            .unwrap()
            .with_scale(scale);
        let renderer = editor.renderer.clone();
        for (label, zoom, props, shadows) in [
            ("overview", 1., true, true),
            ("close", 6., true, true),
            ("low oblique", 3., true, true),
            ("overview, no shadows", 1., true, false),
            ("overview, no props", 1., false, true),
            ("overview, neither", 1., false, false),
            ("low, no shadows", 3., true, false),
            ("low, no props", 3., false, true),
            ("overview, neither, height shading", 1., false, false),
            ("overview, neither, no water", 1., false, false),
        ] {
            if label.ends_with("height shading") {
                editor.shading = super::Shading::Height;
                editor.rebuild_tiles(None);
            }
            if label.ends_with("no water") {
                editor.layers.water = false;
            }
            editor.layers.props = props;
            editor.layers.shadows = shadows;
            editor.scene_stale = true;
            editor.refresh_scene();
            let mut camera = editor.views[0].camera;
            camera.zoom *= zoom;
            // Looking across the island from near the ground, as when
            // editing: hills hide much of what lies behind them.
            if label.starts_with("low") {
                camera.tilt = 1.35;
            }
            let t = Instant::now();
            let shot = target.render(&renderer, camera, editor.extent()).unwrap();
            let first = t.elapsed();
            // PROFILE_SHOTS=DIR saves each view's first frame, to compare
            // renderer changes pixel for pixel.
            if let Some(dir) = std::env::var_os("PROFILE_SHOTS") {
                let name = label.replace([' ', ','], "_");
                image::RgbaImage::from_raw(w, h, shot)
                    .unwrap()
                    .save(std::path::Path::new(&dir).join(format!("{name}.png")))
                    .unwrap();
            }
            // GPU clocks and other work on the machine make single frames
            // noisy: warm up, keep each frame's passes and report the
            // fastest frame.
            for _ in 0..30 {
                target.time_frame(&renderer).unwrap();
            }
            let mut gpu = Vec::new();
            let mut times: Vec<_> = (0..20)
                .map(|_| {
                    let t = target.time_frame(&renderer).unwrap();
                    let stats = renderer.stats();
                    gpu.push((stats.passes, stats.gpu_span_ms));
                    t
                })
                .collect();
            times.sort();
            let total = |p: &[(&str, f32)]| p.iter().map(|(_, ms)| ms).sum::<f32>();
            let (best, span) = (gpu.iter())
                .min_by(|a, b| total(&a.0[..]).total_cmp(&total(&b.0[..])))
                .expect("20 frames");
            let passes: Vec<String> = (best.iter())
                .filter(|(name, _)| !name.is_empty())
                .map(|(name, ms)| format!("{name} {ms:.2}"))
                .collect();
            eprintln!(
                "frame {label}: first (uploads) {first:?}, then median {:?} (min {:?}), GPU {:.2} ms, {span:.2} first to last [{}]; drew {:?}",
                times[10],
                times[0],
                total(&best[..]),
                passes.join(", "),
                renderer.drawn()
            );
        }
        editor.layers = super::Layers::default();
        editor.scene_stale = true;
        // A 40-dab raise stroke across the island, refreshing like frames do.
        for (tool, label) in [(Tool::Raise, "raise"), (Tool::Paint, "paint")] {
            editor.tool = tool;
            let (mut dabs, mut scene) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
            for i in 0..40 {
                let at = [100. + i as f32 * 3., 0., 160.];
                let t = Instant::now();
                editor.dab(at, 5., false, None);
                dabs += t.elapsed();
                let t = Instant::now();
                editor.refresh_scene();
                scene += t.elapsed();
            }
            eprintln!("{label}: 40 dabs {dabs:?}, 40 refreshes {scene:?}");
        }
        // A short raise stroke, then its release as the app handles it: the
        // land grids follow, then the next scene refresh and frame.
        {
            editor.sync_land();
            editor.tool = Tool::Raise;
            let at = [300., editor.island.doc.field.height([300., 300.]), 300.];
            let mut drag = super::Drag::Brush {
                stroke: super::Stroke::default(),
                before: Box::new(editor.island.doc.clone()),
                level: at[1],
                changed: false,
                hold: None,
            };
            for i in 0..20 {
                editor.continue_brush(&mut drag, [300. + i as f32, at[1], 300.], false);
            }
            editor.refresh_scene();
            editor.views[0].drag = Some(drag);
            let t = Instant::now();
            editor.finish_drag(0);
            let release = t.elapsed();
            let t = Instant::now();
            editor.refresh_scene();
            let refresh = t.elapsed();
            let t = Instant::now();
            target.time_frame(&renderer).unwrap();
            let frame = t.elapsed();
            // The land grids, off the UI thread in the app.
            let t = Instant::now();
            let (job, work) = editor.land_job().expect("a land job after a stroke");
            editor.land_synced(job, work());
            eprintln!(
                "stroke release {release:?}, then refresh {refresh:?}, frame {frame:?}; land job {:?}",
                t.elapsed()
            );
        }
        // A gizmo drag: one prop moved 40 times, refreshing like frames do.
        let item = Item::Prop(editor.island.doc.props[0].id);
        let start = editor.island.doc.matrix(item).unwrap();
        let (mut moved, mut scene) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let renderer = editor.renderer.clone();
        for i in 0..40 {
            let t = Instant::now();
            let mut m = start;
            m[12] += i as f32 * 0.5;
            // As the gizmo does: the document, then the item's rows.
            editor.island.doc.set_matrix(item, m);
            editor.placements.move_items(&mut [(item, m)], &renderer);
            editor.moves += 1;
            editor.markers_stale = true;
            moved += t.elapsed();
            let t = Instant::now();
            editor.refresh_scene();
            scene += t.elapsed();
        }
        eprintln!("drag: 40 moves {moved:?}, 40 refreshes {scene:?}");
        let t = Instant::now();
        let job = editor.bake_job().unwrap();
        let baked = job().unwrap();
        eprintln!("paint rebake job {:?}", t.elapsed());
        let t = Instant::now();
        editor.set_ground(baked);
        editor.refresh_scene();
        eprintln!("set_ground + refresh {:?}", t.elapsed());
        let t = Instant::now();
        let snapshot = editor.island.doc.clone();
        eprintln!("doc clone (undo checkpoint) {:?}", t.elapsed());
        drop(snapshot);
        let t = Instant::now();
        editor.history.checkpoint(&editor.island.doc.clone());
        editor.undo(false);
        editor.refresh_scene();
        eprintln!("undo + refresh {:?}", t.elapsed());
    }

    // Needs the game: loaded with the app's install, an island reads its
    // shipped terrain meshes only once their layer shows.
    #[test]
    fn shipped_meshes_load_when_their_layer_shows() {
        let Some(install) = install().map(Arc::new) else {
            return;
        };
        let assets = assets(&install);
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            PIRATE,
            Some((install.clone(), assets)),
        )
        .unwrap();
        editor.refresh_scene();
        assert_eq!(editor.shipped.len(), 0, "not read yet");
        editor.layers.shipped = true;
        editor.refresh_scene();
        assert!(editor.shipped.len() > 0);
        assert!(!editor.shipped_meshes.is_empty());
    }

    // Needs the game: 64 m deco islands keep a 2 texel/m material copy in
    // rd3d, not the 1 texel/m most islands do; they used to fail to open.
    #[test]
    fn small_deco_islands_open_with_their_material_maps() {
        let path = "data/base/provinces/celtic/islands/deco/celtic_island_deco_08/celtic_island_deco_08.a7m";
        let Some((_, editor)) = open(path) else {
            return;
        };
        assert_eq!(editor.world(), [64, 64]);
        assert!(editor.island.doc.materials.is_some());
    }

    // Needs the game: paint dabs update the GPU splat in place, matching a
    // full rebuild from the same materials, with no bake in between.
    // Needs the game: a Tint dab shows in the tint the GPU draws, which
    // reads the working map itself, and the next dab of the stroke paints
    // that map in place: the GPU's view takes no copy of it.
    #[test]
    fn tint_dabs_show_in_the_gpu_tint_without_copying_the_map() {
        let Some((_, mut editor)) = open(SMALL) else {
            return;
        };
        assert!(editor.ground_tint().is_some(), "a ground tint");
        let g = editor.island.doc.field.grid;
        let at = [
            g.origin[0] + g.size()[0] / 2.,
            g.origin[1] + g.size()[1] / 2.,
        ];
        let at = [at[0], editor.island.doc.field.height(at), at[1]];
        editor.tool = Tool::Tint;
        editor.tint_color = tint::PRESETS[1].1;
        (editor.brush.radius, editor.brush.strength) = (6., 1.);
        editor.history.checkpoint(&editor.island.doc);
        assert!(editor.dab(at, 0., false, None));
        // The first dab after the checkpoint copied the map from the undo
        // step's; the scene takes the copy.
        let tint = editor.ground_tint().unwrap();
        let painted = Arc::as_ptr(editor.island.doc.tint.as_ref().unwrap());
        assert!(editor.dab(at, 0., false, None));
        assert!(
            std::ptr::eq(
                Arc::as_ptr(editor.island.doc.tint.as_ref().unwrap()),
                painted
            ),
            "the second dab paints the map in place"
        );
        let map = editor.island.doc.tint.as_ref().unwrap();
        let drawn = tint.map.to_vec();
        assert!(
            (drawn.as_chunks::<4>().0.iter())
                .zip(&map.texels)
                .all(|(d, t)| d[..3] == t[..] && d[3] == 255),
            "the GPU's tint is the map's"
        );
    }

    #[test]
    fn painting_updates_the_splat_live() {
        use anno_render::a7m::Splat;
        let Some((_, mut editor)) = open(SMALL) else {
            return;
        };
        let before = editor.splat.clone().expect("a splat");
        let snapshot = |s: &Splat| -> Vec<Vec<u8>> {
            (s.indices.iter().chain(&s.weights))
                .map(|m| m.to_vec())
                .collect()
        };
        let original = snapshot(&before);
        editor.tool = Tool::Paint;
        editor.paint = *editor
            .palette
            .as_ref()
            .unwrap()
            .materials
            .keys()
            .last()
            .unwrap();
        editor.history.checkpoint(&editor.island.doc);
        for i in 0..6 {
            assert!(editor.dab([90. + i as f32 * 4., 0., 100.], 0., false, None));
        }
        let live = editor.splat.clone().unwrap();
        assert!(
            Arc::ptr_eq(&live, &before),
            "the same splat, edited in place"
        );
        assert_ne!(snapshot(&live), original, "the dabs changed it");
        let a = (editor.island.doc.materials.as_ref().unwrap().paint.a7m()).unwrap();
        let rebuilt = (editor.palette.as_ref().unwrap())
            .splat(
                editor.a7m_rules().unwrap().ground_bake,
                &a.ids,
                &a.alphas,
                None,
            )
            .unwrap()
            .0;
        assert!(
            snapshot(&live) == snapshot(&rebuilt),
            "live edits match a rebuild"
        );
        // A stroke's bake only redoes the texture.
        editor.ground_stale = true;
        let splat = editor.bake_job().unwrap()().unwrap().splat;
        assert!(splat.is_none());
        // Undo brings the old materials back, and the live splat with them,
        // in place as a dab writes it; the bake again redoes the texture.
        assert!(editor.undo(false));
        assert!(editor.ground_stale);
        let splat = editor.bake_job().unwrap()().unwrap().splat;
        assert!(splat.is_none(), "undo rebuilds no splat");
        let live = editor.splat.clone().unwrap();
        assert!(Arc::ptr_eq(&live, &before));
        assert!(snapshot(&live) == original, "back to the unpainted splat");
    }

    // Needs the game: a left-drag in the 3D view with Raise must change the
    // heights under the cursor, through the same input path the window uses.
    // A drag of more than GPU_DRAG items runs on the GPU: the document
    // keeps the starts while it runs, the gizmo follows the drag, and the
    // release writes every item where the drag put it, once.
    #[test]
    fn a_large_drag_lands_in_the_document_on_release() {
        use super::GPU_DRAG;
        use gpui_editor_kit::gpui::{
            Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, point, px, size,
        };
        use gpui_editor_kit::math::{self, Mat4};
        let Some((_, mut editor)) = open(PIRATE) else {
            return;
        };
        editor.views[0].bounds = Bounds::new(point(px(0.), px(0.)), size(px(1000.), px(700.)));
        editor.selection = editor.island.doc.items().map(|(i, _)| i).collect();
        assert!(
            editor.selection.len() > GPU_DRAG,
            "{}",
            editor.selection.len()
        );
        editor.tool = Tool::Select;
        editor.refresh_scene();
        let starts: Vec<(Item, Mat4)> = (editor.selection.iter())
            .map(|&i| (i, editor.island.doc.matrix(i).unwrap()))
            .collect();
        let pivot = editor.pivot().unwrap();
        let v = &editor.views[0];
        let at = v.camera.project(pivot, v.bounds, editor.extent());
        editor.mouse_down(
            0,
            &MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            },
        );
        editor.mouse_move(
            0,
            &MouseMoveEvent {
                position: point(at.x + px(40.), at.y),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            },
        );
        editor.refresh_scene();
        let m = editor.gpu_drag().expect("the drag runs on the GPU");
        let placed = |e: &IslandEditor, i: &Item| e.selection_unplaced.binary_search(i).is_err();
        for (item, start) in starts.iter().filter(|(i, _)| placed(&editor, i)) {
            assert_eq!(
                editor.island.doc.matrix(*item).unwrap(),
                *start,
                "{item:?} mid drag"
            );
        }
        let frame = editor.gizmo_frame(0).unwrap();
        let moved_pivot = math::transform(&m, pivot);
        assert!(math::length(math::sub(frame.pivot(), moved_pivot)) < 1e-3);
        assert!(
            math::length(math::sub(moved_pivot, pivot)) > 0.1,
            "the drag moved"
        );

        editor.finish_drag(0);
        assert!(editor.gpu_drag().is_none());
        for (item, start) in &starts {
            let want = math::translation(&math::multiply(&m, start));
            let got = math::translation(&editor.island.doc.matrix(*item).unwrap());
            assert!(
                math::length(math::sub(got, want)) < 1e-3,
                "{item:?} {got:?} {want:?}"
            );
        }
        assert!(editor.history.can_undo());
    }

    // The gizmo's pivot over a large selection walks the props beside it;
    // it lands where the plain mean of every selected item does.
    #[test]
    fn pivot_of_a_large_selection_is_the_mean_position() {
        let Some((_, mut editor)) = open(PIRATE) else {
            return;
        };
        use gpui_editor_kit::math;
        let items: Vec<(Item, [f32; 3])> = editor.island.doc.items().step_by(3).collect();
        assert!(items.len() >= 64, "{} items", items.len());
        editor.selection = items.iter().map(|(i, _)| *i).collect();
        let sum = items.iter().fold([0.; 3], |s, (_, p)| math::add(s, *p));
        let mean = math::scale(sum, 1. / items.len() as f32);
        let pivot = editor.pivot().unwrap();
        assert!(
            math::length(math::sub(pivot, mean)) < 1e-3,
            "{pivot:?} {mean:?}"
        );
    }

    #[test]
    fn vegetation_follows_a_sculpt_stroke_while_it_runs() {
        use gpui_editor_kit::gpui::{
            Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, point, px, size,
        };
        let Some((_, mut editor)) = open(PIRATE) else {
            return;
        };
        editor.layers.vegetation = true;
        editor.refresh_scene();
        // Instances as translations, sorted: a rescatter appends what it
        // regrows, so only the set compares.
        let sorted = |p: Option<&super::vegetation::Preview>| -> Vec<(String, [u32; 3])> {
            let mut v: Vec<_> = (p.iter())
                .flat_map(|p| p.models.iter())
                .flat_map(|(cfg, ms)| {
                    ms.iter()
                        .map(move |m| (cfg.clone(), [m[12], m[13], m[14]].map(f32::to_bits)))
                })
                .collect();
            v.sort();
            v
        };
        let before = sorted(editor.vegetation.as_deref());
        assert!(!before.is_empty(), "the pirate island grows vegetation");
        editor.views[0].bounds = Bounds::new(point(px(0.), px(0.)), size(px(1000.), px(700.)));
        editor.tool = Tool::Raise;
        let at = point(px(500.), px(350.));
        editor.mouse_down(
            0,
            &MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            },
        );
        for k in 1..20 {
            editor.mouse_move(
                0,
                &MouseMoveEvent {
                    position: point(px(500. + k as f32 * 4.), px(350.)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::default(),
                },
            );
            assert!(!editor.scene_stale, "a dab rebuilds no scene");
            editor.refresh_scene();
        }
        let live = sorted(editor.vegetation.as_deref());
        assert_ne!(live, before, "the raised ground moved its vegetation");
        // The island thins nothing, so the windows add up to a full scatter.
        assert_eq!(live, sorted(editor.vegetation_preview().as_deref()));

        editor.finish_drag(0);
        editor.refresh_scene();
        assert_eq!(
            sorted(editor.vegetation.as_deref()),
            live,
            "the release keeps it"
        );
    }

    #[test]
    fn dragging_raise_in_the_view_changes_heights() {
        use gpui_editor_kit::gpui::{
            Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, point, px, size,
        };
        let Some((_, mut editor)) = open(PIRATE) else {
            return;
        };
        editor.views[0].bounds = Bounds::new(point(px(0.), px(0.)), size(px(1000.), px(700.)));
        editor.tool = Tool::Raise;
        let before = editor.island.doc.field.heights.clone();
        let at = point(px(500.), px(350.));
        let hit = editor.ground(0, at);
        eprintln!("ground under cursor: {hit:?}");
        assert!(hit.is_some(), "cursor ray misses the terrain");
        editor.mouse_down(
            0,
            &MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            },
        );
        for k in 1..20 {
            editor.mouse_move(
                0,
                &MouseMoveEvent {
                    position: point(px(500. + k as f32 * 4.), px(350.)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::default(),
                },
            );
        }
        editor.finish_drag(0);
        let after = &editor.island.doc.field.heights;
        let changed = before.iter().zip(after).filter(|(a, b)| a != b).count();
        let raised = (before.iter().zip(after))
            .map(|(a, b)| b - a)
            .fold(0f32, f32::max);
        eprintln!("changed {changed} samples, max raise {raised} m");
        assert!(
            changed > 100 && raised > 0.5,
            "changed {changed}, raised {raised}"
        );
        assert!(editor.history.can_undo());
    }

    #[test]
    fn height_ramp_and_mix_hit_their_stops() {
        assert_eq!(height_color(-5.), 0xc9b98a);
        assert_eq!(height_color(1.), 0x8fae62);
        assert_eq!(height_color(500.), 0xd8d6d0);
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
        assert_ne!(material_color(1), material_color(2));
    }

    #[test]
    fn painting_marsh_lays_the_ground_shipped_marshes_have() {
        let Some((_, mut editor)) = super::test_support::open(
            "data/base/provinces/celtic/islands/pool/celtic_island_large_01/celtic_island_large_01.a7m",
        ) else {
            return;
        };
        let marsh_at = |e: &IslandEditor, p: [f32; 2]| {
            grids::value_at(
                e.island
                    .doc
                    .grids
                    .as_ref()
                    .unwrap()
                    .layer(Kind::Marsh)
                    .unwrap(),
                p[0],
                p[1],
            )
        };
        // Dry meadow a few metres up, flat enough to stand a marsh on.
        let f = &editor.island.doc.field;
        let size = f.grid.width as f32 * f.grid.spacing;
        let at = (0..2000)
            .map(|k| [40. + (k % 40) as f32 * 10., 40. + (k / 40) as f32 * 10.])
            .filter(|p| p[0] < size - 40. && p[1] < size - 40.)
            .find(|&p| {
                let h = f.height(p);
                (2. ..6.).contains(&h)
                    && (f.height([p[0] + 6., p[1]]) - h).abs() < 1.
                    && marsh_at(&editor, p) == 0
            })
            .expect("dry meadow");
        editor.tool = Tool::Grid;
        editor.grid = Kind::Marsh;
        editor.grid_value = editor.rules().marsh_tile;
        editor.brush.radius = 8.;
        editor.brush.strength = 1.;
        for _ in 0..12 {
            editor.dab(
                [at[0], editor.island.doc.field.height(at), at[1]],
                0.,
                false,
                None,
            );
        }
        assert_eq!(marsh_at(&editor, at), editor.rules().marsh_tile);
        let height = editor.island.doc.field.height(at);
        assert!((height - super::MARSH_LEVEL).abs() < 0.1, "height {height}");
        let env = editor
            .island
            .doc
            .grids
            .as_ref()
            .unwrap()
            .layer(Kind::Environment)
            .unwrap();
        assert_eq!(
            grids::value_at(env, at[0], at[1]),
            super::harbour::MARSH_ENVIRONMENT
        );
        let palette = editor.palette.clone().unwrap();
        let shown = super::doc::shown(
            editor.island.doc.materials.as_ref().unwrap(),
            at,
            Some(&palette),
        );
        let file = &palette.materials[&shown].file;
        assert!(file.contains("marsh"), "ground {file}");

        // Without shaping only the cells change.
        editor.marsh_shape = false;
        let other = [at[0] + 20., at[1]];
        let before = editor.island.doc.field.height(other);
        editor.dab([other[0], before, other[1]], 0., false, None);
        assert_eq!(marsh_at(&editor, other), editor.rules().marsh_tile);
        assert_eq!(editor.island.doc.field.height(other), before);
    }

    // Each Graphics option changes what the pirate island draws and what a
    // still view keeps, and High, the default, draws today's image again
    // byte for byte.
    #[test]
    fn each_graphics_option_changes_the_view_and_high_draws_it_back() {
        use crate::graphics::{CloudShadows, Foliage, Graphics, Shadows, SmallProps, Waves};
        use gpui_editor_kit::gpu::{HELD_CLOCK, Offscreen, Reuse};
        let Some(install) = install() else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Arc::new(install);
        let assets = assets(&install);
        let mut editor = IslandEditor::load_with(
            crate::games::module(crate::games::of(&install)),
            &install,
            PIRATE,
            Some((install.clone(), assets)),
        )
        .unwrap();
        let hold = |t: f32| HELD_CLOCK.store(t.to_bits(), std::sync::atomic::Ordering::Relaxed);
        hold(4.);
        editor.layers.waves = false;
        editor.layers.vegetation = true;
        super::gpu_bench::settle(&mut editor, &install);
        // gpu_bench's pirate view: low over the island, the horizon and
        // its clouds in view.
        let mut camera = editor.views[0].camera;
        camera.zoom *= 1.8;
        camera.tilt = 1.15;
        camera.yaw = 0.6;
        let extent = editor.extent();
        let renderer = editor.renderer.clone();
        let mut target = Offscreen::new(480, 300).unwrap();
        target.render(&renderer, camera, extent).unwrap();
        super::water_shots::show_water_at(&editor, 4.);
        // Ground pages bake and the exposure adapts over frames; it takes
        // what it measures at once after a pause.
        for _ in 0..48 {
            target.frame(&renderer).unwrap();
        }
        // Frames until the view has nothing left to bring in or let go:
        // uploads, ground pages (a new render scale bakes them again), the
        // texture levels its culls ask for, none held past what they ask,
        // which residency lets go on the wall clock, and the sky's clouds,
        // marched again on threads at each size. A fixed count of frames
        // raced those under load. Then a pause: exposure takes what it
        // measures at once after one.
        let sky = editor.sky_pass.clone();
        let settled = |target: &mut Offscreen, renderer: &gpui_editor_kit::gpu::Renderer| {
            let started = std::time::Instant::now();
            for k in 0.. {
                let pages = renderer.drawn().pages;
                // A few frames first: a change shows in the culls' answers
                // a frame or two after it is made.
                let still = renderer.settled() && sky.as_ref().is_none_or(|s| s.settled());
                if k >= 3 && still && pages.baked >= pages.wanted {
                    break;
                }
                assert!(
                    started.elapsed().as_secs() < 90,
                    "still settling after 90 s: {:?}, pages {pages:?}",
                    renderer.waiting_on()
                );
                target.frame(renderer).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
            target.frame(renderer).unwrap()
        };
        // How two frames differ, for a failure to say.
        let diff = |a: &[u8], b: &[u8]| {
            let off = a.iter().zip(b).filter(|(x, y)| x != y).count();
            let most = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max();
            format!("{off} of {} bytes off, by at most {most:?}", a.len())
        };
        let mut draw = |editor: &mut IslandEditor, g: Graphics| {
            editor.set_graphics(g);
            editor.refresh_scene();
            super::water_shots::show_water_at(editor, 4.);
            let first = target.frame(&renderer).unwrap();
            let kept = renderer.drawn().reuse;
            (first, kept, settled(&mut target, &renderer))
        };
        // Twice: the first settles what the views' first frames bring up.
        draw(&mut editor, Graphics::default());
        let high = draw(&mut editor, Graphics::default()).2;
        assert_eq!(renderer.drawn().reuse, Reuse::Whole, "a still view rests");
        let d = Graphics::default();
        for (name, g) in [
            ("clouds", Graphics { clouds: false, ..d }),
            ("haze", Graphics { haze: false, ..d }),
            (
                "reflections",
                Graphics {
                    reflections: false,
                    ..d
                },
            ),
            (
                "waves",
                Graphics {
                    waves: Waves::Reduced,
                    ..d
                },
            ),
            (
                "shadows",
                Graphics {
                    shadows: Shadows::Medium,
                    ..d
                },
            ),
            (
                "no shadows",
                Graphics {
                    shadows: Shadows::Off,
                    ..d
                },
            ),
            (
                "shadow distance",
                Graphics {
                    shadow_distance: 150.,
                    ..d
                },
            ),
            (
                "occlusion",
                Graphics {
                    occlusion: false,
                    ..d
                },
            ),
            (
                "cloud shadows",
                Graphics {
                    cloud_shadows: CloudShadows::Off,
                    ..d
                },
            ),
            (
                "small props",
                Graphics {
                    small_props: SmallProps::Off,
                    ..d
                },
            ),
            (
                "render scale",
                Graphics {
                    render_scale: 0.5,
                    ..d
                },
            ),
            (
                "animation",
                Graphics {
                    animation: false,
                    ..d
                },
            ),
        ] {
            let (_, kept, image) = draw(&mut editor, g);
            assert_ne!(kept, Reuse::Whole, "{name}: the still view drew again");
            assert!(image != high, "{name}: the view changed");
            let (_, kept, back) = draw(&mut editor, d);
            assert_ne!(kept, Reuse::Whole, "{name}: back to High drew again");
            // The clouds' map is made again at each size and settles a few
            // levels apart in the sky (264 of 576 000 bytes, at most 3).
            let close = back.iter().zip(&high).all(|(a, b)| a.abs_diff(*b) <= 3);
            if name == "render scale" {
                assert!(
                    close,
                    "{name}: High draws today's image ({})",
                    diff(&back, &high)
                );
            } else {
                assert!(
                    back == high,
                    "{name}: High draws today's image ({})",
                    diff(&back, &high)
                );
            }
        }
        // Foliage stops where the game's View Distance stops it, 575 m and
        // nearer, past what the view above reaches: from three times as far
        // each level draws the island's far trees otherwise.
        let mut far = camera;
        far.zoom /= 3.;
        target.aim(&renderer, far, extent);
        let mut shot = |editor: &mut IslandEditor, g: Graphics| {
            editor.set_graphics(g);
            editor.refresh_scene();
            super::water_shots::show_water_at(editor, 4.);
            settled(&mut target, &renderer)
        };
        shot(&mut editor, d);
        let reached = shot(&mut editor, d);
        let others = [Foliage::Off, Foliage::Near, Foliage::Game, Foliage::Far];
        for foliage in others.into_iter().filter(|&f| f != d.foliage) {
            let image = shot(&mut editor, Graphics { foliage, ..d });
            assert!(image != reached, "{foliage:?}: the view changed");
            let back = shot(&mut editor, d);
            assert!(
                back == reached,
                "{foliage:?}: High draws it back ({})",
                diff(&back, &reached)
            );
        }
        hold(f32::NAN);
    }

    // Video memory goes with the island that held it: a small island
    // opened after the largest one holds what it holds opened alone, not
    // the large one's as well, nor pools grown to its size. Offscreen
    // frames, so this covers what
    // the renderer owns and frees (arena, atlases, pages, texture pools),
    // not the window's present chain (editor::tests::
    // opening_another_island_drops_the_last_ones_renderer covers that).
    #[test]
    fn switching_islands_frees_the_last_ones_video_memory() {
        use gpui_editor_kit::gpu::Offscreen;
        let Some(install) = install() else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Arc::new(install);
        let game = (install.clone(), assets(&install));
        let mut target = Offscreen::alone(1280, 720).unwrap();
        let mib = |target: &Offscreen| {
            let report = target.device().generate_allocator_report()?;
            Some(report.total_allocated_bytes as f64 / f64::from(1u32 << 20))
        };
        let Some(empty) = mib(&target) else {
            eprintln!("skipping: no allocator report on this backend");
            return;
        };
        // Freed resources go once the device sees their frames finish.
        let closed = |target: &Offscreen, island: IslandEditor| {
            drop(island);
            gpui_editor_kit::gpu::wait_for_freed();
            let wait = wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(10)),
            };
            target.device().poll(wait).unwrap();
            mib(target).unwrap()
        };
        let held = |target: &mut Offscreen, path: &str| {
            let island = super::gpu_bench::open_and_draw(target, &game, path, 0.);
            let used = mib(target).unwrap() - empty;
            (island, used)
        };
        let (small, alone) = held(&mut target, SMALL);
        let after = closed(&target, small);
        assert!(
            after - empty < 8.,
            "{after:.1} MiB held with no island open"
        );
        let (large, big) = held(&mut target, super::gpu_bench::CONTINENTAL_01);
        let after = closed(&target, large);
        assert!(
            after - empty < 8.,
            "{after:.1} MiB held with no island open"
        );
        let (_small, again) = held(&mut target, SMALL);
        eprintln!("small island {alone:.1} MiB, continental {big:.1}, small after it {again:.1}");
        let margin = alone * 0.25 + 32.;
        assert!(
            big - alone > margin,
            "the islands must differ by more than the margin"
        );
        assert!(
            again < alone + margin,
            "the small island holds {again:.1} MiB after the continental one ({big:.1}), {alone:.1} alone"
        );
    }
}
