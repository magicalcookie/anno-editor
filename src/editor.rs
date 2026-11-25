// Owns the window: workspaces and their dock layouts, menus, the status bar,
// keyboard routing, and the open install and mod project shared by every
// workspace.
use crate::games::GameExt;
use crate::{
    browser::Browser, island::IslandEditor, map::MapEditor, model::ModelEditor, project::Project,
    ui::UiEditor,
};
use anno_core::game::{Game, Module};
use anno_formats::vfs::Install;
use gpui_editor_kit::{
    dock::{self, Axis, Dock, DockHost, Layout},
    field::InputBuffer,
    gpui::{prelude::*, *},
    theme,
    widgets::{self, button, menu_item},
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

/// Project islands and recent projects listed in the File menu; more
/// would push the menu off a laptop screen.
const MENU_ISLANDS: usize = 8;
const MENU_RECENT: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Pane {
    ProjectOverview,
    ProjectModels,
    ProjectMods,
    Browser,
    IslandList,
    Preview,
    IslandView,
    IslandTop,
    IslandHierarchy,
    IslandLayers,
    IslandGraphics,
    IslandInspector,
    IslandMaterials,
    IslandLibrary,
    MapView,
    MapSlots,
    MapInspector,
    MapIslands,
    UiTree,
    UiCanvas,
    UiInspector,
    UiFiles,
    UiTimeline,
    UiBindings,
    UiLibrary,
    ModelView,
    ModelList,
    ModelHierarchy,
    ModelInspector,
    ModelTimeline,
}

impl Pane {
    const ALL: [Self; 30] = [
        Self::ProjectOverview,
        Self::ProjectModels,
        Self::ProjectMods,
        Self::Browser,
        Self::IslandList,
        Self::Preview,
        Self::IslandView,
        Self::IslandTop,
        Self::IslandHierarchy,
        Self::IslandLayers,
        Self::IslandGraphics,
        Self::IslandInspector,
        Self::IslandMaterials,
        Self::IslandLibrary,
        Self::MapView,
        Self::MapSlots,
        Self::MapInspector,
        Self::MapIslands,
        Self::UiTree,
        Self::UiCanvas,
        Self::UiInspector,
        Self::UiFiles,
        Self::UiTimeline,
        Self::UiBindings,
        Self::UiLibrary,
        Self::ModelView,
        Self::ModelList,
        Self::ModelHierarchy,
        Self::ModelInspector,
        Self::ModelTimeline,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workspace {
    Project,
    Assets,
    Island,
    Map,
    Ui,
    Model,
}

impl Workspace {
    const ALL: [Self; 6] = [
        Self::Project,
        Self::Assets,
        Self::Island,
        Self::Map,
        Self::Ui,
        Self::Model,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Assets => "Assets",
            Self::Island => "Island",
            Self::Map => "Map",
            Self::Ui => "UI (experimental)",
            Self::Model => "Model",
        }
    }

    /// Names its saved layout file: stays put when a label changes, so a
    /// renamed workspace keeps the layout users arranged.
    fn key(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Assets => "assets",
            Self::Island => "island",
            Self::Map => "map",
            Self::Ui => "ui",
            Self::Model => "model",
        }
    }

    fn layout(self) -> Layout<Pane> {
        let browser = || Layout::tabs(vec![Pane::Browser], Pane::Browser);
        match self {
            // One page at a time: the overview reads top to bottom, and the
            // model grid wants the width.
            Self::Project => Layout::tabs(
                vec![
                    Pane::ProjectOverview,
                    Pane::ProjectModels,
                    Pane::ProjectMods,
                ],
                Pane::ProjectOverview,
            ),
            // A wide sidebar, as file browsers are: at half the window the
            // tree's short rows left most of it empty and the preview
            // lost the width a texture wants. List's Folder column cuts
            // names from the start to fit. The split drags.
            Self::Assets => Layout::split(
                Axis::Horizontal,
                0.35,
                browser(),
                Layout::tabs(vec![Pane::Preview], Pane::Preview),
            ),
            Self::Island => Layout::split(
                Axis::Horizontal,
                0.19,
                Layout::tabs(
                    vec![
                        Pane::IslandList,
                        Pane::IslandHierarchy,
                        Pane::IslandLayers,
                        Pane::IslandGraphics,
                    ],
                    Pane::IslandList,
                ),
                Layout::split(
                    Axis::Horizontal,
                    0.8,
                    // The island gets most of the height; the Library two
                    // whole rows of cards, so picking a model to place is not
                    // a scroll through nine at a time (at 0.72 a second row
                    // showed cut through its pictures, which read as a layout
                    // bug; at 0.78 only one row showed). Its divider drags.
                    Layout::split(
                        Axis::Vertical,
                        0.615,
                        Layout::tabs(vec![Pane::IslandView, Pane::IslandTop], Pane::IslandView),
                        Layout::tabs(
                            vec![Pane::IslandLibrary, Pane::IslandMaterials],
                            Pane::IslandLibrary,
                        ),
                    ),
                    Layout::tabs(vec![Pane::IslandInspector], Pane::IslandInspector),
                ),
            ),
            // The slot list narrow on the left, the map the rest but the
            // Inspector, whose island dropdown and Islands tab want the
            // width of an island name and its size.
            Self::Map => Layout::split(
                Axis::Horizontal,
                0.17,
                Layout::tabs(vec![Pane::MapSlots], Pane::MapSlots),
                Layout::split(
                    Axis::Horizontal,
                    0.76,
                    Layout::tabs(vec![Pane::MapView], Pane::MapView),
                    Layout::tabs(
                        vec![Pane::MapInspector, Pane::MapIslands],
                        Pane::MapInspector,
                    ),
                ),
            ),
            // The Models list and the hierarchy narrow on the left, as the
            // Islands list and the island's hierarchy are; the inspector on
            // the right wide enough for a material's field names, the
            // timeline under the view.
            Self::Model => Layout::split(
                Axis::Horizontal,
                0.18,
                Layout::tabs(
                    vec![Pane::ModelList, Pane::ModelHierarchy, Pane::Browser],
                    Pane::ModelList,
                ),
                Layout::split(
                    Axis::Horizontal,
                    0.72,
                    Layout::split(
                        Axis::Vertical,
                        0.74,
                        Layout::tabs(vec![Pane::ModelView], Pane::ModelView),
                        Layout::tabs(vec![Pane::ModelTimeline], Pane::ModelTimeline),
                    ),
                    Layout::tabs(vec![Pane::ModelInspector], Pane::ModelInspector),
                ),
            ),
            Self::Ui => Layout::split(
                Axis::Horizontal,
                0.2,
                Layout::tabs(
                    vec![Pane::UiFiles, Pane::UiTree, Pane::UiLibrary, Pane::Browser],
                    Pane::UiFiles,
                ),
                Layout::split(
                    Axis::Horizontal,
                    0.74,
                    // The timeline needs room for its ruler, a few lanes
                    // and the key bar.
                    Layout::split(
                        Axis::Vertical,
                        0.68,
                        Layout::tabs(vec![Pane::UiCanvas], Pane::UiCanvas),
                        Layout::tabs(vec![Pane::UiTimeline, Pane::UiBindings], Pane::UiTimeline),
                    ),
                    Layout::tabs(vec![Pane::UiInspector, Pane::Preview], Pane::UiInspector),
                ),
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    File,
    Island,
    /// The Map workspace's own: new, open and the variant actions.
    Map,
    /// The Model workspace's own: open, add, the selection, the gizmo,
    /// playback and what the view shows.
    Model,
    Edit,
    View,
    /// The project switcher under the project's name.
    Project,
}

/// Where the editor goes once unsaved edits are saved or dropped.
#[derive(Clone, Debug, PartialEq)]
pub enum Leave {
    Game,
    Project(PathBuf),
    /// The New island wizard.
    NewIsland,
    /// Another island, by virtual path.
    Island(String),
    /// Another map layout, by virtual path.
    Map(String),
    /// The New map layout dialog.
    NewMap,
    /// Map → Close: the Map workspace left empty.
    CloseMap,
    /// Another UI screen, by virtual path.
    Screen(String),
    /// Another model in the Model workspace, by virtual path.
    Model(String),
    /// Ctrl Q, File → Quit, or the window's close button.
    Quit,
}

/// One of the open documents, each workspace's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doc {
    Island,
    Map,
    Ui,
    Model,
}

impl Leave {
    /// The one document going there replaces; None when it leaves them
    /// all: another game or project, or quitting.
    pub fn replaces(&self) -> Option<Doc> {
        match self {
            Leave::NewIsland | Leave::Island(_) => Some(Doc::Island),
            Leave::Map(_) | Leave::NewMap | Leave::CloseMap => Some(Doc::Map),
            Leave::Screen(_) => Some(Doc::Ui),
            Leave::Model(_) => Some(Doc::Model),
            Leave::Game | Leave::Project(_) | Leave::Quit => None,
        }
    }

    /// The ask's title, what is about to happen ("Save them before …"),
    /// and the save button's words.
    pub(crate) fn words(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Leave::Game => ("Switch game?", "switching game", "Save and switch"),
            Leave::Project(_) => ("Switch project?", "switching project", "Save and switch"),
            Leave::NewIsland => (
                "Start a new island?",
                "starting a new island",
                "Save and go on",
            ),
            Leave::Island(_) => (
                "Open another island?",
                "opening another island",
                "Save and open",
            ),
            Leave::Map(_) => ("Open another map?", "opening another map", "Save and open"),
            Leave::NewMap => (
                "Start a new map layout?",
                "starting a new layout",
                "Save and go on",
            ),
            Leave::CloseMap => ("Close the map?", "closing it", "Save and close"),
            Leave::Screen(_) => (
                "Open another screen?",
                "opening another screen",
                "Save and open",
            ),
            Leave::Model(_) => (
                "Open another model?",
                "opening another model",
                "Save and open",
            ),
            Leave::Quit => ("Quit?", "quitting", "Save and quit"),
        }
    }
}

/// The ask's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Cancel,
    Discard,
    Save,
}

/// What a save did, for whatever waits on it (Save and quit, Save and
/// open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    /// Every edited document is in the mod folder; a warning may say what
    /// the game can't use yet.
    Written,
    /// A shipped map waits for the answer to how it saves; the rest is
    /// written.
    Waiting,
    /// Something wasn't written; the status bar says what.
    Failed,
}

pub struct Editor {
    pub module: &'static dyn Module,
    pub game: Game,
    pub config: PathBuf,
    pub install: Option<Arc<Install>>,
    /// assets.xml, loaded after the index; object names wait for it.
    pub assets: Option<Arc<anno_formats::assets::Assets>>,
    pub project: Project,
    /// Every installed mod but the project's own copy (`scan_mods`).
    pub mods: Vec<anno_core::modinfo::Installed>,
    /// Ids of the dependencies mounted under the project, by mod layer.
    pub mounted: Vec<String>,
    /// What the last save found the open island using from mods the
    /// project does not require.
    pub undeclared: Vec<crate::project::Undeclared>,
    pub browser: Browser,
    pub island: Option<IslandEditor>,
    /// The last frame each island view drew before the island closed for
    /// another one, shown while that one loads and under it until its own
    /// first frame: the empty state flashed between them otherwise.
    pub island_stills: [Option<gpui_editor_kit::gpu::Still>; 2],
    /// The map layout open in the Map workspace.
    pub map: Option<MapEditor>,
    /// The Map workspace's dialog, while one is open.
    pub map_dialog: Option<crate::map::panels::Dialog>,
    /// The game's shipped layouts, listed once for Open and New map.
    pub shipped_maps: Option<Arc<Vec<anno_core::map::ShippedMap>>>,
    pub shipped_maps_loading: bool,
    pub ui: Option<UiEditor>,
    /// The cfg open in the Model workspace.
    pub model: Option<ModelEditor>,
    /// Where the Model timeline's ruler was last laid out, for scrubbing.
    pub model_ruler: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    /// The timeline clip being dragged.
    pub model_clip_drag: Option<crate::model::timeline::ClipDrag>,
    /// The Model workspace's asset picker, while open.
    pub model_picker: Option<crate::model::picker::Picker>,
    /// The sounds models' sequences play, across models.
    pub model_sound: crate::model::sound::Sound,
    /// Models opened this session, newest first: Open model lists them
    /// first and Back goes to the one before. Not kept across sessions.
    pub model_recent: Vec<String>,
    /// The Model workspace's Models list.
    pub model_list: crate::model::list::List,
    /// The UI files pane: index of every compiled UI asset and its search.
    pub ui_find: crate::ui::files::Find,
    /// The screen that last failed to open, named on the empty canvas
    /// until another opens.
    pub ui_failed: Option<String>,
    /// The last copied widget; pastes into any open UI asset.
    pub ui_clipboard: Option<anno_formats::phx::WidgetCopy>,
    /// The animation player's frame loop is running.
    pub ui_ticking: bool,
    pub workspace: Workspace,
    pub(crate) docks: [Dock<Pane>; 6],
    /// Each pane's view, made on first render; see `PaneView`.
    pane_views: Vec<(Pane, Entity<PaneView>)>,
    /// Read by every render, so notifying it redraws the window while
    /// every pane stays cached; see `camera_moved`.
    ticker: Entity<FrameTick>,
    /// Everything but the viewport readouts, cached (see `Shell`); made at
    /// the first render.
    shell: Option<Entity<Shell>>,
    /// The window title as last set: setting it is an X11 request.
    title: String,
    /// `ANNO_EDITOR_BENCH=SECONDS`: orbit the first view once the island
    /// has loaded, print frame times every second, then quit.
    /// `ANNO_EDITOR_UI_BENCH=SECONDS`: pan the UI canvas a pixel each frame
    /// instead, once a UI file is open.
    bench: Option<Bench>,
    /// When the viewport readouts last redrew during camera moves, and
    /// whether a redraw is scheduled (see `refresh_readouts`).
    readouts_at: Option<std::time::Instant>,
    readouts_due: bool,
    menu: Option<Menu>,
    /// The New island wizard, while open.
    pub wizard: Option<crate::island::Wizard>,
    /// The Import from the other game wizard, while open.
    pub import: Option<crate::island::Import>,
    /// The Islands pane: every shipped island once listed, and its filter.
    pub island_list: Option<Arc<Vec<crate::island::Skeleton>>>,
    /// The island row last hovered while none is open, and its map once
    /// decoded: the empty Inspector previews it.
    pub island_peek: Option<(String, Option<Arc<RenderImage>>)>,
    /// The UI file row last hovered while no screen is open: the empty
    /// Inspector describes it.
    pub ui_peek: Option<crate::ui::files::Entry>,
    /// The list row the pointer rests on, by key, while a hover preview
    /// waits to switch to it.
    dwell: Option<String>,
    pub island_list_loading: bool,
    pub island_filter: String,
    /// The island list's kind filter (playable, decoration, other).
    pub island_kind: crate::island::IslandKind,
    /// Scroll state of the Islands pane's list.
    pub island_list_rows: gpui_editor_kit::tree::Tree<()>,
    /// Groups of the Islands list the user folded, by region key.
    pub island_regions_closed: std::collections::BTreeSet<String>,
    /// The island tool the bottom pane was last matched to.
    island_dock_tool: Option<crate::island::Tool>,
    /// Right inset that centres the project menu under the project button,
    /// as last laid out.
    project_button_right: f32,
    /// Whether a UI screen was open when the UI bottom panes were last set.
    ui_dock_open: Option<bool>,
    pub island_filter_edit: Option<gpui_editor_kit::field::TextEdit<()>>,
    /// The Test in game dialog, while open.
    pub playtest: Option<crate::playtest::Playtest>,
    /// Every placeable prop, decoration and object, once built.
    pub catalog: Option<Arc<crate::island::catalog::Catalog>>,
    pub catalog_loading: bool,
    /// Scroll positions of the panes, so their scrollbars can track them.
    pub scrolls: gpui_editor_kit::widgets::ScrollHandles,
    /// Library thumbnails; started with the first Library view.
    pub thumbs: Option<crate::island::catalog::Thumbs>,
    pub thumbs_polling: bool,
    /// The World Machine dialog is open.
    pub wm_dialog: bool,
    /// Its channel previews, for the linked folder.
    pub wm_previews: Option<crate::island::WmPreviews>,
    pub focus: FocusHandle,
    pub pending_save: bool,
    /// An install is copying into the game (`install_project`).
    installing: bool,
    status: String,
    error: bool,
    /// The workspace the status message is about; others show none.
    status_in: Workspace,
    /// When the message was set, so an "Opened …" note can fade.
    status_at: std::time::Instant,
    /// The island tool a message was said under: a tool's complaint
    /// ("Choose something in the Library first") is about that tool.
    status_tool: Option<crate::island::Tool>,
    /// "Place on island" was asked for with no island open: the next
    /// island to open arms the Place tool with this model.
    pub(crate) place_pending: Option<String>,
    /// View → Statistics: load timings and frame times over the viewport.
    pub stats: bool,
    /// View → GPU pass times: each timed render pass under the frame times.
    /// Only drawn with `stats`; turning it on turns that on too.
    pub pass_times: bool,
    /// Asked to switch game or project with unsaved edits.
    pub leaving: Option<Leave>,
    /// Project workspace state: the field being edited, the selected model
    /// and filter, the New project dialog.
    pub project_ui: crate::project::ProjectUi,
}

