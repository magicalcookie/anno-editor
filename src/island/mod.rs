// Owns the island workspace state: tools, brushes, selection, both viewports
// and their GPU scenes, and input. The document lives in `doc`, the panes in
// `panels`, the island's look in `crate::terrain`.
//
// Terrain draws as one mesh per heightfield tile; a dab rebuilds only the
// tiles it touched and every other tile keeps its GPU buffers. Props draw as
// markers until model (.rdm) decoding exists.
mod blockers;
pub mod catalog;
mod cliffs;
pub mod convert;
mod doc;
pub mod fertility;
mod grids;
mod harbour;
mod import;
pub(crate) mod models;
mod new;
mod panels;
mod placements;
pub mod port;
mod river;
mod scatter;
mod sound;
#[cfg(test)]
mod test_support;
mod tint;
mod vegetation;
mod wizard;
pub(crate) use panels::{IslandKind, island_title, pretty, selection_status};
pub(crate) use wizard::{province, province_short};
mod worldmachine;

pub use doc::island_data_dir;
use doc::{Doc, Materials, Prop};
pub use import::Import;
pub use new::{Skeleton, Spec, Start, file_name, island_path, islands, skeletons};
pub use wizard::Wizard;
pub use worldmachine::{Link as WmLink, Previews as WmPreviews};

use crate::{
    project::{Listing, Project},
    terrain,
};
use anno_formats::{
    grids::{Grids, Kind},
    island::Island,
    landscape::{self, Landscape},
    objects, rda,
    tint::TintMap,
    vfs::Install,
};
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    camera::{Camera, Extent},
    field::{EditEvent, Filter, TextEdit},
    gizmo::{self, Gesture, Marquee},
    gpu::{
        self, Batch, Fog, Instances, Light, Marker, Mesh, MeshData, Occlusion, Renderer, Scene,
        Sea, Shadows, Shape, Sky, Splat, Tone, Viewport,
    },
    gpui::{prelude::*, *},
    heightfield::{Brush, Falloff, Grid, Heightfield, Mode, Rect, Sculpt, Stroke, files},
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
fn buildable_overlay(layer: &anno_formats::grids::Layer, p: Vec3) -> Option<([f32; 3], f32)> {
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
fn curve_length(path: &[anno_formats::objects::PathPoint]) -> f32 {
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

/// A selectable thing on the island: an rd3d prop or a gamedata object, by
/// their stable ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Item {
    Prop(u64),
    Object(u64),
}

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

/// What the viewports draw, toggled from the hierarchy pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layers {
    pub props: bool,
    pub objects: bool,
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
    /// Harbour coasts as cyan lines on the waterline; the Harbour tool
    /// shows them regardless.
    pub harbours: bool,
}

