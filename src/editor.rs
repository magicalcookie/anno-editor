// Owns the window: workspaces and their dock layouts, menus, the status bar,
// keyboard routing, and the open install and mod project shared by every
// workspace.
use crate::{browser::Browser, island::IslandEditor, project::Project, ui::UiEditor};
use anno_formats::game::Game;
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
    IslandInspector,
    IslandMaterials,
    IslandLibrary,
    UiTree,
    UiCanvas,
    UiInspector,
    UiFiles,
    UiTimeline,
    UiBindings,
    UiLibrary,
}

impl Pane {
    const ALL: [Self; 20] = [
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
        Self::IslandInspector,
        Self::IslandMaterials,
        Self::IslandLibrary,
        Self::UiTree,
        Self::UiCanvas,
        Self::UiInspector,
        Self::UiFiles,
        Self::UiTimeline,
        Self::UiBindings,
        Self::UiLibrary,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workspace {
    Project,
    Assets,
    Island,
    Ui,
}

impl Workspace {
    const ALL: [Self; 4] = [Self::Project, Self::Assets, Self::Island, Self::Ui];

    fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Assets => "Assets",
            Self::Island => "Island",
            Self::Ui => "UI (experimental)",
        }
    }

    /// Names its saved layout file: stays put when a label changes, so a
    /// renamed workspace keeps the layout users arranged.
    fn key(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Assets => "assets",
            Self::Island => "island",
            Self::Ui => "ui",
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
                    vec![Pane::IslandList, Pane::IslandHierarchy, Pane::IslandLayers],
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
    /// Ctrl Q, File → Quit, or the window's close button.
    Quit,
}

pub struct Editor {
    pub game: Game,
    pub config: PathBuf,
    pub install: Option<Arc<Install>>,
    /// assets.xml, loaded after the index; object names wait for it.
    pub assets: Option<Arc<anno_formats::assets::Assets>>,
    pub project: Project,
    /// Every installed mod but the project's own copy (`scan_mods`).
    pub mods: Vec<anno_formats::modinfo::Installed>,
    /// Ids of the dependencies mounted under the project, by mod layer.
    pub mounted: Vec<String>,
    /// What the last save found the open island using from mods the
    /// project does not require.
    pub undeclared: Vec<crate::project::Undeclared>,
    pub browser: Browser,
    pub island: Option<IslandEditor>,
    pub ui: Option<UiEditor>,
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
    pub(crate) docks: [Dock<Pane>; 4],
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
        game: Game,
        config: PathBuf,
        install: Option<PathBuf>,
        open: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
            game,
            config: config.clone(),
            install: None,
            assets: None,
            project: Project::load(&config, game),
            mods: Vec::new(),
            mounted: Vec::new(),
            undeclared: Vec::new(),
            browser: Browser::default(),
            island: None,
            ui: None,
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
            bench: std::env::var("ANNO_EDITOR_BENCH")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(Bench::new),
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
            pending_save: false,
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
                this.end_typing();
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
    fn status_line(&self) -> (String, bool) {
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

    /// Indexes the install off the UI thread; 31 archive headers take a
    /// moment on a cold cache.
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
        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let result = cx
                .background_executor()
                .spawn(async move { Install::open_game(game, &dir) })
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
                        this.browser.attach(&install);
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
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { anno_formats::assets::Assets::load(&install) })
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
        if lower.ends_with(".a7m") || lower.ends_with(".a7t") {
            self.open_island(path, cx);
        } else if lower.contains("/ui/studio/generated/") {
            self.open_ui_asset(path, cx);
        } else {
            self.browser.select_path(path);
            if let Some(file) = self.browser.selected {
                self.select_file(file, cx);
            }
            self.switch(Workspace::Assets);
        }
    }