pub struct SwitchGame;
impl EventEmitter<SwitchGame> for Editor {}

impl Editor {
    /// The editor for `game`, keeping its layouts, recent projects and
    /// indexes in `config` (`platform::config_dir`; window tests pass a
    /// folder of their own).
    pub fn new(
        project: Project,
        config: PathBuf,
        install: Option<PathBuf>,
        open: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let module = project.module;
        let game = module.game();
        let mut errors = Vec::new();
        let docks = Workspace::ALL.map(|w| {
            let path = config.join(format!("layout-{}.json", w.key()));
            let (mut dock, error) = Dock::persistent(path, w.layout());
            errors.extend(error);
            // A layout saved before panes were added would hide them for good.
            let default = w.layout();
            if Pane::ALL
                .iter()
                .any(|&p| default.contains(p) && !dock.layout.contains(p))
            {
                dock.reset();
            }
            dock
        });
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut editor = Self {
            module,
            game,
            config: config.clone(),
            install: None,
            assets: None,
            project,
            mods: Vec::new(),
            mounted: Vec::new(),
            undeclared: Vec::new(),
            browser: Browser::default(),
            island: None,
            island_stills: [None, None],
            map: None,
            map_dialog: None,
            shipped_maps: None,
            shipped_maps_loading: false,
            ui: None,
            model: None,
            model_ruler: Default::default(),
            model_clip_drag: None,
            model_picker: None,
            model_sound: Default::default(),
            model_recent: Vec::new(),
            model_list: Default::default(),
            ui_find: Default::default(),
            ui_failed: None,
            ui_clipboard: None,
            ui_ticking: false,
            workspace: Workspace::Assets,
            docks,
            pane_views: Vec::new(),
            ticker: cx.new(|_| FrameTick),
            shell: None,
            title: String::new(),
            readouts_at: None,
            readouts_due: false,
            bench: (std::env::var("ANNO_EDITOR_BENCH").ok())
                .and_then(|s| s.parse().ok())
                .map(Bench::new)
                .or_else(|| {
                    let seconds = std::env::var("ANNO_EDITOR_UI_BENCH").ok()?.parse().ok()?;
                    Some(Bench {
                        ui: true,
                        ..Bench::new(seconds)
                    })
                }),
            menu: None,
            wizard: None,
            import: None,
            island_list: None,
            island_peek: None,
            ui_peek: None,
            dwell: None,
            island_list_loading: false,
            island_filter: String::new(),
            island_kind: Default::default(),
            island_list_rows: Default::default(),
            island_filter_edit: None,
            island_regions_closed: Default::default(),
            island_dock_tool: None,
            project_button_right: 266.,
            ui_dock_open: None,
            playtest: None,
            catalog: None,
            catalog_loading: false,
            scrolls: Default::default(),
            thumbs: None,
            thumbs_polling: false,
            wm_dialog: false,
            wm_previews: None,
            focus,
            status: String::new(),
            error: false,
            status_in: Workspace::Assets,
            status_at: std::time::Instant::now(),
            status_tool: None,
            place_pending: None,
            stats: false,
            pass_times: false,
            pending_save: false,
            installing: false,
            leaving: None,
            project_ui: Default::default(),
        };
        if let Some(error) = errors.into_iter().next() {
            editor.message(format!("Layout reset: {error:#}"), true);
        }
        crate::project::remember(&config, &editor.project.folder());
        editor.open_install(install, open, cx);
        // Coming back from the file manager is when a mod was dropped into
        // the mods folder: rescan then, so no screen needs a Rescan button.
        cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.rescan_mods_on_return(cx);
            }
        })
        .detach();
        // World Machine links re-import when their export changes; checking
        // a folder's file times once a second costs nothing noticeable.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this.update(cx, |this, cx| this.poll_wm(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // The close button asks like Ctrl Q (round 12: it dropped a
        // session's edits); a clean editor lets the window close, which
        // quits. Once the editor is gone (back at the game picker) nothing
        // is left to lose.
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            this.update(cx, |this, cx| {
                // A value its field refuses keeps the window open, the
                // field still taking keys: closing would drop it.
                if let Err(e) = this.end_typing() {
                    this.message(e, true);
                    cx.notify();
                    return false;
                }
                if this.unsaved() {
                    this.leaving = Some(Leave::Quit);
                    cx.notify();
                }
                this.leaving.is_none()
            })
            .unwrap_or(true)
        });
        editor
    }

    /// A modal dialog covers the window.
    fn dialog_open(&self) -> bool {
        self.wizard.is_some()
            || self.import.is_some()
            || self.playtest.is_some()
            || self.wm_dialog
            || self.leaving.is_some()
            || self.project_ui.new_project.is_some()
            || self.map_dialog.is_some()
            || self.model_picker.is_some()
    }

    /// Runs `then` once the pointer has rested on the row `key` for a
    /// moment, unless it left first. Hover previews switch through this:
    /// at once, a pointer crossing other rows on its way to the preview's
    /// Open button swapped the preview, and Open took the wrong row.
    pub(crate) fn after_dwell(
        &mut self,
        key: String,
        hovered: bool,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        if !hovered {
            if self.dwell.as_ref() == Some(&key) {
                self.dwell = None;
            }
            return;
        }
        self.dwell = Some(key.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            this.update(cx, |this, cx| {
                if this.dwell.as_ref() == Some(&key) {
                    this.dwell = None;
                    then(this, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The status bar's message and whether it is an error. A message about
    /// another workspace would read as this one's, so it shows only there.
    pub(crate) fn status_line(&self) -> (String, bool) {
        if self.status_in != self.workspace {
            return (String::new(), false);
        }
        // An "Opened …" note is news for a few seconds, then reads as a
        // stale log line. An error is about the try that failed: it goes
        // with the tool it was said under, and after a few seconds; left
        // up, a Place complaint stood red under Select and under the New
        // island dialog, reading as a fault that would not clear.
        let opened = self.status.starts_with("Opened ");
        let stale = (opened && self.status_at.elapsed().as_secs() >= 6)
            || (self.error
                && (self.status_at.elapsed().as_secs() >= 8
                    || (self.workspace == Workspace::Island
                        && self.island.as_ref().map(|i| i.tool) != self.status_tool)));
        // While something is selected, the selection replaces a note that
        // went stale the moment a prop was picked.
        let selected = self
            .island
            .as_ref()
            .filter(|_| self.workspace == Workspace::Island && (opened || stale))
            .and_then(|i| crate::island::selection_status(i, self.assets.as_deref()));
        match selected {
            Some(selected) => (selected, false),
            None if stale => (String::new(), false),
            None => (self.status.clone(), self.error),
        }
    }

    pub fn message(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.error = error;
        self.status_in = self.workspace;
        self.status_at = std::time::Instant::now();
        self.status_tool = self.island.as_ref().map(|i| i.tool);
        if error {
            eprintln!("{}", self.status);
        }
    }

    /// Indexes the install off the UI thread: every archive's header
    /// tables, a few hundredths of a second warm, longer from a cold disk.
    fn open_install(&mut self, dir: Option<PathBuf>, open: Option<String>, cx: &mut Context<Self>) {
        let game = self.game;
        let Some(dir) = dir.or_else(|| game.locate()) else {
            self.message(
                format!(
                    "{} not found. Choose its folder on the game selection screen or set {}.",
                    game.profile().title,
                    game.profile().env_var
                ),
                true,
            );
            return;
        };
        self.message(format!("Indexing {}…", dir.display()), false);
        let families = game.profile().archive_families;
        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let result = cx
                .background_executor()
                .spawn(async move { Install::open(&dir, families) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(install) => {
                        let install = Arc::new(install);
                        this.message(
                            // Totals are on the status bar's right; the load
                            // time only with View → Developer → Load and
                            // frame times on.
                            // Otherwise the "Indexing…" line just clears.
                            if this.stats {
                                format!(
                                    "Read {} game archives in {:.1} s",
                                    install.archives().len(),
                                    started.elapsed().as_secs_f32()
                                )
                            } else {
                                String::new()
                            },
                            false,
                        );
                        this.browser.attach(&install, this.module);
                        // The island view's pipelines and the game's water
                        // and sky passes' build as soon as there is a
                        // window device, not when the first island opens.
                        let (module, warm) = (this.module, install.clone());
                        gpui_editor_kit::gpu::warm_passes(Arc::new(
                            move |gpu: &gpui_editor_kit::gpu::Warm<'_>| {
                                anno_render::a7m::warm(gpu);
                                if let Some(scene) = module.scene() {
                                    scene.warm(&warm, gpu)
                                }
                            },
                        ));
                        this.order_names(cx);
                        this.load_assets(install.clone(), cx);
                        this.install = Some(install);
                        this.scan_mods();
                        this.mount_project();
                        this.refresh_installed();
                        let missing = this.missing_dependencies();
                        if !missing.is_empty() {
                            this.message(
                                format!(
                                    "Required mods not installed: {}. Their models show as missing",
                                    missing.join(", ")
                                ),
                                true,
                            );
                        }
                        if let Some(path) = open {
                            this.open_path(&path, cx);
                        }
                    }
                    Err(e) => this.message(format!("Could not index the install: {e:#}"), true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_assets(&mut self, install: Arc<Install>, cx: &mut Context<Self>) {
        let module = self.module;
        // Only the editing workspaces name objects by their assets.
        if self.module.database().is_none() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { crate::games::assets(module, &install) })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok(assets) => this.assets = Some(Arc::new(assets)),
                    Err(e) => this.message(
                        format!("assets.xml unavailable, objects show GUIDs: {e:#}"),
                        true,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens a virtual path in the workspace that edits it, or previews it.
    pub fn open_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let lower = path.to_ascii_lowercase();
        // The file's kind names its editor; a game without that capability
        // (2070 has no UI screens) shows the file in the browser.
        match crate::browser::editor_for(self.module, &lower) {
            Some(Workspace::Island) => self.open_island(path, cx),
            Some(Workspace::Map) => self.open_map(path, cx),
            Some(Workspace::Ui) => self.open_ui_asset(path, cx),
            // A model has no file kind of its own: its cfg or prp opens in
            // the Model workspace, as Edit model does.
            _ if crate::browser::is_model(&lower) && self.supports(Workspace::Model) => {
                self.open_model(path, cx)
            }
            _ => {
                self.browser.select_path(path);
                if let Some(file) = self.browser.selected {
                    self.select_file(file, cx);
                }
                self.switch(Workspace::Assets);
            }
        }
    }

    pub(crate) fn supports(&self, workspace: Workspace) -> bool {
        match workspace {
            // A game with screens and no islands still saves projects.
            Workspace::Project => !self.module.project_roots().is_empty(),
            Workspace::Island => self.module.islands().is_some(),
            Workspace::Map => self.module.islands().and_then(|i| i.maps()).is_some(),
            Workspace::Ui => self.module.ui().is_some(),
            // Every game's cfgs edit; a project holds the result.
            Workspace::Model => !self.module.project_roots().is_empty(),
            Workspace::Assets => true,
        }
    }

    pub(crate) fn require_workspace(&mut self, workspace: Workspace) -> bool {
        if self.supports(workspace) {
            return true;
        }
        let title = self.module.profile().title;
        let reason = if self.module.islands().is_none() && self.module.ui().is_none() {
            format!(
                "{title} is browse only: the Assets workspace lists, previews and extracts its files"
            )
        } else {
            format!("{title} has no {} workspace", workspace.label())
        };
        self.message(reason, false);
        false
    }

    fn workspaces(&self) -> Vec<Workspace> {
        Workspace::ALL
            .into_iter()
            .filter(|&w| self.supports(w))
            .collect()
    }

    pub fn switch(&mut self, workspace: Workspace) {
        if !self.require_workspace(workspace) {
            return;
        }
        self.workspace = workspace;
        self.menu = None;
        if let Some(ui) = &mut self.ui {
            ui.close_popovers();
        }
        // A switch ends typing everywhere and gives the keys to the pane the
        // new workspace's dock last focused (round 11: the browser's caret
        // stayed up after Ctrl+3 while Delete went to the widget).
        self.keys_to_focused();
    }

    /// View → Reset panel layout: the workspace's panes as shipped, and
    /// the reset dock's focus takes the keys (round 13: a value went on
    /// typing, and 12 became 124).
    fn reset_layout(&mut self) {
        let i = self.dock_index();
        self.docks[i].reset();
        self.keys_to_focused();
    }

    /// The pane the shown dock focuses takes the keys, and typing ends
    /// (applied, as a click elsewhere would). For panes the UI editor does
    /// not route keys for, its keys stay as they are.
    fn keys_to_focused(&mut self) {
        let focused = self.docks[self.dock_index()].focused;
        let keys = self
            .pane_keys(focused)
            .or_else(|| self.ui.as_ref().map(|u| u.keys))
            .unwrap_or_default();
        self.keys_to(keys);
    }

    /// Esc in the graph or timeline: the canvas (or the tree beside it)
    /// takes the keys through the dock when it shows. Under a maximized
    /// pane nothing visible would take them (round 12: Esc, then Delete
    /// deleted the hidden widget), so they stay and the status says why.
    fn hand_back(&mut self) {
        let i = self.dock_index();
        let shown = self.docks[i].visible_panes();
        if let Some(&pane) = [Pane::UiCanvas, Pane::UiTree]
            .iter()
            .find(|p| shown.contains(p))
        {
            self.docks[i].focus(pane);
            self.keys_to(crate::ui::Keys::Canvas);
        } else if let Some(ui) = &mut self.ui {
            ui.message = Some((
                "The canvas is hidden: restore this pane to work on widgets".into(),
                false,
            ));
        }
    }

    /// Which keys a UI workspace pane takes when it is pressed or picked;
    /// None for panes the UI editor does not route keys for.
    fn pane_keys(&self, pane: Pane) -> Option<crate::ui::Keys> {
        use crate::ui::Keys;
        Some(match pane {
            Pane::UiTimeline => Keys::Timeline,
            Pane::UiBindings => Keys::Graph,
            Pane::UiCanvas | Pane::UiInspector | Pane::UiTree | Pane::UiFiles | Pane::UiLibrary => {
                Keys::Canvas
            }
            Pane::Browser | Pane::Preview if self.workspace == Workspace::Ui => Keys::Other,
            _ => return None,
        })
    }

    /// The games Import island reads from, the island rules' order.
    pub fn import_sources(&self) -> Vec<&'static dyn Module> {
        let from = self
            .module
            .islands()
            .map_or(&[][..], |i| i.rules().import_from);
        (from.iter())
            .map(|&g| crate::games::module(g))
            .filter(|m| m.islands().is_some())
            .collect()
    }

    /// Import's label where one button stands for every source: the game
    /// when there is one, else the picker in the dialog chooses.
    pub fn import_label(&self) -> String {
        match self.import_sources()[..] {
            [only] => format!("Import island from {}…", only.profile().title),
            _ => "Import island…".to_owned(),
        }
    }

    /// Switches to the workspace holding `pane`, brings it to the front and
    /// gives it the keys, as a click on its tab does.
    pub fn show_pane(&mut self, pane: Pane) {
        if let Some(i) = Workspace::ALL
            .iter()
            .position(|w| w.layout().contains(pane))
        {
            // Focus first: the switch hands the keys to the focused pane
            // (round 12: the "follows" link opened the graph with the
            // inspector's keys, and Delete deleted the widget).
            self.docks[i].focus(pane);
            self.switch(Workspace::ALL[i]);
        }
    }

    pub(crate) fn dock_index(&self) -> usize {
        Workspace::ALL
            .iter()
            .position(|&w| w == self.workspace)
            .expect("workspace in ALL")
    }

    fn undo(&mut self, redo: bool) {
        let done = match self.workspace {
            Workspace::Island => self.island.as_mut().is_some_and(|i| i.undo(redo)),
            Workspace::Map => self.map.as_mut().is_some_and(|m| m.undo(redo)),
            Workspace::Ui => self.ui.as_mut().is_some_and(|u| u.undo(redo)),
            Workspace::Model => self.model.as_mut().is_some_and(|m| m.undo(redo)),
            Workspace::Assets | Workspace::Project => false,
        };
        if done && self.workspace == Workspace::Map {
            self.check_map();
        }
        if !done {
            self.message(
                if redo {
                    "Nothing to redo"
                } else {
                    "Nothing to undo"
                },
                false,
            );
        }
    }

    /// Stages the open documents into the mod project and writes its archive.
    pub(crate) fn save(&mut self) -> Saved {
        // A half-typed value is applied first: it is what the field shows
        // (round 11 saved while Position X read 07, and Enter then made the
        // file dirty again). One the field refuses waits, as typed.
        if let Err(e) = self.end_typing() {
            self.message(format!("Not saved: {e}"), true);
            return Saved::Failed;
        }
        // Staging marks documents clean before disk I/O. A failed project
        // write must still count as unsaved when returning to the picker.
        self.pending_save = true;
        let mut staged = 0;
        // A new island gets its player marker from the shipped records.
        if self
            .island
            .as_ref()
            .is_some_and(|i| i.is_new() && i.dirty())
        {
            self.load_harbour_library();
        }
        if let Some(island) = &mut self.island
            && island.dirty()
            && let Some(install) = &self.install
        {
            match island.stage(install, &mut self.project) {
                Ok(()) => staged += 1,
                Err(e) => {
                    self.message(format!("Island not saved: {e:#}"), true);
                    return Saved::Failed;
                }
            }
        }
        match self.stage_map() {
            Ok(true) => staged += 1,
            Ok(false) => {}
            Err(e) => {
                self.message(format!("Map not saved: {e:#}"), true);
                return Saved::Failed;
            }
        }
        let waiting = matches!(
            self.map_dialog,
            Some(crate::map::panels::Dialog::SaveShipped(_))
        );
        // A shipped map waits for the dialog's answer; the rest saves now.
        let map_warning = self
            .map
            .as_mut()
            .and_then(|m| m.message.take())
            .filter(|m| m.1)
            .map(|m| m.0);
        // Staging warns about islands the game cannot use yet.
        let warning = self
            .island
            .as_mut()
            .and_then(|i| i.message.take())
            .filter(|(_, error)| *error)
            .map(|(text, _)| text)
            .or(map_warning);
        if let Some(m) = &mut self.model
            && m.dirty()
        {
            match m.stage(&mut self.project) {
                Ok(()) => staged += 1,
                Err(e) => {
                    self.message(format!("Model not saved: {e:#}"), true);
                    return Saved::Failed;
                }
            }
        }
        if let Some(ui) = &mut self.ui
            && ui.dirty()
        {
            match ui.stage(&mut self.project) {
                Ok(n) => staged += n,
                Err(e) => {
                    self.message(format!("UI asset not saved: {e:#}"), true);
                    return Saved::Failed;
                }
            }
        }
        let written = self.project.write();
        if written.is_ok() {
            self.pending_save = false;
        }
        self.mount_project();
        self.refresh_installed();
        self.undeclared = self.undeclared_models();
        let warning = warning.or_else(|| {
            let u = self.undeclared.first()?;
            Some(format!(
                "The island uses {} from {} ({}), which this mod does not require; players without it see gaps. Require it under Project → Dependencies",
                crate::project::plural(u.models.len(), "model", "models"),
                u.name,
                u.id
            ))
        });
        // One message for the save. A warning says what the game can't use
        // yet; the save itself went through, so it isn't shown as an error.
        match written {
            // Nothing else was edited: the shipped-map question is the news.
            Ok(_) if waiting && staged == 0 => {}
            Ok(path) => {
                let saved = format!(
                    "Saved {} to {}",
                    crate::project::plural(staged, "document", "documents"),
                    crate::project::short_path(&path),
                );
                match warning {
                    Some(warning) => self.message(format!("{saved}. {warning}"), false),
                    None => self.message(
                        format!(
                            "{saved}; {} in the mod. Update game copy (top right) puts it in the game",
                            crate::project::plural(self.project.file_count(), "file", "files"),
                        ),
                        false,
                    ),
                }
            }
            Err(e) => {
                self.message(format!("Mod not written: {e:#}"), true);
                return Saved::Failed;
            }
        }
        if waiting {
            Saved::Waiting
        } else {
            Saved::Written
        }
    }

    pub(crate) fn install_mod(&mut self, cx: &mut Context<Self>) {
        self.install_project(
            |path| {
                format!(
                    "Installed as {}. The game’s mod loader activates it on the next start",
                    path.display()
                )
            },
            cx,
        );
    }

    /// The islands and map layouts that can't leave the editor (level 5)
    /// and why, as one line: the project's islands as their last save
    /// found them, and the open island as it stands, since Install copies what was saved but a
    /// dirty island is about to be. None when nothing blocks.
    pub(crate) fn broken_islands(&self) -> Option<String> {
        let open = (self.island.as_ref().zip(self.install.as_deref())).map(|(island, install)| {
            (island.path.clone(), island.validate(install, &self.project))
        });
        let open_path = open.as_ref().map(|(path, _)| path.clone());
        let stored = (self.project.islands.iter())
            .filter(|i| open_path.as_ref() != Some(&i.path))
            .map(|i| (i.path.clone(), i.listing.broken.clone()));
        let named: Vec<String> = (open.into_iter().chain(stored))
            .filter(|(_, problems)| !problems.is_empty())
            .map(|(path, problems)| {
                format!(
                    "{}: {}",
                    crate::project::island_name(&path),
                    crate::island::said(&problems)
                )
            })
            .chain(self.broken_maps())
            .collect();
        (!named.is_empty()).then(|| named.join(". "))
    }

    /// Writes the project and copies it into the game off the UI thread:
    /// a first install of an 1800 project copies gigabytes across drives.
    /// `done` words the message once the copy has landed.
    pub(crate) fn install_project(
        &mut self,
        done: impl FnOnce(&std::path::Path) -> String + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.module.packaging().is_none() {
            return self.message("This game has no project installer", false);
        }
        if self.installing {
            return self.message("Still installing; try again once it is done", false);
        }
        if let Some(why) = self.broken_islands() {
            return self.message(format!("Not installed, fix first: {why}"), true);
        }
        let Some(install) = &self.install else {
            return self.message("No install is open", true);
        };
        let job = match self.project.prepare_install(install.dir()) {
            Ok(job) => job,
            Err(e) => return self.message(format!("Install failed: {e:#}"), true),
        };
        self.installing = true;
        self.message("Installing into the game…", false);
        cx.spawn(async move |this, cx| {
            let installed = cx.background_spawn(async move { job.run() }).await;
            this.update(cx, |this, cx| {
                this.installing = false;
                this.mount_project();
                this.refresh_installed();
                match installed {
                    Ok(path) => this.message(done(&path), false),
                    Err(e) => this.message(format!("Install failed: {e:#}"), true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Keys from the window. While a field is being typed into, keys that
    /// typed a character stop here: the platform would otherwise insert
    /// the same character again through the input handler.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let typing = self.active_edit().is_some();
        self.route_key(event, window, cx);
        let text = event.keystroke.key_char.is_some()
            && event.keystroke.modifiers.is_subset_of(&Modifiers::shift());
        if typing && text {
            cx.stop_propagation();
        }
        if self.active_edit().is_none() {
            gpui_editor_kit::field::end_composing();
        }
    }

    /// The field being typed into, in the order `route_key` tries them:
    /// what the platform's text input (IME, OS text services) works on.
    pub fn active_edit(&mut self) -> Option<&mut dyn InputBuffer> {
        if self.leaving.is_some() {
            return None;
        }
        if self.project_ui.new_project.is_some() {
            return self.project_ui.new_project.as_mut().map(|d| d.typing());
        }
        if self.project_ui.editing.is_some() {
            return self
                .project_ui
                .editing
                .as_mut()
                .map(|e| e as &mut dyn InputBuffer);
        }
        if self.wizard.is_some() {
            return self.wizard.as_mut().map(|w| w.typing());
        }
        if self.import.is_some() {
            return self.import.as_mut().map(|w| w.typing());
        }
        if self.island_filter_edit.is_some() {
            return self
                .island_filter_edit
                .as_mut()
                .map(|e| e as &mut dyn InputBuffer);
        }
        if self.model_list.filter_edit.is_some() {
            return (self.model_list.filter_edit.as_mut()).map(|e| e as &mut dyn InputBuffer);
        }
        match &mut self.map_dialog {
            Some(crate::map::panels::Dialog::New(d)) => return Some(&mut d.name),
            Some(crate::map::panels::Dialog::SaveShipped(d)) => return Some(&mut d.name),
            Some(crate::map::panels::Dialog::Open(d)) => return Some(&mut d.filter),
            None => {}
        }
        if let Some(p) = &mut self.model_picker {
            return Some(p.typing());
        }
        match self.workspace {
            Workspace::Island => self.island.as_mut().and_then(|i| i.typing()),
            Workspace::Map => self.map.as_mut().and_then(|m| m.typing_buffer()),
            Workspace::Ui => {
                if self.ui_find.typing().is_some() {
                    return self.ui_find.typing();
                }
                // The asset browser's search, shown in this workspace too.
                if self
                    .ui
                    .as_ref()
                    .is_some_and(|u| u.keys == crate::ui::Keys::Other)
                    && self.browser.typing().is_some()
                {
                    return self.browser.typing();
                }
                self.ui.as_mut().and_then(|u| u.typing())
            }
            Workspace::Model => self.model.as_mut().and_then(|m| m.typing_buffer()),
            Workspace::Assets => self.browser.typing(),
            Workspace::Project => None,
        }
    }

    /// Redraws every pane: a composing edit can sit in any of them.
    fn notify_panes(&self, cx: &mut Context<Self>) {
        for (_, view) in &self.pane_views {
            App::notify(cx, view.entity_id());
        }
        cx.notify();
    }

    fn route_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.leaving.is_some() {
            if event.keystroke.key == "escape" {
                self.leaving = None;
                cx.notify();
            }
            return;
        }
        if self.project_ui.new_project.is_some() || self.project_ui.editing.is_some() {
            return self.project_key(event, cx);
        }
        if self.wizard.is_some() {
            return self.wizard_key(event, cx);
        }
        if self.import.is_some() {
            return self.import_key(event, cx);
        }
        if self.wm_dialog && event.keystroke.key == "escape" {
            self.wm_dialog = false;
            cx.notify();
            return;
        }
        if self.map_dialog.is_some() {
            return self.map_dialog_key(event, cx);
        }
        if self.model_picker.is_some() {
            self.model_picker_key(event, cx);
            return;
        }
        if self.playtest.is_some() {
            if event.keystroke.key == "escape" {
                self.playtest = None;
                cx.notify();
            }
            return;
        }
        if let Some(edit) = &mut self.island_filter_edit {
            use gpui_editor_kit::field::EditEvent;
            let used = match edit.key_down_in(event, cx) {
                EditEvent::Commit | EditEvent::Cancel => {
                    self.island_filter_edit = None;
                    true
                }
                EditEvent::Changed => {
                    self.island_filter = edit.text.clone();
                    true
                }
                // Keys the field does not use (Ctrl 1, Ctrl S) fall through
                // to the shortcuts below.
                EditEvent::Ignored => false,
            };
            if used {
                cx.notify();
                return;
            }
        }
        if let Some(edit) = &mut self.model_list.filter_edit {
            use gpui_editor_kit::field::EditEvent;
            let used = match edit.key_down_in(event, cx) {
                EditEvent::Commit | EditEvent::Cancel => {
                    self.model_list.filter_edit = None;
                    true
                }
                EditEvent::Changed => {
                    self.model_list.filter = edit.text.clone();
                    true
                }
                EditEvent::Ignored => false,
            };
            if used {
                self.notify_pane(Pane::ModelList, cx);
                cx.notify();
                return;
            }
        }
        let consumed = match self.workspace {
            Workspace::Island => self
                .island
                .as_mut()
                .is_some_and(|i| i.key_down(event, window, cx)),
            Workspace::Ui => {
                // After a click in the timeline, its keys (Delete, Home,
                // frame steps) go to it before the canvas and the tree.
                let timeline_first = self.ui.as_ref().is_some_and(|u| u.timeline_focused())
                    && self.ui.as_mut().is_some_and(|u| u.typing().is_none());
                // An open text field (an inspector value being typed) keeps its keys.
                let graph_first = self
                    .ui
                    .as_mut()
                    .is_some_and(|u| u.graph_focused() && u.typing().is_none());
                let typing = self.ui_find.typing().is_some()
                    || self.ui.as_mut().is_some_and(|u| u.typing().is_some());
                // The asset browser or preview has the keys: its search and
                // tree take them, and nothing else but the editor's own
                // shortcuts and Space (play) goes on to a widget.
                let other = self
                    .ui
                    .as_ref()
                    .is_some_and(|u| u.keys == crate::ui::Keys::Other);
                let consumed = self.ui_find.key_down(event)
                    || (other
                        && (self.browser_key(event, cx)
                            || !(crate::ui::editor_shortcut(&event.keystroke)
                                || event.keystroke.key == "space")))
                    || (graph_first && self.ui.as_mut().is_some_and(|u| u.graph_key_down(event)))
                    || (timeline_first && self.timeline_key(event, cx))
                    || self.ui.as_mut().is_some_and(|u| u.key_down(event))
                    // A chord the typed field passed on stops here, before
                    // the tree's widget commands.
                    || (typing && crate::ui::field_keeps(&event.keystroke))
                    // Widget commands only while the canvas and tree have
                    // the keys: the graph's and the timeline's stop there.
                    || (self
                        .ui
                        .as_ref()
                        .is_some_and(|u| u.keys == crate::ui::Keys::Canvas)
                        && self.ui_tree_key(event))
                    || self.timeline_key(event, cx);
                if self
                    .ui
                    .as_mut()
                    .is_some_and(|u| std::mem::take(&mut u.hand_back))
                {
                    self.hand_back();
                }
                consumed
            }
            Workspace::Map => self.map_typing_key(event, cx) || self.map_key(event, cx),
            // The hierarchy first while it has the keys (last clicked, or its
            // search typed into): its arrows and Home move the selection,
            // which elsewhere turn and reset the view.
            // Alt Left goes back a model before the hierarchy takes Left.
            Workspace::Model if event.keystroke.modifiers.alt && event.keystroke.key == "left" => {
                self.model_back(cx);
                true
            }
            Workspace::Model => {
                let searching = (self.model.as_ref())
                    .and_then(|m| m.typing.as_ref())
                    .is_some_and(|t| t.field == crate::model::Field::HierarchyFilter);
                let hierarchy = self.docks[self.dock_index()].focused == Pane::ModelHierarchy;
                ((hierarchy || searching) && self.model_hierarchy_key(event, cx))
                    || self.model_typing_key(event, cx)
                    || self.model_key(event, cx)
            }
            Workspace::Assets => self.browser_key(event, cx),
            Workspace::Project => false,
        };
        if consumed {
            self.drain_messages();
            cx.notify();
            return;
        }
        let mods = event.keystroke.modifiers;
        let ctrl = mods.control || mods.platform;
        let key = event.keystroke.key.as_str();
        // Project, island and UI commands, which a game only browsed lacks.
        let project_key =
            ctrl && (matches!(key, "s" | "n" | "o") || mods.shift && key == "u") || key == "f5";
        match key {
            _ if project_key && !self.require_workspace(Workspace::Project) => {}
            "z" if ctrl => self.undo(mods.shift),
            "y" if ctrl => self.undo(true),
            // On the Island workspace the viewport pill promises Ctrl S
            // copies a game original into the mod, so an untouched one is
            // staged too.
            "s" if ctrl => {
                if self.workspace == Workspace::Island
                    && let Some(i) = &mut self.island
                    && !self.project.islands.iter().any(|p| p.path == i.path)
                {
                    i.want_in_mod();
                }
                self.save();
            }
            "0" if ctrl => self.switch(Workspace::Project),
            "q" if ctrl => self.leave(Leave::Quit, cx),
            "n" if ctrl => self.open_new_project(cx),
            "u" if ctrl && event.keystroke.modifiers.shift => self.install_mod(cx),
            "f5" => self.open_playtest(),
            "o" if ctrl => self.pick_project_folder(cx),
            "1" if ctrl => self.switch(Workspace::Assets),
            "2" if ctrl => self.switch(Workspace::Island),
            "3" if ctrl => self.switch(Workspace::Map),
            "4" if ctrl => self.switch(Workspace::Ui),
            "5" if ctrl => self.switch(Workspace::Model),
            "p" if ctrl && self.workspace == Workspace::Model => {
                self.open_model_picker(crate::model::picker::Want::Open, cx)
            }
            "escape" => self.menu = None,
            _ => return,
        }
        cx.notify();
    }

    /// Sub-editors report through their own status slot so they stay free of
    /// the window type; surface it here.
    pub fn drain_messages(&mut self) {
        let message = self
            .island
            .as_mut()
            .and_then(|i| i.message.take())
            .or_else(|| self.map.as_mut().and_then(|m| m.message.take()))
            .or_else(|| self.ui.as_mut().and_then(|u| u.message.take()))
            .or_else(|| self.model.as_mut().and_then(|m| m.message.take()))
            .or_else(|| self.browser.message.take());
        if let Some((text, error)) = message {
            self.message(text, error);
        }
    }

    fn menu_popup(&self, menu: Menu, cx: &mut Context<Self>) -> Stateful<Div> {
        let close = cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        let item = |label: &'static str, shortcut: &'static str, f: fn(&mut Self)| {
            menu_item(label, shortcut).on_click(cx.listener(move |this, _, _, cx| {
                this.menu = None;
                f(this);
                cx.notify();
            }))
        };
        let with_cx = |label: &'static str, f: fn(&mut Self, &mut Context<Self>)| {
            menu_item(label, "").on_click(cx.listener(move |this, _, _, cx| {
                this.menu = None;
                f(this, cx);
                cx.notify();
            }))
        };
        let popup = match menu {
            // Centred under the project button, measured last frame.
            Menu::Project => widgets::menu_popup_right(self.project_button_right),
            // Under each menu's button; Terrain follows View.
            _ => widgets::menu_popup(match menu {
                Menu::File => 4.,
                Menu::Edit => 40.,
                Menu::Island | Menu::Map | Menu::Model => 125.,
                _ => 80.,
            }),
        }
        .on_mouse_down_out(close);
        // Island commands need an open island; without one they say so
        // instead of failing on a click. Each carries a second line saying
        // what it reads, writes or replaces.
        let generate =
            |label: &'static str, detail: &'static str, f: fn(&mut Self, &mut Context<Self>)| {
                if self.island.is_some() {
                    widgets::menu_item_detail(label, label, detail, "", false)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            f(this, cx);
                            cx.notify();
                        }))
                        .into_any_element()
                } else {
                    widgets::menu_item_off(label, "Open an island first").into_any_element()
                }
            };
        match menu {
            // A game only browsed has no project to make, save or install.
            Menu::File if self.module.islands().is_none() => popup
                .child(with_cx("Switch game…", |this, cx| {
                    this.leave(Leave::Game, cx)
                }))
                .child(widgets::separator())
                .child(
                    menu_item("Quit", "Ctrl Q")
                        .on_click(cx.listener(|this, _, _, cx| this.leave(Leave::Quit, cx))),
                ),
            Menu::File => {
                let current = self.project.folder();
                let recent: Vec<PathBuf> = crate::project::recent(&self.config)
                    .into_iter()
                    .filter(|p| *p != current)
                    .take(MENU_RECENT)
                    .collect();
                // Headed groups, as the Terrain and View menus are.
                popup
                    .child(widgets::menu_header("Project"))
                    .child(menu_item("New project…", "Ctrl N").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.menu = None;
                            this.open_new_project(cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        menu_item("Open mod folder…", "Ctrl O").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.menu = None;
                                this.pick_project_folder(cx);
                                cx.notify();
                            },
                        )),
                    )
                    // In the Project group, as in the project switcher.
                    .child(with_cx("Switch game…", |this, cx| {
                        this.leave(Leave::Game, cx)
                    }))
                    .when(!recent.is_empty(), |p| {
                        p.child(widgets::menu_section("Recent"))
                    })
                    .children(recent.into_iter().map(|folder| {
                        let name = folder
                            .file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        menu_item(name, "").on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            this.leave(Leave::Project(folder.clone()), cx);
                            cx.notify();
                        }))
                    }))
                    .child(widgets::menu_section("Islands"))
                    .child(with_cx("New island…", Self::open_wizard))
                    // One row per game Import reads from.
                    .children(self.import_sources().into_iter().map(|from| {
                        menu_item(format!("Import island from {}…", from.profile().title), "")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.menu = None;
                                this.open_import_from(from, cx);
                                cx.notify();
                            }))
                    }))
                    // A game without regions (2070) has nothing to convert
                    // between.
                    .when(!self.island_rules().regions.is_empty(), |p| {
                        p.child(with_cx(
                            "Convert island to another region…",
                            Self::open_convert,
                        ))
                    })
                    // The project's islands under Islands, in its group: past
                    // a rule of their own they read as a stray block.
                    .children(self.project.islands.iter().take(MENU_ISLANDS).map(|i| {
                        let path = i.path.clone();
                        let name = crate::project::island_name(&path);
                        menu_item(format!("Open {name}"), "").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.menu = None;
                                this.open_island(&path, cx);
                                cx.notify();
                            },
                        ))
                    }))
                    .when(self.supports(Workspace::Model), |p| {
                        p.child(widgets::menu_section("Models"))
                            .child(menu_item("Open model…", "Ctrl P").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.menu = None;
                                    if this.require_workspace(Workspace::Model) {
                                        this.switch(Workspace::Model);
                                        this.open_model_picker(
                                            crate::model::picker::Want::Open,
                                            cx,
                                        );
                                    }
                                    cx.notify();
                                },
                            )))
                            .children(self.model_recent.iter().take(MENU_ISLANDS).map(|path| {
                                let path = path.clone();
                                let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
                                menu_item(format!("Open {name}"), "").on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.menu = None;
                                        this.open_model(&path, cx);
                                        cx.notify();
                                    },
                                ))
                            }))
                    })
                    .child(widgets::menu_section("Save and play"))
                    // One name for Ctrl S on one screen: on a game original
                    // it copies the island into the mod, which the
                    // Inspector's button says; "Save project" hid that.
                    .child(
                        match self
                            .island
                            .as_ref()
                            .filter(|i| !self.project.islands.iter().any(|p| p.path == i.path))
                        {
                            Some(_) => menu_item(
                                format!("Add island to ‘{}’", self.project.info.name),
                                "Ctrl S",
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.menu = None;
                                if let Some(i) = &mut this.island {
                                    i.want_in_mod();
                                }
                                this.save();
                                cx.notify();
                            })),
                            None => item("Save project", "Ctrl S", |this| {
                                this.save();
                            }),
                        },
                    )
                    .when(self.module.packaging().is_some(), |p| {
                        p.child(menu_item(self.install_label(), "Ctrl Shift U").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.menu = None;
                                this.install_mod(cx);
                                cx.notify();
                            }),
                        ))
                    })
                    .child(match self.playtest_blocker() {
                        // The next step on its own line, short enough for
                        // the menu; the full sentence is the status message.
                        Some(why) => widgets::menu_item_off_why(
                            "Test in game…",
                            if why.starts_with("Save") {
                                "Save the island first"
                            } else if self.module.packaging().is_none() {
                                "This game has no project installer"
                            } else {
                                "Needs a saved island on the test map"
                            },
                            "F5",
                        )
                        .into_any_element(),
                        // The Overview's words while the game copy is old:
                        // one command, one description of what it does.
                        None => item(
                            if self.project_ui.installed == crate::project::Installed::Older {
                                "Update & test in game…"
                            } else {
                                "Test in game…"
                            },
                            "F5",
                            Self::open_playtest,
                        )
                        .into_any_element(),
                    })
                    // The test map alone, into the project folder: the step
                    // a game without packaging (2070) has.
                    .when(
                        self.module.islands().and_then(|i| i.maps()).is_some(),
                        |p| {
                            p.child(match self.test_map_blocker() {
                                Some(why) => widgets::menu_item_off_why(
                                    "Make test map",
                                    if why.starts_with("Save") {
                                        "Save the island first"
                                    } else {
                                        "Needs a saved island on the test map"
                                    },
                                    "",
                                )
                                .into_any_element(),
                                None => item("Make test map", "", Self::make_test_map)
                                    .into_any_element(),
                            })
                        },
                    )
                    .child(widgets::separator())
                    .child(
                        menu_item("Quit", "Ctrl Q")
                            .on_click(cx.listener(|this, _, _, cx| this.leave(Leave::Quit, cx))),
                    )
            }
            // Without an island the tools have nothing to work on; say so
            // once instead of listing them all greyed out.
            Menu::Island if self.island.is_none() => popup
                .child(
                    div()
                        .px_2()
                        .py_2()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child("Terrain and tint tools work on an open island."),
                )
                .child(item("Go to the islands list", "Ctrl 2", |this| {
                    this.switch(Workspace::Island)
                })),
            // Files in and out first, then what is computed from the
            // terrain, so a command's consequence shows in its group.
            Menu::Island => popup
                .child(widgets::menu_header("Import"))
                // Each import replaces data as soon as a file is picked, so it
                // says what, as Generate's items do; the heightmap the range an
                // 8-bit image is read as (16-bit is the game's own units). No
                // "Ctrl Z undoes it" on each: said five times, it hid what
                // differed, and every edit undoes.
                .child(generate(
                    "Import heightmap…",
                    // Non-breaking spaces before the units: wrapped, a lone "m"
                    // sat on its own line.
                    "Replaces heights · 8-bit: black −12\u{a0}m, white 40\u{a0}m",
                    Self::import_heightmap,
                ))
                .child(generate(
                    "Import terrain mesh (OBJ/glTF)…",
                    "Replaces heights",
                    Self::import_terrain_mesh,
                ))
                // A second line, as Generate's: a link keeps acting after
                // the dialog closes.
                .child(generate(
                    "Link World Machine export…",
                    "Replaces heights on each re-export",
                    |this, _| this.open_wm_dialog(),
                ))
                .child(generate(
                    "Import tint map…",
                    "Replaces painted tint",
                    Self::import_tint,
                ))
                .child(widgets::menu_section("Export"))
                // The format each writes, the round trip its import reads.
                .child(generate(
                    "Export heightmap…",
                    "16-bit PNG in the game’s height units",
                    Self::export_heightmap,
                ))
                .child(generate(
                    "Export tint map…",
                    "PNG of the painted tint",
                    Self::export_tint,
                ))
                .child(widgets::menu_section("Generate"))
                // Each says what it overwrites before it runs; both undo.
                .child(generate(
                    "Rebuild land and buildable areas",
                    "Replaces painted Land and Buildable cells",
                    |this, _| {
                        if let Some(i) = &mut this.island {
                            i.derive_land();
                        }
                        this.drain_messages();
                    },
                ))
                .child(generate(
                    "Bake tint from terrain",
                    "Replaces painted tint",
                    Self::bake_tint,
                )),
            Menu::Map => {
                let variants = self.map.as_ref().map_or(0, |m| m.map.variants.len());
                popup
                    .child(widgets::menu_header("Layouts"))
                    .child(with_cx("New map layout…", Self::open_new_map))
                    .child(with_cx("Open map…", Self::open_map_dialog))
                    .when(self.map.is_some(), |p| {
                        p.child(with_cx("Close map", |this, cx| {
                            this.leave(Leave::CloseMap, cx)
                        }))
                    })
                    .children(self.project.layouts.iter().take(MENU_ISLANDS).map(|l| {
                        let path = l.path.clone();
                        menu_item(format!("Open {}", l.name), "").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.menu = None;
                                this.open_map(&path, cx);
                                cx.notify();
                            },
                        ))
                    }))
                    .when(variants > 1, |p| {
                        p.child(widgets::menu_section("Variants")).child(
                            menu_item(crate::map::view::copy_label(variants), "").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.menu = None;
                                    this.copy_variant(cx);
                                    cx.notify();
                                }),
                            ),
                        )
                    })
            }
            Menu::Model => {
                use crate::model::picker::{Kind, Want};
                use gpui_editor_kit::gizmo::Mode;
                // How a row shows its state: an action, an option, or one of
                // a set (the gizmo's modes).
                enum Mark {
                    Plain,
                    Check(bool),
                    Radio(bool),
                }
                // One row that runs `f` on the open model and closes the menu.
                let on_model =
                    |label: String,
                     shortcut: &'static str,
                     mark: Mark,
                     f: fn(&mut crate::model::ModelEditor)| {
                        let row = match mark {
                            Mark::Plain => menu_item(label.clone(), shortcut),
                            Mark::Check(on) => {
                                widgets::menu_item_checked(label.clone(), on, shortcut)
                            }
                            Mark::Radio(on) => {
                                widgets::menu_item_radio(label.clone(), on, shortcut)
                            }
                        };
                        match self.model.is_some() {
                            true => row
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.menu = None;
                                    if let Some(m) = &mut this.model {
                                        f(m);
                                    }
                                    this.model_edited(cx);
                                }))
                                .into_any_element(),
                            false => widgets::menu_item_off(label, "Open a model first")
                                .into_any_element(),
                        }
                    };
                let m = self.model.as_ref();
                let selected = m.and_then(|m| m.selected).is_some();
                let gizmo = m.map(|m| m.gizmo);
                let shown = m.map(|m| m.view.shown);
                let back = self
                    .previous_model()
                    .map(|p| p.rsplit('/').next().unwrap_or(p).to_owned());
                let picker = |label: &'static str, shortcut: &'static str, want: Want| {
                    menu_item(label, shortcut).on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        this.open_model_picker(want, cx);
                        cx.notify();
                    }))
                };
                let mut popup = popup
                    .child(widgets::menu_header("Models"))
                    .child(picker("Open model…", "Ctrl P", Want::Open))
                    .child(match back {
                        Some(name) => menu_item(format!("Back to {name}"), "Alt ←")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.menu = None;
                                this.model_back(cx);
                                cx.notify();
                            }))
                            .into_any_element(),
                        None => widgets::menu_item_off("Back", "No model was open before this one")
                            .into_any_element(),
                    })
                    .child(with_cx(
                        "Browse game models in Assets",
                        Self::browse_game_models,
                    ));
                if self.model.is_some() {
                    // Meshes and decals add to 117's and 1800's cfgs, not to a
                    // prp (one mesh) or a 2070 cfg, as the inspector offers.
                    let baked = m.is_some_and(|m| {
                        m.doc.prefix().is_empty()
                            && m.doc.child(anno_formats::xml::Id::ROOT, "Prop").is_none()
                    });
                    popup = popup
                        .child(widgets::menu_section("Add"))
                        .child(picker("Add prop…", "", Want::Add(Kind::Prop)))
                        .child(picker("Add particle effect…", "", Want::Add(Kind::Effect)))
                        .when(baked, |p| {
                            p.child(picker("Add mesh…", "", Want::Add(Kind::Mesh)))
                                .child(picker("Add decal…", "", Want::Add(Kind::Texture)))
                        })
                        .child(widgets::menu_section("Selection"))
                        .child(match selected {
                            true => on_model("Duplicate".into(), "Ctrl D", Mark::Plain, |m| {
                                if let Some(s) = m.selected {
                                    m.duplicate(s);
                                }
                            }),
                            false => widgets::menu_item_off("Duplicate", "Select an entry first")
                                .into_any_element(),
                        })
                        .child(match selected {
                            true => on_model("Delete".into(), "Del", Mark::Plain, |m| {
                                if let Some(s) = m.selected {
                                    m.remove(s);
                                }
                            }),
                            false => widgets::menu_item_off("Delete", "Select an entry first")
                                .into_any_element(),
                        })
                        .child(on_model("Frame selection".into(), "F", Mark::Plain, |m| {
                            m.frame_selection()
                        }))
                        .child(on_model(
                            "Reset view".into(),
                            "Ctrl Home",
                            Mark::Plain,
                            |m| m.view.reset(),
                        ))
                        .child(widgets::menu_section("Gizmo"))
                        .child(on_model(
                            "Move".into(),
                            "W",
                            Mark::Radio(gizmo == Some(Mode::Move)),
                            |m| m.gizmo = Mode::Move,
                        ))
                        .child(on_model(
                            "Rotate".into(),
                            "E",
                            Mark::Radio(gizmo == Some(Mode::Rotate)),
                            |m| m.gizmo = Mode::Rotate,
                        ))
                        .child(on_model(
                            "Scale".into(),
                            "R",
                            Mark::Radio(gizmo == Some(Mode::Scale)),
                            |m| m.gizmo = Mode::Scale,
                        ))
                        .child(on_model(
                            "Local axes".into(),
                            "",
                            Mark::Check(m.is_some_and(|m| m.view.local)),
                            |m| m.view.local = !m.view.local,
                        ))
                        .child(widgets::menu_section("Playback"))
                        .child(on_model(
                            "Play".into(),
                            "Space",
                            Mark::Check(m.is_some_and(|m| m.playback.playing)),
                            |m| m.playback.playing = !m.playback.playing,
                        ))
                        .child(on_model("Rewind".into(), "", Mark::Plain, |m| {
                            m.playback.time = 0.;
                            m.pose();
                        }))
                        .child(widgets::menu_section("Light"))
                        .child(on_model(
                            "Night".into(),
                            "N",
                            Mark::Check(m.is_some_and(|m| m.view.night)),
                            |m| m.set_night(!m.view.night),
                        ))
                        .child(widgets::menu_section("Show"));
                    for (overlay, label, key) in crate::model::view::OVERLAYS {
                        let on = shown.is_some_and(|s| s.has(overlay));
                        popup = popup.child(widgets::menu_item_checked(label, on, key).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.menu = None;
                                if let Some(m) = &mut this.model {
                                    m.toggle_overlay(overlay);
                                }
                                this.model_edited(cx);
                            }),
                        ));
                    }
                }
                popup
            }
            Menu::Project => {
                let current = self.project.folder();
                popup
                    .child(widgets::menu_header("Projects"))
                    .children(
                        crate::project::known(&self.config, self.module)
                            .into_iter()
                            .take(MENU_RECENT * 2)
                            .map(|p| {
                                let here = p.folder == current;
                                let folder = p.folder.clone();
                                // Names collide across games and copies;
                                // the folder tells them apart.
                                widgets::menu_item_detail(
                                    format!("project-{}", p.folder.display()),
                                    p.name,
                                    crate::project::short_path(&p.folder),
                                    crate::project::plural(p.islands, "island", "islands"),
                                    here,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.menu = None;
                                        if !here {
                                            this.leave(Leave::Project(folder.clone()), cx);
                                        }
                                        cx.notify();
                                    },
                                ))
                            }),
                    )
                    .child(widgets::separator())
                    // The File menu's shortcuts, here too.
                    .child(menu_item("New project…", "Ctrl N").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.menu = None;
                            this.open_new_project(cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        menu_item("Open mod folder…", "Ctrl O").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.menu = None;
                                this.pick_project_folder(cx);
                                cx.notify();
                            },
                        )),
                    )
                    // The other game's projects are behind Switch game; the
                    // breadcrumb names the game, so the way out is here too.
                    .child(with_cx("Switch game…", |this, cx| {
                        this.leave(Leave::Game, cx)
                    }))
            }
            Menu::Edit => popup
                .child(item("Undo", "Ctrl Z", |this| this.undo(false)))
                .child(item("Redo", "Ctrl Y", |this| this.undo(true))),
            // Workspaces switch in the bar (Ctrl 0-4); the menu holds what
            // the bar does not.
            Menu::View => {
                let (stats, passes) = (self.stats, self.stats && self.pass_times);
                let perf = |cx: &mut Context<Self>| {
                    [
                        widgets::menu_item_checked("Performance overlay", stats, "")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.menu = None;
                                this.stats = !this.stats;
                                cx.notify();
                            }))
                            .into_any_element(),
                        widgets::menu_item_checked("GPU pass times", passes, "")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.menu = None;
                                this.pass_times = !passes;
                                this.stats |= this.pass_times;
                                cx.notify();
                            }))
                            .into_any_element(),
                    ]
                };
                let mut perf_in_viewport = false;
                popup
                    // Workspaces first, with the shortcuts the switcher has.
                    .child(widgets::menu_header("Workspace"))
                    .children(self.workspaces().into_iter().map(|w| {
                        let n = Workspace::ALL.iter().position(|a| *a == w).unwrap_or(0);
                        widgets::menu_item_radio(
                            w.label(),
                            self.workspace == w,
                            format!("Ctrl {n}"),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            this.switch(w);
                            cx.notify();
                        }))
                    }))
                    // Viewports apart from the panels around them, each group
                    // in the workspace's own order; a click closes an open one
                    // or brings a closed one back.
                    .children({
                        let i = self.dock_index();
                        let open = &self.docks[i].layout;
                        let viewport = |p: Pane| {
                            matches!(
                                p,
                                Pane::IslandView | Pane::IslandTop | Pane::UiCanvas | Pane::Preview
                            )
                        };
                        let mut items: Vec<AnyElement> = Vec::new();
                        // On Project, with no viewports, the overlay stays
                        // under Window.
                        for (heading, views) in [("Viewport", true), ("Panes", false)] {
                            // As they sit on screen: the default layout's order.
                            let panes: Vec<Pane> = self
                                .workspace
                                .layout()
                                .panes()
                                .into_iter()
                                .filter(|_| self.workspace != Workspace::Project)
                                .filter(|p| viewport(*p) == views)
                                .collect();
                            if panes.is_empty() {
                                continue;
                            }
                            items.push(widgets::menu_section(heading).into_any_element());
                            for p in panes {
                                let shown = open.contains(p);
                                items.push(
                                    // "Perspective tab": two checked tabs read as
                                    // two camera modes at once.
                                    widgets::menu_item_checked(
                                        if views {
                                            format!("{} tab", self.pane_label(p))
                                        } else {
                                            self.pane_label(p).to_string()
                                        },
                                        shown,
                                        "",
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.menu = None;
                                        let i = this.dock_index();
                                        if this.docks[i].layout.contains(p) {
                                            this.docks[i].close(p);
                                        } else {
                                            this.docks[i].focus(p);
                                        }
                                        // Opened, or the focus moved off a
                                        // closed pane: as through the dock.
                                        this.keys_to_focused();
                                        cx.notify();
                                    }))
                                    .into_any_element(),
                                );
                            }
                            // The overlay draws over the viewports: with
                            // them, not under a heading of its own.
                            if views {
                                perf_in_viewport = true;
                                items.extend(perf(cx));
                            }
                        }
                        // Reset after the panes it resets, past a rule: a
                        // "Window" heading over it alone read as generated.
                        // An action, so no box: it must not read as a toggle
                        // that is off. "panel": "Reset project layout" read
                        // as resetting the mod.
                        if !items.is_empty() {
                            items.push(widgets::separator().into_any_element());
                            items.push(
                                widgets::menu_item("Reset panel layout", "")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.menu = None;
                                        this.reset_layout();
                                        cx.notify();
                                    }))
                                    .into_any_element(),
                            );
                        }
                        items
                    })
                    // On Project, with no panels to reset, the overlay alone.
                    .when(!perf_in_viewport, |p| {
                        p.child(widgets::menu_section("Window")).children(perf(cx))
                    })
                    // Last: set once, then rarely touched.
                    .child(widgets::menu_section("Theme"))
                    .children(
                        [
                            (crate::colors::Choice::Dark, "Dark"),
                            (crate::colors::Choice::Light, "Light"),
                            (crate::colors::Choice::System, "Match system"),
                        ]
                        .map(|(choice, label)| {
                            widgets::menu_item_radio(label, crate::colors::choice() == choice, "")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.menu = None;
                                    crate::colors::choose(choice, window, cx);
                                    cx.notify();
                                }))
                        }),
                    )
            }
        }
    }
}