impl Default for Layers {
    fn default() -> Self {
        Self {
            props: true,
            objects: true,
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
            // The heaviest line on the island otherwise; the Harbour tool
            // draws them regardless.
            harbours: false,
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
}

enum Drag {
    Brush {
        stroke: Stroke,
        before: Box<Doc>,
        level: f32,
        changed: bool,
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
    },
    Marquee(Marquee),
    /// Harbour tool: drawing a harbour along the shore; the preview follows
    /// the cursor.
    Shore {
        from: Vec3,
        to: Vec3,
        preview: Option<Vec<anno_formats::objects::PathPoint>>,
    },
    /// Harbour and slot tools: a ground rectangle to auto-place in.
    Region {
        from: Vec3,
        to: Vec3,
    },
    /// River tool: dragging a draft point or its width handle.
    RiverKnot {
        index: usize,
        handle: river::Handle,
    },
    /// Dragging one harbour curve point along the shore.
    CoastPoint {
        id: u64,
        index: usize,
        before: Box<Doc>,
        changed: bool,
    },
    Orbit(Point<Pixels>),
    /// Last cursor, and the ground point grabbed (perspective pans keep it
    /// under the cursor).
    Pan(Point<Pixels>, Option<Vec3>),
}

struct View {
    camera: Camera,
    bounds: Bounds<Pixels>,
    viewport: Arc<Viewport>,
    drag: Option<Drag>,
}

pub struct IslandEditor {
    game: anno_formats::game::Game,
    pub path: String,
    source: Island,
    // Inputs matching `source`. The install mounts saved mods, so reading it
    // again after Save would apply edits to already edited terrain files.
    terrain_original: std::collections::BTreeMap<String, Option<Vec<u8>>>,
    pub doc: Doc,
    map: Option<Arc<Texture>>,
    ground: Option<Arc<Texture>>,
    /// Native splat of the material maps; terrain draws with it in Ground.
    splat: Option<Arc<Splat>>,
    palette: Option<Arc<terrain::Palette>>,
    /// Material id and thumbnail, palette order.
    thumbs: Vec<(u8, Arc<RenderImage>)>,
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
    /// Flatten target; `None` uses the height where each stroke starts.
    level: Option<f32>,
    /// Prop file index the Place tool adds.
    place: Option<u32>,
    /// What the Place tool puts down instead of a prop: an object record
    /// from a shipped island, and the model a decoration carries.
    pub place_object: Option<(anno_formats::objects::Template, Option<String>)>,
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
    /// Overlay and tint switch the terrain tiles were last built with, so a
    /// layer toggle rebuilds them.
    drawn_look: Option<(Option<Kind>, bool)>,
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
    /// Shipped records the harbour and slot tools copy; loaded on first use.
    pub harbour: Option<Arc<harbour::Library>>,
    /// The records are being read off the UI thread (`start_harbour_library`).
    harbour_loading: bool,
    /// Reading them failed for this island; not retried on every click.
    harbour_failed: bool,
    /// The harbour tool places the island's start coast instead.
    pub start_coast: bool,
    pub resource: harbour::Resource,
    /// The slot tool places always-active (fixed) slots, not random ones.
    pub fixed_slots: bool,
    /// Harbour and slot tools: dragging marks a region to auto-place in.
    pub auto_region: bool,
    /// How many auto placement adds per region.
    pub auto_count: u32,
    /// Shipped sound spot and build blocker records by GUID, loaded on the
    /// first auto placement.
    pub spot_records: Option<Arc<HashMap<u32, anno_formats::objects::Template>>>,
    /// The harbour curve point picked with the Harbour tool: object, index.
    pub coast_point: Option<(u64, usize)>,
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
    /// Flow brush: the last dab, whose direction to the next one sets the
    /// current.
    flow_from: Option<[f32; 2]>,
    /// `listing` changed since the last save.
    listing_dirty: bool,
    /// The install's prop sets; `None` when `prop_sets.xml` did not load.
    pub prop_sets: Option<Arc<vegetation::PropSets>>,
    /// Ambient presets the inspector offers for this island's region.
    pub ambients: Vec<String>,
    /// How each of those presets, and the island's own, lights the scene.
    ambient_lights: Vec<(String, vegetation::Ambient)>,
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
    /// Rivers and lakes as one surface.
    inland_water: Option<Arc<Mesh>>,
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
    /// A drag moved a river or lake curve: its water mesh follows.
    inland_stale: bool,
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
    /// Decoded object areas, kept for writing `doc.objects` back on save.
    object_store: Option<objects::Objects>,
    views: [View; 2],
    /// Draws both views; each has a viewport on it.
    renderer: Arc<Renderer>,
    tiles: Vec<Arc<Mesh>>,
    /// Heights on the GPU; every tile mesh displaces by it, so a sculpt dab
    /// uploads only its rectangle and rebuilds no mesh.
    gpu_heights: Option<Arc<gpu::HeightMap>>,
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
    next_id: u64,
    pub message: Option<(String, bool)>,
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

    /// Reads an island archive through the install and builds the document.
    pub fn load(install: &Install, path: &str) -> Result<Self> {
        Self::load_with(install, path, None)
    }

    /// `load`, given the app's install and asset registry: the island's
    /// models then load on their own threads while the rest of the island
    /// does. Waiting for the document first put 0.8 s before them on the
    /// continental island.
    pub fn load_with(
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
        let bytes = install.read(path)?;
        let archive = rda::Archive::from_bytes(bytes, path).context("island archive")?;
        let member = |name: &str| {
            archive
                .read_path(name)
                .with_context(|| format!("{path}: {name}"))
        };
        let gamedata = member("gamedata.data")?;
        let rd3d = member("rd3d.data")?;
        let source = match install.read(&format!("{path}info")) {
            Ok(info) => Island::parse_with_info(&gamedata, &rd3d, &info)?,
            Err(_) => Island::parse(&gamedata, &rd3d)?,
        };
        // The terrain's material set and the shipped terrain meshes need
        // only the island's header: with the app's install they load on
        // their own threads, beside the material maps and the models.
        let metres = source.world_size()[0] as f32;
        let early = shared.as_ref().map(|(install, _)| {
            let (install, set, path) = (
                install.clone(),
                source.material_set_file_name().to_owned(),
                path.to_owned(),
            );
            let palette = std::thread::spawn({
                let install = install.clone();
                move || terrain::Palette::load(&install, Some(&install), &set)
            });
            (palette, (install, path))
        });
        let (early_palette, shipped_source) = early.unzip();
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
        let mut library = models::Library::default();
        let models = shared.as_ref().map(|(install, assets)| {
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
            std::thread::spawn(library.job(install.clone(), batch))
        });
        let (materials, level0) = match Materials::load_level0(install, path, &source) {
            Ok((m, level0)) => (Some(m), level0),
            Err(e) => {
                eprintln!("{path}: material maps unavailable, painting disabled: {e:#}");
                (None, None)
            }
        };
        let mut doc = Doc::from_island(&source, materials)?;
        doc.objects = Arc::new(objects);
        doc.grids = Grids::read(&source)
            .map_err(|e| eprintln!("{path}: grids unavailable: {e:#}"))
            .ok()
            .map(Arc::new);
        doc.tint = island_data_dir(path)
            .ok()
            .map(|dir| Arc::new(tint::load(install, &dir, source.world_size())));
        lap("Terrain / heightmap", &mut log);
        let map = load_map_image(install, path);
        let palette = match early_palette {
            Some(thread) => thread.join().expect("material set"),
            None => terrain::Palette::load(install, None, source.material_set_file_name()),
        };
        let palette = palette
            .map_err(|e| eprintln!("{path}: material set unavailable: {e:#}"))
            .ok()
            .map(Arc::new);
        lap("Material textures", &mut log);
        // Level 0 as decoded, else copied out of the paint chunks once.
        let level0 = level0.or_else(|| {
            doc.materials
                .as_ref()
                .map(|m| (m.ids.grid(), m.alphas.grid()))
        });
        // The GPU splat draws the ground; the baked texture only colours
        // shipped cliff tops, map images and the other shadings, so with a
        // splat it bakes off the UI thread after loading (`bake_job`), not
        // here: 100-180 ms of the continental island's load.
        let splat = palette
            .as_ref()
            .zip(level0.as_ref())
            .and_then(|(p, (ids, alphas))| {
                p.splat(ids, alphas)
                    .map_err(|e| eprintln!("{path}: splat unavailable: {e:#}"))
                    .ok()
                    .map(Arc::new)
            });
        let ground = match (&palette, &level0, &splat) {
            (Some(p), Some((ids, alphas)), None) => p
                .bake(ids, alphas, metres)
                .map_err(|e| eprintln!("{path}: ground bake failed: {e:#}"))
                .ok()
                .map(Arc::new),
            _ => None,
        };
        let bake_later = ground.is_none() && splat.is_some();
        lap("Splat layers", &mut log);
        drop(level0);
        // With the app's install the shipped meshes wait for their layer
        // (64 MB on the continental island, off by default).
        let shipped = match &shipped_source {
            Some(_) => terrain::ShippedMeshes::empty(metres),
            None => terrain::ShippedMeshes::load(install, path, metres),
        };
        lap("Shipped meshes", &mut log);
        let thumbs = palette.as_ref().map_or_else(Vec::new, |p| {
            p.materials
                .iter()
                .map(|(&id, m)| (id, m.preview.render_image()))
                .collect()
        });
        let size = doc.field.grid.size();
        let renderer = Arc::new(Renderer::default());
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
            game: install.game(),
            path: path.to_owned(),
            next_id: doc.props.len() as u64 + 1,
            source,
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
            paint: 1,
            load_log: Vec::new(),
            shading: if ground.is_some() || splat.is_some() {
                Shading::Ground
            } else if map.is_some() {
                Shading::Map
            } else {
                Shading::Height
            },
            shipped_meshes: Vec::new(),
            water: terrain::water(metres),
            inland_water: None,
            map,
            splat,
            ground,
            palette,
            thumbs,
            ground_stale: bake_later,
            splat_stale: false,
            baking: false,
            land_stale: false,
            land_job: None,
            land_jobs: 0,
            layers: Layers::default(),
            level: None,
            place: None,
            scatter: scatter::Scatter::default(),
            cliffs: cliffs::Settings::default(),
            grid: Kind::Water,
            grid_value: 1,
            drawn_look: None,
            drawn_lines: None,
            tint_color: tint::PRESETS[3].1,
            tint_loaded: None,
            terrain_original: std::collections::BTreeMap::new(),
            is_new: false,
            listing: Listing::default(),
            listing_dirty: false,
            prop_sets: vegetation::PropSets::load(install)
                .map_err(|e| eprintln!("{}: {e:#}", install.game().profile().prop_sets_path))
                .ok()
                .map(Arc::new),
            ambients: Vec::new(),
            ambient_lights: Vec::new(),
            vegetation: None,
            wm_busy: false,
            wm_stamp: None,
            harbour: None,
            harbour_loading: false,
            harbour_failed: false,
            start_coast: false,
            resource: harbour::Resource::Mine,
            fixed_slots: false,
            auto_region: false,
            auto_count: 2,
            spot_records: None,
            coast_point: None,
            river: None,
            river_history: river::DraftHistory::default(),
            lake_mode: false,
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
            inland_stale: false,
            selection_sent: None,
            selection_unplaced: Vec::new(),
            vegetation_window: Vec::new(),
            moves: 0,
            loading_models: false,
            models_settled: None,
            assets: None,
            object_store,
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
            terrain_colors: None,
            terrain: None,
            stale_colors: Rect::default(),
            dirty: Rect::default(),
            scene_stale: true,
            identity: Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![],
            }),
            cursor: None,
            editing: None,
            doc,
            message: None,
        };
        editor.tint_loaded = editor.doc.tint.clone();
        editor.ambients = vegetation::ambients(install, &editor.region());
        let own = editor.doc.session.global_ambient.clone();
        editor.ambient_lights =
            vegetation::Ambient::read_all(install, editor.ambients.iter().chain([&own]));
        lap("Ambient presets", &mut log);
        editor.rebuild_tiles(None);
        lap("Terrain chunks", &mut log);
        log.push(format!(
            "{} objects · {} props · {} materials · {} triangles",
            editor.doc.objects.len(),
            editor.doc.props.len(),
            editor.thumbs.len(),
            editor.triangles()
        ));
        if let Some(models) = models {
            editor
                .library
                .models
                .finish(models.join().expect("model loader"));
            editor.pieces_stale = true;
            lap("Models, alongside", &mut log);
        }
        editor.assets = shared.map(|(_, assets)| assets);
        editor.load_log = log;
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
        let mut used = vec![false; self.doc.meshes.len()];
        for p in self.doc.props.iter() {
            if let Some(u) = used.get_mut(p.source.file as usize) {
                *u = true;
            }
        }
        let props = (self.doc.meshes.iter().zip(used))
            .filter(|(_, u)| *u)
            .map(|(m, _)| m.clone());
        let objects = self.doc.objects.iter().filter_map(|o| {
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
                let p = self.doc.prop(id)?;
                self.doc
                    .meshes
                    .get(p.source.file as usize)
                    .map(|m| m.replace('\\', "/"))
            }
            Item::Object(id) => {
                let o = self.doc.objects.iter().find(|o| o.id == id)?;
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
        let mut world = self.doc.matrix(item)?;
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
            .doc
            .meshes
            .iter()
            .map(|m| m.replace('\\', "/"))
            .collect();
        let mut out = Vec::with_capacity(self.doc.props.len() + self.doc.objects.len());
        for p in self.doc.props.iter() {
            out.push((
                Item::Prop(p.id),
                files.get(p.source.file as usize).cloned(),
                p.matrix,
            ));
        }
        for o in self.doc.objects.iter() {
            let cfgs = self
                .assets
                .as_ref()
                .and_then(|a| a.get(o.guid))
                .map(|a| a.cfgs.as_slice())
                .unwrap_or(&[]);
            out.push((
                Item::Object(o.id),
                o.model(cfgs).map(|m| m.replace('\\', "/")),
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
        let cfgs = self.item_cfgs();
        let grown = self
            .vegetation
            .as_ref()
            .filter(|_| self.layers.vegetation)
            .map(|p| p.models.keys().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let batch = self.library.batch(
            (cfgs.iter())
                .filter_map(|(_, c, m)| Some((c.as_deref()?, Some(m))))
                .chain(grown.into_iter().map(|c| (c, None))),
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
            .get(&self.doc.session.vegetation_prop_set)?;
        let statics: Vec<([f32; 2], f32)> = self
            .doc
            .props
            .iter()
            .map(|p| {
                let m = &p.matrix;
                ([m[12], m[14]], math::length([m[0], m[1], m[2]]))
            })
            .collect();
        let ground = vegetation::Ground {
            field: &self.doc.field,
            materials: self.doc.materials.as_ref()?,
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
    pub fn edit_session(&mut self, f: impl FnOnce(&mut anno_formats::session::SessionSettings)) {
        let mut next = self.doc.session.clone();
        f(&mut next);
        if next == self.doc.session {
            return;
        }
        self.checkpoint();
        self.doc.session = next;
        self.scene_stale = true;
    }

    pub fn triangles(&self) -> usize {
        let tiles: usize = self.tiles.iter().map(|m| m.data().indices.len() / 3).sum();
        let grid = self.doc.field.grid;
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
    pub fn bake_job(
        &mut self,
    ) -> Option<impl FnOnce() -> Result<(Texture, Option<Splat>)> + Send + 'static> {
        self.ground_stale = false;
        let rebuild_splat = self.splat.is_none() || std::mem::take(&mut self.splat_stale);
        let palette = self.palette.clone()?;
        let m = self.doc.materials.as_ref()?;
        let (ids, alphas) = (m.ids.grid(), m.alphas.grid());
        let size = self.doc.field.grid.size()[0];
        Some(move || {
            let splat = rebuild_splat
                .then(|| {
                    palette
                        .splat(&ids, &alphas)
                        .map_err(|e| eprintln!("splat: {e:#}"))
                        .ok()
                })
                .flatten();
            Ok((palette.bake(&ids, &alphas, size)?, splat))
        })
    }

    pub fn set_ground(&mut self, (texture, splat): (Texture, Option<Splat>)) {
        self.ground = Some(Arc::new(texture));
        match splat {
            Some(splat) => {
                self.splat = Some(Arc::new(splat));
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
        let g = &self.doc.field.grid;
        let [w, d] = g.size();
        Extent {
            center: [g.origin[0] + w / 2., 0., g.origin[1] + d / 2.],
            size: w.max(d),
        }
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
        self.history.checkpoint(&self.doc);
        self.patch_history();
    }

    /// Stores undo steps' heights as patches against the step after each,
    /// newest first, down to the first already stored so. A step that
    /// still shares the live heights may be the snapshot before an edit
    /// not yet made: it waits for the next checkpoint.
    fn patch_history(&mut self) {
        self.trim_heap();
        let live = self.doc.field.clone();
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
        let live = self.doc.field.clone();
        let left = self.history.revision();
        // What tiles are built from, to rebuild only what the step changes.
        let before = (
            self.doc.field.grid,
            self.doc.field.clone(),
            self.doc.grids.clone(),
            self.doc.tint.clone(),
        );
        let done = if redo {
            self.history.redo(&mut self.doc)
        } else {
            self.history.undo(&mut self.doc)
        };
        if done {
            self.trim_heap();
            // The step restored may hold its heights as a patch against the
            // one left, which now waits on the other stack: store that one
            // as a patch against the restored heights in turn.
            self.doc.unpatch_heights(&live);
            let restored = self.doc.field.clone();
            let other = if redo {
                self.history.past_mut().next()
            } else {
                self.history.future_mut().next()
            };
            if let Some(other) = other {
                other.patch_heights(&restored);
            }
            let doc = &self.doc;
            self.selection.retain(|&item| doc.matrix(item).is_some());
            // Materials may have changed back; the ground follows.
            if self.doc.materials.is_some() {
                self.ground_stale = true;
                self.splat_stale = true;
            }
            let (grid, heights, grids, tint) = before;
            let same_colors = Option::zip(grids.as_ref(), self.doc.grids.as_ref())
                .map_or(grids.is_none() && self.doc.grids.is_none(), |(a, b)| Arc::ptr_eq(a, b))
                && Option::zip(tint.as_ref(), self.doc.tint.as_ref())
                    .map_or(tint.is_none() && self.doc.tint.is_none(), |(a, b)| Arc::ptr_eq(a, b))
                // Materials shading colors tiles from the material maps.
                && self.shading != Shading::Materials;
            if grid != self.doc.field.grid || !same_colors {
                self.rebuild_tiles(None);
            } else if let Some(rect) =
                changed_samples(grid, &heights.heights, &self.doc.field.heights)
            {
                // A sculpt step: the tiles under it, as a stroke updates them.
                if let Some(gpu) = &self.gpu_heights {
                    gpu.update(rect, &self.doc.field.heights);
                }
                self.rebuild_tiles(Some(rect));
            }
            // Props and objects may have moved back.
            self.scene_stale = true;
            self.river_written(left, redo);
        }
        done
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
        let textured = texture.is_some() || splat.is_some();
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
        let material = match &splat {
            Some(splat) => gpu::Material::Splat(splat.clone()),
            None => gpu::Material::Lit,
        };
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
        if self.is_new
            && let Some(warning) = self.prepare_new_island()
        {
            self.say(warning, true);
        }
        // Every stage starts from the same tree. Differential grid/object
        // writers must also be able to restore values after undoing a save.
        let mut source = self.source.clone();
        if let Some(store) = &mut self.object_store {
            store.objects.clone_from(&self.doc.objects);
            store.write_into(source.gamedata_mut())?;
        }
        self.derive_water();
        let levels = self.doc.write_into(&mut source)?;
        if let Some(grids) = &self.doc.grids {
            grids.apply(&mut source)?;
        }
        let base = island_data_dir(&self.path)?;
        let mut current = |rel: &str| {
            let path = format!("{base}/{rel}");
            self.terrain_original
                .entry(path.clone())
                .or_insert_with(|| {
                    project
                        .files
                        .get(&path)
                        .cloned()
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
        let coverage_changed = source.active_map().data != self.source.active_map().data;
        let tint = match (&self.doc.tint, &self.tint_loaded) {
            (Some(now), Some(loaded)) if Arc::ptr_eq(now, loaded) && !coverage_changed => None,
            (Some(now), _) => Some(anno_formats::tint::bake(
                self.game,
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
                .zip(self.doc.materials.as_ref())
                .map(|(p, m)| p.bake(&m.ids.grid(), &m.alphas.grid(), self.world()[0] as f32))
                .transpose()?;
            new::map_images(ground.as_ref().or(self.ground.as_deref()), &self.doc.field)
        } else {
            Vec::new()
        };
        let (gamedata, mut rd3d) = source.to_members()?;
        let grids_in_rd3d = self
            .doc
            .grids
            .as_ref()
            .is_some_and(|g| g.layers.iter().any(|l| l.is_edited() && l.in_rd3d()));
        if grids_in_rd3d || tint.is_some() {
            let mut tree = anno_formats::adf::Adf::decode(&rd3d)?;
            if let Some(grids) = &self.doc.grids {
                grids.write_rd3d(&mut tree)?;
            }
            if let Some(tint) = &tint {
                anno_formats::tint::write_rd3d(&mut tree, &tint.rd3d)?;
            }
            rd3d = tree.encode();
        }
        for tile in tint.as_ref().into_iter().flat_map(|t| &t.tiles) {
            let path = tile.path(&base);
            self.terrain_original
                .entry(path.clone())
                .or_insert_with(|| {
                    project
                        .files
                        .get(&path)
                        .cloned()
                        .or_else(|| install.read(&path).ok())
                });
        }
        for (rel, _) in &images {
            let path = format!("{base}/{rel}");
            self.terrain_original
                .entry(path.clone())
                .or_insert_with(|| {
                    project
                        .files
                        .get(&path)
                        .cloned()
                        .or_else(|| install.read(&path).ok())
                });
        }
        // A reverted node/page may no longer be emitted by a differential
        // writer. Restore its original file before overlaying this stage.
        for (path, bytes) in &self.terrain_original {
            match bytes {
                Some(bytes) => project.stage(path, bytes.clone()),
                None => {
                    project.files.remove(path);
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
        let tree = anno_formats::adf::Adf::decode(&rd3d)?;
        let flags = anno_formats::tint::read_rd3d(&tree)?.flags;
        let prefix = format!("{base}/tintmaps/");
        project.files.retain(|path, _| {
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
        if self.is_new || self.game.profile().info_from_island {
            self.stage_island_side_files(install, project, &source)?;
        }
        // Shipped islands store each member as its own zlib block.
        let mut packer = rda::Packer::new(rda::PackOptions::island());
        packer.add("gamedata.data", &gamedata)?;
        packer.add("rd3d.data", &rd3d)?;
        project.stage(&self.path, packer.to_vec()?);
        self.history.mark_saved();
        self.listing_dirty = false;
        Ok(())
    }

    /// Rebuilds the water render bytes that follow the coast before a save.
    /// The source keeps the loaded heights that the stored bytes belong to:
    /// only cells whose derived value changed are
    /// rewritten, so shipped bytes survive where the terrain did not move.
    /// New islands rebuild everything, their bytes came from the template.
    fn derive_water(&mut self) {
        let Some(g) = &mut self.doc.grids else { return };
        let saved = self.source.heights();
        let was = (!self.is_new && saved.data.len() == self.doc.field.heights.len()).then(|| {
            anno_formats::water::Heights {
                data: &saved.data,
                width: saved.width,
                spacing: self.doc.field.grid.spacing,
            }
        });
        if was.is_some_and(|w| w.data == self.doc.field.heights.as_slice()) {
            return;
        }
        anno_formats::water::derive(Arc::make_mut(g), new::heights(&self.doc.field), was, None);
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
        } else {
            let active = self.source.active_map();
            for (i, _) in active.data.iter().enumerate().filter(|(_, a)| **a) {
                include(i % active.width, i / active.width);
            }
            self.world()[0] as f32 / active.width.max(1) as f32
        };
        (hi[0].saturating_sub(lo[0]).max(hi[1].saturating_sub(lo[1])) as f32 * cell) as u32
    }

    /// `IslandPlaceholderSize` the land gives: `Small` … `XL`.
    pub fn land_class(&self) -> &'static str {
        self.game.profile().size_class(self.land_extent())
    }

    /// The renderer's smoothed frame times, for the viewport readout.
    pub fn frame_stats(&self) -> gpui_editor_kit::gpu::Stats {
        self.renderer.stats()
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
            Item::Prop(id) => self.doc.prop(id).map(|p| Group::Prop(p.source.file)),
            Item::Object(id) => (self.doc.objects.iter())
                .find(|o| o.id == id)
                .map(|o| Group::Object(o.guid)),
        }
    }

    pub fn region(&self) -> String {
        self.game.island_region(&self.path).to_owned()
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
        for p in Arc::make_mut(&mut self.doc.props).iter_mut() {
            let t = math::translation(&p.matrix);
            p.auto_cliff = entry
                .listing
                .auto_cliffs
                .iter()
                .any(|a| (0..3).all(|i| (a[i] - t[i]).abs() < 0.01));
        }
        self.is_new = true;
        self.source.mark_unshipped();
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
        use anno_formats::a7minfo::{
            A7mInfo, A7me, RandomSlotObject, Slots, TypedSlotGroup, TypedSlotObject,
        };
        let world = self.world();
        let stem = self.path.trim_end_matches(".a7m");
        let a7me = format!("{stem}.a7me");
        let active = source.active_map();
        let mut info = if self.game.profile().info_from_island {
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
        let lib = self.harbour.clone();
        let assets = self.assets.clone();
        info.slots = match &info.slots {
            Slots::Random(_) => {
                Slots::Random(harbour::slot_groups(&self.doc.objects, |g| self.is_slot(g)))
            }
            Slots::Typed(old) => {
                let mut known: std::collections::BTreeMap<_, _> = old
                    .iter()
                    .flat_map(|g| g.objects.iter().map(|o| (o.guid, g.kind)))
                    .collect();
                if let Some(lib) = &lib {
                    for group in lib.typed_slots(&self.doc.objects) {
                        known.extend(group.objects.iter().map(|o| (o.guid, group.kind)));
                    }
                }
                // Imported slot records may come from another island. Read
                // its small placement header for the engine's slot-kind ID.
                let mut missing: std::collections::BTreeSet<_> = self
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
                for o in self.doc.objects.iter() {
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
        let start_guid = lib
            .as_ref()
            .and_then(|l| l.guid(harbour::Piece::StartCoast));
        let start = self.doc.objects.iter().find(|o| {
            Some(o.guid) == start_guid
                || assets
                    .as_ref()
                    .and_then(|a| a.get(o.guid))
                    .is_some_and(|a| a.template == "Coastline" && a.name.contains("StartCoast"))
        });
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
            .retain(|id, _| self.doc.objects.iter().any(|o| o.id == *id));
        listing.auto_cliffs = self
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
        let before = self.doc.clone();
        match f(Arc::make_mut(&mut self.doc.field)) {
            Ok(mut done) => {
                // An island made here has nothing hand-made in its land
                // grids yet: land, active area and buildable areas follow
                // the new terrain in the same undo step.
                if self.is_new && self.doc.grids.is_some() {
                    let environment = self.land_environment();
                    new::derive_land(&mut self.doc, environment);
                    self.derive_areas_from_slope();
                    done += "; land and buildable areas follow it";
                }
                self.sync_land();
                self.checkpoint_owned(before);
                self.rebuild_tiles(None);
                self.say(done, false);
            }
            Err(e) => {
                self.doc = before;
                self.say(format!("Import failed: {e:#}"), true);
            }
        }
    }

    /// Island size in metres, as the tint and grid formats take it.
    pub fn world(&self) -> [u32; 2] {
        self.source.world_size()
    }

    /// Replaces the tint as one undo step and redraws.
    pub fn replace_tint(&mut self, tint: Result<TintMap>, done: String) {
        match tint {
            Ok(map) => {
                self.checkpoint();
                self.doc.tint = Some(Arc::new(map));
                self.layers.tint = true;
                self.rebuild_tiles(None);
                self.say(done, false);
            }
            Err(e) => self.say(format!("Tint failed: {e:#}"), true),
        }
    }

    pub fn export_heightmap(&mut self, path: &std::path::Path) {
        let mut field = (*self.doc.field).clone();
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
            self.gpu_heights = gpu::HeightMap::new(&self.doc.field)
                .map_err(|e| eprintln!("GPU heights unavailable, meshing on the CPU: {e:#}"))
                .ok()
                .map(Arc::new);
        }
        self.drawn_look = Some((self.overlay(), self.layers.tint));
        if self.gpu_heights.is_some() {
            return self.recolor_terrain(rect);
        }
        let grid = self.doc.field.grid;
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
        let grid = self.doc.field.grid;
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
    fn make_terrain(&self) -> Option<Arc<gpu::Terrain>> {
        let inputs = self.tile_inputs();
        Some(Arc::new(gpu::Terrain {
            heights: self.gpu_heights.clone()?,
            colors: self.terrain_colors.clone()?,
            color_gain: inputs.gain,
            // Overlays and other shadings recolor the ground; models keep
            // their own color then.
            tints: inputs.shading == Shading::Ground && inputs.overlay.is_none(),
            material: match &inputs.splat {
                Some(splat) => gpu::Material::Splat(splat.clone()),
                None => gpu::Material::Lit,
            },
            texture: inputs.texture.clone().map(Into::into),
        }))
    }

    /// What terrain tiles are built from, borrowed so that tiles can be
    /// built on several threads (the editor itself is not `Sync`).
    fn tile_inputs(&self) -> TileInputs<'_> {
        let splat = self
            .splat
            .clone()
            .filter(|_| self.shading == Shading::Ground);
        TileInputs {
            field: &self.doc.field,
            overlay: self
                .overlay()
                .and_then(|kind| self.doc.grids.as_ref()?.layer(kind)),
            tint: self.doc.tint.as_deref().filter(|_| self.layers.tint),
            materials: self.doc.materials.as_ref(),
            palette: self.palette.as_deref(),
            shading: self.shading,
            game: self.game,
            gain: self.color_gain(),
            texture: if splat.is_some() {
                None
            } else {
                self.texture()
            },
            splat,
            heights: self.gpu_heights.clone(),
        }
    }

    fn texture(&self) -> Option<Arc<Texture>> {
        match self.shading {
            Shading::Ground => self.ground.clone(),
            Shading::Map => self.map.clone(),
            Shading::Height | Shading::Materials => None,
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
        if self.shading == Shading::Ground && self.layers.tint && self.doc.tint.is_some() {
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

    fn refresh_scene(&mut self) {
        let _span = gpui_editor_kit::profile::span("island refresh_scene");
        for view in 0..self.views.len() {
            self.apply_gizmo(view);
        }
        self.markers_stale |= self.sync_selection();
        if Some((self.overlay(), self.layers.tint)) != self.drawn_look {
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
                terrain::ShippedMeshes::load(&install, &path, self.doc.field.grid.size()[0]);
        }
        if self.layers.shipped && self.shipped_meshes.is_empty() && self.shipped.len() > 0 {
            self.refresh_shipped_meshes();
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
        let size = self.doc.field.grid.size();
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
            self.vegetation = Some(preview);
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
                self.placements
                    .set_pieces(&self.library.models.loaded, &renderer);
            }
            // Every visible item with a model, then the vegetation preview.
            self.vegetation = self.vegetation_preview();
            let files: Vec<String> = (self.doc.meshes.iter())
                .map(|m| m.replace('\\', "/"))
                .collect();
            let hidden = &self.hidden_groups;
            let props = (self.doc.props.iter())
                .filter(|_| layers.props)
                .filter(|p| !hidden.contains(&Group::Prop(p.source.file)))
                .filter_map(|p| {
                    let cfg = files.get(p.source.file as usize)?;
                    Some((Some(Item::Prop(p.id)), cfg.clone(), p.matrix))
                });
            let assets = self.assets.clone();
            let objects = (self.doc.objects.iter())
                .filter(|_| layers.objects)
                .filter(|o| !hidden.contains(&Group::Object(o.guid)))
                .filter_map(|o| {
                    let cfgs = assets
                        .as_ref()
                        .and_then(|a| a.get(o.guid))
                        .map(|a| a.cfgs.as_slice())
                        .unwrap_or(&[]);
                    let cfg = o.model(cfgs)?.replace('\\', "/");
                    Some((
                        Some(Item::Object(o.id)),
                        cfg,
                        doc::trs(o.position, o.rotation, o.scale),
                    ))
                });
            let vegetation = self.vegetation.clone();
            let grown = vegetation.iter().flat_map(|p| {
                p.models
                    .iter()
                    .flat_map(|(cfg, ms)| ms.iter().map(move |m| (None, cfg.clone(), *m)))
            });
            let placed: Vec<(Option<Item>, String, Mat4)> =
                props.chain(objects).chain(grown).collect();
            self.undrawn = self.placements.set(
                placed.iter().map(|(i, c, m)| (*i, c.as_str(), *m)),
                &renderer,
            );
        }
        if std::mem::take(&mut self.inland_stale) || self.scene_stale {
            let curves: Vec<_> = (self.doc.objects.iter())
                .filter_map(|o| Some((o.path.as_deref()?, self.water_kind(o.guid)?)))
                .collect();
            self.inland_water = river::water_mesh(&curves);
        }
        // `set` lays out new rows: the selection names them again.
        self.sync_selection();
        self.scene_stale = false;
        self.markers_stale = false;
        let inland = self.inland_water.as_ref();
        let identity = self.identity.clone();
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
                [Some(&self.water), inland]
                    .into_iter()
                    .flatten()
                    .filter(|_| layers.water),
            )
            .map(|mesh| Batch::new(mesh.clone(), identity.clone()))
            .collect();
        // Markers for items no model draws, and rings on the selected ones
        // among them; the GPU rings the rest (`sync_selection`).
        let marked: BTreeSet<Item> = self
            .undrawn
            .iter()
            .chain(&self.selection_unplaced)
            .copied()
            .collect();
        // One pass over the document: before models load every item is
        // marked, and looking each up by id was quadratic (~90 ms at 16k).
        let props =
            (self.doc.props.iter()).map(|p| (Item::Prop(p.id), math::translation(&p.matrix)));
        let objects = (self.doc.objects.iter()).map(|o| (Item::Object(o.id), o.position));
        let markers = props
            .chain(objects)
            .filter(|(item, _)| marked.contains(item))
            .map(|(item, position)| {
                let selected = self.selection.contains(&item);
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
            })
            .collect();
        let ambient = (self.ambient_lights.iter())
            .find(|(name, _)| *name == self.doc.session.global_ambient)
            .map_or_else(vegetation::Ambient::default, |(_, a)| *a);
        let scene = Scene {
            batches,
            terrain: self.terrain.clone(),
            lines: Arc::new(self.water_lines()),
            markers: Arc::new(markers),
            light: Light {
                // From the south-west at the preset's elevation.
                sun: [
                    -0.67 * ambient.elevation.cos(),
                    ambient.elevation.sin(),
                    0.74 * ambient.elevation.cos(),
                ],
                sun_color: ambient.sun,
                sky: ambient.sky,
                ground: ambient.ground,
                // 4096² (64 MB a viewport): the map spans the whole island
                // and is not redrawn per camera move, so on a 768 m island
                // a texel is a quarter metre; a tight filter keeps trees'
                // shadows from smearing.
                shadows: layers.shadows.then_some(Shadows {
                    resolution: 4096,
                    softness: 1.,
                }),
                // Into the horizon's haze, so the island and the sea
                // beyond the water quad meet the sky without a line.
                fog: Some(Fog {
                    color: ambient.horizon,
                    start: self.extent().size * 1.5,
                    end: self.extent().size * 6.,
                }),
            },
            tone: Some(Tone {
                exposure: ambient.exposure,
                sun_occlusion: ambient.sun_occlusion,
            }),
            // Anno's SSAO (ambient_ssao.fx, AOIntensity ~1 in the presets),
            // reaching about a tree's foot and a rock's crevices.
            occlusion: Some(Occlusion {
                radius: 2.5,
                strength: 0.85,
            }),
            sky: Some(Sky {
                zenith: ambient.zenith,
                horizon: ambient.horizon,
                // Past the water quad (terrain::water): about what the
                // quad shows over deep floor.
                sea: Some(Sea {
                    level: 0.,
                    color: terrain::SEA_BODY,
                }),
            }),
        };
        self.renderer.set_scene(&scene);
    }

    // --- input ---------------------------------------------------------------

    fn ground(&self, view: usize, cursor: Point<Pixels>) -> Option<Vec3> {
        let v = &self.views[view];
        let ray = v.camera.ray(cursor, v.bounds, self.extent());
        self.doc.field.raycast(ray)
    }

    /// The selection's mean position, where the gizmo sits. Every paint
    /// asks: a few items look themselves up; more walk the props beside
    /// the selection, both sorted by id, as 20k lookups took 1.7 ms.
    fn pivot(&self) -> Option<Vec3> {
        let add = |(s, n): (Vec3, usize), p: Vec3| (math::add(s, p), n + 1);
        let props = &self.doc.props;
        let (sum, n) = if self.selection.len() < 64 || !props.windows(2).all(|w| w[0].id < w[1].id)
        {
            (self.selection.iter())
                .filter_map(|&item| self.doc.matrix(item))
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
            (self.doc.objects.iter())
                .filter(|o| self.selection.contains(&Item::Object(o.id)))
                .fold(acc, |acc, o| add(acc, o.position))
        };
        (n > 0).then(|| math::scale(sum, 1. / n as f32))
    }

    fn gizmo_frame(&self, view: usize) -> Option<gizmo::Frame> {
        if self.tool != Tool::Select {
            return None;
        }
        let v = &self.views[view];
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
                let Some(m) = &mut self.doc.materials else {
                    return false;
                };
                if !m.paint(center, &self.brush, self.paint) {
                    return false;
                }
                // The splat shows the paint on the next frame: recompute the
                // control texels under the brush in place.
                let r = self.brush.radius + 1.;
                if let (Some(palette), Some(splat)) = (&self.palette, &self.splat) {
                    let size = self.doc.field.grid.size()[0];
                    let area = [center[0] - r, center[1] - r, center[0] + r, center[1] + r];
                    palette.update_splat(splat, &m.ids, &m.alphas, size, area);
                }
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
                    let rect = self.doc.field.grid.footprint(center, self.brush.radius);
                    self.dirty = self.dirty.union(rect);
                }
                true
            }
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
                let rect = self.doc.field.grid.footprint(center, self.brush.radius);
                self.dirty = self.dirty.union(rect);
                true
            }
            Tool::Tint => {
                let Some(map) = &mut self.doc.tint else {
                    return false;
                };
                if !tint::paint(
                    Arc::make_mut(map),
                    center,
                    &self.brush,
                    self.tint_color,
                    invert,
                ) {
                    return false;
                }
                let rect = self.doc.field.grid.footprint(center, self.brush.radius);
                self.dirty = self.dirty.union(rect);
                true
            }
            Tool::Scatter => {
                if self.scatter.files.is_empty()
                    && let Some(file) = self.place
                {
                    self.scatter.files.push(file);
                }
                let n = if invert {
                    self.scatter.thin(&mut self.doc, center, &self.brush)
                } else {
                    self.scatter
                        .fill(&mut self.doc, center, &self.brush, &mut self.next_id)
                };
                if n == 0 {
                    return false;
                }
                // Props changed mid-stroke; the model batches must notice
                // before the checkpoint bumps the revision.
                self.moves += 1;
                self.scene_stale = true;
                if self.scatter.mark_trees && self.overlay() == Some(Kind::Trees) {
                    let rect = self.doc.field.grid.footprint(center, self.brush.radius);
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
                let rect = Arc::make_mut(&mut self.doc.field).sculpt_from(
                    center,
                    &self.brush,
                    op,
                    baseline,
                );
                match &self.gpu_heights {
                    Some(heights) if !rect.is_empty() => {
                        heights.update(rect, &self.doc.field.heights);
                        self.stale_colors = self.stale_colors.union(rect);
                    }
                    _ => self.dirty = self.dirty.union(rect),
                }
                if !rect.is_empty() {
                    let g = self.doc.field.grid;
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
                let lit = |id: u64| {
                    self.hover_water == Some(id) || self.selection.contains(&Item::Object(id))
                };
                self.markers_stale |= !rect.is_empty()
                    && (self.doc.objects.iter()).any(|o| {
                        o.path.is_some() && lit(o.id) && self.water_kind(o.guid) == Some(false)
                    });
                !rect.is_empty()
            }
        }
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
            MouseButton::Left if self.tool == Tool::Harbour => {
                ground.and_then(|at| self.harbour_press(view, event, at))
            }
            MouseButton::Left if self.tool == Tool::River => {
                ground.and_then(|at| self.river_press(view, event, at))
            }
            MouseButton::Left if self.tool == Tool::Slot => ground.and_then(|at| {
                if self.auto_region && !event.modifiers.shift {
                    return Some(Drag::Region { from: at, to: at });
                }
                self.slot_at(at, event.modifiers.shift);
                None
            }),
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
                        .filter_map(|&item| Some((item, self.doc.matrix(item)?)))
                        .collect(),
                    gesture,
                    before: Box::new(self.doc.clone()),
                    changed: false,
                    pending: None,
                    gpu: None,
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
                    before: Box::new(self.doc.clone()),
                    level: at[1],
                    changed: false,
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
            if let Some(m) = &self.doc.materials {
                self.paint = m.shown([at[0], at[2]], self.palette.as_deref());
                self.say(format!("Picked material {}", self.paint), false);
            }
        } else if self.tool == Tool::Grid {
            if let Some(layer) = self.doc.grids.as_ref().and_then(|g| g.layer(self.grid)) {
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
            if let Some(map) = &self.doc.tint {
                self.tint_color = tint::sample(map, at[0], at[2]);
                let [r, g, b] = self.tint_color;
                self.say(format!("Picked tint ×{r:.2} ×{g:.2} ×{b:.2}"), false);
            }
        } else {
            self.level = Some(at[1]);
            self.say(format!("Target height {:.2} m", at[1]), false);
        }
    }

    /// Adds the Place tool's prop at `at` with a random yaw, copying tint and
    /// flags from an existing instance of the same file.
    fn place_at(&mut self, at: Vec3) {
        // Cheap spread of yaws; reproducible per placement count.
        let yaw = (self.next_id as f32 * 2.399_963).rem_euclid(std::f32::consts::TAU);
        if let Some((template, config)) = self.place_object.clone() {
            let Some(store) = &mut self.object_store else {
                return self.say("This island has no object areas", true);
            };
            store.objects.clone_from(&self.doc.objects);
            let area = self
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
            Arc::make_mut(&mut self.doc.objects).push(o);
            self.selection = BTreeSet::from([Item::Object(id)]);
            self.scene_stale = true;
            return;
        }
        let Some(file) = self.place else {
            return self.say("Choose something in the Library first", true);
        };
        // A prop new to the island takes default settings.
        let mut source = scatter::template(&self.doc, file);
        source.position = at;
        source.rotation = [0., (yaw / 2.).sin(), 0., (yaw / 2.).cos()];
        self.checkpoint();
        let id = self.next_id;
        self.next_id += 1;
        Arc::make_mut(&mut self.doc.props).push(Prop {
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
    /// The island's prop file index of `path`, adding it to the file list
    /// when the island has never used that model.
    fn mesh_index(&mut self, path: &str) -> u32 {
        let index = match self
            .doc
            .meshes
            .iter()
            .position(|m| m.eq_ignore_ascii_case(path))
        {
            Some(i) => i,
            None => {
                Arc::make_mut(&mut self.doc.meshes).push(path.to_owned());
                self.doc.meshes.len() - 1
            }
        };
        index as u32
    }

    /// Adds the prop model `path` to the scatter set, or takes it out.
    /// True when it is in the set afterwards.
    pub fn toggle_scatter(&mut self, path: &str) -> bool {
        let file = self.mesh_index(path);
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
        self.scatter.files = paths.iter().map(|p| self.mesh_index(p)).collect();
    }

    /// Whether the prop model `path` is in the scatter set.
    pub fn in_scatter(&self, path: &str) -> bool {
        self.scatter.files.iter().any(|&f| {
            self.doc
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
        let kit = match cliffs::Kit::load(install, &self.region()) {
            Ok(kit) => kit,
            Err(e) => return self.say(format!("Auto cliffs: {e:#}"), true),
        };
        let before = self.doc.clone();
        let old = self.doc.props.len();
        if self.cliffs.replace {
            Arc::make_mut(&mut self.doc.props).retain(|p| !p.auto_cliff);
        }
        let removed = old - self.doc.props.len();
        let shape_of = |doc: &Doc, p: &Prop| {
            let cfg = doc.meshes.get(p.source.file as usize)?;
            kit.shapes
                .iter()
                .find(|s| s.cfg.eq_ignore_ascii_case(&cfg.replace('\\', "/")))
        };
        // Rocks of unknown size count as 4 m: the small end of the big
        // rocks, so hand-placed scatter does not block a whole face.
        let obstacles: Vec<cliffs::Obstacle> = self
            .doc
            .props
            .iter()
            .filter_map(|p| match shape_of(&self.doc, p) {
                Some(shape) => Some(cliffs::Obstacle::of(shape, &p.matrix)),
                None => {
                    let cfg = self.doc.meshes.get(p.source.file as usize)?;
                    let t = math::translation(&p.matrix);
                    cliffs::is_rock(cfg).then_some(cliffs::Obstacle {
                        at: [t[0], t[2]],
                        width: 4.,
                    })
                }
            })
            .collect();
        let placed = cliffs::plan(&self.doc.field, &kit, &self.cliffs, &obstacles);
        if placed.is_empty() && removed == 0 {
            self.doc = before;
            return self.say("No new spot steeper than 35° above sea for cliffs", true);
        }
        for p in &placed {
            let file = self.mesh_index(&kit.shapes[p.shape].cfg);
            // Retail cliffs and rocks never carry AdaptTerrainHeight and
            // always the neutral color.
            let source = anno_formats::island::Prop {
                file,
                position: p.position,
                rotation: p.rotation,
                scale: [p.scale; 3],
                color: [1.; 4],
                adapt_terrain_height: None,
            };
            Arc::make_mut(&mut self.doc.props).push(Prop {
                id: self.next_id,
                matrix: doc::trs(source.position, source.rotation, source.scale),
                source,
                auto_cliff: true,
            });
            self.next_id += 1;
        }
        let painted = self.paint_cliff_ground(&kit, &placed);
        let share = self.auto_cliff_coverage(&kit);
        self.checkpoint_owned(before);
        self.selection
            .retain(|item| self.doc.matrix(*item).is_some());
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
        let (Some(palette), Some(m)) = (self.palette.clone(), &mut self.doc.materials) else {
            return false;
        };
        let names: Vec<(u8, String)> = palette
            .materials
            .iter()
            .map(|(&id, mat)| (id, mat.file.clone()))
            .collect();
        let Some(id) = cliffs::ground_material(self.game, &names) else {
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
            for at in cliffs::paint_spots(&self.doc.field, center, radius) {
                m.paint(at, &dab, id);
            }
            lo = [lo[0].min(center[0] - radius), lo[1].min(center[1] - radius)];
            hi = [hi[0].max(center[0] + radius), hi[1].max(center[1] + radius)];
        }
        if placed.is_empty() {
            return false;
        }
        if let Some(splat) = &self.splat {
            let size = self.doc.field.grid.size()[0];
            palette.update_splat(splat, &m.ids, &m.alphas, size, [lo[0], lo[1], hi[0], hi[1]]);
        }
        // The cliff-top bake and map shading follow the new paint.
        self.ground_stale = true;
        true
    }

    /// Share of ground over 35° under the auto-placed pieces.
    fn auto_cliff_coverage(&self, kit: &cliffs::Kit) -> f32 {
        let boxes: Vec<_> = self
            .doc
            .props
            .iter()
            .filter(|p| p.auto_cliff)
            .filter_map(|p| {
                let cfg = self.doc.meshes.get(p.source.file as usize)?;
                let shape = kit
                    .shapes
                    .iter()
                    .find(|s| s.cfg.eq_ignore_ascii_case(cfg))?;
                Some((p.matrix, shape.center, shape.extent))
            })
            .collect();
        cliffs::coverage(&self.doc.field, &boxes)
    }

    pub fn choose_prop(&mut self, path: &str) {
        let index = self.mesh_index(path);
        self.place = Some(index);
        self.place_object = None;
        self.tool = Tool::Place;
    }

    /// Places copies of a shipped object record with the Place tool.
    pub fn choose_object(
        &mut self,
        template: anno_formats::objects::Template,
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
        self.harbour.is_none() && matches!(self.tool, Tool::Harbour | Tool::Slot | Tool::River)
    }

    /// Says why a tool has no shipped records yet. A failed load is not
    /// retried, so it must not read as "still loading" forever.
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
            let near = self.doc.objects.iter().position(|o| {
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
            let gone = Arc::make_mut(&mut self.doc.objects).remove(i);
            self.selection.remove(&Item::Object(gone.id));
            self.coast_point = None;
            self.refresh_areas();
            return self.say("Harbour spot removed", false);
        }
        let Some(curve) = harbour::trace_coast(&self.doc.field, [at[0], at[2]]) else {
            return self.say("Click closer to the shore: no coast within 24 m", true);
        };
        let before = self.doc.clone();
        if self.push_harbour(curve) {
            self.checkpoint_owned(before);
        }
    }

    /// Adds a harbour (or the start coast) along `curve`, without an undo
    /// step of its own. False when nothing was added.
    fn push_harbour(&mut self, curve: Vec<anno_formats::objects::PathPoint>) -> bool {
        let Some(lib) = self.harbour.clone() else {
            self.say_no_records("Harbour");
            return false;
        };
        let piece = if self.start_coast {
            harbour::Piece::StartCoast
        } else {
            harbour::Piece::Coast
        };
        let Some(guid) = lib.guid(piece) else {
            self.say("This province ships no start coast to copy", true);
            return false;
        };
        let Some(store) = &mut self.object_store else {
            self.say("This island has no object areas", true);
            return false;
        };
        store.objects.clone_from(&self.doc.objects);
        let areas = self.doc.grids.as_ref().and_then(|g| g.layer(Kind::AreaIds));
        let Some(mut o) = lib.place(store, piece, areas, curve[0].p) else {
            return false;
        };
        o.area = harbour::CONSTRUCTION as u16;
        let length = curve_length(&curve);
        o.path = Some(curve);
        // One start coast per island.
        if piece == harbour::Piece::StartCoast {
            Arc::make_mut(&mut self.doc.objects).retain(|x| x.guid != guid);
        }
        let id = o.id;
        Arc::make_mut(&mut self.doc.objects).push(o);
        self.refresh_areas();
        self.selection = BTreeSet::from([Item::Object(id)]);
        self.coast_point = None;
        self.scene_stale = true;
        let coasts = self.coast_guids();
        let count = self
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

    /// Press with the Harbour tool: pick a point handle, select a harbour or
    /// add a point to it, start drawing one, or start a region.
    fn harbour_press(&mut self, view: usize, event: &MouseDownEvent, at: Vec3) -> Option<Drag> {
        if event.modifiers.shift {
            self.harbour_at(at, true);
            return None;
        }
        if self.auto_region {
            return Some(Drag::Region { from: at, to: at });
        }
        if let Some((id, index)) = self.coast_handle_at(view, event.position) {
            self.coast_point = Some((id, index));
            self.selection = BTreeSet::from([Item::Object(id)]);
            return Some(Drag::CoastPoint {
                id,
                index,
                before: Box::new(self.doc.clone()),
                changed: false,
            });
        }
        self.coast_point = None;
        if let Some((id, segment)) = self.coast_segment_at(view, event.position) {
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
    pub(super) fn coast_handles(&self, view: usize) -> Vec<(u64, usize, Point<Pixels>)> {
        let v = &self.views[view];
        let extent = self.extent();
        let coasts = self.coast_guids();
        self.doc
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

    fn coast_handle_at(&self, view: usize, at: Point<Pixels>) -> Option<(u64, usize)> {
        // Handles are drawn 6 px across; give the cursor a little more.
        const HIT: f32 = 9.;
        self.coast_handles(view)
            .into_iter()
            .map(|(id, i, p)| (id, i, f32::from(p.x - at.x).hypot(f32::from(p.y - at.y))))
            .filter(|&(.., d)| d < HIT)
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(id, i, _)| (id, i))
    }

    /// The harbour segment under the cursor: object, and the index of the
    /// segment's first point.
    fn coast_segment_at(&self, view: usize, at: Point<Pixels>) -> Option<(u64, usize)> {
        const HIT: f32 = 8.;
        let handles = self.coast_handles(view);
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

    /// Rewrites one harbour's points through `f` on their ground positions,
    /// keeping heights on the waterline and `d` toward the sea.
    fn edit_coast(&mut self, id: u64, f: impl FnOnce(&mut Vec<[f32; 2]>)) {
        let Some(o) = Arc::make_mut(&mut self.doc.objects)
            .iter_mut()
            .find(|o| o.id == id)
        else {
            return;
        };
        let Some(path) = &o.path else { return };
        let mut ground: Vec<[f32; 2]> = path.iter().map(|p| [p.p[0], p.p[2]]).collect();
        f(&mut ground);
        let curve = harbour::with_directions(&self.doc.field, &ground);
        o.position = curve[0].p;
        o.path = Some(curve);
        self.scene_stale = true;
    }

    fn insert_coast_point(&mut self, id: u64, segment: usize, at: Vec3) {
        let Some(p) = harbour::snap_to_shore(&self.doc.field, [at[0], at[2]]) else {
            return self.say("No shore there to add a point on", true);
        };
        self.checkpoint();
        self.edit_coast(id, |pts| pts.insert(segment + 1, p));
        self.coast_point = Some((id, segment + 1));
        self.refresh_areas();
        self.say("Point added; drag it along the shore", false);
    }

    fn remove_coast_point(&mut self) {
        let Some((id, index)) = self.coast_point else {
            return;
        };
        let len = self
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
        self.coast_point = None;
        self.refresh_areas();
    }

    /// Turns the seaward side of the selected harbours around, for curves
    /// where the shore test picked the wrong side.
    pub fn flip_selected_coasts(&mut self) {
        let coasts = self.coast_guids();
        let selection = self.selection.clone();
        self.checkpoint();
        for o in Arc::make_mut(&mut self.doc.objects)
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
        let layer = self.doc.grids.as_ref()?.layer(Kind::AreaIds)?;
        let h = |x: f32, z: f32| self.doc.field.height([x, z]);
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
        self.doc
            .objects
            .iter()
            .any(|o| coasts.contains(&o.guid) && self.selection.contains(&Item::Object(o.id)))
    }

    /// Auto placement in a dragged region, for the active tool; one undo
    /// step.
    fn auto_place(&mut self, region: harbour::Region) {
        let count = self.auto_count as usize;
        let before = self.doc.clone();
        let placed = if self.tool == Tool::Harbour {
            let coasts = self.coast_guids();
            let existing: Vec<Vec<anno_formats::objects::PathPoint>> = self
                .doc
                .objects
                .iter()
                .filter(|o| coasts.contains(&o.guid))
                .filter_map(|o| o.path.clone())
                .collect();
            let refs: Vec<&[anno_formats::objects::PathPoint]> =
                existing.iter().map(Vec::as_slice).collect();
            let curves = harbour::auto_harbours(&self.doc.field, region, &refs, count);
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
            let Some(lib) = self.harbour.clone() else {
                return self.say_no_records("Slot");
            };
            let Some(slot_guid) = lib.guid(self.resource.pieces(self.fixed_slots).0) else {
                return self.say("This province ships no slot of that kind", true);
            };
            let existing: Vec<[f32; 2]> = self
                .doc
                .objects
                .iter()
                .filter(|o| o.guid == slot_guid)
                .map(|o| [o.position[0], o.position[2]])
                .collect();
            let river = self.doc.grids.as_ref().and_then(|g| g.layer(Kind::River));
            let environment = self
                .doc
                .grids
                .as_ref()
                .and_then(|g| g.layer(Kind::Environment));
            let spots = harbour::auto_slots(
                &self.doc.field,
                river,
                environment,
                region,
                self.resource,
                &existing,
                count,
            );
            if spots.is_empty() {
                return self.say(
                    match self.resource {
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
        placed: Vec<(&anno_formats::objects::Template, [f32; 3], u32)>,
    ) -> usize {
        let Some(store) = &mut self.object_store else {
            self.say("This island has no object areas", true);
            return 0;
        };
        let before = self.doc.clone();
        Arc::make_mut(&mut self.doc.objects)
            .retain(|o| !(guids.contains(&o.guid) && is_generated(o.id)));
        store.objects.clone_from(&self.doc.objects);
        let areas = self.doc.grids.as_ref().and_then(|g| g.layer(Kind::AreaIds));
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
            Arc::make_mut(&mut self.doc.objects).push(o);
        }
        let doc = &self.doc;
        self.selection.retain(|&item| doc.matrix(item).is_some());
        if self.doc.objects != before.objects {
            self.checkpoint_owned(before);
        }
        self.scene_stale = true;
        n
    }

    /// Sound spots and build blockers for the overlay: position, blocker
    /// side in metres (0 for a spot), and whether the editor generated it.
    pub(super) fn spot_marks(&self) -> Vec<([f32; 3], f32, bool)> {
        self.doc
            .objects
            .iter()
            .filter_map(|o| {
                let side = if o.guid == blockers::BLOCKER {
                    *blockers::SIZES.get(o.variation as usize)? as f32
                } else if sound::guids(self.game).contains(&o.guid)
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
        let name = |g: u32| {
            self.assets
                .as_ref()
                .and_then(|a| a.get(g))
                .map_or(String::new(), |a| a.name.clone())
        };
        let curves: Vec<_> = self
            .doc
            .objects
            .iter()
            .filter(|o| coasts.contains(&o.guid))
            .collect();
        let starts = curves
            .iter()
            .filter(|o| name(o.guid).contains("StartCoast"))
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
        if self.listing.is_start() {
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
        self.harbour.as_ref().is_some_and(|l| l.is_slot(guid))
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
        let o = self.doc.objects.iter().find(|o| o.id == id)?;
        if self.game.profile().slots == anno_formats::game::ResourceSlots::Random {
            let asset = self.assets.as_ref()?.get(o.guid)?;
            if asset.template != "Slot" || asset.slot_type.as_deref() != Some("Random") {
                return None;
            }
            let kind = if let Some(kind) = self
                .harbour
                .as_ref()
                .and_then(|lib| lib.resource_of(o.guid))
            {
                kind
            } else if asset.slot_type.as_deref() == Some("River")
                || asset.name.to_ascii_lowercase().contains("river")
            {
                harbour::Resource::River
            } else if asset.slot_type.as_deref() == Some("Clay") || asset.name.contains("clay") {
                harbour::Resource::Clay
            } else if asset.slot_type.as_deref() == Some("Oil") || asset.name.contains("oil pump") {
                harbour::Resource::Oil
            } else if asset.slot_type.as_deref() == Some("Gas") || asset.name.contains("gas arctic")
            {
                harbour::Resource::Gas
            } else {
                harbour::Resource::Mine
            };
            return Some((id, kind));
        }
        Some((id, self.harbour.as_ref()?.resource_of(o.guid)?))
    }

    /// Gives the selected slot a deposit for the test map, or clears it.
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
    }

    /// Slots with a test-map deposit: position and deposit name.
    pub(super) fn slot_labels(&self) -> Vec<([f32; 3], String)> {
        let Some(assets) = &self.assets else {
            return Vec::new();
        };
        self.doc
            .objects
            .iter()
            .filter_map(|o| {
                let g = self.listing.slot_resources.get(&o.id)?;
                Some((o.position, fertility::label(assets, *g)))
            })
            .collect()
    }

    /// Places a slot and its work area at `at`, or removes the ones there.
    fn slot_at(&mut self, at: Vec3, remove: bool) {
        let Some(lib) = self.harbour.clone() else {
            return self.say_no_records("Slot");
        };
        // Removal takes random and fixed slots of the resource alike.
        let ((slot, area), (fixed, _)) = (self.resource.pieces(false), self.resource.pieces(true));
        let guids: Vec<u32> = [Some(slot), Some(fixed), area]
            .iter()
            .flatten()
            .filter_map(|&p| lib.guid(p))
            .collect();
        // A click on a slot selects it, for the resource picker, instead of
        // stacking a second one there.
        if !remove
            && let Some(o) = self.doc.objects.iter().find(|o| {
                lib.is_slot(o.guid) && (o.position[0] - at[0]).hypot(o.position[2] - at[2]) < 4.
            })
        {
            self.selection = BTreeSet::from([Item::Object(o.id)]);
            return self.say("Slot selected; pick its test-map resource above", false);
        }
        if remove {
            let before = self.doc.objects.len();
            let snapshot = self.doc.clone();
            Arc::make_mut(&mut self.doc.objects).retain(|o| {
                !(guids.contains(&o.guid)
                    && (o.position[0] - at[0]).hypot(o.position[2] - at[2]) < 6.)
            });
            if self.doc.objects.len() == before {
                return self.say("No slot of that kind there", true);
            }
            let doc = &self.doc;
            self.selection.retain(|&item| doc.matrix(item).is_some());
            self.checkpoint_owned(snapshot);
            self.scene_stale = true;
            return self.say("Slot removed", false);
        }
        let before = self.doc.clone();
        if self.push_slot(at) {
            self.checkpoint_owned(before);
            self.say(
                "Slot placed; pick a test-map resource above, or let the map generator decide",
                false,
            );
        }
    }

    /// Adds a slot of the chosen resource and its work area at `at`,
    /// without an undo step of its own. False when nothing was added.
    fn push_slot(&mut self, at: Vec3) -> bool {
        let Some(lib) = self.harbour.clone() else {
            self.say_no_records("Slot");
            return false;
        };
        let (slot, area) = self
            .resource
            .pieces(self.fixed_slots && self.resource.has_fixed());
        let Some(store) = &mut self.object_store else {
            self.say("This island has no object areas", true);
            return false;
        };
        store.objects.clone_from(&self.doc.objects);
        let areas = self.doc.grids.as_ref().and_then(|g| g.layer(Kind::AreaIds));
        let Some(s) = lib.place(store, slot, areas, at) else {
            self.say("This province ships no slot of that kind", true);
            return false;
        };
        // The work area's ID must not collide with the slot's.
        store.objects.push(s.clone());
        let work = harbour::work_area_position(&self.doc.field, at);
        let w = area
            .and_then(|area| lib.place(store, area, areas, work))
            .map(|mut w| {
                harbour::link_work_area(&mut w, at);
                w
            });
        store.objects.pop();
        let id = s.id;
        Arc::make_mut(&mut self.doc.objects).push(s);
        Arc::make_mut(&mut self.doc.objects).extend(w);
        self.selection = BTreeSet::from([Item::Object(id)]);
        self.scene_stale = true;
        true
    }

    /// GUIDs of coastline assets, from the library or assets.xml.
    pub(super) fn coast_guids(&self) -> Vec<u32> {
        if let Some(lib) = &self.harbour {
            return [harbour::Piece::Coast, harbour::Piece::StartCoast]
                .iter()
                .filter_map(|&p| lib.guid(p))
                .collect();
        }
        self.doc
            .objects
            .iter()
            .map(|o| o.guid)
            .filter(|&g| {
                self.assets
                    .as_ref()
                    .and_then(|a| a.get(g))
                    .is_some_and(|a| a.template == "Coastline")
            })
            .collect()
    }

    /// Synchronizes terrain-derived cells after sculpting/import, preserving
    /// manually painted cells whose terrain classification did not change.
    /// The land grids follow the heights now, taking over from a land job
    /// still running (its result is dropped).
    pub(super) fn sync_land(&mut self) {
        if let Some((_, was)) = self.land_job.take() {
            self.doc.land_heights = was;
        }
        self.land_stale = false;
        let coasts = self.coast_paths();
        let environment = self.land_environment();
        if new::sync_land(&mut self.doc, &coasts, environment) {
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
        if Arc::ptr_eq(&self.doc.land_heights, &self.doc.field) {
            return None;
        }
        let grids = self.doc.grids.clone()?;
        let was = std::mem::replace(&mut self.doc.land_heights, self.doc.field.clone());
        let now = self.doc.land_heights.clone();
        let (grid, coasts) = (self.doc.field.grid, self.coast_paths());
        let environment = self.land_environment().to_owned();
        self.land_jobs += 1;
        self.land_job = Some((self.land_jobs, was.clone()));
        Some((self.land_jobs, move || {
            new::LandChanges::between(
                grid,
                &was.heights,
                &now.heights,
                &grids,
                &coasts,
                &environment,
            )
        }))
    }

    /// Applies land job `job`'s changes, unless `sync_land` took over.
    pub fn land_synced(&mut self, job: u64, changes: Option<new::LandChanges>) {
        if self.land_job.as_ref().is_none_or(|(j, _)| *j != job) {
            return;
        }
        self.land_job = None;
        if let (Some(changes), Some(grids)) = (changes, &mut self.doc.grids) {
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
    fn coast_paths(&self) -> Vec<Vec<anno_formats::objects::PathPoint>> {
        let guids = self.coast_guids();
        (self.doc.objects.iter())
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
        let paths: Vec<Vec<anno_formats::objects::PathPoint>> = self
            .doc
            .objects
            .iter()
            .filter(|o| coasts.contains(&o.guid))
            .filter_map(|o| o.path.clone())
            .collect();
        let Some(grids) = &mut self.doc.grids else {
            return;
        };
        let grids = Arc::make_mut(grids);
        let Some(ground) = grids.layer(Kind::Water).cloned() else {
            return;
        };
        let Some(areas) = grids.layer_mut(Kind::AreaIds) else {
            return;
        };
        let refs: Vec<&[anno_formats::objects::PathPoint]> =
            paths.iter().map(Vec::as_slice).collect();
        let interior = if from_slope {
            harbour::Interior::Slope(&self.doc.field)
        } else {
            harbour::Interior::Keep
        };
        harbour::derive_areas(&ground, areas, &refs, interior, None);
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
        let marsh = self
            .palette
            .as_ref()
            .is_some_and(|p| p.materials.values().any(|m| m.file.contains("marsh")));
        let doc = &mut self.doc;
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
                        .is_some_and(|m| landscape::is_harvestable_tree(self.game, m))
                })
                .map(|p| {
                    let t = math::translation(&p.matrix);
                    [t[0], t[2]]
                })
                .collect(),
            marsh,
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
        self.refresh_areas();
        let coasts = self.coast_guids();
        let first = self
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
        let lib = self.harbour.clone()?;
        let marker = lib.guid(harbour::Piece::Marker)?;
        if self.doc.objects.iter().any(|o| o.guid == marker) {
            return None;
        }
        // Shipped markers sit just off a harbour, at sea level.
        let mid = curve[curve.len() / 2];
        let d = mid.d.unwrap_or([0., -1.]);
        let at = [mid.p[0] + d[0] * 6., 0.3, mid.p[2] + d[1] * 6.];
        let store = self.object_store.as_mut()?;
        store.objects.clone_from(&self.doc.objects);
        let areas = self.doc.grids.as_ref().and_then(|g| g.layer(Kind::AreaIds));
        let o = lib.place(store, harbour::Piece::Marker, areas, at)?;
        Arc::make_mut(&mut self.doc.objects).push(o);
        None
    }

    fn continue_brush(&mut self, drag: &mut Drag, at: Vec3, invert: bool) {
        let Drag::Brush {
            stroke,
            level,
            changed,
            before,
        } = drag
        else {
            return;
        };
        // Quarter-radius spacing keeps strokes even without over-applying.
        // Absolute mode measures against the heights from stroke start.
        for dab in stroke.to([at[0], at[2]], self.brush.radius * 0.25) {
            let y = self.doc.field.height(dab);
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
                    *preview =
                        harbour::trace_between(&self.doc.field, [from[0], from[2]], [at[0], at[2]]);
                }
            }
            Drag::Region { to, .. } => {
                if let Some(at) = self.cursor {
                    *to = at;
                }
            }
            &mut Drag::RiverKnot { index, handle } => {
                if let Some(at) = self.cursor {
                    self.river_drag(index, handle, at);
                }
            }
            Drag::CoastPoint {
                id, index, changed, ..
            } => {
                if let Some(at) = self.cursor
                    && let Some(p) = harbour::snap_to_shore(&self.doc.field, [at[0], at[2]])
                {
                    let i = *index;
                    self.edit_coast(*id, |pts| pts[i] = p);
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
        let Some(Drag::Gizmo {
            gesture,
            targets,
            changed,
            pending,
            gpu,
            ..
        }) = &mut self.views[view].drag
        else {
            return;
        };
        let Some((position, snap)) = pending.take() else {
            return;
        };
        let Some(delta) = gesture.delta(position, snap.then_some(self.snap_step)) else {
            return;
        };
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
        for (item, start) in &cpu {
            let m = delta.apply(start, pivot);
            // Props thin the vegetation around them, where they were and
            // where they land.
            if let Item::Prop(_) = item
                && let Some(was) = self.doc.matrix(*item)
            {
                for p in [was, m] {
                    let r = vegetation::prop_reach(math::length([p[0], p[1], p[2]]));
                    self.vegetation_changed([p[12] - r, p[14] - r, p[12] + r, p[14] + r]);
                }
            }
            self.doc.set_matrix(*item, m);
            // Objects store TRS: read back what was kept.
            if let Some(m) = self.doc.matrix(*item) {
                moved.push((*item, m));
            }
        }
        if let Some(Drag::Gizmo { targets: t, .. }) = &mut self.views[view].drag {
            *t = targets;
        }
        if moved.is_empty() {
            return;
        }
        self.placements.move_items(&mut moved, &renderer);
        // Curves move with their object (`set_matrix`). `moved` is sorted
        // now.
        self.inland_stale |= (self.doc.objects.iter()).any(|o| {
            o.path.is_some()
                && moved
                    .binary_search_by_key(&Item::Object(o.id), |(i, _)| *i)
                    .is_ok()
        });
        self.moves += 1;
        self.markers_stale = true;
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
            for (item, start) in &targets {
                if self.selection_unplaced.binary_search(item).is_ok() {
                    continue;
                }
                self.doc.set_matrix(*item, math::multiply(&m, start));
                if let Some(m) = self.doc.matrix(*item) {
                    moved.push((*item, m));
                }
            }
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
                self.ground_stale |= self.tool == Tool::Paint;
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
                    let before = self.doc.clone();
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
            Some(Drag::Region { from, to }) => {
                self.auto_place(harbour::Region::spanning(
                    [from[0], from[2]],
                    [to[0], to[2]],
                ));
            }
            Some(Drag::CoastPoint {
                before,
                changed: true,
                ..
            }) => {
                self.checkpoint_owned(*before);
                self.refresh_areas();
            }
            Some(Drag::RiverKnot { .. }) => self.river_moved(),
            Some(Drag::Marquee(m)) => {
                let v = &self.views[view];
                let extent = self.extent();
                let layers = self.layers;
                // Hidden layers are not selectable.
                let props = self.doc.items().filter(|(item, _)| {
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
            "delete" | "backspace" if self.tool == Tool::Harbour && self.coast_point.is_some() => {
                self.remove_coast_point()
            }
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
                        Some(Drag::Brush { before, .. } | Drag::Gizmo { before, .. }) => {
                            Some(*before)
                        }
                        _ => None,
                    })
                    .collect();
                // A GPU drag's rows go back to the table, which still holds
                // the starts.
                self.renderer.drag_selection(None);
                if let Some(before) = befores.into_iter().next() {
                    self.doc = before;
                    self.dirty = Rect::default();
                    self.rebuild_tiles(None);
                }
                // Drops a river draft; nothing of it is in the document yet.
                self.river = None;
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
                self.grid_value = (value.max(0.) as u32).min(grids::max_value(self.game, self.grid))
            }
            Field::Position(_) | Field::Yaw | Field::Scale => {
                if self.selection.is_empty() {
                    return;
                }
                self.checkpoint();
                for &item in &self.selection {
                    let Some(mut m) = self.doc.matrix(item) else {
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
                    self.doc.set_matrix(item, m);
                }
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
            Field::GridValue => (0., grids::max_value(self.game, self.grid) as f32, false),
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
        let v = if matches!(field, Field::GridValue | Field::Jitter | Field::Inner) {
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
            Field::Position(_) | Field::Yaw | Field::Scale => {
                let Some(m) = self.selection.iter().find_map(|&i| self.doc.matrix(i)) else {
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
        let (center, size) = match self.doc.field.bounds_above(0.5) {
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
        let selection = &self.selection;
        Arc::make_mut(&mut self.doc.props).retain(|p| !selection.contains(&Item::Prop(p.id)));
        Arc::make_mut(&mut self.doc.objects).retain(|o| !selection.contains(&Item::Object(o.id)));
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
            .doc
            .props
            .iter()
            .filter(|p| selection.contains(&Item::Prop(p.id)))
            .cloned()
            .collect();
        for mut p in copies {
            p.id = self.next_id;
            self.next_id += 1;
            // A copy is the user's own; a rerun of Auto cliffs keeps it.
            p.auto_cliff = false;
            // Offset copies so they are visible and grabbable.
            p.matrix[12] += 4.;
            self.selection.insert(Item::Prop(p.id));
            Arc::make_mut(&mut self.doc.props).push(p);
        }
        // Objects get fresh ids from the store so they stay unique on save.
        if let Some(store) = &mut self.object_store {
            store.objects.clone_from(&self.doc.objects);
            let picked: Vec<usize> = (0..store.objects.len())
                .filter(|&i| selection.contains(&Item::Object(store.objects[i].id)))
                .collect();
            for i in picked {
                let mut copy = store.duplicate(i);
                copy.position[0] += 4.;
                self.selection.insert(Item::Object(copy.id));
                store.objects.push(copy.clone());
                Arc::make_mut(&mut self.doc.objects).push(copy);
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
    overlay: Option<&'a anno_formats::grids::Layer>,
    /// The working tint, when shown.
    tint: Option<&'a anno_formats::tint::TintMap>,
    materials: Option<&'a Materials>,
    palette: Option<&'a crate::terrain::Palette>,
    shading: Shading,
    game: anno_formats::game::Game,
    gain: f32,
    texture: Option<Arc<Texture>>,
    splat: Option<Arc<Splat>>,
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
                        let c = self.terrain_color(p, self.overlay, self.gain);
                        texel[..3].copy_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8]);
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
        let (splat, texture) = (self.splat.clone(), self.texture.clone());
        // Displaced tiles take height and normals from the GPU map.
        let normals = if self.heights.is_some() {
            Vec::new()
        } else {
            tile.samples.iter().map(|&i| self.normal_at(i)).collect()
        };
        Arc::new(
            Mesh::new(MeshData {
                positions: tile.positions,
                uvs: if texture.is_some() || splat.is_some() {
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
                material: match splat {
                    Some(splat) => gpu::Material::Splat(splat),
                    None => gpu::Material::Lit,
                },
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
        overlay: Option<&anno_formats::grids::Layer>,
        gain: f32,
    ) -> u32 {
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
                Some(m) => rgb(material_color(m.shown([p[0], p[2]], self.palette))),
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
        if let Some(layer) = overlay.filter(|l| l.kind == Kind::AreaIds) {
            if let Some((color, alpha)) = buildable_overlay(layer, p) {
                c = lerp(c, color, alpha);
            }
        } else if let Some(layer) = overlay
            && let Some((color, alpha)) =
                grids::overlay(self.game, layer.kind, grids::value_at(layer, p[0], p[2]))
        {
            // Textured ground multiplies the color; lift it so it reads.
            let lift = if matches!(self.shading, Shading::Ground | Shading::Map) {
                1.6
            } else {
                1.
            };
            c = lerp(c, color.map(|v| v * lift), alpha);
        }
        let [r, g, b] = c.map(|v| ((v / gain).clamp(0., 1.) * 255.).round() as u32);
        r << 16 | g << 8 | b
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

/// The sample rectangle (half-open) where two height arrays of `grid`
/// differ, `None` when they are equal.
fn changed_samples(grid: Grid, a: &[f32], b: &[f32]) -> Option<Rect> {
    let mut rect: Option<Rect> = None;
    for (z, (ra, rb)) in a.chunks(grid.width).zip(b.chunks(grid.width)).enumerate() {
        if ra == rb {
            continue;
        }
        let x0 = ra.iter().zip(rb).position(|(p, q)| p != q).unwrap_or(0);
        let x1 = grid.width
            - ra.iter()
                .zip(rb)
                .rev()
                .position(|(p, q)| p != q)
                .unwrap_or(0);
        let r = rect.get_or_insert(Rect {
            x0,
            z0: z,
            x1,
            z1: z + 1,
        });
        r.x0 = r.x0.min(x0);
        r.x1 = r.x1.max(x1);
        r.z1 = z + 1;
    }
    rect
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
    use anno_formats::{island, vfs::Install};
    use gpui_editor_kit::heightfield::{Brush, Falloff};
    use std::sync::Arc;

    /// The editor's ground materials by palette id and file.
    fn material_names(editor: &IslandEditor) -> Vec<(u8, String)> {
        (editor.palette.as_ref().unwrap().materials.iter())
            .map(|(&id, m)| (id, m.file.clone()))
            .collect()
    }

    #[test]
    fn sculpted_coast_updates_gameplay_without_erasing_other_grid_edits() {
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = new_island(&install, "moderate", 192, Start::Sea, "CoastSync");
        let grids = Arc::make_mut(editor.doc.grids.as_mut().unwrap());
        grids.layer_mut(Kind::Environment).unwrap().set(0, 0, 7);
        grids.layer_mut(Kind::River).unwrap().set(5, 5, 1);
        let before = editor.doc.clone();
        let width = editor.doc.field.grid.width;
        let heights = &mut Arc::make_mut(&mut editor.doc.field).heights;
        for z in 144..240 {
            for x in 144..240 {
                heights[z * width + x] = 4.;
            }
        }
        editor.history.checkpoint_owned(before);
        let (_config, mut project) = staged(&install, &mut editor);
        let at_centre = |e: &IslandEditor, kind| grids::value_at(layer(&e.doc, kind), 96., 96.);
        assert_eq!(at_centre(&editor, Kind::Water), 1);
        assert_eq!(
            at_centre(&editor, Kind::AreaIds),
            super::harbour::CONSTRUCTION
        );
        assert_eq!(layer(&editor.doc, Kind::Environment).get(0, 0), 7);
        assert_eq!(layer(&editor.doc, Kind::River).get(5, 5), 1);
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
            let game = install.game();
            let mut editor = IslandEditor::load(&install, path).unwrap();
            let before = editor.doc.clone();
            let at = [160., 160.];
            let width = editor.doc.field.grid.width;
            let heights = &mut Arc::make_mut(&mut editor.doc.field).heights;
            for (i, h) in heights.iter_mut().enumerate() {
                if (318..323).contains(&(i % width)) && (318..323).contains(&(i / width)) {
                    *h += 1.5;
                }
            }
            Arc::make_mut(&mut editor.doc.objects)[0].position[1] += 1.;
            let active = Arc::make_mut(editor.doc.grids.as_mut().unwrap())
                .layer_mut(Kind::ActiveMap)
                .unwrap();
            active.set(0, 0, 1 - active.get(0, 0));
            editor.brush.radius = 3.;
            editor.brush.strength = 1.;
            editor.tool = Tool::Paint;
            editor.paint = 31;
            editor.dab([at[0], editor.doc.field.height(at), at[1]], 0., false, None);
            editor.tool = Tool::Tint;
            editor.tint_color = tint::PRESETS[1].1;
            editor.dab([at[0], editor.doc.field.height(at), at[1]], 0., false, None);
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
            install.mount_mod(&project.folder()).unwrap();
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
                back.doc.field.height(at),
                before.field.height(at),
                "{game:?}: height undo"
            );
            assert_eq!(
                back.doc.objects[0].position, before.objects[0].position,
                "{game:?}: object undo"
            );
            assert_eq!(
                layer(&back.doc, Kind::ActiveMap).get(0, 0),
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
        let mut editor = IslandEditor::load(&install, MODERATE_M_01).unwrap();
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
        let channels = default_channels(&material_names(&editor));
        assert!(channels.iter().all(Option::is_some));
        let grass = channels[3].unwrap();
        let mut link = Link::new(exports.path().to_owned());
        link.channels = vec![channels];
        editor.set_listing(|l| l.world_machine = Some(link));
        let old_height = editor.doc.field.height([160., 160.]);
        let imported = editor.wm_job(true).unwrap()().unwrap();
        assert!(
            imported.heights.is_some() && imported.materials.is_some() && imported.tint.is_some()
        );
        editor.apply_wm(Ok(imported));
        let height = editor.doc.field.height([160., 160.]);
        assert!((height - 40.).abs() < 0.01);
        let dominant = |e: &IslandEditor| e.doc.materials.as_ref().unwrap().dominant([160., 160.]);
        assert_eq!(dominant(&editor), grass);
        assert!(editor.undo(false));
        assert_eq!(editor.doc.field.height([160., 160.]), old_height);
        assert!(editor.undo(true));
        let (_config, mut project) = staged(&install, &mut editor);
        let back = reopen(&install, &mut project, MODERATE_M_01);
        assert!((back.doc.field.height([160., 160.]) - height).abs() < 1. / 128.);
        assert_eq!(back.doc.materials.as_ref().unwrap().per_metre, 2.);
        assert_eq!(dominant(&back), grass);
        assert!(back.doc.tint.is_some());
        assert!(!project.files.keys().any(|p| p.contains("materialmaps/")));
    }

    #[test]
    fn installed_1800_land_class_follows_unsaved_active_area() {
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = IslandEditor::load(&install, MODERATE_M_01).unwrap();
        let layer = Arc::make_mut(editor.doc.grids.as_mut().unwrap())
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
        let skeletons = super::skeletons(&install, Some(&assets));
        let mut qa = std::env::var_os("ANNO_EDITOR_QA_EXPORT").map(|dir| {
            let mut project = Project::load(std::path::Path::new(&dir), install.game());
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
                (editor.doc.grids.as_ref().unwrap())
                    .layer(Kind::AreaIds)
                    .is_some(),
                "{region} areas"
            );
            editor.assets = Some(assets.clone());
            let lib = harbour_library(&install, &assets, &editor);
            editor.harbour = Some(lib.clone());
            let size = editor.world()[0] as f32;
            let c = size * 0.5;
            editor.harbour_at([c, 0., size * 0.16], false);
            let coast = editor.coast_guids();
            assert!(
                (editor.doc.objects.iter()).any(|o| coast.contains(&o.guid)),
                "{region}: harbour"
            );
            for (lake, piece) in [(false, harbour::Piece::River), (true, harbour::Piece::Lake)] {
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
                    (editor.doc.objects.iter()).any(|o| Some(o.guid) == lib.guid(piece)),
                    "{region}: {piece:?}"
                );
            }
            let mut slots = Vec::new();
            for (i, (resource, _)) in harbour::Resource::ALL.into_iter().enumerate() {
                if lib.guid(resource.pieces(false).0).is_none() {
                    continue;
                }
                editor.resource = resource;
                let x = c - 18. + i as f32 * 6.;
                assert!(
                    editor.push_slot([x, editor.doc.field.height([x, c]), c]),
                    "{region}: {resource:?}"
                );
                let &Item::Object(id) = editor.selection.first().unwrap() else {
                    unreachable!()
                };
                let slot = editor.doc.objects.iter().find(|o| o.id == id).unwrap();
                slots.push((id, slot.guid));
                if let Some(replacement) =
                    (assets.slot_replacements(slot.guid, &editor.region())).first()
                {
                    editor.set_slot_resource(Some(replacement.guid));
                }
            }
            assert!(!slots.is_empty(), "{region}: resources");
            let fertilities = fertility::choices(&assets, &editor.region());
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
            let info = anno_formats::a7minfo::A7mInfo::parse(
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
                let template = anno_formats::template::MapTemplate::parse(bytes).unwrap();
                for slot in template.slots.iter().filter(|s| matches!(&s.kind, anno_formats::template::SlotKind::Fixed {path} if *path == editor.path)) {
                    assert_eq!(slot.fertilities, editor.listing.fertilities.clone().unwrap(), "{region}: fertilities");
                }
                let tree = anno_formats::adf::Adf::decode(bytes).unwrap();
                let root = tree.node(tree.root(), "MapTemplate").unwrap();
                for element in tree.children_named(root, "TemplateElement", false) {
                    if !tree
                        .value(element, "Element/MapFilePath")
                        .is_some_and(|v| tree.read_utf16(v).unwrap() == editor.path)
                    {
                        continue;
                    }
                    let random = tree.value(element, "Element/RandomizeFertilities").unwrap();
                    assert_eq!(tree.payload(random), &[0]);
                    let mapping = tree.node(element, "Element/MineSlotMapping").unwrap();
                    let values: Vec<_> = tree.children(mapping).collect();
                    for (&id, &guid) in &editor.listing.slot_resources {
                        assert!(
                            values
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .any(|p| tree.read_u64(p[0]).unwrap() == id
                                    && tree.read_u32(p[1]).unwrap() == guid),
                            "{region}: slot assignment"
                        );
                    }
                }
            }
            let back = reopen(&install, &mut project, &editor.path);
            for (id, guid) in &slots {
                assert!(
                    (back.doc.objects.iter()).any(|o| o.id == *id && o.guid == *guid),
                    "{region}: slot reload"
                );
            }
            assert!(
                (back.doc.objects.iter()).any(|o| coast.contains(&o.guid)),
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
        let mut editor = IslandEditor::load(&install, MODERATE_M_01).unwrap();
        let before = editor.doc.clone();
        let at = [160., 160.];
        let old_height = editor.doc.field.height(at);
        let prop_count = editor.doc.props.len();
        editor.brush.radius = 4.;
        editor.brush.strength = 1.;
        let paint = if editor.doc.materials.as_ref().unwrap().dominant(at) == 1 {
            2
        } else {
            1
        };
        editor.paint = paint;
        editor.grid = Kind::River;
        editor.place = Some(editor.doc.props[0].source.file);
        editor.tint_color = tint::PRESETS[1].1;
        for tool in [
            Tool::Raise,
            Tool::Paint,
            Tool::Grid,
            Tool::Scatter,
            Tool::Tint,
        ] {
            editor.tool = tool;
            let y = editor.doc.field.height(at);
            assert!(editor.dab([at[0], y, at[1]], 0., false, None), "{tool:?}");
        }
        editor.history.checkpoint_owned(before);
        let new_height = editor.doc.field.height(at);
        assert!(new_height > old_height);
        assert!(editor.undo(false));
        assert_eq!(editor.doc.field.height(at), old_height);
        assert!(editor.undo(true));
        assert_eq!(editor.doc.field.height(at), new_height);
        let (_config, mut project) = staged(&install, &mut editor);
        let base = island_data_dir(MODERATE_M_01).unwrap();
        for dir in ["meshes", "normalmaps", "tintmaps/0"] {
            let dir = format!("{base}/{dir}/");
            assert!(project.files.keys().any(|p| p.starts_with(&dir)), "{dir}");
        }
        assert!(!project.files.keys().any(|p| p.contains("materialmaps/")));
        let info =
            anno_formats::a7minfo::A7mInfo::parse(&project.files[&format!("{MODERATE_M_01}info")])
                .unwrap();
        assert!(matches!(info.slots, anno_formats::a7minfo::Slots::Typed(_)));
        let back = reopen(&install, &mut project, MODERATE_M_01);
        assert!((back.doc.field.height(at) - new_height).abs() < 1. / 128.);
        assert!(back.doc.props.len() > prop_count);
        assert_eq!(back.doc.materials.as_ref().unwrap().dominant(at), paint);
        assert_eq!(
            grids::value_at(layer(&back.doc, Kind::River), at[0], at[1]),
            1
        );
    }

    #[test]
    fn installed_1800_new_island_has_its_own_headers_and_pool_registration() {
        use super::harbour;
        let Some(install) = install_1800() else {
            return;
        };
        let mut editor = new_island(&install, "moderate", 192, Start::Plateau, "Steamstead");
        let assets = assets(&install);
        editor.harbour = Some(harbour_library(&install, &assets, &editor));
        editor.assets = Some(assets);
        editor.harbour_at([96., 0., 25.], false);
        let coast = editor.coast_guids();
        assert!(editor.doc.objects.iter().any(|o| coast.contains(&o.guid)));
        assert!(editor.push_slot([65., editor.doc.field.height([65., 70.]), 70.]));
        let slot = (editor.doc.objects.iter())
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
        let river_guid = (editor.harbour.as_ref().unwrap())
            .guid(harbour::Piece::River)
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
        assert!(editor.doc.objects.iter().any(|o| o.guid == river_guid));
        let (_config, mut project) = staged(&install, &mut editor);
        let folder = project.write().unwrap();
        let entry = project.islands[0].clone();
        assert_eq!(entry.region, "Moderate");
        assert!(
            entry
                .path
                .starts_with("data/sessions/islands/custom/moderate/")
        );
        let info =
            anno_formats::a7minfo::A7mInfo::parse(&project.files[&format!("{}info", entry.path)])
                .unwrap();
        assert!(matches!(info.slots, anno_formats::a7minfo::Slots::Typed(_)));
        assert!(info.source_map.contains("steamstead"));
        assert_eq!(info.default_session_guid, None);
        let assets_xml = |folder: &std::path::Path| {
            let profile = anno_formats::game::Game::Anno1800.profile();
            std::fs::read_to_string(folder.join(profile.assets_path)).unwrap()
        };
        let xml = assets_xml(&folder);
        assert!(xml.contains("GUID=\"140363\""));
        assert!(!xml.contains("IslandPlaceholderSize"));
        install.mount_mod(&folder).unwrap();
        assert!(IslandEditor::load(&install, &entry.path).is_ok());
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
            let info = anno_formats::adf::Adf::decode(
                &project.files[&map.path.replace(".a7t", ".a7tinfo")],
            )
            .unwrap();
            let root = info.node(info.root(), "MapTemplate").unwrap();
            let fixed = info
                .children_named(root, "TemplateElement", false)
                .find(|n| {
                    info.value(*n, "Element/MapFilePath")
                        .is_some_and(|v| info.read_utf16(v).unwrap() == entry.path)
                })
                .unwrap();
            assert!(info.node(fixed, "Element/MineSlotMapping").is_some());
            assert!(info.node(fixed, "Element/MineSlotActivation").is_none());
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
        let ground = |e: &IslandEditor| [at[0], e.doc.field.height(at), at[1]];
        let props_before = editor.doc.props.len();
        let objects_before = editor.doc.objects.len();
        assert!(objects_before > 1000, "{objects_before}");
        let before = editor.doc.clone();
        editor.brush.radius = 8.;
        editor.brush.strength = 1.;
        for tool in [Tool::Raise, Tool::Grid, Tool::Scatter, Tool::Tint] {
            editor.tool = tool;
            editor.grid = Kind::River;
            editor.place = Some(editor.doc.props[0].source.file);
            editor.tint_color = tint::PRESETS[1].1;
            assert!(
                editor.dab(ground(&editor), 0., false, None),
                "{tool:?} changed nothing"
            );
        }
        let m = (editor.doc.materials.as_mut()).expect("pirate_01 ships material maps");
        let brush = Brush {
            radius: 6.,
            strength: 1.,
            falloff: Falloff::Constant,
            ..Brush::default()
        };
        assert!(m.paint(at, &brush, 5));
        assert_eq!(m.dominant(at), 5);
        let item = Item::Object(editor.doc.objects[0].id);
        let mut m = editor.doc.matrix(item).unwrap();
        m[12] += 3.;
        editor.doc.set_matrix(item, m);
        let moved = editor.doc.objects[0].position;
        editor.history.checkpoint_owned(before);
        let added = editor.doc.props.len() - props_before;
        assert!(added > 0, "scatter added no props");
        let river_at = |g: &Grids| grids::value_at(g.layer(Kind::River).unwrap(), at[0], at[1]);
        assert_eq!(river_at(editor.doc.grids.as_ref().unwrap()), 1);

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
                island::material_words(&project.files[&key]).unwrap();
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
        let skeleton = super::skeletons(install, None)
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
            anno_formats::game::Game::Anno117,
        );
        editor.stage(&install, &mut project).unwrap();
        project.write().unwrap();
        println!("{}", editor.path);
    }

    #[test]
    fn installed_1800_automatic_tools_save_and_undo_in_each_region() {
        use super::{blockers, cliffs, harbour, sound};
        let Some(install) = install_1800() else {
            return;
        };
        let assets = assets(&install);
        let mut wanted = sound::guids(install.game()).to_vec();
        wanted.extend([blockers::BLOCKER, 100961]);
        let records = Arc::new(harbour::find_records(&install, &wanted).unwrap());
        for guid in &wanted {
            assert!(records.contains_key(guid), "missing record {guid}");
        }
        let mut export = std::env::var_os("ANNO_EDITOR_QA_EXPORT").map(|dir| {
            let mut p = Project::load(std::path::Path::new(&dir), install.game());
            p.info.name = "Anno 1800 Automatic Tools QA".into();
            p.set_mod_id("anno-1800-automatic-tools-qa").unwrap();
            p
        });
        let blocker_count = |e: &IslandEditor| {
            (e.doc.objects.iter())
                .filter(|o| o.guid == blockers::BLOCKER)
                .count()
        };
        for region in ["moderate", "colony01", "arctic", "africa"] {
            let mut editor = mesa_island(&install, region, &format!("auto_tools_{region}"));
            assert_eq!(
                editor.doc.session.vegetation_prop_set,
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
            let kit = cliffs::Kit::load(&install, &editor.region()).unwrap();
            assert!((kit.shapes.iter()).all(|s| s.extent.iter().all(|e| *e > 0. && e.is_finite())));
            let rock = cliffs::ground_material(install.game(), &material_names(&editor))
                .expect("regional rock material");
            editor.auto_cliffs(&install);
            let count = editor.doc.props.len();
            assert!(count > 5, "{region}: {count} cliffs");
            assert!(
                editor.auto_cliff_coverage(&kit) > 0.15,
                "{region}: cliff coverage"
            );
            let materials = editor.doc.materials.as_ref().unwrap();
            assert!(
                (editor.doc.props.iter())
                    .any(|p| materials.dominant([p.matrix[12], p.matrix[14]]) == rock)
            );
            editor.auto_cliffs(&install);
            assert_eq!(editor.doc.props.len(), count, "{region}: cliffs stacked");
            assert!(editor.undo(false));
            assert_eq!(editor.doc.props.len(), count);

            // A rock on the flat plateau requires blockers; the automatic
            // cliff pieces lie on unbuildable slopes.
            let file = editor.mesh_index(&kit.shapes[2].cfg);
            editor.place = Some(file);
            let scale = editor.world()[0] as f32 / 256.;
            let x = 128. * scale;
            let z = 70. * scale;
            editor.place_at([x, editor.doc.field.height([x, z]), z]);
            editor.choose_object(records[&100961].clone(), None);
            let z = 82. * scale;
            editor.place_at([x, editor.doc.field.height([x, z]), z]);
            let before_sound = editor.doc.objects.len();
            editor.auto_sound_spots();
            let sounds = editor.doc.objects.len() - before_sound;
            assert!(sounds >= 3, "{region}: sound emitters");
            let placed_sound: Vec<_> = (editor.doc.objects.iter())
                .filter(|o| sound::guids(install.game()).contains(&o.guid))
                .map(|o| (o.guid, o.position))
                .collect();
            let revision = editor.history.revision();
            editor.auto_sound_spots();
            assert_eq!(
                editor.doc.objects.len(),
                before_sound + sounds,
                "{region}: sounds stacked"
            );
            assert_eq!(
                editor.history.revision(),
                revision,
                "{region}: identical sounds added undo"
            );
            assert!(editor.undo(false));
            assert_eq!(editor.doc.objects.len(), before_sound);
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
                reopened.doc.props.iter().filter(|p| p.auto_cliff).count(),
                count
            );
            assert_eq!(blocker_count(&reopened), blockers);
            for (guid, position) in placed_sound {
                assert!(
                    (reopened.doc.objects.iter()).any(|o| o.guid == guid && o.position == position),
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
            let kit = cliffs::Kit::load(&install, &editor.region()).unwrap();
            let rock = cliffs::ground_material(editor.game, &material_names(&editor))
                .expect("no cliff material");
            let ground =
                |e: &IslandEditor, p: [f32; 2]| e.doc.materials.as_ref().unwrap().dominant(p);
            // Plateau top and the shore below the mesa are flat.
            let flat = [[128., 128.], [128., 60.]];
            let before = flat.map(|p| ground(&editor, p));
            editor.auto_cliffs(&install);
            let auto: Vec<_> = editor.doc.props.iter().filter(|p| p.auto_cliff).collect();
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
            assert!(!editor.doc.props.iter().any(|p| p.auto_cliff));
            let still = on_rock(&editor);
            assert!(
                still * 10 < centers.len(),
                "{region}: undo kept {still} rock"
            );
            editor.auto_cliffs(&install);
            let auto: Vec<_> = editor.doc.props.iter().filter(|p| p.auto_cliff).collect();
            let field = &editor.doc.field;
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
                let cfg = &editor.doc.meshes[p.source.file as usize];
                assert!(cfg.contains(&format!("/{region}/")), "{region}: {cfg}");
            }
            let share = editor.auto_cliff_coverage(&kit);
            eprintln!("{region}: {} pieces cover {share:.2}", auto.len());
            assert!((0.33..=0.8).contains(&share), "{region}: covers {share}");

            // Rerun: replacing gives the same set, filling gaps adds little.
            let count = auto.len();
            editor.auto_cliffs(&install);
            let now = editor.doc.props.iter().filter(|p| p.auto_cliff).count();
            assert_eq!(now, count, "{region}: replace changed the count");
            editor.cliffs.replace = false;
            editor.auto_cliffs(&install);
            let more = editor.doc.props.len() - count;
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
                for p in Arc::make_mut(&mut editor.doc.props).iter_mut() {
                    p.auto_cliff = false;
                }
                editor.adopt(entry);
                let known = editor.doc.props.iter().filter(|p| p.auto_cliff).count();
                assert_eq!(known, count, "listing lost track of auto pieces");
            }

            // Undo steps back over the replacing rerun, then takes the
            // pieces away; the mesa stays.
            assert!(editor.undo(false));
            assert_eq!(editor.doc.props.len(), count);
            assert!(editor.undo(false));
            assert!(editor.doc.props.iter().all(|p| !p.auto_cliff));
            assert!(editor.doc.field.height([128., 128.]) > 27.);
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
        let before = editor.doc.clone();
        let (a, b) = ([100., 0., 120.], [104., 0., 120.]);
        assert!(
            !editor.dab(a, 0., false, None),
            "a lone dab has no direction"
        );
        assert!(editor.dab(b, 0., false, None));
        editor.history.checkpoint_owned(before);
        let flow = |g: &Grids| grids::value_at(g.layer(Kind::WaterFlow).unwrap(), b[0], b[2]);
        let [x, z] = grids::flow(flow(editor.doc.grids.as_ref().unwrap()));
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
            grids::flow(flow(editor.doc.grids.as_ref().unwrap())),
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
                let mut editor = match IslandEditor::load_with(&install, &path, shared) {
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
        let mut editor =
            IslandEditor::load_with(&install, path, Some((install.clone(), assets))).unwrap();
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
            let at = [300., editor.doc.field.height([300., 300.]), 300.];
            let mut drag = super::Drag::Brush {
                stroke: super::Stroke::default(),
                before: Box::new(editor.doc.clone()),
                level: at[1],
                changed: false,
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
        let item = Item::Prop(editor.doc.props[0].id);
        let start = editor.doc.matrix(item).unwrap();
        let (mut moved, mut scene) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let renderer = editor.renderer.clone();
        for i in 0..40 {
            let t = Instant::now();
            let mut m = start;
            m[12] += i as f32 * 0.5;
            // As the gizmo does: the document, then the item's rows.
            editor.doc.set_matrix(item, m);
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
        let snapshot = editor.doc.clone();
        eprintln!("doc clone (undo checkpoint) {:?}", t.elapsed());
        drop(snapshot);
        let t = Instant::now();
        editor.history.checkpoint(&editor.doc.clone());
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
        let mut editor =
            IslandEditor::load_with(&install, PIRATE, Some((install.clone(), assets))).unwrap();
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
        assert!(editor.doc.materials.is_some());
    }

    // Needs the game: paint dabs update the GPU splat in place, matching a
    // full rebuild from the same materials, with no bake in between.
    #[test]
    fn painting_updates_the_splat_live() {
        use gpui_editor_kit::gpu::Splat;
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
        editor.history.checkpoint(&editor.doc);
        for i in 0..6 {
            assert!(editor.dab([90. + i as f32 * 4., 0., 100.], 0., false, None));
        }
        let live = editor.splat.clone().unwrap();
        assert!(
            Arc::ptr_eq(&live, &before),
            "the same splat, edited in place"
        );
        assert_ne!(snapshot(&live), original, "the dabs changed it");
        let m = editor.doc.materials.as_ref().unwrap();
        let rebuilt = (editor.palette.as_ref().unwrap())
            .splat(&m.ids.grid(), &m.alphas.grid())
            .unwrap();
        assert!(
            snapshot(&live) == snapshot(&rebuilt),
            "live edits match a rebuild"
        );
        // A stroke's bake only redoes the texture.
        editor.ground_stale = true;
        let (_, splat) = editor.bake_job().unwrap()().unwrap();
        assert!(splat.is_none());
        // Undo brings the old materials back, and the splat with them.
        assert!(editor.undo(false));
        assert!(editor.ground_stale);
        let (_, splat) = editor.bake_job().unwrap()().unwrap();
        let splat = splat.expect("undo rebuilds the splat");
        assert!(snapshot(&splat) == original, "back to the unpainted splat");
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
        editor.selection = editor.doc.items().map(|(i, _)| i).collect();
        assert!(
            editor.selection.len() > GPU_DRAG,
            "{}",
            editor.selection.len()
        );
        editor.tool = Tool::Select;
        editor.refresh_scene();
        let starts: Vec<(Item, Mat4)> = (editor.selection.iter())
            .map(|&i| (i, editor.doc.matrix(i).unwrap()))
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
                editor.doc.matrix(*item).unwrap(),
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
            let got = math::translation(&editor.doc.matrix(*item).unwrap());
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
        let items: Vec<(Item, [f32; 3])> = editor.doc.items().step_by(3).collect();
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
        let before = editor.doc.field.heights.clone();
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
        let after = &editor.doc.field.heights;
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
}