    pub fn switch(&mut self, workspace: Workspace) {
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

    /// The import command's one label, wherever it is offered.
    pub fn import_label(&self) -> &'static str {
        crate::game_text::of(self.game).import_label
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
            Workspace::Ui => self.ui.as_mut().is_some_and(|u| u.undo(redo)),
            Workspace::Assets | Workspace::Project => false,
        };
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
    pub(crate) fn save(&mut self) {
        // A half-typed value is applied first: it is what the field shows
        // (round 11 saved while Position X read 07, and Enter then made the
        // file dirty again). One the field refuses waits, as typed.
        self.ui_find.stop_typing();
        self.browser.stop_typing();
        if let Some(ui) = &mut self.ui
            && let Err(e) = ui.apply_typing()
        {
            ui.message = Some((format!("Not saved: {e}"), true));
            return;
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
                Err(e) => return self.message(format!("Island not saved: {e:#}"), true),
            }
        }
        // Staging warns about islands the game cannot use yet.
        let warning = self
            .island
            .as_mut()
            .and_then(|i| i.message.take())
            .filter(|(_, error)| *error)
            .map(|(text, _)| text);
        if let Some(ui) = &mut self.ui
            && ui.dirty()
        {
            match ui.stage(&mut self.project) {
                Ok(n) => staged += n,
                Err(e) => return self.message(format!("UI asset not saved: {e:#}"), true),
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
        match written {
            Ok(_) if let Some(warning) = warning => {
                self.message(format!("Saved. {warning}"), true)
            }
            Ok(path) => self.message(
                format!(
                    "Saved {} to {}; {} in the mod. Update game copy (top right) puts it in the game",
                    crate::project::plural(staged, "document", "documents"),
                    crate::project::short_path(&path),
                    crate::project::plural(self.project.files.len(), "file", "files"),
                ),
                false,
            ),
            Err(e) => self.message(format!("Mod not written: {e:#}"), true),
        }
    }

    pub(crate) fn install_mod(&mut self) {
        let Some(install) = &self.install else {
            return self.message("No install is open", true);
        };
        let installed = self.project.install(install.dir());
        self.mount_project();
        self.refresh_installed();
        match installed {
            Ok(path) => self.message(
                format!(
                    "Installed as {}. The game’s mod loader activates it on the next start",
                    path.display()
                ),
                false,
            ),
            Err(e) => self.message(format!("Install failed: {e:#}"), true),
        }
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
        match self.workspace {
            Workspace::Island => self.island.as_mut().and_then(|i| i.typing()),
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
        match event.keystroke.key.as_str() {
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
                self.save()
            }
            "0" if ctrl => self.switch(Workspace::Project),
            "q" if ctrl => self.leave(Leave::Quit, cx),
            "n" if ctrl => self.open_new_project(cx),
            "u" if ctrl && event.keystroke.modifiers.shift => self.install_mod(),
            "f5" => self.open_playtest(),
            "o" if ctrl => self.pick_project_folder(cx),
            "1" if ctrl => self.switch(Workspace::Assets),
            "2" if ctrl => self.switch(Workspace::Island),
            "3" if ctrl => self.switch(Workspace::Ui),
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
            .or_else(|| self.ui.as_mut().and_then(|u| u.message.take()))
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
                Menu::Island => 125.,
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
                    .child(item("New island…", "", Self::open_wizard))
                    .child(with_cx(self.import_label(), Self::open_import))
                    // The project's islands under Islands, in its group: past
                    // a rule of their own they read as a stray block.
                    .children(self.project.islands.iter().take(MENU_ISLANDS).map(|i| {
                        let path = i.path.clone();
                        let name = path
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .trim_end_matches(".a7m");
                        menu_item(format!("Open {name}"), "").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.menu = None;
                                this.open_island(&path, cx);
                                cx.notify();
                            },
                        ))
                    }))
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
                            None => item("Save project", "Ctrl S", Self::save),
                        },
                    )
                    .child(
                        menu_item(self.install_label(), "Ctrl Shift U").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.menu = None;
                                this.install_mod();
                                cx.notify();
                            },
                        )),
                    )
                    .child(match self.playtest_blocker() {
                        // The next step on its own line, short enough for
                        // the menu; the full sentence is the status message.
                        Some(why) => widgets::menu_item_off_why(
                            "Test in game…",
                            if why.starts_with("Save") {
                                "Save the island first"
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
            Menu::Project => {
                let current = self.project.folder();
                popup
                    .child(widgets::menu_header("Projects"))
                    .children(
                        crate::project::known(&self.config, self.game)
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
            // Workspaces switch in the bar (Ctrl 0-3); the menu holds what
            // the bar does not.
            Menu::View => {
                let stats = self.stats;
                let perf = |cx: &mut Context<Self>| {
                    widgets::menu_item_checked("Performance overlay", stats, "").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            this.stats = !this.stats;
                            cx.notify();
                        }),
                    )
                };
                let mut perf_in_viewport = false;
                popup
                    // Workspaces first, with the shortcuts the switcher has.
                    .child(widgets::menu_header("Workspace"))
                    .children(Workspace::ALL.into_iter().enumerate().map(|(n, w)| {
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
                                items.push(perf(cx).into_any_element());
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
                        p.child(widgets::menu_section("Window")).child(perf(cx))
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
                | Pane::UiCanvas
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
            Pane::IslandInspector => "Inspector",
            Pane::IslandMaterials => "Ground material",
            Pane::IslandLibrary => "Library",
            Pane::UiTree => "Widget tree",
            Pane::UiCanvas => "Canvas",
            Pane::UiInspector => "Inspector",
            Pane::UiFiles => "UI files",
            Pane::UiTimeline => "Timeline",
            Pane::UiBindings => "Bindings",
            Pane::UiLibrary => "Widgets",
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
    /// When loading ended (the orbit waits a further two seconds), when the
    /// orbit started, and the current second's start and frames.
    loaded: Option<std::time::Instant>,
    started: Option<std::time::Instant>,
    second: std::time::Instant,
    frames: u32,
    /// Frames per second of each whole second.
    rates: Vec<u32>,
}

impl Bench {
    fn new(seconds: f32) -> Self {
        Self {
            seconds,
            running: false,
            loaded: None,
            started: None,
            second: std::time::Instant::now(),
            frames: 0,
            rates: Vec::new(),
        }
    }
}

impl Editor {
    /// One benchmark frame: turn the first view a little, present, and
    /// report each second.
    fn bench_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.entity();
        let Some(island) = &mut self.island else {
            window.on_next_frame(move |w, cx| editor.update(cx, |e, cx| e.bench_frame(w, cx)));
            return;
        };
        let bench = self.bench.as_mut().expect("benchmarking");
        let now = std::time::Instant::now();
        if island.loading() {
            bench.loaded = None;
        } else if bench.loaded.is_none() {
            bench.loaded = Some(now);
            eprintln!("bench: loaded");
        }
        let ready = bench
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
                cx.quit();
                return;
            }
            self.camera_moved(0, window, cx);
        }
        window.on_next_frame(move |w, cx| editor.update(cx, |e, cx| e.bench_frame(w, cx)));
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
            Pane::IslandInspector => self.render_island_inspector(window, cx),
            Pane::IslandMaterials => self.render_island_materials(cx),
            Pane::IslandLibrary => self.render_island_library(window, cx),
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
        let dirty = self.pending_save
            || self.island.as_ref().is_some_and(|i| i.dirty())
            || self.ui.as_ref().is_some_and(|u| u.dirty());
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
        if let Some(island) = &self.island {
            island.mark_built();
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
        ] {
            // Terrain tools belong to the Island workspace.
            if menu == Menu::Island && self.workspace != Workspace::Island {
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
            Workspace::ALL.map(|w| (w, w.label().into())),
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
        let chip = button(format!("{game}  ›  {}  ▾", self.project.info.name), false)
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
                let (dot, text, action) = if unsaved {
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
                                    this.install_mod();
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
                crate::project::count(island.doc.objects.len()),
                crate::project::count(island.doc.props.len()),
            ),
            // Nothing open: what the list on the left offers.
            (Some(_), None, Workspace::Island) if self.project.files.is_empty() => {
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
                            e.0 += usize::from(kind.admits(&s.path));
                        }
                        // No "0 in this mod": the list's This mod row says it,
                        // and the filter should lead.
                        // Largest first, the order the list uses.
                        let mut regions: Vec<_> = regions.into_iter().collect();
                        regions.sort_by_key(|(_, (shown, _))| std::cmp::Reverse(*shown));
                        let mut parts: Vec<String> = Vec::new();
                        parts.extend(regions.into_iter().map(|(region, (shown, total))| {
                            let name = crate::island::province(region);
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
            // Shown of total, where Assets and Island put their counts.
            (Some(_), _, Workspace::Ui) if self.project.files.is_empty() => {
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
                if self.project.files.is_empty()
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
            (Some(_), ..) if self.project.files.is_empty() => String::new(),
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
        if let Some(leave) = self.leaving.clone() {
            let (title, what) = match &leave {
                Leave::Game => ("Switch game?", "switching game"),
                Leave::Project(_) => ("Switch project?", "switching project"),
                Leave::NewIsland => ("Start a new island?", "starting a new island"),
                Leave::Island(_) => ("Open another island?", "opening another island"),
                Leave::Quit => ("Quit?", "quitting"),
            };
            let save = if leave == Leave::Quit {
                "Save and quit"
            } else {
                "Save and switch"
            };
            let go = leave.clone();
            root = root.child(widgets::modal(
                "leave",
                widgets::dialog(title)
                    .child(format!(
                        "The open document has unsaved edits. Save them before {what}, or discard them."
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                                this.leaving = None;
                                cx.notify();
                            })))
                                                        .child(button("Discard edits", false).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.leaving = None;
                                    // Dropping the island drops its edits;
                                    // the next one opens clean.
                                    if matches!(leave, Leave::NewIsland | Leave::Island(_)) {
                                        this.island = None;
                                    }
                                    this.go(leave.clone(), cx);
                                    cx.notify();
                                },
                            )))
                            .child(button(save, true).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.leaving = None;
                                    this.save();
                                    if !this.error {
                                        this.go(go.clone(), cx);
                                    }
                                    cx.notify();
                                },
                            ))),
                    ),
            ));
        }
        root.into_any_element()
    }

    /// An open document has edits not yet in the mod folder.
    pub fn unsaved(&self) -> bool {
        self.pending_save
            || self.island.as_ref().is_some_and(|i| i.dirty())
            || self.ui.as_ref().is_some_and(|u| u.dirty())
    }

    /// Switches game, project or island, or quits, asking first when edits
    /// would be lost. A half-typed value counts as an edit: it is applied
    /// first, from the menu as from the shortcut.
    pub fn leave(&mut self, to: Leave, cx: &mut Context<Self>) {
        self.end_typing();
        if self.unsaved() {
            self.leaving = Some(to);
            cx.notify();
        } else {
            self.go(to, cx);
        }
    }

    fn go(&mut self, to: Leave, cx: &mut Context<Self>) {
        match to {
            Leave::Game => cx.emit(SwitchGame),
            Leave::Project(folder) => self.open_project(&folder),
            Leave::NewIsland => self.open_wizard(),
            Leave::Island(path) => self.open_island(&path, cx),
            Leave::Quit => cx.quit(),
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