impl DockHost for Editor {
    type Pane = Pane;

    fn dock(&mut self) -> &mut Dock<Pane> {
        let i = self.dock_index();
        &mut self.docks[i]
    }

    // The bottom strips whose panes act on an open document: with none,
    // their tab bar sat under the empty state with nothing behind it.
    fn pane_idle(&self, pane: Pane) -> bool {
        match pane {
            Pane::IslandLibrary | Pane::IslandMaterials => self.island.is_none(),
            Pane::MapIslands => self.map.is_none(),
            Pane::ModelTimeline => self.model.is_none(),
            Pane::UiTimeline | Pane::UiBindings => self.ui.is_none(),
            _ => false,
        }
    }

    fn pane_chrome(&self, pane: Pane) -> bool {
        // Pages and viewports are the workspace itself; only side panes
        // maximize or close.
        !matches!(
            pane,
            Pane::ProjectOverview
                | Pane::ProjectModels
                | Pane::ProjectMods
                | Pane::IslandView
                | Pane::IslandTop
                | Pane::MapView
                | Pane::UiCanvas
                | Pane::ModelView
        )
    }

    fn pane_label(&self, pane: Pane) -> SharedString {
        match pane {
            Pane::ProjectOverview => "Overview",
            Pane::ProjectModels => "Models",
            Pane::ProjectMods => "Dependencies",
            // "Asset": beside UI files and Canvas, a bare Browser and
            // Preview did not say what they browse.
            Pane::Browser => "Asset browser",
            Pane::IslandList => "Islands",
            Pane::Preview => "Asset preview",
            Pane::IslandView => "Perspective",
            Pane::IslandTop => "Top",
            Pane::IslandHierarchy => "Hierarchy",
            Pane::IslandLayers => "Layers",
            Pane::IslandGraphics => "Graphics",
            Pane::IslandInspector => "Inspector",
            Pane::IslandMaterials => "Ground material",
            Pane::IslandLibrary => "Library",
            Pane::MapView => "Map",
            Pane::MapSlots => "Slots",
            Pane::MapInspector => "Inspector",
            Pane::MapIslands => "Islands",
            Pane::UiTree => "Widget tree",
            Pane::UiCanvas => "Canvas",
            Pane::UiInspector => "Inspector",
            Pane::UiFiles => "UI files",
            Pane::UiTimeline => "Timeline",
            Pane::UiBindings => "Bindings",
            Pane::UiLibrary => "Widgets",
            Pane::ModelView => "Model",
            Pane::ModelList => "Models",
            Pane::ModelHierarchy => "Hierarchy",
            Pane::ModelInspector => "Inspector",
            Pane::ModelTimeline => "Timeline",
        }
        .into()
    }

    fn render_pane(
        &mut self,
        pane: Pane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = match self.pane_views.iter().find(|(p, _)| *p == pane) {
            Some((_, view)) => view.clone(),
            None => {
                let editor = cx.entity();
                let view = cx.new(|cx| {
                    cx.observe(&editor, |_, _, cx| cx.notify()).detach();
                    PaneView {
                        editor: editor.downgrade(),
                        pane,
                    }
                });
                self.pane_views.push((pane, view.clone()));
                view
            }
        };
        // Cached views take their size from this style alone, not their
        // content; panes fill their dock slot, so that is exact.
        view.cached(StyleRefinement::default().size_full())
            .into_any_element()
    }

    fn pane_focused(&mut self, _: Pane, _: &mut Context<Self>) {
        if let Some(island) = &mut self.island {
            island.end_drags();
        }
    }

    fn pane_chosen(&mut self, pane: Pane, _: &mut Context<Self>) {
        // A pane picked through the dock (tab, maximize, unfold, close, drop)
        // takes the keys, as a press inside it does (round 9: the Timeline
        // tab left Delete with the canvas; round 11: maximizing the
        // Timeline did, and Delete removed a widget off screen). Presses in
        // a pane go through its own capture, which lets a press in the field
        // being typed place the caret.
        if let Some(keys) = self.pane_keys(pane) {
            self.keys_to(keys);
        }
    }
}

/// One dock pane as its own view, so GPUI reuses its last layout and paint
/// until it is notified. Each pane observes the editor (GPUI does not
/// invalidate cached views for what they read), so notifying the editor
/// redraws every pane; camera moves notify only the viewports
/// (`Editor::notify_viewports`) and the UI canvas's hover, pan and zoom
/// only it (`Editor::notify_pane`), and the rest of the window stays as
/// drawn (the vendored GPUI keeps nested cached views, see its PATCH.md).
pub struct PaneView {
    editor: WeakEntity<Editor>,
    pane: Pane,
}

impl Render for PaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.pane;
        self.editor
            .update(cx, |editor, cx| {
                editor.render_pane_content(pane, window, cx)
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

/// See `Editor::ticker`.
pub struct FrameTick;

/// A camera orbit timed frame by frame (`ANNO_EDITOR_BENCH`), for measuring
/// the window as it runs on a real display.
pub struct Bench {
    seconds: f32,
    running: bool,
    /// Pans the UI canvas instead of orbiting an island (`ui_bench_frame`).
    ui: bool,
    /// When loading ended (the orbit waits a further two seconds), when the
    /// orbit started, and the current second's start and frames.
    loaded: Option<std::time::Instant>,
    started: Option<std::time::Instant>,
    second: std::time::Instant,
    frames: u32,
    /// Frames per second of each whole second.
    rates: Vec<u32>,
    /// The last frame's start, and the UI thread's frame intervals and its
    /// time on the 3D view while the island opens and while it orbits.
    last: Option<std::time::Instant>,
    /// Frame callbacks' ends, from a task each frame spawns at its start:
    /// the foreground runs it once the frame's callback returns.
    ends: std::rc::Rc<std::cell::RefCell<Vec<f32>>>,
    opening: Samples,
    orbit: Samples,
}

/// One bench phase's frames (`Bench`).
#[derive(Default)]
struct Samples {
    interval_ms: Vec<f32>,
    /// Per frame: the UI thread from the frame's start to the end of its
    /// frame callback, less the swapchain's wait.
    busy_ms: Vec<f32>,
    /// Per frame: the UI thread blocked on the renderer's lock while a
    /// view's thread recorded.
    blocked_ms: Vec<f32>,
    timing: gpui_editor_kit::gpu::PaneTiming,
}

impl Samples {
    fn add(
        &mut self,
        interval: Option<f32>,
        busy: Vec<f32>,
        blocked: std::time::Duration,
        t: gpui_editor_kit::gpu::PaneTiming,
    ) {
        self.interval_ms.extend(interval);
        self.blocked_ms.push(blocked.as_secs_f32() * 1000.);
        // A frame's end lands with the next frame's samples, its swapchain
        // wait with its own: pair them from the end.
        let waits = (t.surface_ms.iter().rev().copied()).chain(std::iter::repeat(0.));
        let mut busy: Vec<f32> = (busy.iter().rev().zip(waits))
            .map(|(b, w)| (b - w).max(0.))
            .collect();
        busy.reverse();
        self.busy_ms.extend(busy);
        let s = &mut self.timing;
        s.ui_ms.extend(t.ui_ms);
        s.surface_ms.extend(t.surface_ms);
        s.move_ms.extend(t.move_ms);
        s.overtaken += t.overtaken;
        s.late += t.late;
        for (a, b) in s.moves.iter_mut().zip(t.moves) {
            *a += b;
        }
    }

    fn report(&self, phase: &str) {
        let t = &self.timing;
        eprintln!(
            "bench: {phase}: {} frames · interval {} · UI thread busy {} · of it waiting on 3D {} · blocked on the renderer {} · swapchain wait {}",
            self.interval_ms.len(),
            spread(&self.interval_ms),
            spread(&self.busy_ms),
            spread(&t.ui_ms),
            spread(&self.blocked_ms),
            spread(&t.surface_ms)
        );
        eprintln!(
            "bench: {phase}: camera moves shown in the next frame {}, one later {}, two later {}, more {}, overtaken {} · move to frame {} · frames that drew an older 3D frame than asked {}",
            t.moves[0],
            t.moves[1],
            t.moves[2],
            t.moves[3],
            t.overtaken,
            spread(&t.move_ms),
            t.late
        );
        // Every frame, in order, as a spread hides which frames stalled.
        let list = |v: &[f32]| -> String {
            let v: Vec<String> = v.iter().map(|ms| format!("{ms:.1}")).collect();
            v.join(" ")
        };
        eprintln!(
            "bench: {phase}: per frame, ms: blocked on the renderer [{}] · UI thread busy [{}]",
            list(&self.blocked_ms),
            list(&self.busy_ms)
        );
    }
}

/// Median, 95th percentile and worst of `v`, in milliseconds.
fn spread(v: &[f32]) -> String {
    let mut v = v.to_vec();
    v.sort_by(f32::total_cmp);
    let at = |q: f32| {
        v.get(((v.len() as f32 - 1.) * q) as usize)
            .copied()
            .unwrap_or(0.)
    };
    format!(
        "p50 {:.2}, p95 {:.2}, max {:.2} ms",
        at(0.5),
        at(0.95),
        at(1.)
    )
}

impl Bench {
    fn new(seconds: f32) -> Self {
        Self {
            seconds,
            running: false,
            ui: false,
            loaded: None,
            started: None,
            second: std::time::Instant::now(),
            frames: 0,
            rates: Vec::new(),
            last: None,
            ends: Default::default(),
            opening: Samples::default(),
            orbit: Samples::default(),
        }
    }
}

impl Editor {
    /// One benchmark frame: turn the first view a little, present, and
    /// report each second.
    fn bench_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bench.as_ref().is_some_and(|b| b.ui) {
            return self.ui_bench_frame(window, cx);
        }
        let editor = cx.entity();
        let Some(island) = &mut self.island else {
            window.on_next_frame(move |w, cx| editor.update(cx, |e, cx| e.bench_frame(w, cx)));
            return;
        };
        let bench = self.bench.as_mut().expect("benchmarking");
        let now = std::time::Instant::now();
        // Frame to frame on the UI thread: every bench frame presents, so
        // frames follow each other as fast as the window takes them.
        let interval = (bench.last.replace(now)).map(|at| (now - at).as_secs_f32() * 1000.);
        let timing = island.take_timing(0);
        let blocked = island.take_blocked();
        let busy = std::mem::take(&mut *bench.ends.borrow_mut());
        let ends = bench.ends.clone();
        cx.spawn(async move |_, _| {
            ends.borrow_mut().push(now.elapsed().as_secs_f32() * 1000.);
        })
        .detach();
        // The opening runs from the view's first frame to the first time
        // loading ends; the orbit can stream more in later.
        let drawn = !timing.ui_ms.is_empty() || !bench.opening.timing.ui_ms.is_empty();
        let opened = bench.loaded.is_some() || bench.started.is_some();
        if bench.started.is_some() {
            bench.orbit.add(interval, busy, blocked, timing);
        } else if !opened {
            bench.opening.add(interval, busy, blocked, timing);
            window.request_present();
        }
        if island.loading() || !drawn {
            bench.loaded = None;
        } else if bench.loaded.is_none() {
            bench.loaded = Some(now);
            eprintln!("bench: loaded");
            if !opened {
                bench.opening.report("opening");
            }
        }
        // Once orbiting, streaming the new view in does not pause it.
        let ready = bench.started.is_some()
            || bench
                .loaded
                .is_some_and(|at| at.elapsed().as_secs_f32() >= 2.);
        if ready {
            let started = *bench.started.get_or_insert_with(|| {
                bench.second = now;
                now
            });
            island.turn(0, 0.004);
            bench.frames += 1;
            if bench.second.elapsed().as_secs_f32() >= 1. {
                let s = island.frame_stats();
                let passes: Vec<String> = (s.passes.iter())
                    .filter(|(name, _)| !name.is_empty())
                    .map(|(name, ms)| format!("{name} {ms:.2}"))
                    .collect();
                eprintln!(
                    "bench: {} fps · CPU {:.2} ms (build {:.2}, layout {:.2}, encode {:.2}) · GPU {:.2} ms, span {:.2} [{}]",
                    bench.frames,
                    s.cpu_ms,
                    s.build_ms,
                    s.layout_ms,
                    s.encode_ms,
                    s.gpu_ms,
                    s.gpu_span_ms,
                    passes.join(", ")
                );
                bench.rates.push(bench.frames);
                bench.frames = 0;
                bench.second = now;
            }
            if started.elapsed().as_secs_f32() >= bench.seconds {
                let mut rates = bench.rates.clone();
                rates.sort_unstable();
                eprintln!(
                    "bench: done, median {} fps over {} s",
                    rates.get(rates.len() / 2).copied().unwrap_or(0),
                    rates.len()
                );
                bench.orbit.report("orbit");
                cx.quit();
                return;
            }
            self.camera_moved(0, window, cx);
        }
        window.on_next_frame(move |w, cx| editor.update(cx, |e, cx| e.bench_frame(w, cx)));
    }

    /// One UI benchmark frame: the canvas pans a pixel and back on
    /// alternate frames, so it draws whole every frame while the other
    /// panes stay cached. Starts three seconds after a UI file is open.
    fn ui_bench_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.entity();
        window.on_next_frame(move |w, cx| editor.update(cx, |e, cx| e.bench_frame(w, cx)));
        let bench = self.bench.as_mut().expect("benchmarking");
        let Some(ui) = &mut self.ui else {
            window.request_present();
            return;
        };
        let now = std::time::Instant::now();
        let interval = (bench.last.replace(now)).map(|at| (now - at).as_secs_f32() * 1000.);
        let busy = std::mem::take(&mut *bench.ends.borrow_mut());
        let ends = bench.ends.clone();
        cx.spawn(async move |_, _| {
            ends.borrow_mut().push(now.elapsed().as_secs_f32() * 1000.);
        })
        .detach();
        if bench.started.is_some() {
            bench.orbit.interval_ms.extend(interval);
            bench.orbit.busy_ms.extend(busy);
        }
        let loaded = *bench.loaded.get_or_insert(now);
        if loaded.elapsed().as_secs_f32() < 3. {
            window.request_present();
            return;
        }
        let started = *bench.started.get_or_insert(now);
        if started.elapsed().as_secs_f32() >= bench.seconds {
            let o = &bench.orbit;
            eprintln!(
                "bench: ui canvas: {} frames · interval {} · UI thread busy {}",
                o.interval_ms.len(),
                spread(&o.interval_ms),
                spread(&o.busy_ms)
            );
            cx.quit();
            return;
        }
        ui.pan_by(if bench.frames % 2 == 0 { 1. } else { -1. });
        bench.frames += 1;
        self.notify_pane(Pane::UiCanvas, cx);
    }
}

impl Editor {
    /// A camera move or hover in island view `view`. The viewport canvases
    /// draw as GPUI presents, so when the pane paints nothing that follows
    /// the camera itself, presenting the last frame again is all it takes:
    /// no element of the window is laid out or painted, which reusing the
    /// cached panes still cost 1.1 ms a frame. Otherwise the viewports
    /// paint again.
    pub fn camera_moved(&mut self, view: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.island.as_ref().is_some_and(|i| i.sync_camera(view)) {
            window.request_present();
            self.refresh_readouts(cx);
        } else {
            self.notify_viewports(cx);
        }
    }

    /// One readout redraw later, and another after that while the viewport
    /// is still presenting on its own. Notifies the ticker, which this root
    /// reads, so the cached shell is not rebuilt.
    ///
    /// Every redraw is a frame of its own, presented whatever the
    /// viewport's idle cap: at 10 a second they took a capped resting view
    /// from 15 frames a second to 20. Under a cap they follow once a second.
    fn arm_readout(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.readouts_due, true) {
            return;
        }
        let every = match crate::graphics::current().idle_fps {
            crate::graphics::IdleFps::Off => std::time::Duration::from_millis(100),
            _ => std::time::Duration::from_secs(1),
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(every).await;
            let _ = this.update(cx, |this, cx| {
                this.readouts_due = false;
                this.readouts_at = Some(std::time::Instant::now());
                if this.island.as_ref().is_some_and(|i| i.wants_frames()) {
                    App::notify(cx, this.ticker.entity_id());
                }
            });
        })
        .detach();
    }

    /// Redraws the viewport readouts (cursor, frame times) at most every
    /// 100 ms while the camera moves, and once more after it stops, so they
    /// settle on the last values.
    fn refresh_readouts(&mut self, cx: &mut Context<Self>) {
        const EVERY: std::time::Duration = std::time::Duration::from_millis(100);
        if self.readouts_at.is_none_or(|at| at.elapsed() >= EVERY) {
            self.readouts_at = Some(std::time::Instant::now());
            App::notify(cx, self.ticker.entity_id());
            return;
        }
        if std::mem::replace(&mut self.readouts_due, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(EVERY).await;
            let _ = this.update(cx, |this, cx| {
                this.readouts_due = false;
                this.readouts_at = Some(std::time::Instant::now());
                App::notify(cx, this.ticker.entity_id());
            });
        })
        .detach();
    }

    /// Panes on screen in the current workspace.
    pub(crate) fn dock_visible_panes(&self) -> Vec<Pane> {
        self.docks[self.dock_index()].visible_panes()
    }

    /// Redraws just the island viewports, for changes nothing else shows:
    /// orbit, pan, zoom and hover.
    pub fn notify_viewports(&self, cx: &mut Context<Self>) {
        for (pane, view) in &self.pane_views {
            if matches!(pane, Pane::IslandView | Pane::IslandTop) {
                App::notify(cx, view.entity_id());
            }
        }
    }

    /// Redraws just `pane`, for changes only it shows.
    pub fn notify_pane(&self, pane: Pane, cx: &mut Context<Self>) {
        for (p, view) in &self.pane_views {
            if *p == pane {
                App::notify(cx, view.entity_id());
            }
        }
    }

    fn render_pane_content(
        &mut self,
        pane: Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let _span = gpui_editor_kit::profile::span(match pane {
            Pane::IslandView | Pane::IslandTop => "pane viewport",
            Pane::IslandHierarchy => "pane hierarchy",
            Pane::IslandInspector => "pane inspector",
            Pane::IslandLibrary => "pane library",
            Pane::IslandList => "pane island list",
            Pane::MapView => "pane map view",
            Pane::UiCanvas => "pane ui canvas",
            Pane::UiInspector => "pane ui inspector",
            Pane::UiTimeline => "pane ui timeline",
            Pane::UiFiles => "pane ui files",
            Pane::UiLibrary => "pane ui library",
            Pane::UiBindings => "pane ui bindings",
            _ => "pane other",
        });
        match pane {
            Pane::ProjectOverview => self.render_project_overview(window, cx),
            Pane::ProjectModels => self.render_project_models(window, cx),
            Pane::ProjectMods => self.render_project_mods(window, cx),
            // Shown in the UI workspace too: a press there takes the keys
            // away from the widget (round 10: Delete in the browser's
            // search deleted the selected container).
            Pane::Browser if self.workspace == Workspace::Ui => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Other)
                }))
                .child(self.render_browser(window, cx))
                .into_any_element(),
            Pane::Browser => self.render_browser(window, cx),
            Pane::IslandList => self.render_island_list(window, cx),
            Pane::Preview if self.workspace == Workspace::Ui => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Other)
                }))
                .child(self.render_preview(window, cx))
                .into_any_element(),
            Pane::Preview => self.render_preview(window, cx),
            Pane::IslandView => self.render_island_view(0, window, cx),
            Pane::IslandTop => self.render_island_view(1, window, cx),
            Pane::IslandHierarchy => self.render_island_hierarchy(window, cx),
            Pane::IslandLayers => self.render_island_layers(window, cx),
            Pane::IslandGraphics => self.render_graphics(window, cx),
            Pane::IslandInspector => self.render_island_inspector(window, cx),
            Pane::IslandMaterials => self.render_island_materials(cx),
            Pane::IslandLibrary => self.render_island_library(window, cx),
            Pane::MapView => self.render_map_view(window, cx),
            Pane::MapSlots => self.render_map_slots(window, cx),
            Pane::MapInspector => self.render_map_inspector(window, cx),
            Pane::MapIslands => self.render_map_islands(window, cx),
            // Files dragged from the Asset browser onto the model: props
            // and particle effects place at its origin, animations go to
            // the selected part.
            Pane::ModelView => div()
                .size_full()
                .on_drop(
                    cx.listener(|this, drag: &crate::browser::AssetDrag, _, cx| {
                        this.model_drop(&drag.0, cx)
                    }),
                )
                .child(self.render_model_view(window, cx))
                .into_any_element(),
            Pane::ModelList => self.render_model_list(window, cx),
            Pane::ModelHierarchy => self.render_model_hierarchy(window, cx),
            // A file dropped on the inspector adds as one dropped on the model.
            Pane::ModelInspector => div()
                .size_full()
                .on_drop(
                    cx.listener(|this, drag: &crate::browser::AssetDrag, _, cx| {
                        this.model_drop(&drag.0, cx)
                    }),
                )
                .child(self.render_model_inspector(window, cx))
                .into_any_element(),
            Pane::ModelTimeline => self.render_model_timeline(window, cx),
            Pane::UiTree => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Canvas)
                }))
                .child(self.render_ui_tree(window, cx))
                .into_any_element(),
            // Its toolbar (Show hidden) and the middle and right buttons too:
            // any press ends typing in the field before (round 11: Show
            // hidden left Position X typing, and 123 became 1234).
            Pane::UiCanvas => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Canvas)
                }))
                .child(self.render_ui_canvas(cx))
                .into_any_element(),
            // A press anywhere in the inspector ends typing in the field
            // before, as a click on another field does (round 8: an on/off
            // property left Position X typing, and 123 became 1234).
            Pane::UiInspector => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Canvas)
                }))
                .child(self.render_ui_inspector(window, cx))
                .into_any_element(),
            Pane::UiFiles => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Canvas)
                }))
                .child(self.render_ui_files(window, cx))
                .into_any_element(),
            Pane::UiLibrary => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Canvas)
                }))
                .child(self.render_ui_library(window, cx))
                .into_any_element(),
            // Any press in the timeline gives it the keys before a control
            // acts (round 8: a click below the lanes, or on the pane with no
            // clips, left Delete with the canvas, which deleted the widget).
            Pane::UiTimeline => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Timeline)
                }))
                .child(self.render_ui_timeline(window, cx))
                .into_any_element(),
            // Its toolbar (Fit, Tidy, Graph and List) and list too: a press
            // anywhere in it gives the graph the keys, which stops Delete
            // (round 9: Delete after Fit deleted the selected widget).
            Pane::UiBindings => div()
                .size_full()
                .capture_any_mouse_down(cx.listener(|this, e: &MouseDownEvent, _, _| {
                    this.press_keys_to(e.position, crate::ui::Keys::Graph)
                }))
                .child(self.render_ui_bindings(cx))
                .into_any_element(),
        }
    }
}

// The root renders every frame a camera tick draws, for the viewport
// readouts (cursor, frame times); the rest of the window is a cached
// `Shell`, so those frames lay out and paint only the readouts. Rebuilding
// the menu bar, dock and status bar on every tick cost about 0.3 ms of build
// and more of GPUI's layout.
impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = gpui_editor_kit::profile::span("editor render");
        // Registers the ticker with this window (see `camera_moved`).
        self.ticker.read(cx);
        if let Some(island) = &self.island {
            island.mark_frame();
        }
        self.drain_messages();
        let dirty = self.unsaved();
        let title = format!(
            "{}{} — {} — {} Editor",
            self.project.info.name,
            if dirty { " *" } else { "" },
            self.workspace.label(),
            self.game.profile().title,
        );
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
        if let Some(bench) = &mut self.bench
            && !bench.running
        {
            bench.running = true;
            let editor = cx.entity();
            window.on_next_frame(move |window, cx| {
                editor.update(cx, |e, cx| e.bench_frame(window, cx));
            });
        }
        let shell = self
            .shell
            .get_or_insert_with(|| {
                let editor = cx.entity();
                cx.new(|cx| {
                    cx.observe(&editor, |_, _, cx| cx.notify()).detach();
                    Shell {
                        editor: editor.downgrade(),
                    }
                })
            })
            .clone();
        let mut root = div()
            .size_full()
            .relative()
            .child(shell.cached(StyleRefinement::default().size_full()));
        // The readouts draw over the cached shell, so they would sit on top
        // of its dialogs too.
        if self.workspace == Workspace::Island && !self.dialog_open() {
            root = root.children(self.island_status(cx));
        }
        // GPUI lays out and paints after this returns.
        let follow = self.island.as_ref().is_some_and(|island| {
            island.mark_built();
            // Waves present without notifying this view. Tick the readout
            // anyway, so its frame times keep moving. The shell stays cached.
            island.wants_frames()
        });
        if follow {
            self.arm_readout(cx);
        }
        root
    }
}

/// The window but the viewport readouts, as one cached view: reused while
/// only the camera moves, drawn again whenever the editor notifies (it
/// observes it, as `PaneView` does).
pub struct Shell {
    editor: WeakEntity<Editor>,
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.editor
            .update(cx, |editor, cx| editor.render_shell(window, cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl Editor {
    /// Matches the island's bottom pane to the tool when it changes: the
    /// Library for placing, Ground material for painting, and folded to
    /// its tabs otherwise, so sculpting gets the viewport's full height. A
    /// tab click unfolds it until the next tool change.
    fn sync_island_dock(&mut self) {
        use crate::island::Tool;
        let Some(tool) = self.island.as_ref().map(|i| i.tool) else {
            // Nothing to place or paint before an island opens; the next
            // island's tool decides again.
            self.island_dock_tool = None;
            if let Some(i) = Workspace::ALL.iter().position(|w| *w == Workspace::Island) {
                for pane in [Pane::IslandLibrary, Pane::IslandMaterials] {
                    self.docks[i].set_collapsed(pane, true);
                }
            }
            return;
        };
        if self.island_dock_tool == Some(tool) {
            return;
        }
        self.island_dock_tool = Some(tool);
        let Some(i) = Workspace::ALL.iter().position(|w| *w == Workspace::Island) else {
            return;
        };
        let dock = &mut self.docks[i];
        let show = match tool {
            Tool::Place | Tool::Scatter => Some(Pane::IslandLibrary),
            Tool::Paint | Tool::Tint => Some(Pane::IslandMaterials),
            _ => None,
        };
        for pane in [Pane::IslandLibrary, Pane::IslandMaterials] {
            dock.set_collapsed(pane, show.is_none());
        }
        if let Some(pane) = show {
            dock.layout.activate(pane);
        }
    }

    /// The UI workspace's timeline and bindings fold while no screen is
    /// open: nothing to animate or bind yet.
    fn sync_ui_dock(&mut self) {
        let open = self.ui.is_some();
        // Only when a screen opens or closes, so a fold by hand stays.
        if self.ui_dock_open == Some(open) {
            return;
        }
        self.ui_dock_open = Some(open);
        if let Some(i) = Workspace::ALL.iter().position(|w| *w == Workspace::Ui) {
            for pane in [Pane::UiTimeline, Pane::UiBindings] {
                self.docks[i].set_collapsed(pane, !open);
            }
        }
    }

    fn render_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.sync_island_dock();
        self.sync_ui_dock();
        let docked = dock::render(self, window, cx);
        // The menus sit in a fixed-width block so the workspace control
        // does not jump when Terrain comes and goes.
        let mut menus = div().w(px(190.)).flex_shrink_0().flex().items_center();
        for (label, menu) in [
            // View before the workspace's own menu, so File, Edit and View
            // sit in the same place in every workspace.
            ("File", Menu::File),
            ("Edit", Menu::Edit),
            ("View", Menu::View),
            ("Terrain", Menu::Island),
            ("Map", Menu::Map),
            ("Model", Menu::Model),
        ] {
            // Terrain tools belong to the Island workspace, map layouts to
            // the Map one, model commands to the Model one.
            if (menu == Menu::Island && self.workspace != Workspace::Island)
                || (menu == Menu::Map && self.workspace != Workspace::Map)
                || (menu == Menu::Model && self.workspace != Workspace::Model)
            {
                continue;
            }
            menus = menus.child(
                // Open, a menu title takes the hover fill: the selected blue
                // beside the active workspace read as two selected modes.
                button(label, false)
                    .when(self.menu == Some(menu), |d| d.bg(theme::get().hover))
                    .h(px(24.))
                    .px_2()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = (this.menu != Some(menu)).then_some(menu);
                        cx.notify();
                    })),
            );
        }
        // Workspaces as one joined control, so they do not read as more
        // menus.
        let mut bar = widgets::menu_bar().child(menus).child(widgets::segmented(
            "workspaces",
            (self.workspaces().into_iter()).map(|w| (w, w.label().into())),
            &self.workspace,
            cx,
            |this, w, _, _| this.switch(w),
        ));
        let unsaved = self.unsaved();
        // The project is what every save and install acts on, so it stays
        // in view: name (a click opens it), unsaved mark, Save, Install.
        // Game › project, then the project's state: the target every save
        // and install acts on.
        let game = self.game.profile().title;
        let game = game.split(':').next().unwrap_or(game);
        // Open, the hover fill as the menu titles take, not the selected
        // blue beside the active workspace.
        let chip = if self.module.islands().is_some() {
            format!("{game}  ›  {}  ▾", self.project.info.name)
        } else {
            if self.module.ui().is_some() {
                game.to_owned()
            } else {
                format!("{game}  ›  browse only")
            }
        };
        let chip = button(chip, false)
            .when(self.menu == Some(Menu::Project), |d| {
                d.bg(theme::get().hover)
            })
            .h(px(24.))
            .px_3()
            // Framed like the state chip at the right, so the bar's two chips
            // read as one family.
            .rounded(px(4.))
            .border_1()
            .border_color(theme::get().border)
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.require_workspace(Workspace::Project) {
                    return;
                }
                this.menu = (this.menu != Some(Menu::Project)).then_some(Menu::Project);
                cx.notify();
            }));
        bar = bar
            .relative()
            .child(div().flex_1())
            // Centred on the window, as Figma centres the file name: beside
            // the state chip it moved whenever the chip's width changed.
            // The full-width layer has no listeners, so clicks reach the
            // menus and tabs under it.
            .child({
                let entity = cx.entity();
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div().relative().child(chip).child(
                            canvas(
                                move |bounds, window, cx| {
                                    // The 300px menu centred under the
                                    // centred button, as its right inset.
                                    let right =
                                        f32::from(window.viewport_size().width - bounds.right())
                                            - (150. - f32::from(bounds.size.width) / 2.);
                                    entity.update(cx, |this, _| this.project_button_right = right);
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0(),
                        ),
                    )
            })
            // Save state and the game's copy as one control: what is due
            // next, and the button for it.
            .child({
                use crate::project::Installed;
                let installed = self.project_ui.installed;
                let (dot, text, action) = if self.module.islands().is_none() {
                    (theme::get().muted, "Files only", None)
                } else if unsaved {
                    (
                        theme::get().highlight,
                        "Unsaved changes",
                        Some(
                            widgets::action("Save", widgets::Tone::Primary)
                                .tooltip(widgets::Tooltip::text(
                                    "Save project",
                                    "Ctrl S",
                                    "Writes every open edit into the mod folder",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.save();
                                    cx.notify();
                                })),
                        ),
                    )
                } else if self.module.packaging().is_none() {
                    (theme::get().muted, "Saved", None)
                } else {
                    // The same control in every workspace, so it is where
                    // the eye left it.
                    // Always the same weight and the same text on every tab,
                    // the Overview too: one state, read in one place. The
                    // button steps aside while the Overview's banner shows
                    // its own, so a screen never offers the action twice.
                    let tone = widgets::Tone::Secondary;
                    let on_overview = self.workspace == Workspace::Project
                        && self.docks[self.dock_index()]
                            .visible_panes()
                            .contains(&Pane::ProjectOverview);
                    // What an update brings, in its tooltip.
                    let changes = self.install_change_lines(8);
                    let install = |label: &'static str| {
                        let detail = if changes.is_empty() {
                            "Saves, then copies the mod into the mods folder".to_owned()
                        } else {
                            format!(
                                "Saves, then copies the mod into the game. {}",
                                changes.join(" · ")
                            )
                        };
                        (!on_overview).then(|| {
                            widgets::action(label, tone)
                                .tooltip(widgets::Tooltip::text(
                                    "Install into the game",
                                    "",
                                    detail,
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.install_mod(cx);
                                    cx.notify();
                                }))
                        })
                    };
                    match installed {
                        Installed::Current => (theme::get().ok, "Mod saved · in the game", None),
                        Installed::Older => (
                            theme::get().highlight,
                            // The banner's words: "out of date", not
                            // "outdated", for one state.
                            "Mod saved · Game copy out of date",
                            // The File menu's and the Overview's words: a bare
                            // "Update" beside it could read as updating the app.
                            install("Update game copy"),
                        ),
                        // Installing would overwrite another mod's folder.
                        Installed::Other => (
                            theme::get().error,
                            "Mod saved · mod id taken in the game",
                            None,
                        ),
                        Installed::No => (
                            theme::get().muted,
                            "Mod saved · not in the game",
                            install("Install"),
                        ),
                    }
                };
                // Natural width against the right gutter: the project button
                // no longer sits beside it, so nothing needs a fixed slot.
                widgets::status_chip(dot, text, action)
            })
            .child(div().w(px(8.)));
        // Each workspace's own counts: the scene on Island, the archives on
        // Assets, the mod elsewhere.
        let right = match (&self.install, &self.island, self.workspace) {
            (Some(_), Some(island), Workspace::Island) => format!(
                // No triangle count: the terrain's, fixed while editing and
                // nothing a modder acts on; File & stats in the Inspector has it.
                "{}{} objects · {} props",
                match island.models_pending() {
                    0 => String::new(),
                    n => format!("loading {n} models · "),
                },
                crate::project::count(island.island.doc.objects.len()),
                crate::project::count(island.island.doc.props.len()),
            ),
            // Nothing open: what the list on the left offers.
            (Some(_), None, Workspace::Island) if (self.project.file_count() == 0) => {
                // Per region, as the list groups them: one total for every
                // region read as contradicting the open group's count.
                match &self.island_list {
                    Some(all) => {
                        // Shown of total per region, under the list's kind
                        // filter: a bare total read as contradicting the
                        // filtered list's header.
                        let kind = self.island_kind;
                        let mut regions = std::collections::BTreeMap::<&str, (usize, usize)>::new();
                        for s in all.iter() {
                            let e = regions.entry(&s.region).or_default();
                            e.1 += 1;
                            e.0 += usize::from(kind.admits(s.kind));
                        }
                        // No "0 in this mod": the list's This mod row says it,
                        // and the filter should lead.
                        // Largest first, the order the list uses.
                        let mut regions: Vec<_> = regions.into_iter().collect();
                        regions.sort_by_key(|(_, (shown, _))| std::cmp::Reverse(*shown));
                        let mut parts: Vec<String> = Vec::new();
                        parts.extend(regions.into_iter().map(|(region, (shown, total))| {
                            let name = self.island_rules().province(region);
                            let short = name.split(" (").next().unwrap_or(&name).to_owned();
                            if kind == crate::island::IslandKind::All {
                                format!("{} {short}", crate::project::count(total))
                            } else {
                                format!(
                                    "{} of {} {short}",
                                    crate::project::count(shown),
                                    crate::project::count(total)
                                )
                            }
                        }));
                        // The filter leads the counts it qualifies; trailing,
                        // "· Playable" read as a fourth count.
                        if kind != crate::island::IslandKind::All
                            && let Some(first) = parts.get_mut(0)
                        {
                            *first = format!("{}: {first}", kind.label());
                        }
                        parts.join(" · ")
                    }
                    None => String::new(),
                }
            }
            // What the open model holds, as Island counts its objects.
            (Some(_), _, Workspace::Model) if self.model.is_some() => {
                let m = &self.model.as_ref().expect("guarded").model;
                let mut parts = vec![crate::project::plural(m.parts.len(), "mesh", "meshes")];
                for (n, one, many) in [
                    (m.props.len(), "prop", "props"),
                    (m.particles.len(), "particle effect", "particle effects"),
                    (m.lights.len(), "light", "lights"),
                    (m.sequences.len(), "sequence", "sequences"),
                ] {
                    if n > 0 {
                        parts.push(crate::project::plural(n, one, many));
                    }
                }
                parts.join(" · ")
            }
            // Nothing open: what the Models list on the left offers.
            (Some(_), _, Workspace::Model) => match &self.model_list.game {
                Some(all) => format!("{} game models", crate::project::count(all.len())),
                None => String::new(),
            },
            // Shown of total, where Assets and Island put their counts.
            (Some(_), _, Workspace::Ui) if (self.project.file_count() == 0) => {
                self.ui_find.shown().unwrap_or_default()
            }
            // The list's count leads the archive totals: in a footer of its
            // own it stacked a second status line on this one.
            (Some(install), _, Workspace::Assets) => {
                let totals = format!(
                    "{} archives · {} files",
                    crate::project::count(install.archives().len()),
                    crate::project::count(install.file_count())
                );
                match self.browser.count_line() {
                    c if c.is_empty() => totals,
                    c => format!("{c} · {totals}"),
                }
            }
            // Data files only; mod details alone (modinfo.json) are not
            // "0 files", which read as an empty mod beside "outdated".
            // With no edited files, the islands are the mod's content;
            // "no files edited" read as a contradiction beside "game copy
            // outdated", and a blank strip as broken chrome.
            // Elsewhere "0 edited game files" read as a lost mod beside the
            // island list, so a mod with nothing counts says nothing there.
            (Some(_), _, ws)
                if (self.project.file_count() == 0)
                    // Not on Project: its Islands card already counts them.
                    && ws != Workspace::Project
                    && !self.project.islands.is_empty() =>
            {
                crate::project::plural(
                    self.project.islands.len(),
                    "island in the mod",
                    "islands in the mod",
                )
            }
            // What is in the mod: the one workspace whose status strip
            // was otherwise empty.
            // Each Project tab counts its own page: "0 islands in the mod"
            // under Models and Dependencies said nothing about them.
            (Some(_), _, Workspace::Project)
                if self.dock_visible_panes().contains(&Pane::ProjectModels) =>
            {
                let all = self.model_entries();
                let ours = all
                    .iter()
                    .filter(|e| !matches!(e.source, crate::project::models::Source::Mod(_)))
                    .count();
                format!(
                    "{} · {} from dependencies",
                    crate::project::plural(ours, "model in the mod", "models in the mod"),
                    crate::project::count(all.len() - ours)
                )
            }
            (Some(_), _, Workspace::Project)
                if self.dock_visible_panes().contains(&Pane::ProjectMods) =>
            {
                match self.project.info.requires.len() {
                    0 => "No dependencies".to_owned(),
                    n => crate::project::plural(n, "dependency required", "dependencies required"),
                }
            }
            // Not the folder: the Overview's subtitle says it, with Show in
            // file manager beside it. The Overview's own terms elsewhere too.
            (Some(_), _, Workspace::Project) => self.mod_contents(),
            (Some(_), ..) if (self.project.file_count() == 0) => String::new(),
            (Some(_), ..) => self.mod_contents(),
            (None, ..) => "no install".into(),
        };
        let status = self.status_line();
        let mut root = widgets::window_root("anno-editor")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            // The platform's text input, while a field takes keys: IME
            // composition and commits reach the field through it.
            .when(self.active_edit().is_some(), |d| {
                let editor = cx.entity();
                d.child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            if let Some(focused) = window.focused(cx) {
                                window.handle_input(
                                    &focused,
                                    ElementInputHandler::new(bounds, editor.clone()),
                                    cx,
                                );
                            }
                        },
                    )
                    .absolute()
                    .size_0(),
                )
            })
            .child(bar)
            .child(div().flex().flex_1().min_h_0().child(docked))
            .child(widgets::status_bar(
                status.0,
                status.1,
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .children(self.cursor_readout())
                    .child(right),
            ));
        if let Some(menu) = self.menu {
            root = root.child(self.menu_popup(menu, cx));
        }
        if let Some(wizard) = self.render_wizard(cx) {
            root = root.child(wizard);
        }
        if let Some(import) = self.render_import(cx) {
            root = root.child(import);
        }
        if let Some(playtest) = self.render_playtest(cx) {
            root = root.child(playtest);
        }
        if let Some(dialog) = self.render_new_project(cx) {
            root = root.child(dialog);
        }
        if let Some(dialog) = self.render_wm_dialog(cx) {
            root = root.child(dialog);
        }
        if let Some(dialog) = self.render_map_dialog(window, cx) {
            root = root.child(dialog);
        }
        if let Some(picker) = self.render_model_picker(window, cx) {
            root = root.child(picker);
        }
        if let Some(leave) = self.leaving.clone() {
            let (title, what, save) = leave.words();
            let names = self.unsaved_names(leave.replaces());
            let said = match names.as_slice() {
                [] => "The last save didn't reach the mod folder.".to_owned(),
                [one] => format!("{one} has unsaved edits."),
                [rest @ .., last] => format!("{} and {last} have unsaved edits.", rest.join(", ")),
            };
            let answer = |a: Answer| {
                cx.listener(move |this: &mut Self, _: &ClickEvent, _: &mut Window, cx| {
                    this.answer_leave(a, cx);
                    cx.notify();
                })
            };
            root = root.child(widgets::modal(
                "leave",
                widgets::dialog(title)
                    .child(format!("{said} Save them before {what}, or discard them."))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("Cancel", false).on_click(answer(Answer::Cancel)))
                            .child(button("Discard edits", false).on_click(answer(Answer::Discard)))
                            .child(button(save, true).on_click(answer(Answer::Save))),
                    ),
            ));
        }
        root.into_any_element()
    }

    /// An open document has edits not yet in the mod folder.
    pub fn unsaved(&self) -> bool {
        self.pending_save
            || self.island.as_ref().is_some_and(|i| i.dirty())
            || self.map.as_ref().is_some_and(|m| m.dirty())
            || self.ui.as_ref().is_some_and(|u| u.dirty())
            || self.model.as_ref().is_some_and(|m| m.dirty())
    }

    /// The documents with edits not yet in the mod folder, by name: the
    /// one `only` names, or all of them.
    pub fn unsaved_names(&self, only: Option<Doc>) -> Vec<String> {
        let wants = |d: Doc| only.is_none_or(|o| o == d);
        let mut out = Vec::new();
        if wants(Doc::Island)
            && let Some(i) = self.island.as_ref().filter(|i| i.dirty())
        {
            out.push(crate::island::island_title(self.asset_words(), &i.path));
        }
        if wants(Doc::Map)
            && let Some(m) = self.map.as_ref().filter(|m| m.dirty())
        {
            out.push(m.title());
        }
        if wants(Doc::Ui)
            && let Some(u) = self.ui.as_ref().filter(|u| u.dirty())
        {
            out.push(self.screen_name(&u.path));
        }
        if wants(Doc::Model)
            && let Some(m) = self.model.as_ref().filter(|m| m.dirty())
        {
            let name = crate::project::models::model_name(&m.path);
            out.push(crate::island::pretty(self.asset_words(), name));
        }
        out
    }

    /// Opens, starts or closes a document, switches game or project, or
    /// quits, asking first when edits would be lost: only the replaced
    /// document's, or every one's when leaving them all. A half-typed value
    /// counts as an edit: it is applied first, from the menu as from the
    /// shortcut, and one its field refuses stops here.
    pub fn leave(&mut self, to: Leave, cx: &mut Context<Self>) {
        if let Err(e) = self.end_typing() {
            return self.message(e, true);
        }
        if self.show_open(&to) {
            return;
        }
        let replaces = to.replaces();
        // A save that staged but didn't write: the documents read clean,
        // the mod folder doesn't have them. Only leaving the project
        // loses that.
        let lost = replaces.is_none() && self.pending_save;
        if lost || !self.unsaved_names(replaces).is_empty() {
            self.leaving = Some(to);
            cx.notify();
        } else {
            self.go(to, false, cx);
        }
    }

    /// `to` opens the document already open: shows it, edits and all.
    fn show_open(&mut self, to: &Leave) -> bool {
        let workspace = match to {
            Leave::Island(p) if self.island.as_ref().is_some_and(|i| i.path == *p) => {
                Workspace::Island
            }
            Leave::Map(p) if self.map.as_ref().is_some_and(|m| m.is_at(p)) => Workspace::Map,
            Leave::Screen(p) if self.ui.as_ref().is_some_and(|u| u.path == *p) => Workspace::Ui,
            Leave::Model(p) if self.model.as_ref().is_some_and(|m| m.path == *p) => {
                Workspace::Model
            }
            _ => return false,
        };
        self.switch(workspace);
        true
    }

    /// Goes where `to` says, the edits saved or not to be kept.
    /// `discarded`: the modder chose Discard, so a load finishing
    /// replaces the edited document; otherwise one edited while it loaded
    /// asks again.
    pub(crate) fn go(&mut self, to: Leave, discarded: bool, cx: &mut Context<Self>) {
        match to {
            Leave::Game => cx.emit(SwitchGame),
            Leave::Project(folder) => self.open_project(&folder),
            Leave::NewIsland => self.wizard_now(),
            Leave::Island(path) => self.load_island(&path, discarded, cx),
            Leave::Map(path) => self.read_map(&path, cx),
            Leave::NewMap => self.new_map_dialog(cx),
            Leave::CloseMap => self.close_map(),
            Leave::Screen(path) => self.load_ui_asset(&path, discarded, cx),
            Leave::Model(path) => self.load_model(&path, discarded, cx),
            Leave::Quit => cx.quit(),
        }
    }

    /// The ask's answer. Discard leaves the document in place until what
    /// replaces it is there: a cancelled wizard or a failed load keeps it,
    /// edits and all.
    pub fn answer_leave(&mut self, answer: Answer, cx: &mut Context<Self>) {
        let Some(to) = self.leaving.take() else {
            return;
        };
        match answer {
            Answer::Cancel => {}
            Answer::Discard => self.go(to, true, cx),
            Answer::Save => self.save_then(to, cx),
        }
    }

    /// Save, then go once everything is written: at once, or after the
    /// shipped-map question is answered. A failed save goes nowhere.
    pub(crate) fn save_then(&mut self, to: Leave, cx: &mut Context<Self>) {
        match self.save() {
            Saved::Written => self.go(to, false, cx),
            Saved::Waiting => {
                if let Some(crate::map::panels::Dialog::SaveShipped(d)) = &mut self.map_dialog {
                    d.then = Some(to);
                }
            }
            Saved::Failed => {}
        }
    }
}

/// A centered hint for panes with nothing to show yet.
// TODO(alex): fold into `widgets::empty_state(..).into_any_element()` at the
// call sites; src/ui has uncommitted work from another session right now.
pub fn empty_pane(text: impl Into<SharedString>) -> AnyElement {
    widgets::empty_state(text).into_any_element()
}

/// A side pane with nothing to show yet: one quiet line at its top
/// saying what it will hold, while the main pane carries the big empty
/// state and its buttons.
pub fn pane_note(text: &'static str) -> AnyElement {
    // Top-left, where the pane's content will appear, as the UI
    // Inspector's own note sits: centred in a tall side column one quiet
    // line read as floating. Centred empty states are for the main views.
    div()
        .size_full()
        .bg(theme::get().panel)
        .p_3()
        .child(widgets::hint(text))
        .into_any_element()
}

/// The platform's text input, forwarded to the field being typed into.
/// Committed text goes through `route_key` as a key with that text and no
/// name, so each field reacts to it as to typing (a search filters); what
/// an IME is still composing is shown in the field, underlined.
impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let edit = self.active_edit()?;
        let range = range.start.min(edit.len16())..range.end.min(edit.len16());
        *adjusted = Some(range.clone());
        Some(edit.text_for_range16(range))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(self.active_edit()?.selection16())
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        gpui_editor_kit::field::composing()
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = self.active_edit() {
            edit.unmark();
        }
        self.notify_panes(cx);
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.active_edit() else {
            return;
        };
        edit.select_for_replace(range);
        let event = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: String::new(),
                key_char: Some(text.to_owned()),
            },
            is_held: false,
            prefer_character_input: true,
        };
        self.route_key(&event, window, cx);
        self.notify_panes(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        selected: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edit) = self.active_edit() {
            edit.compose(range, text, selected);
        }
        self.notify_panes(cx);
    }

    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        gpui_editor_kit::field::caret_bounds()
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod capabilities {
    use super::{Editor, Module, Project, Workspace};
    use crate::games::fixtures::PartialGame;
    use anno_formats::rda::{PackOptions, Packer};
    use gpui::TestApp;

    fn routes(module: &'static dyn Module, cases: &[(&str, bool)], keys: &[(&str, Workspace)]) {
        let dir = tempfile::tempdir().unwrap();
        let archives = dir.path().join("maindata");
        std::fs::create_dir(&archives).unwrap();
        let mut pack = Packer::new(PackOptions::default());
        for &(path, _) in cases {
            pack.add(path, b"fixture").unwrap();
        }
        std::fs::write(archives.join("data0.rda"), pack.encode().unwrap()).unwrap();
        let mut app = TestApp::new();
        let mut w = app.open_window(|window, cx| {
            Editor::new(
                Project::load(&dir.path().join("config"), module).unwrap(),
                dir.path().join("config"),
                Some(dir.path().to_owned()),
                None,
                window,
                cx,
            )
        });
        app.run_until_parked();
        for &(path, browser) in cases {
            w.update(|ed, _, cx| {
                ed.browser.selected = None;
                ed.open_path(path, cx);
                assert_eq!(ed.browser.selected.is_some(), browser, "{path}");
                if browser {
                    assert_eq!(ed.workspace, Workspace::Assets);
                    assert_eq!(ed.browser.selected, ed.install.as_ref().unwrap().file(path));
                }
            });
            app.run_until_parked();
        }
        w.update(|ed, _, cx| {
            if ed.module.islands().is_some() {
                // Not the missing test-map island: packaging alone turns it off.
                assert_eq!(
                    ed.playtest_blocker(),
                    Some("This game has no playtest delivery")
                );
                ed.install_mod(cx);
                assert_eq!(ed.status, "This game has no project installer");
                assert!(!ed.installing);
            }
        });
        for &(key, expected) in keys {
            w.simulate_keystrokes(key);
            w.read(|ed, _| assert_eq!(ed.workspace, expected, "{key}"));
        }
    }

    // A game without islands or screens (2070 was one until it edited
    // islands) shows both kinds of file in Assets.
    #[test]
    fn a_browse_only_game_opens_islands_and_screens_in_the_browser() {
        routes(
            &PartialGame::Bare,
            &[("data/one.fixture", true), ("data/one.screen", true)],
            &[
                ("ctrl-2", Workspace::Assets),
                ("ctrl-3", Workspace::Assets),
                ("ctrl-4", Workspace::Assets),
                ("ctrl-0", Workspace::Assets),
            ],
        );
    }

    #[test]
    fn islands_and_ui_route_independently_through_files_and_shortcuts() {
        routes(
            &PartialGame::Islands,
            &[("data/one.screen", true), ("data/one.fixture", false)],
            // Its islands have maps, so Ctrl 3 opens Map; it has no UI, so
            // Ctrl 4 leaves the keys there.
            &[
                ("ctrl-2", Workspace::Island),
                ("ctrl-3", Workspace::Map),
                ("ctrl-4", Workspace::Map),
                ("ctrl-0", Workspace::Project),
            ],
        );
        routes(
            &PartialGame::Screens,
            &[("data/one.fixture", true), ("data/one.screen", false)],
            // It saves projects, so it has their workspace.
            // Ctrl 3 is the Map workspace, which it lacks: the keys stay.
            &[
                ("ctrl-4", Workspace::Ui),
                ("ctrl-3", Workspace::Ui),
                ("ctrl-2", Workspace::Ui),
                ("ctrl-0", Workspace::Project),
            ],
        );
    }
}
