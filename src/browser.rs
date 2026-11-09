// Owns the asset browser: the folder tree over the layered install, search,
// selection, and the preview pane (textures, text, hex, archive layers).
// Opening an island or a compiled UI asset hands off to its workspace.
use crate::editor::{Editor, empty_pane};
use anno_formats::vfs::{FileId, FolderId, Install, Node};
use gpui_editor_kit::{
    code::{Code, Language, code_view},
    field::{EditEvent, Filter, TextEdit, text_field},
    gpu::{self, Mesh, MeshData},
    gpui::{prelude::*, *},
    icon::Icon,
    math::{self, Mat4},
    model_view::ModelView,
    texture, theme,
    tree::{self, Nav, Row, Tree},
    widgets::{self, hint, info_row},
};
use std::sync::Arc;

/// Search stops after this many hits; more than a screenful of thousands of
/// rows only means the query is too broad.
const SEARCH_LIMIT: usize = 5000;
/// Hex previews show the head of the file only.
const HEX_BYTES: usize = 4096;
/// Text previews read this much: the view lays out only the lines on
/// screen, so the bound is memory and tokenizing time (~20 ms), not drawing.
const TEXT_BYTES: usize = 4 << 20;

pub enum Content {
    Texture {
        image: Arc<RenderImage>,
        /// Dark artwork on transparency: Auto shows it on Light.
        light: bool,
        summary: String,
        /// Mip levels to pick from, and the one shown; full size.
        mips: u32,
        mip: u32,
        dims: (u32, u32),
    },
    /// PNG/JPEG, decoded by GPUI's own image loader.
    Image(Arc<Image>),
    /// Source text, tokenized off the UI thread, and what it is ("Lua ·
    /// 1,204 lines · first 4 MB of 9.1 MB").
    Text {
        code: Arc<Code>,
        summary: String,
    },
    Hex(String),
    Island,
    UiAsset(String),
    /// A model's meshes, handed to the preview's renderer on arrival, and
    /// its facts; a `.cfg` keeps its XML for the Source view.
    Model {
        parts: Vec<(Arc<Mesh>, Mat4)>,
        summary: String,
        source: Option<Arc<Code>>,
    },
    Error(String),
}

pub struct Preview {
    file: FileId,
    size: usize,
    content: Content,
}

/// The Assets type filter: what a modder is usually after.
/// How a type's files show: in the folder tree, pruned to the folders
/// holding that type; as a flat list; or as pictures (textures only).
#[derive(Clone, Copy, PartialEq)]
enum View {
    Tree,
    List,
    Grid,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Types {
    #[default]
    All,
    Textures,
    Models,
    Islands,
    Ui,
    Text,
}

impl Types {
    /// What the footer counts while this type is listed without a search.
    fn noun(self) -> (&'static str, &'static str) {
        match self {
            Types::All => ("file", "files"),
            // No format: the filter takes PNG and JPEG files too.
            Types::Textures => ("texture", "textures"),
            Types::Models => ("model file", "model files"),
            Types::Islands => ("island file", "island files"),
            Types::Ui => ("UI file", "UI files"),
            Types::Text => ("text file", "text files"),
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }

    fn admits(self, path: &str) -> bool {
        let ext = path.rsplit('.').next().unwrap_or_default();
        match self {
            Types::All => true,
            Types::Textures => matches!(ext, "dds" | "png" | "jpg" | "jpeg"),
            Types::Models => matches!(ext, "cfg" | "prp" | "rdm" | "rdp" | "fc"),
            Types::Islands => matches!(ext, "a7m" | "a7t" | "a7me" | "a7te"),
            Types::Ui => kind(path) == "ui",
            Types::Text => matches!(ext, "xml" | "json" | "txt" | "lua" | "ini" | "loca"),
        }
    }
}

/// Image memory the thumbnails may hold; past it the cache starts over.
/// 64 MB is 1024 thumbnails at 128 px, 256 at 256 px: two screens of grid
/// tiles and the rows scrolled past.
const THUMB_BYTES: usize = 64 << 20;

/// How big the grid's tiles are; the list's rows are one size.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum TileSize {
    #[default]
    Small,
    Medium,
    Large,
}

impl TileSize {
    fn px(self) -> f32 {
        match self {
            Self::Small => 104.,
            Self::Medium => 144.,
            Self::Large => 192.,
        }
    }
}

/// Pictures for textures in a flat list: the first mip within `side` px
/// (128 for rows and small tiles, 256 for bigger tiles, which at 2x
/// showed 128 px pictures blown up), decoded off the UI thread for the
/// rows the list draws. The list's icon hook asks through `wanted`; a
/// poller decodes and repaints.
#[derive(Default)]
struct RowThumbs {
    /// Decode size of everything in `done`; 0 until the first draw sets it.
    side: u32,
    /// `None` when the file did not decode, so it is not asked again.
    done: foldhash::HashMap<FileId, Option<Arc<RenderImage>>>,
    /// Pixel size and format read from the same header ("512² · BC7"):
    /// what decides whether a replacement fits, where the byte size
    /// repeated down the list said little.
    info: foldhash::HashMap<FileId, SharedString>,
    wanted: Vec<FileId>,
    polling: bool,
    /// Thumbnails Auto puts on a light backdrop (`wants_light`).
    light: foldhash::HashSet<FileId>,
}

fn decode_thumb(
    install: &Install,
    file: FileId,
    side: u32,
) -> (Option<Arc<RenderImage>>, Option<SharedString>, bool) {
    let Some(bytes) = install.read_file(file).ok() else {
        return (None, None, false);
    };
    let Ok(dds) = texture::Dds::parse(&bytes) else {
        return (None, None, false);
    };
    let size = if dds.width == dds.height {
        // As the preview writes it, so one size reads one way.
        format!("{}×{}", dds.width, dds.height)
    } else {
        format!("{}×{}", dds.width, dds.height)
    };
    let format = match dds.format {
        texture::Format::Block(b) => format!("{b:?}"),
        f => format!("{f:?}"),
    }
    .to_uppercase();
    let (mut flat, mut light) = (false, false);
    let image = dds.decode(&bytes, dds.mip_for(side)).ok().map(|mut i| {
        one_channel_as_grey(dds.format, &mut i.rgba);
        // A thumbnail only: heights and masks often use a narrow band, and
        // grey at one level read as an unloaded tile. The preview shows the
        // true values.
        if dds.format == texture::Format::Block(texture::Block::Bc4) {
            flat = !stretch_grey(&mut i.rgba);
        }
        light = wants_light(&i.rgba);
        bleed_clear(&mut i.rgba, i.width as usize, i.height as usize);
        // Flattened onto the tile's own backdrop and letterboxed to a square
        // of it, so the tile holds no image edge at all: scaled down, the
        // picture's bounds still showed as a faint frame inside the tile.
        let [br, bg, bb]: [u8; 3] = if light { LIGHT } else { DARK }.to_be_bytes()[1..4]
            .try_into()
            .unwrap_or([0, 0, 0]);
        let (w, h) = (i.width as usize, i.height as usize);
        let side = w.max(h);
        let (ox, oy) = ((side - w) / 2, (side - h) / 2);
        let mut square = [br, bg, bb, 255].repeat(side * side);
        for y in 0..h {
            for x in 0..w {
                let (s, d) = ((y * w + x) * 4, ((y + oy) * side + x + ox) * 4);
                let a = u16::from(i.rgba[s + 3]);
                for (c, b) in [br, bg, bb].into_iter().enumerate() {
                    square[d + c] =
                        ((u16::from(i.rgba[s + c]) * a + u16::from(b) * (255 - a)) / 255) as u8;
                }
            }
        }
        texture::Texture::from_rgba(side as u32, side as u32, square)
            .unwrap_or(i)
            .render_image()
    });
    // A one-value map (many height maps hold a constant 50%) says so, as
    // its plain grey tile otherwise read as a failed load.
    let info = Some(SharedString::from(if flat {
        format!("flat · {format}")
    } else {
        format!("{size} · {format}")
    }));
    (image, info, light)
}

/// The Name|Folder divider being dragged in a flat list's header.
#[derive(Clone)]
struct ColumnDrag;

/// Nothing follows the cursor while the divider drags; the columns move.
struct NoDragPreview;

impl Render for NoDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// What a flat list is ordered by; the bool is descending.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SortBy {
    #[default]
    Name,
    /// Path order, which groups a folder's files.
    Folder,
    Size,
}

/// Files in case-insensitive name order, ties in path order, and each file's
/// place in it, so a filter walks `by_name` and a search sorts by `rank`.
struct NameOrder {
    by_name: Vec<FileId>,
    rank: Vec<u32>,
}

impl NameOrder {
    fn new(install: &Install) -> Self {
        let mut by_name: Vec<FileId> = (0..install.file_count() as u32).map(FileId).collect();
        // Ids are in path order, so the id breaks name ties by path.
        by_name.sort_by_cached_key(|&f| (install.file_name(f).to_ascii_lowercase(), f.0));
        let mut rank = vec![0u32; by_name.len()];
        for (i, f) in by_name.iter().enumerate() {
            rank[f.0 as usize] = i as u32;
        }
        Self { by_name, rank }
    }
}

#[derive(Default)]
pub struct Browser {
    install: Option<Arc<Install>>,
    tree: Tree<FolderId>,
    /// Shared with the list's row closures each frame; a type filter can
    /// list 100k+ files, too many to copy per render.
    rows: Arc<Vec<Row<Node>>>,
    pub selected: Option<FileId>,
    /// Keyboard focus on a folder row; files select through `selected`.
    folder: Option<FolderId>,
    query: String,
    /// The search (and whether it showed as a grid) that Show in tree
    /// left, offered back until another search starts.
    back: Option<(String, bool)>,
    editing: Option<TextEdit<()>>,
    hits: Vec<FileId>,
    overrides_only: bool,
    /// Which kind of file the list shows. Any filter turns the tree into a
    /// flat list of matching paths: inside a collapsed tree it hid nothing.
    types: Types,
    preview: Option<Preview>,
    /// Every file by name, built on the first flat list: name-first rows in
    /// path order read as unsorted.
    names: Option<NameOrder>,
    thumbs: std::rc::Rc<std::cell::RefCell<RowThumbs>>,
    sort: (SortBy, bool),
    /// Textures as a grid of pictures instead of rows.
    grid: bool,
    tile: TileSize,
    /// The file last extracted and where it went: a row under the
    /// preview's actions offers its folder while that file is shown.
    extracted: Option<(FileId, std::path::PathBuf)>,
    /// The model preview's renderer and camera, one for the pane's life.
    model: ModelView,
    /// A `.cfg` shown as its XML instead of the model.
    model_source: bool,
    /// The texture preview's channel and mip; reset per file.
    channel: Channel,
    mip: Option<u32>,
    /// What the texture sits on, and whether it shows fitted or at a zoom
    /// of its own pixels (scrolled); kept across files.
    backdrop: Backdrop,
    native: bool,
    /// Screen pixels per texel while not fitted: 1 is 1:1. The wheel steps
    /// it through ZOOMS.
    zoom: f32,
    /// The wheel's last zoom, what the zoom control's third segment returns
    /// to from Fit or 1:1.
    wheel_zoom: f32,
    /// A flat list limited to the files under this folder, else the
    /// whole install.
    scope: Option<FolderId>,
    /// The folder last focused in the tree: what a flat list offers to
    /// look inside, kept while files in the list are picked.
    near: Option<FolderId>,
    /// A type shown in the folder tree instead of a flat list.
    tree_view: bool,
    /// Per folder (by id), a bit per type whose files are somewhere under
    /// it: what lets the tree drop folders a type has nothing in. One
    /// pass over every file, on the first type shown as a tree.
    folder_kinds: Arc<Vec<u8>>,
    /// Files per type, by `Types as usize`, from the same pass: the Tree
    /// view's footer, as the flat views count theirs.
    kind_counts: [usize; 6],
    /// The Folder column's share of a flat list's width, once dragged.
    folder_share: Option<f32>,
    /// The list's width as last drawn, measured in list view too, so the
    /// grid's first frame has its columns right.
    list_width: std::rc::Rc<std::cell::Cell<f32>>,
    /// Device pixels per logical pixel, as the preview pane was last drawn:
    /// what a texture is enlarged to on load.
    dpr: f32,
    /// Where the fitted texture's canvas starts and the preview pane ends,
    /// as last drawn: the canvas runs to the pane's bottom.
    preview_span: std::rc::Rc<std::cell::Cell<[f32; 2]>>,
    /// Bumped per selection so a slow preview cannot replace a newer one.
    generation: u64,
    retired: Vec<Arc<RenderImage>>,
    pub message: Option<(String, bool)>,
}

impl Browser {
    /// Ends typing in the search field; the query stays.
    pub(crate) fn stop_typing(&mut self) {
        self.editing = None;
    }

    /// The search field while it takes keys.
    pub(crate) fn typing(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        self.editing
            .as_mut()
            .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer)
    }

    pub fn attach(&mut self, install: &Arc<Install>) {
        self.install = Some(install.clone());
        self.names = None;
        self.folder_kinds = Arc::default();
        self.tree.collapse_all();
        self.tree.expand(install.root());
        // Everything moddable is under data/; open it to start there.
        if let Some(data) = install.folder("data") {
            self.tree.expand(data);
        }
        self.rebuild();
    }

    /// What the list holds, for the status bar's left of the archive totals:
    /// a type's count or a search's matches; empty for the plain tree. Once
    /// a footer of its own, which sat on the status bar as a second one.
    pub fn count_line(&self) -> String {
        let n = self.rows.len();
        if self.flat() && !self.query.is_empty() {
            return crate::project::plural(n, "match", "matches");
        }
        if self.flat() && self.types == Types::All {
            // The checkbox is what flattened it.
            return format!(
                "{} patched by DLC or updates",
                crate::project::plural(n, "file", "files")
            );
        }
        if self.types == Types::All {
            return String::new();
        }
        // A type's tree counts what it shows, as its list does.
        let n = if self.flat() {
            n
        } else {
            self.kind_counts[self.types as usize]
        };
        let (one, many) = self.types.noun();
        format!(
            "{} {}",
            crate::project::count(n),
            if n == 1 { one } else { many }
        )
    }

    /// A search or filter lists matching files by full path instead of
    /// the folder tree.
    /// The zoom while not fitted; 1:1 until the wheel moves it.
    fn zoom(&self) -> f32 {
        if self.zoom > 0. { self.zoom } else { 1. }
    }

    /// One wheel notch in or out from `from`, through ZOOMS.
    fn zoom_step(from: f32, zoom_in: bool) -> f32 {
        if zoom_in {
            ZOOMS
                .iter()
                .copied()
                .find(|z| *z > from + 0.01)
                .unwrap_or(ZOOMS[ZOOMS.len() - 1])
        } else {
            ZOOMS
                .iter()
                .rev()
                .copied()
                .find(|z| *z < from - 0.01)
                .unwrap_or(ZOOMS[0])
        }
    }

    fn flat(&self) -> bool {
        !self.query.is_empty()
            || (self.types != Types::All && !self.tree_view)
            || self.overrides_only
    }

    /// Fills `folder_kinds`: each file's type bit on its folder and every
    /// folder above it, stopping where an ancestor already has it.
    fn index_folder_kinds(&mut self, install: &Install) {
        if !self.folder_kinds.is_empty() {
            return;
        }
        const KINDS: [Types; 5] = [
            Types::Textures,
            Types::Models,
            Types::Islands,
            Types::Ui,
            Types::Text,
        ];
        let mut kinds = vec![0u8; 1];
        let mut counts = [0usize; 6];
        for f in (0..install.file_count() as u32).map(FileId) {
            let path = install.file_path(f);
            let bits = KINDS
                .iter()
                .filter(|t| t.admits(path))
                .fold(0u8, |b, &t| b | t.bit());
            if bits == 0 {
                continue;
            }
            for t in KINDS {
                if bits & t.bit() != 0 {
                    counts[t as usize] += 1;
                }
            }
            let mut folder = Some(install.file_folder(f));
            while let Some(d) = folder {
                let i = d.0 as usize;
                if i >= kinds.len() {
                    kinds.resize(i + 1, 0);
                }
                if kinds[i] & bits == bits {
                    break;
                }
                kinds[i] |= bits;
                folder = install.parent(d);
            }
        }
        self.folder_kinds = Arc::new(kinds);
        self.kind_counts = counts;
    }

    fn rebuild(&mut self) {
        let Some(install) = self.install.clone() else {
            self.rows = Arc::default();
            return;
        };
        let types = self.types;
        // Indexed before the closures below borrow `self`.
        if types != Types::All && !self.flat() {
            self.index_folder_kinds(&install);
        }
        // Under the scope folder only, when the list is flat and has one.
        let prefix = self
            .scope
            .filter(|_| self.flat())
            .map(|d| format!("{}/", install.folder_path(d)));
        let shown = |f: FileId| {
            (!self.overrides_only || install.layers(f).len() > 1)
                && types.admits(install.file_path(f))
                && prefix
                    .as_deref()
                    .is_none_or(|p| install.file_path(f).starts_with(p))
        };
        let leaf = |f: FileId| Row {
            node: Node::File(f),
            depth: 0,
            disclosure: widgets::Disclosure::Leaf,
        };
        if self.flat() && self.names.is_none() {
            self.names = Some(NameOrder::new(&install));
        }
        let names = self.names.as_ref();
        if !self.query.is_empty() {
            let mut hits: Vec<FileId> = self.hits.clone();
            match (self.sort.0, names) {
                (SortBy::Name, Some(n)) => hits.sort_by_key(|f| n.rank[f.0 as usize]),
                (SortBy::Folder, _) => hits.sort_unstable(),
                (SortBy::Size, _) => hits.sort_by_key(|&f| install.winner(f).raw_size),
                _ => {}
            }
            if self.sort.1 {
                hits.reverse();
            }
            self.rows = Arc::new(
                hits.iter()
                    .filter(|&&f| shown(f))
                    .map(|&f| leaf(f))
                    .collect(),
            );
            return;
        }
        if let Some(n) = names.filter(|_| self.flat()) {
            // One pass over every file in name order, ~429k on 117: a few
            // ms, and only when the filter changes. The cache folder stays
            // out, as in the tree.
            let keep =
                |f: FileId| shown(f) && !install.file_path(f).starts_with("filebrowsercache/");
            let mut files: Vec<FileId> = match self.sort.0 {
                SortBy::Name => n.by_name.iter().copied().filter(|&f| keep(f)).collect(),
                SortBy::Folder | SortBy::Size => (0..install.file_count() as u32)
                    .map(FileId)
                    .filter(|&f| keep(f))
                    .collect(),
            };
            if self.sort.0 == SortBy::Size {
                // Stable, so equal sizes stay in path order.
                files.sort_by_key(|&f| install.winner(f).raw_size);
            }
            if self.sort.1 {
                files.reverse();
            }
            self.rows = Arc::new(files.into_iter().map(leaf).collect());
            return;
        }
        // A type in the tree: only folders with some file of it below.
        let kinds = &self.folder_kinds;
        let holds = |d: FolderId| {
            types == Types::All
                || kinds
                    .get(d.0 as usize)
                    .is_some_and(|k| k & types.bit() != 0)
        };
        // The game's file browser cache sits beside data/ but holds nothing
        // a mod changes; it stays out of the tree (search still finds it).
        let root = install.root();
        let children = |node: &Node| {
            let Node::Folder(folder) = *node else {
                return Vec::new();
            };
            install
                .children(folder)
                .filter(|n| !matches!(n, Node::File(f) if !shown(*f)))
                .filter(|n| !matches!(n, Node::Folder(d) if !holds(*d)))
                .filter(|n| {
                    !(folder == root
                        && matches!(n, Node::Folder(d) if install.folder_name(*d) == "filebrowsercache"))
                })
                .collect()
        };
        self.rows = Arc::new(self.tree.flatten(
            children(&Node::Folder(install.root())),
            folder_key,
            children,
        ));
    }

    /// Back in the tree after a filter: open the folders above the selected
    /// file so the pick made in the flat list is still in view.
    fn reveal_selected(&mut self) {
        match (self.selected, self.install.clone()) {
            // A type's tree reveals the pick only when it is one of that
            // type; otherwise the chip would fall back to every file.
            (Some(file), Some(install))
                if !self.flat() && self.types.admits(install.file_path(file)) =>
            {
                let path = install.file_path(file).to_owned();
                self.select_path(&path);
            }
            _ => self.rebuild(),
        }
    }

    /// The row the keyboard is on: the selected file, else the focused folder.
    fn cursor(&self) -> Option<usize> {
        let node = match (self.selected, self.folder) {
            (_, Some(f)) => Node::Folder(f),
            (Some(f), None) => Node::File(f),
            (None, None) => return None,
        };
        self.rows.iter().position(|r| r.node == node)
    }

    fn set_query(&mut self, query: String) {
        if !query.is_empty() {
            self.back = None;
        }
        self.query = query;
        self.hits = match &self.install {
            Some(install) => install.search(&self.query, SEARCH_LIMIT),
            None => Vec::new(),
        };
        self.reveal_selected();
    }

    /// Expands the folders above `path` and selects it.
    pub fn select_path(&mut self, path: &str) {
        let Some(install) = self.install.clone() else {
            return;
        };
        let Some(file) = install.file(path) else {
            self.message = Some((format!("No file {path}"), true));
            return;
        };
        self.tree.reveal(std::iter::successors(
            Some(install.file_folder(file)),
            |&f| install.parent(f),
        ));
        // The tree shows it with the search and patched filter off; a type
        // stays when the file is one (the Tree view of a type), else the
        // tree is every file's.
        self.query.clear();
        if !self.types.admits(path) {
            self.types = Types::All;
        }
        if self.types == Types::All {
            self.tree_view = false;
        }
        self.overrides_only = false;
        self.rebuild();
        self.selected = Some(file);
        self.folder = None;
        if let Some(i) = self.rows.iter().position(|r| r.node == Node::File(file)) {
            self.tree.scroll_to(i);
        }
    }

    fn toggle(&mut self, folder: FolderId) {
        self.tree.toggle(folder);
        self.folder = Some(folder);
        self.near = Some(folder);
        self.rebuild();
    }

    /// Search typing and arrow keys. Returns the file the keyboard moved to,
    /// for the caller to preview, and whether the key was used.
    fn key_down(&mut self, event: &KeyDownEvent, cx: &mut App) -> (bool, Option<FileId>) {
        if let Some(edit) = &mut self.editing {
            match edit.key_down_in(event, cx) {
                EditEvent::Commit | EditEvent::Cancel => self.editing = None,
                EditEvent::Changed => {
                    let text = edit.text.clone();
                    self.set_query(text);
                }
                // Ctrl 2, Ctrl S and the like stay the editor's shortcuts.
                EditEvent::Ignored => return (false, None),
            }
            return (true, None);
        }
        let cursor = self.cursor();
        match self.tree.navigate(event, &self.rows, cursor, folder_key) {
            None => (false, None),
            Some(Nav::Relayout) => {
                self.rebuild();
                (true, None)
            }
            Some(Nav::Select(i)) => match self.rows[i].node {
                Node::Folder(f) => {
                    self.folder = Some(f);
                    self.near = Some(f);
                    (true, None)
                }
                Node::File(f) => {
                    self.folder = None;
                    (true, Some(f))
                }
            },
        }
    }
}

/// What a folder holds directly, split so "12" is not read as the files
/// below it: "3 folders", "9 files" or both.
/// With a type in the tree, only what holds or is that type, as the rows
/// under it show; "9 folders" over three shown read as a broken filter.
fn folder_counts(install: &Install, f: FolderId, types: Types, kinds: &[u8]) -> String {
    let (folders, files) = if types == Types::All {
        (install.subfolders(f).len(), install.folder_files(f).len())
    } else {
        (
            (install.subfolders(f).iter())
                .filter(|d| {
                    kinds
                        .get(d.0 as usize)
                        .is_some_and(|k| k & types.bit() != 0)
                })
                .count(),
            (install.folder_files(f).iter())
                .filter(|&&file| types.admits(install.file_path(file)))
                .count(),
        )
    };
    match (folders, files) {
        (0, n) => crate::project::plural(n, "file", "files"),
        (d, 0) => crate::project::plural(d, "folder", "folders"),
        (d, n) => format!(
            "{} · {}",
            crate::project::plural(d, "folder", "folders"),
            crate::project::plural(n, "file", "files")
        ),
    }
}

/// A grid tile's folder: the telling folder's bare name, the one above a
/// generic last folder ("maps", "textures"), whole; the tile cuts it from
/// the start at its own width, once, keeping the end where siblings differ
/// ("…xilia_lean_m_01" / "…nary_lean_m_01"). "…/maps" alone told no unit
/// apart; a cut made here at a guessed width collided or was cut again, and
/// a middle cut dropped the very words siblings differ by.
/// A folder name every model or unit repeats, which tells nothing apart.
fn generic_folder(name: &str) -> bool {
    matches!(
        name,
        "maps" | "textures" | "texture" | "lod0" | "lod1" | "lod2" | "lod3" | "lod4"
    )
}

fn tile_note(path: &str) -> String {
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let mut parts = dir.rsplit('/');
    let last = parts.next().unwrap_or("");
    match (generic_folder(last), parts.next()) {
        (true, Some(parent)) => parent.to_owned(),
        _ => last.to_owned(),
    }
}

/// A flat list's folder for a file: the last two folders ("…/calidus_unit/
/// maps") when they fit the column, else the one that tells it apart, as
/// the grid captions it ("…/calidus_unit"); a cut inside a folder name
/// halved words, and a caption longer than the column was cut a second
/// time by the row. `room` is the characters the column holds as laid
/// out. Beside a row with the same name, `twin` is that row's path: when
/// both would read alike, the note starts at the first folder where they
/// differ ("dlc01/…/olive_trees").
/// Texture preview zooms the wheel steps through: whole steps from 100%,
/// where each texel takes whole screen pixels and stays square.
const ZOOMS: [f32; 9] = [0.25, 0.5, 1., 2., 3., 4., 5., 6., 8.];

// The Folder column's default share of a flat list: repeated names
// ("02_cloth_diff_0") are told apart by their folders, but with the size
// columns beside it a wider share cut the names' own endings.
const FOLDER_SHARE: f32 = 0.45;

/// The Folder column's width and its font's advances, so a note is sized
/// in pixels: counted in characters, a note of wide letters ("muscular")
/// passed and the row cut it a second time.
struct Room {
    px: f32,
    /// Advance of each ASCII char; others take the average.
    ascii: [f32; 128],
    average: f32,
}

impl Room {
    /// `px` wide in the window's font at `size`.
    fn measure(window: &Window, size: f32, px_wide: f32) -> Room {
        let ts = window.text_system();
        let font = ts.resolve_font(&window.text_style().font());
        let mut ascii = [0f32; 128];
        for (c, w) in ascii.iter_mut().enumerate() {
            *w = ts
                .advance(font, px(size), c as u8 as char)
                .map_or(size * 0.56, |s| f32::from(s.width));
        }
        let average = ascii[b'a' as usize..=b'z' as usize].iter().sum::<f32>() / 26.;
        ascii[0] = average;
        Room {
            px: px_wide.max(40.),
            ascii,
            // The "…" is not ASCII: other chars take its width, a
            // little over a letter's.
            average: ts
                .advance(font, px(size), '…')
                .map_or(average, |s| f32::from(s.width)),
        }
    }

    fn width(&self, text: &str) -> f32 {
        text.chars()
            .map(|c| self.ascii.get(c as usize).copied().unwrap_or(self.average))
            .sum()
    }

    fn fits(&self, text: &str) -> bool {
        self.width(text) <= self.px
    }
}

/// `name` cut once in its middle, at word breaks, to fit: its first
/// word, "…", then the longest ending after a "_" that fits
/// ("legionary_…lean_m_01"). Siblings differ at the end and kinds at the
/// start; a cut from the start lost the kind. The list and grid cut alike,
/// so one folder reads the same in both. `lead` comes first ("…/" in the
/// list).
fn cut_middle(name: &str, lead: &str, room: &Room) -> String {
    let whole = format!("{lead}{name}");
    if room.fits(&whole) {
        return whole;
    }
    let breaks: Vec<usize> = name.match_indices(['_', '-']).map(|(i, _)| i + 1).collect();
    let head = breaks.first().map_or(0, |&b| b);
    let tail = breaks
        .iter()
        .skip(1)
        .copied()
        .find(|&b| room.fits(&format!("{lead}{}…{}", &name[..head], &name[b..])));
    if let Some(b) = tail {
        // The room left over goes to the start of the words the cut drops:
        // the next word is the one siblings differ by (legionary, auxilia),
        // and dropped whole it left them reading alike with room to spare.
        let mut end = head;
        for (i, c) in name[head..b - 1].char_indices() {
            let next = head + i + c.len_utf8();
            if !room.fits(&format!("{lead}{}…{}", &name[..next], &name[b..])) {
                break;
            }
            end = next;
        }
        return format!("{lead}{}…{}", &name[..end], &name[b..]);
    }
    // Too narrow for a head: the longest ending alone.
    breaks
        .iter()
        .map(|&b| format!("{lead}…{}", &name[b..]))
        .find(|n| room.fits(n))
        .unwrap_or_else(|| format!("{lead}…{}", name.rsplit(['_', '-']).next().unwrap_or(name)))
}

fn folder_note(path: &str, room: &Room, twin: Option<&str>) -> Option<String> {
    let (dir, _) = path.rsplit_once('/')?;
    let note = |path: &str| -> Option<String> {
        let (dir, _) = path.rsplit_once('/')?;
        let parts: Vec<&str> = dir.rsplitn(3, '/').collect();
        if parts.len() < 3 {
            return Some(dir.to_owned());
        }
        // Both folders, unless the last is one every unit repeats
        // ("maps"): spending room on it cut the telling folder instead.
        // No "…/" lead: under a Folder header it is implied, and with it
        // only on cells that had room the column showed three cut styles
        // ("…/calidus_unit", "calidus_l…_m_01", a bare name).
        let both = format!("{}/{}", parts[1], parts[0]);
        if !generic_folder(parts[0]) && room.fits(&both) {
            return Some(both);
        }
        // Too long even alone: cut once, in its middle.
        Some(cut_middle(&tile_note(path), "", room))
    };
    let mine = note(path)?;
    let Some(other) = twin.filter(|t| note(t).as_deref() == Some(mine.as_str())) else {
        return Some(mine);
    };
    let other = other.rsplit_once('/').map_or("", |(d, _)| d);
    let k = dir
        .split('/')
        .zip(other.split('/'))
        .position(|(a, b)| a != b)?;
    let segments: Vec<&str> = dir.split('/').collect();
    let last = segments[segments.len() - 1];
    // The root that tells the twins apart stays whole; when the note does
    // not fit, its last folder is cut instead ("shared/…/olive_tr…"), once.
    Some(match segments.len() - k {
        1 => cut_middle(last, "", room),
        2 => cut_middle(last, &format!("{}/", segments[k]), room),
        _ => cut_middle(last, &format!("{}/…/", segments[k]), room),
    })
}

fn folder_key(node: &Node) -> Option<FolderId> {
    match node {
        Node::Folder(f) => Some(*f),
        Node::File(_) => None,
    }
}

fn kind(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or_default();
    match ext {
        "dds" => "texture",
        "png" | "jpg" | "jpeg" => "image",
        "a7m" | "a7t" => "island",
        "cfg" | "rdm" => "model",
        "a7me" | "a7te" | "xml" | "json" | "txt" | "lua" | "ini" | "fxh" | "hlsl" => "text",
        _ if path.contains("/ui/studio/generated/") => "ui",
        _ => "binary",
    }
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / f64::from(1 << 30)),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / f64::from(1 << 20)),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.),
        b => format!("{b} B"),
    }
}

/// Behind a texture preview: dark, light, or a checkerboard, so dark
/// content and transparency can be told apart from empty space.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Backdrop {
    /// Light behind dark artwork on transparency, else dark: a mixed folder
    /// holds both white icons and brown line art.
    #[default]
    Auto,
    Dark,
    Light,
    Checker,
}

/// The backdrops' colours, one swatch each for the grid and the preview.
const DARK: u32 = crate::colors::BACKDROP_DARK;
const LIGHT: u32 = crate::colors::BACKDROP_LIGHT;

impl Backdrop {
    /// What Auto settles on for a texture that `wants_light`.
    fn resolve(self, wants_light: bool) -> Backdrop {
        match (self, wants_light) {
            (Backdrop::Auto, true) => Backdrop::Light,
            (Backdrop::Auto, false) => Backdrop::Dark,
            (b, _) => b,
        }
    }
}

/// Gives fully transparent texels the colour of their nearest opaque
/// neighbour, along rows then columns. Drawn smaller, a thumbnail's
/// texels blend with their neighbours, and the black that transparent
/// texels carry drew dark seams along artwork's edges. Alpha is kept.
fn bleed_clear(rgba: &mut [u8], w: usize, h: usize) {
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return;
    }
    // Nearly clear counts too: an edge texel at alpha 8 still carried
    // black and left a faint line.
    let clear = |rgba: &[u8], i: usize| rgba[i * 4 + 3] < 16;
    // Rows with any opaque texel, as bits: the column pass fills the rest
    // from them.
    let mut lit = vec![0u64; h.div_ceil(64)];
    for y in 0..h {
        let row = y * w;
        let Some(first) = (0..w).find(|&x| !clear(rgba, row + x)) else {
            continue;
        };
        lit[y / 64] |= 1 << (y % 64);
        let mut last = first;
        for x in 0..w {
            if clear(rgba, row + x) {
                let from = (row + last) * 4;
                let to = (row + x) * 4;
                rgba.copy_within(from..from + 3, to);
            } else {
                last = x;
            }
        }
    }
    let is_lit = |y: usize| lit[y / 64] >> (y % 64) & 1 == 1;
    let Some(first) = (0..h).find(|&y| is_lit(y)) else {
        return;
    };
    let mut last = first;
    for y in 0..h {
        if is_lit(y) {
            last = y;
            continue;
        }
        for x in 0..w {
            let (from, to) = ((last * w + x) * 4, (y * w + x) * 4);
            rgba.copy_within(from..from + 3, to);
        }
    }
}

/// Dark artwork on transparency: some texels see-through and what is
/// opaque dark on average. On a dark backdrop it nearly disappears.
fn wants_light(rgba: &[u8]) -> bool {
    let (mut clear, mut lum, mut weight) = (0usize, 0f32, 0f32);
    let texels = rgba.as_chunks::<4>().0;
    for [r, g, b, a] in texels {
        if *a < 250 {
            clear += 1;
        }
        let w = *a as f32 / 255.;
        lum += w * (0.2126 * *r as f32 + 0.7152 * *g as f32 + 0.0722 * *b as f32) / 255.;
        weight += w;
    }
    clear * 20 > texels.len() && weight > 0. && lum / weight < 0.35
}

/// The fitted texture's scale, in logical px per texel, for a mip `w`
/// pixels wide on a display of `dpr` device px per logical px: within
/// 900 px and at most twice its pixels, and a whole number of device px per
/// texel when enlarging, so texels stay square (1.56× smeared a 576 px HUD
/// strip); on a 2× display that allows 1.5×. Smaller to fit a wider one.
fn fit_scale(w: u32, dpr: f32) -> f32 {
    let s = (900. / w as f32).min(2.);
    if s * dpr >= 1. {
        (s * dpr).floor() / dpr
    } else {
        s
    }
}

/// Records where its parent starts (`end` 0) or ends (1) into `span`, and
/// redraws the pane being drawn when that moved: panes are cached views, so
/// only a notify to this one reaches it.
fn span_probe(
    span: &std::rc::Rc<std::cell::Cell<[f32; 2]>>,
    end: usize,
    window: &mut Window,
) -> impl IntoElement {
    let (span, view) = (span.clone(), window.current_view());
    canvas(
        move |bounds, window, cx| {
            let y = f32::from(if end == 0 {
                bounds.top()
            } else {
                bounds.bottom()
            });
            let mut v = span.get();
            if (v[end] - y).abs() > 0.5 {
                v[end] = y;
                span.set(v);
                window.refresh();
                cx.notify(view);
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

/// The backdrop as an element filling its parent: a flat fill, or 12 px
/// checks painted as quads (a 1024 px view is a few thousand).
fn backdrop(kind: Backdrop) -> AnyElement {
    match kind {
        // Auto is resolved by the caller, per texture.
        Backdrop::Dark | Backdrop::Auto => {
            div().absolute().inset_0().bg(rgb(DARK)).into_any_element()
        }
        Backdrop::Light => div().absolute().inset_0().bg(rgb(LIGHT)).into_any_element(),
        Backdrop::Checker => canvas(
            |_, _, _| (),
            |bounds, _, window, _| {
                const CELL: f32 = 12.;
                let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                window.paint_quad(fill(bounds, rgb(crate::colors::CHECKER[0])));
                let (cols, rows) = ((w / CELL).ceil() as u32, (h / CELL).ceil() as u32);
                for y in 0..rows {
                    for x in (y % 2..cols).step_by(2) {
                        let cell = Bounds::new(
                            point(
                                bounds.origin.x + px(x as f32 * CELL),
                                bounds.origin.y + px(y as f32 * CELL),
                            ),
                            size(
                                px(CELL.min(w - x as f32 * CELL)),
                                px(CELL.min(h - y as f32 * CELL)),
                            ),
                        );
                        window.paint_quad(fill(cell, rgb(crate::colors::CHECKER[1])));
                    }
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element(),
    }
}

/// Which of a texture's channels the preview shows: all, or one as grey.
/// Normal and metal maps keep their data in single channels, and shown as
/// colour they read as flat or black.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Channel {
    #[default]
    Rgb,
    R,
    G,
    B,
    A,
}

impl Channel {
    /// The texels as the preview shows them: one channel spread to grey and
    /// opaque, or all of them as decoded.
    fn apply(self, rgba: &mut [u8]) {
        let k = match self {
            Channel::Rgb => return,
            Channel::R => 0,
            Channel::G => 1,
            Channel::B => 2,
            Channel::A => 3,
        };
        for px in rgba.as_chunks_mut::<4>().0 {
            let v = px[k];
            px.copy_from_slice(&[v, v, v, 255]);
        }
    }
}

/// A one-channel block texture (BC4: heights, masks) decodes to red; shown
/// as grey, as R8 and R16 already are, a row of them read as failed decodes.
fn one_channel_as_grey(format: texture::Format, rgba: &mut [u8]) {
    if format == texture::Format::Block(texture::Block::Bc4) {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[1] = px[0];
            px[2] = px[0];
        }
    }
}

/// Stretches a grey image's values to the full range, so a narrow band of
/// heights shows its relief; a truly flat one stays as it is. Whether it
/// had a range to stretch.
fn stretch_grey(rgba: &mut [u8]) -> bool {
    let (lo, hi) = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .fold((u8::MAX, 0u8), |(lo, hi), px| {
            (lo.min(px[0]), hi.max(px[0]))
        });
    if hi <= lo.saturating_add(1) {
        return false;
    }
    let span = (hi - lo) as u32;
    for px in rgba.as_chunks_mut::<4>().0 {
        let v = ((px[0] - lo) as u32 * 255 / span) as u8;
        px[..3].fill(v);
    }
    true
}

/// `dpr` and `native` say how the texture will be shown (device pixels per
/// logical, and 1:1 or fitted), so it is enlarged here, texel by texel, to
/// the whole number of device pixels each texel takes on screen.
fn load_preview(
    install: &Install,
    file: FileId,
    mip: Option<u32>,
    channel: Channel,
    dpr: f32,
    // The zoom when not fitted.
    native: Option<f32>,
) -> Preview {
    let path = install.file_path(file);
    let bytes = match install.read_file(file) {
        Ok(b) => b,
        Err(e) => {
            return Preview {
                file,
                size: 0,
                content: Content::Error(format!("{e:#}")),
            };
        }
    };
    let size = bytes.len();
    let content = match kind(path) {
        "texture" => match texture::Dds::parse(&bytes).and_then(|dds| {
            // The asked-for mip, else the largest within 1024 px.
            let mip = mip.map_or_else(|| dds.mip_for(1024), |m| m.min(dds.mips - 1));
            let mut image = dds.decode(&bytes, mip)?;
            one_channel_as_grey(dds.format, &mut image.rgba);
            channel.apply(&mut image.rgba);
            // Device pixels per texel as drawn: whole, as fit_scale makes
            // them; the GPU's linear sampler then draws square texels, where
            // enlarging the texture itself smeared them (a 576 px HUD strip
            // at 150%). Bounded at 4096 px a side.
            let per_texel = if let Some(zoom) = native {
                dpr * zoom
            } else {
                fit_scale(image.width, dpr) * dpr
            };
            let mut k = per_texel.round().max(1.) as u32;
            while k > 1 && image.width.max(image.height) * k > 4096 {
                k -= 1;
            }
            let light = wants_light(&image.rgba);
            // Texel edges from 8 device pixels a texel (400% at 2×), faint
            // there (8%) and firmer by 16 (25%): full strength at once read
            // as graph paper over the alpha edges being inspected.
            let image = image.upscaled(k);
            let image = if k >= 8 {
                let strength = 0.08 + (k as f32 - 8.).min(8.) / 8. * 0.17;
                image.texel_grid(k, strength)
            } else {
                image
            };
            Ok((dds, mip, image, light))
        }) {
            Ok((dds, mip, image, light)) => Content::Texture {
                light,
                image: image.render_image(),
                mips: dds.mips,
                mip,
                dims: (dds.width, dds.height),
                // The facts in one line, as the list names them; the raw
                // DXGI code was engine detail beside them.
                summary: format!(
                    "{}×{} · {} · {} mips · {}",
                    dds.width,
                    dds.height,
                    match dds.format {
                        texture::Format::Block(b) => format!("{b:?}").to_uppercase(),
                        f => format!("{f:?}"),
                    },
                    dds.mips,
                    human(size as u64),
                ),
            },
            Err(e) => Content::Error(format!("{e:#}")),
        },
        "island" => Content::Island,
        "model" => load_model(install, path, &bytes),
        "image" => Content::Image(Arc::new(Image::from_bytes(
            if path.ends_with(".png") {
                ImageFormat::Png
            } else {
                ImageFormat::Jpeg
            },
            bytes,
        ))),
        "ui" => {
            Content::UiAsset(String::from_utf8_lossy(&bytes[..bytes.len().min(5)]).into_owned())
        }
        _ => match text_head(&bytes) {
            Some(text) => text_content(path, text, size),
            _ => Content::Hex(hex_dump(&bytes[..bytes.len().min(HEX_BYTES)])),
        },
    };
    Preview {
        file,
        size,
        content,
    }
}

/// The `.cfg` an `.rdm` most likely belongs to, and which of its detail
/// levels the file is: `…/olive_trees/rdm/olive_01_lod2.rdm` is level 2 of
/// `…/olive_trees/olive_01.cfg`. The game keeps that layout; a cfg named
/// otherwise is not found and the mesh shows as clay.
fn owning_cfg(path: &str) -> Option<(String, &str, usize)> {
    let (dir, file) = path.rsplit_once('/')?;
    let stem = file.strip_suffix(".rdm")?;
    let (base, level) = match stem.rsplit_once("_lod") {
        Some((base, n)) if n.len() == 1 => (base, n.parse().ok()?),
        _ => (stem, 0),
    };
    let mesh = &path[..dir.len() + 1 + base.len()];
    let dir = dir.strip_suffix("/rdm").unwrap_or(dir);
    Some((format!("{dir}/{base}.cfg"), mesh, level))
}

/// A `.cfg` as the island draws it, textured, nested props and all. A bare
/// `.rdm` borrows the materials of the `.cfg` that uses it, else shows as
/// grey clay. A `.cfg` that is not a model shows as text.
fn load_model(install: &Install, path: &str, bytes: &[u8]) -> Content {
    let text = || text_head(bytes);
    let mut borrowed = None;
    let parts: Vec<(Arc<Mesh>, Mat4)> = if path.ends_with(".rdm") {
        // The owning cfg's pieces drawn from this file: the full mesh, or
        // the shipped level the file is. Levels the renderer keeps no room
        // for (a tree's lod3) show as clay.
        let textured: Vec<_> = owning_cfg(path)
            .and_then(|(cfg, mesh, level)| {
                let pieces =
                    crate::island::models::load(install, &Default::default(), &cfg, 1.).ok()?;
                let parts: Vec<_> = pieces
                    .into_iter()
                    .filter(|p| {
                        let stem = p.rdm.strip_suffix(".rdm").unwrap_or(&p.rdm);
                        stem.strip_suffix("_lod0").unwrap_or(stem) == mesh
                    })
                    .filter_map(|p| match level {
                        0 => Some((p.mesh, p.local)),
                        k => p.lods.get(k - 1).map(|m| (m.clone(), p.local)),
                    })
                    .collect();
                borrowed = Some(cfg);
                Some(parts)
            })
            .unwrap_or_default();
        if textured.is_empty() {
            borrowed = None;
            match anno_formats::rdm::Rdm::parse(bytes) {
                Ok(rdm) => clay(&rdm)
                    .into_iter()
                    .map(|m| (m, math::IDENTITY))
                    .collect(),
                Err(e) => return Content::Error(format!("{e:#}")),
            }
        } else {
            textured
        }
    } else {
        match crate::island::models::load(install, &Default::default(), path, 1.) {
            Ok(pieces) => pieces.into_iter().map(|p| (p.mesh, p.local)).collect(),
            Err(_) => Vec::new(),
        }
    };
    if parts.is_empty() {
        return match text() {
            Some(t) if path.ends_with(".cfg") => text_content(path, t, bytes.len()),
            _ => Content::Error("Nothing to draw".to_owned()),
        };
    }
    let triangles: usize = parts.iter().map(|(m, _)| m.triangles()).sum();
    let [lo, hi] = math::bounds(
        parts
            .iter()
            .flat_map(|(m, l)| math::corners(m.bounds()).map(|c| math::transform(l, c))),
    );
    let metres = |v: f32| {
        if v >= 10. {
            format!("{v:.0}")
        } else {
            format!("{v:.1}")
        }
    };
    Content::Model {
        summary: format!(
            // Named axes: a bare triple needed the game's convention known.
            // Width × height × depth, one fact; with W/H/D between dots
            // the parts read as separate facts without a unit.
            "{} × {} × {} m at scale 1 · {} triangles · {}",
            metres(hi[0] - lo[0]),
            metres(hi[1] - lo[1]),
            metres(hi[2] - lo[2]),
            crate::project::count(triangles),
            match &borrowed {
                Some(cfg) => format!("materials from {}", cfg.rsplit('/').next().unwrap_or(cfg)),
                None if path.ends_with(".rdm") => "untextured: no .cfg found".to_owned(),
                None => crate::project::plural(parts.len(), "mesh", "meshes"),
            },
        ),
        parts,
        source: text()
            .filter(|_| path.ends_with(".cfg"))
            .map(|t| Arc::new(Code::new(t, Language::Xml))),
    }
}

/// An rdm's every group as one untextured mesh.
fn clay(rdm: &anno_formats::rdm::Rdm) -> Option<Arc<Mesh>> {
    let data = MeshData {
        positions: rdm.positions.clone(),
        normals: (0..rdm.positions.len())
            .map(|i| rdm.normals.get(i).copied().unwrap_or([0., 1., 0.]))
            .collect(),
        colors: vec![0xa49a88; rdm.positions.len()],
        indices: rdm.indices.clone(),
        material: gpu::Material::Lit,
        ..Default::default()
    };
    Mesh::compact(data).ok().map(Arc::new)
}

/// The first `TEXT_BYTES` as text, a character cut at the end dropped;
/// `None` for binary (invalid UTF-8 or a NUL).
fn text_head(bytes: &[u8]) -> Option<&str> {
    let head = &bytes[..bytes.len().min(TEXT_BYTES)];
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        // Cut inside a character at the end: the rest is fine.
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&head[..e.valid_up_to()]).ok()?,
        Err(_) => return None,
    };
    (!text.contains('\0')).then_some(text)
}

fn text_content(path: &str, text: &str, size: usize) -> Content {
    let language = Language::guess(path, text);
    let code = Code::new(text, language);
    let mut summary = format!(
        "{}{}",
        match language {
            Language::Xml => "XML · ",
            Language::Lua => "Lua · ",
            Language::Json => "JSON · ",
            Language::Plain => "",
        },
        // "184,185 lines": `plural` prints the bare number.
        match code.line_count() {
            1 => "1 line".to_owned(),
            n => format!("{} lines", crate::project::count(n)),
        },
    );
    if size > text.len() {
        summary += &format!(
            " · first {} of {}",
            human(text.len() as u64),
            human(size as u64)
        );
    }
    // The view looks like an editor; edits start with Extract, said where
    // the reader is looking. Use in mod is named once it is on screen.
    summary += " · Read-only · Extract a copy to edit it";
    Content::Text {
        code: Arc::new(code),
        summary,
    }
}

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", row * 16));
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
        }
        out.push(' ');
        out.extend(chunk.iter().map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        }));
        out.push('\n');
    }
    out
}

impl Editor {
    pub fn browser_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let (used, file) = self.browser.key_down(event, cx);
        if let Some(file) = file {
            self.select_file(file, cx);
        }
        used
    }

    /// A type listed with nothing chosen previews its first file: an
    /// empty preview over two thirds of the window was the first thing
    /// every visit showed. Every way to list a type goes through here.
    fn preview_first(&mut self, cx: &mut Context<Self>) {
        if !self.browser.flat() || self.browser.selected.is_some() {
            return;
        }
        let first = self.browser.rows.iter().find_map(|r| match r.node {
            Node::File(f) => Some(f),
            Node::Folder(_) => None,
        });
        if let Some(f) = first {
            self.select_file(f, cx);
            self.notify_pane(crate::editor::Pane::Preview, cx);
        }
    }

    /// Opens `path` in Assets: its folder tree unfolded to it, selected and
    /// previewed. From a selected prop, the way to its model's files.
    pub fn show_in_assets(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(install) = self.install.clone() else {
            return;
        };
        let Some(file) = install.file(&path.replace('\\', "/")) else {
            return self.message(format!("{path} is not in the game’s archives"), true);
        };
        // The tree of every file, so the search or type filter of the last
        // visit does not hide it.
        self.browser.query.clear();
        self.browser.editing = None;
        self.browser.types = Types::All;
        self.browser.overrides_only = false;
        self.browser.selected = Some(file);
        self.browser.reveal_selected();
        self.select_file(file, cx);
        self.switch(crate::editor::Workspace::Assets);
        self.notify_pane(crate::editor::Pane::Browser, cx);
    }

    /// Assets listing every model, the Models page's way to the game's own.
    pub fn browse_game_models(&mut self, cx: &mut Context<Self>) {
        let b = &mut self.browser;
        b.query.clear();
        b.back = None;
        b.editing = None;
        b.overrides_only = false;
        b.types = Types::Models;
        b.tree_view = false;
        b.grid = false;
        b.reveal_selected();
        self.preview_first(cx);
        self.switch(crate::editor::Workspace::Assets);
        self.notify_pane(crate::editor::Pane::Browser, cx);
        cx.notify();
    }

    /// The preview stage's top and the pane's foot, the top as laid out
    /// before the pane scrolls: measured on screen, a scrolled pane made the
    /// stage taller by the scroll.
    /// Before the stage's top is first measured (it reads 0) the callers
    /// fall back to a fixed height: sized from 0 the first frame overflowed
    /// by the header's height, and the pane's scrollbar, drawn from the
    /// previous frame's overflow, stayed up once the stage had shrunk.
    fn preview_span_unscrolled(&self) -> [f32; 2] {
        let [top, bottom] = self.browser.preview_span.get();
        if top <= 0. {
            return [0., 0.];
        }
        let scrolled = f32::from(self.scrolls.get("preview").offset().y);
        [top - scrolled, bottom]
    }

    pub fn select_file(&mut self, file: FileId, cx: &mut Context<Self>) {
        // Each file's preview from its top.
        self.scrolls.get("preview").set_offset(Point::default());
        self.browser.selected = Some(file);
        self.browser.folder = None;
        self.browser.channel = Channel::Rgb;
        self.browser.mip = None;
        self.reload_preview(cx);
    }

    /// Loads the selected file's preview with the chosen channel and mip,
    /// off the UI thread; a newer request wins.
    fn reload_preview(&mut self, cx: &mut Context<Self>) {
        let (Some(install), Some(file)) = (self.install.clone(), self.browser.selected) else {
            return;
        };
        let (mip, channel) = (self.browser.mip, self.browser.channel);
        let (dpr, native) = (
            self.browser.dpr,
            self.browser.native.then(|| self.browser.zoom()),
        );
        self.browser.generation += 1;
        let generation = self.browser.generation;
        cx.spawn(async move |this, cx| {
            let preview = cx
                .background_executor()
                .spawn(async move { load_preview(&install, file, mip, channel, dpr, native) })
                .await;
            this.update(cx, |this, cx| {
                if this.browser.generation != generation {
                    if let Content::Texture { image, .. } = preview.content {
                        this.browser.retired.push(image);
                    }
                    return;
                }
                let mut preview = preview;
                if let Content::Model { parts, .. } = &mut preview.content {
                    // Anno's models are left-handed, as the island views.
                    this.browser.model.show(std::mem::take(parts), true);
                }
                if let Some(Preview {
                    content: Content::Texture { image, .. },
                    ..
                }) = this.browser.preview.replace(preview)
                {
                    this.browser.retired.push(image);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        let (Some(install), Some(file)) = (self.install.clone(), self.browser.selected) else {
            return;
        };
        let path = install.file_path(file).to_owned();
        match kind(&path) {
            "island" => self.open_island(&path, cx),
            "ui" => self.open_ui_asset(&path, cx),
            // Nothing else has an editor; the preview is the view.
            _ => {}
        }
    }

    /// Writes the winning bytes under the extract folder, mirroring the path.
    fn extract_selected(&mut self) {
        let (Some(install), Some(file)) = (self.install.clone(), self.browser.selected) else {
            return;
        };
        let path = install.file_path(file);
        let target = self.config.join("extracted").join(path);
        // A copy already there may hold edits: kept, not overwritten, and
        // offered again (Show in folder, Use in mod) as a fresh one is.
        if target.exists() {
            self.message(
                format!(
                    "Kept the copy already at {}; delete it to extract again",
                    target.display()
                ),
                false,
            );
            self.browser.extracted = Some((file, target));
            return;
        }
        let result = install.read_file(file).and_then(|bytes| {
            std::fs::create_dir_all(target.parent().expect("extracted paths have a parent"))?;
            std::fs::write(&target, bytes)?;
            Ok(())
        });
        match result {
            Ok(()) => {
                self.message(format!("Extracted to {}", target.display()), false);
                self.browser.extracted = Some((file, target));
            }
            Err(e) => self.message(format!("Extract failed: {e:#}"), true),
        }
    }

    /// Stages the extracted copy of the selected file, edited or not, at
    /// its own path in the mod: the second half of Extract, edit, use.
    fn use_extracted(&mut self, cx: &mut Context<Self>) {
        let (Some(install), Some((file, target))) =
            (self.install.clone(), self.browser.extracted.clone())
        else {
            return;
        };
        let path = install.file_path(file).to_owned();
        match std::fs::read(&target) {
            Ok(bytes) => {
                self.project.stage(&path, bytes);
                self.pending_save = true;
                self.message(
                    format!(
                        "Staged the extracted copy as {path}; saving writes it into the mod folder"
                    ),
                    false,
                );
            }
            Err(e) => self.message(format!("Could not read {}: {e}", target.display()), true),
        }
        cx.notify();
    }

    /// Picks a file from disk and stages it at the selected virtual path.
    fn replace_selected(&mut self, cx: &mut Context<Self>) {
        let (Some(install), Some(file)) = (self.install.clone(), self.browser.selected) else {
            return;
        };
        let path = install.file_path(file).to_owned();
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(format!("Replace {path}").into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(source) = paths.into_iter().next() else {
                return;
            };
            let read = std::fs::read(&source);
            this.update(cx, |this, cx| {
                match read {
                    Ok(bytes) => {
                        this.project.stage(&path, bytes);
                        this.pending_save = true;
                        this.message(
                            format!(
                                "Staged {} as {path}; saving writes it into the mod folder",
                                source.display()
                            ),
                            false,
                        );
                    }
                    Err(e) => {
                        this.message(format!("Could not read {}: {e}", source.display()), true)
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Decodes the texture thumbnails the list asked for, a batch every
    /// 60 ms, and repaints the list; stops once nothing is asked for a
    /// while, and the next render starts it again.
    fn poll_row_thumbs(&self, cx: &mut Context<Self>) {
        let thumbs = self.browser.thumbs.clone();
        if std::mem::replace(&mut thumbs.borrow_mut().polling, true) {
            return;
        }
        let Some(install) = self.install.clone() else {
            thumbs.borrow_mut().polling = false;
            return;
        };
        cx.spawn(async move |this, cx| {
            let mut idle = 0;
            while idle < 30 {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(60))
                    .await;
                let (batch, side): (Vec<FileId>, u32) = {
                    let mut t = thumbs.borrow_mut();
                    let mut wanted = std::mem::take(&mut t.wanted);
                    wanted.sort_unstable();
                    wanted.dedup();
                    wanted.retain(|f| !t.done.contains_key(f));
                    // The newest asks are the rows on screen now.
                    wanted.truncate(64);
                    (wanted, t.side.max(128))
                };
                if batch.is_empty() {
                    idle += 1;
                    continue;
                }
                idle = 0;
                let install = install.clone();
                let decoded = cx
                    .background_spawn(async move {
                        batch
                            .into_iter()
                            .map(|f| (f, decode_thumb(&install, f, side)))
                            .collect::<Vec<_>>()
                    })
                    .await;
                let ok = this.update(cx, |this, cx| {
                    let mut t = thumbs.borrow_mut();
                    // Decoded for a tile size since left: asked again.
                    if t.side.max(128) != side {
                        return;
                    }
                    let cap = THUMB_BYTES / (side as usize * side as usize * 4);
                    if t.done.len() + decoded.len() > cap {
                        // Start over; the images go to the GPU's drop list.
                        let old = std::mem::take(&mut t.done);
                        this.browser.retired.extend(old.into_values().flatten());
                        t.info.clear();
                    }
                    for (f, (image, info, light)) in decoded {
                        t.done.insert(f, image);
                        if light {
                            t.light.insert(f);
                        }
                        if let Some(info) = info {
                            t.info.insert(f, info);
                        }
                    }
                    drop(t);
                    this.notify_pane(crate::editor::Pane::Browser, cx);
                });
                if ok.is_err() {
                    break;
                }
            }
            thumbs.borrow_mut().polling = false;
        })
        .detach();
    }

    pub fn render_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(install) = self.install.clone() else {
            return empty_pane("Indexing the install…");
        };
        // Bigger tiles want bigger pictures: a new decode size starts the
        // cache over, the old images to the GPU's drop list.
        {
            let b = &mut self.browser;
            let side =
                if b.flat() && b.types == Types::Textures && b.grid && b.tile != TileSize::Small {
                    256
                } else {
                    128
                };
            let thumbs = b.thumbs.clone();
            let mut t = thumbs.borrow_mut();
            if t.side != side {
                let old = std::mem::take(&mut t.done);
                b.retired.extend(old.into_values().flatten());
                t.wanted.clear();
                t.side = side;
            }
        }
        let b = &self.browser;
        let search = text_field(
            "browser-search",
            &b.query,
            b.editing.as_ref().map(|e| e.display()),
            "Search filenames or paths…",
        )
        .flex_1()
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus.focus(window, cx);
            this.browser.editing =
                Some(TextEdit::new((), this.browser.query.clone(), Filter::Text));
            cx.notify();
        }));
        // How a type's files show: in the folder tree (pruned to folders
        // holding that type), as rows, or (textures) as pictures. Icons
        // beside the search, so the type row keeps the pane's width: as a
        // second set of words it was cut off in a narrow browser. Always in
        // place, dimmed with why: on All only Tree (a list of every file is
        // a search), Grid only for textures, no Tree while searching (a
        // search is its own flat list).
        let current = if !b.query.is_empty() {
            if b.grid && b.types == Types::Textures {
                View::Grid
            } else {
                View::List
            }
        } else if b.types == Types::All || b.tree_view {
            View::Tree
        } else if b.grid && b.types == Types::Textures {
            View::Grid
        } else {
            View::List
        };
        let (types, searching) = (b.types, !b.query.is_empty());
        let views = widgets::segmented_icons(
            "browser-view",
            [
                (View::Tree, Icon::TREE, "Tree"),
                (View::List, Icon::LIST, "List"),
                (View::Grid, Icon::GRID, "Grid"),
            ],
            &current,
            // A search is a flat list, but its matches show as rows or (for
            // textures) as pictures: disabling both had the grid out of
            // reach exactly when hunting for one picture.
            move |v| match (v, types) {
                (View::Tree, _) if searching => Some("A search lists its matches".into()),
                (View::Tree, _) => None,
                (View::List, _) if searching => None,
                (_, Types::All) => Some("Pick a type to list its files, or search".into()),
                (View::Grid, t) if t != Types::Textures => Some("Pictures are for textures".into()),
                _ => None,
            },
            cx,
            |this, view, _, _| {
                let b = &mut this.browser;
                b.tree_view = view == View::Tree;
                b.grid = view == View::Grid;
                b.reveal_selected();
            },
        );
        let header = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    // A filled search gets a way out in one click, as the
                    // UI files search has.
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .flex()
                            .child(search)
                            .when(!b.query.is_empty(), |d| {
                                d.child(
                                    div()
                                        .id("browser-search-clear")
                                        .absolute()
                                        .right(px(6.))
                                        .top(px(4.))
                                        .bottom(px(4.))
                                        .flex()
                                        .items_center()
                                        .px_1()
                                        .cursor_pointer()
                                        .text_color(theme::get().muted)
                                        .hover(|s| s.text_color(theme::get().text))
                                        .child("×")
                                        .tooltip(widgets::Tooltip::text("Clear search", "", ""))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.browser.query.clear();
                                            this.browser.editing = None;
                                            this.browser.rebuild();
                                            cx.notify();
                                        })),
                                )
                            })
                            // The search Show in tree left, one click away: the
                            // trip to a file's siblings cost the search.
                            .when_some(
                                b.back.clone().filter(|_| b.query.is_empty() && b.editing.is_none()),
                                |d, (query, grid)| {
                                    d.child(
                                        div()
                                            .id("browser-search-back")
                                            .absolute()
                                            .right(px(6.))
                                            .top(px(4.))
                                            .bottom(px(4.))
                                            .flex()
                                            .items_center()
                                            .px_1()
                                            .rounded(px(3.))
                                            .text_size(px(11.))
                                            .text_color(theme::get().accent)
                                            .cursor_pointer()
                                            .hover(|s| s.bg(theme::get().hover))
                                            .child(format!("← Results for ‘{query}’"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                let b = &mut this.browser;
                                                b.grid = grid;
                                                b.tree_view = false;
                                                b.set_query(query.clone());
                                                this.notify_pane(crate::editor::Pane::Browser, cx);
                                                cx.notify();
                                            })),
                                    )
                                },
                            ),
                    )
                    .child(views),
            )
            .child(widgets::segmented(
                "browser-types",
                [
                    (Types::All, "All".into()),
                    (Types::Textures, "Textures".into()),
                    (Types::Models, "Models".into()),
                    (Types::Islands, "Islands".into()),
                    (Types::Ui, "UI".into()),
                    (Types::Text, "Text".into()),
                ],
                &b.types,
                cx,
                |this, t, _, cx| {
                    this.browser.types = t;
                    this.browser.reveal_selected();
                    this.preview_first(cx);
                },
            ))
            // One row: what the list shows (patched files only) and where
            // it looks, right-aligned; both narrow the list, so they sit at
            // one depth. The scope is the whole install or the folder last
            // focused in the tree, so a type or search can stay inside one
            // DLC or one building's maps without typing its path.
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                widgets::checkbox(
                    // The rows' mark, said as its legend: a bare trailing
                    // ◆ read as a stray character. Short, so the scope fits
                    // beside it in a sidebar-wide browser; the tooltip says
                    // who patches.
                    // Said as what it is: "patched" and "overridden" both
                    // read as the mod's own edits, beside Replace in mod
                    // and in the mod loader's own words.
                    "Only files in several archives ◆",
                    b.overrides_only,
                    "",
                )
                .flex_shrink_0()
                .px_1()
                .tooltip(widgets::Tooltip::text(
                    "In several archives",
                    "",
                    "Files that a later archive (a DLC or a patch) replaces; the list shows the version the game loads",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.browser.overrides_only = !this.browser.overrides_only;
                    this.browser.reveal_selected();
                    cx.notify();
                })),
                    )
                    .child(div().flex_1())
                    .when_some(b.scope.or(b.near).filter(|_| b.flat()), |d, folder| {
                        // A content pack by the name the tree gives it ("Base
                        // game", "DLC 1"), other folders by their path: the
                        // bare "data/base" read as a path, not a scope, and a
                        // bare "maps" named nothing.
                        let name = {
                            let path = install.folder_path(folder);
                            let last = install.folder_name(folder);
                            match (path.strip_prefix("data/"), pack_name(last)) {
                                (Some(rest), Some(pack)) if !rest.contains('/') => pack,
                                (Some("base"), _) => "Base game".to_owned(),
                                _ => path.to_owned(),
                            }
                        };
                        d.child(
                            widgets::segmented(
                            "browser-scope",
                            // "Within …", not "Only …": beside the "Only
                            // patched files" box, two "Only"s read as rival
                            // filters; "Folder: Base game" read as a label of
                            // where the list was, not a choice of where to
                            // look. The full path and its origin (the tree's
                            // last folder) are on the tooltip.
                            [
                                (None, "All game folders".into()),
                                (Some(folder), format!("Within {name}").into()),
                            ],
                            &b.scope,
                            cx,
                            |this, scope, _, _| {
                                this.browser.scope = scope;
                                this.browser.rebuild();
                            },
                        )
                        .tooltip(widgets::Tooltip::text(
                            "Where to look",
                            "",
                            format!(
                                "Within {}, the folder last open in the tree. All game folders lists every file of the type.",
                                install.folder_path(folder)
                            ),
                        )),
                        )
                    }),
            );
        let rows = b.rows.clone();
        let (selected, folder) = (b.selected, b.folder);
        let searching = b.flat();
        let list_install = install.clone();
        let (list_types, list_kinds) = (b.types, b.folder_kinds.clone());
        let info_thumbs = b.thumbs.clone();
        let click_rows = rows.clone();
        // A mark per row for what it is, matching the type filter.
        let glyph_rows = rows.clone();
        let glyph_install = install.clone();
        let (note_rows, note_install) = (rows.clone(), install.clone());
        // Texture rows in a flat list carry their picture: sibling textures
        // share names and sizes, and only the image tells them apart.
        let pictures = searching && b.types == Types::Textures;
        let (thumb_rows, thumb_install, thumbs) = (rows.clone(), install.clone(), b.thumbs.clone());
        let picture = std::rc::Rc::new(move |i: usize| -> Option<Arc<RenderImage>> {
            let Node::File(f) = thumb_rows[i].node else {
                return None;
            };
            if !pictures || !thumb_install.file_path(f).ends_with(".dds") {
                return None;
            }
            let mut t = thumbs.borrow_mut();
            match t.done.get(&f) {
                Some(image) => image.clone(),
                None => {
                    t.wanted.push(f);
                    None
                }
            }
        });
        let gridded = pictures && b.grid;
        // Column room from the list's width as last laid out (0 before the
        // first layout: drawn wide, then redrawn). In a narrow browser, a
        // sidebar, the Name column keeps at least half the free row: the
        // Pixels column goes first (the preview says it), then Folder
        // narrows and finally goes (a row's tooltip has the path).
        let list_w = match b.list_width.get() {
            0. => 1000.,
            w => w,
        };
        let show_pixels = pictures && list_w >= 560.;
        let lead = 24. + if pictures { 24. } else { 14. };
        let detail_width = if show_pixels { 160. } else { 92. };
        let inner = list_w - 22.;
        let free = inner - lead - detail_width;
        let folder_px =
            (inner * b.folder_share.unwrap_or(FOLDER_SHARE)).min(free - (free * 0.5).max(120.));
        // No wider than its notes need, until the divider is dragged: the
        // widest whole note among the first rows (a list sorted by name
        // keeps one folder's files together), so names get the rest; at a
        // fixed share a model's LOD names were cut beside a half-empty
        // Folder column.
        let folder_px = match (b.folder_share, b.flat()) {
            (None, true) => {
                let measure = Room::measure(window, 11., f32::MAX);
                let widest = b
                    .rows
                    .iter()
                    .take(400)
                    .filter_map(|r| match r.node {
                        Node::File(f) => {
                            let dir = install.file_path(f).rsplit_once('/')?.0;
                            let mut parts = dir.rsplitn(3, '/');
                            let (last, parent) = (parts.next()?, parts.next().unwrap_or(""));
                            // A same-named neighbour's note starts at the
                            // first folder where the two part ("shared/…/
                            // olive_trees"), which can be the wider one.
                            let first = dir.split('/').next().unwrap_or("");
                            Some(
                                measure
                                    .width(&format!("…/{parent}/{last}"))
                                    .max(measure.width(&format!("{first}/…/{last}"))),
                            )
                        }
                        Node::Folder(_) => None,
                    })
                    .fold(0f32, f32::max);
                // The names' own need, so room they leave goes to the
                // folders: capped at half the free width, a short-named
                // list still cut "…/calidus_legio…lean_m_01" twice beside
                // an emptier Name column.
                let names = b
                    .rows
                    .iter()
                    .take(400)
                    .filter_map(|r| match r.node {
                        Node::File(f) => {
                            Some(measure.width(install.file_path(f).rsplit('/').next()?))
                        }
                        Node::Folder(_) => None,
                    })
                    .fold(0f32, f32::max);
                let folder_px = if names > 0. {
                    folder_px.max(free - names - 24.)
                } else {
                    folder_px
                };
                if widest > 0. {
                    folder_px.min(widest + 16. + 8.).max(folder_px.min(120.))
                } else {
                    folder_px
                }
            }
            _ => folder_px,
        };
        let share = (folder_px >= 72.).then(|| folder_px / inner);
        let (has_rows, has_thumbs) = (rows.clone(), b.thumbs.clone());
        let has_picture = move |i: usize| match has_rows[i].node {
            Node::File(f) => matches!(has_thumbs.borrow().done.get(&f), Some(Some(_))),
            Node::Folder(_) => false,
        };
        let (tip_rows, tip_install) = (rows.clone(), install.clone());
        // Column captions over a flat list, each a sort: name to find a file,
        // folder to see a folder's set together, size for the heaviest.
        let columns = searching.then(|| {
            let (by, desc) = b.sort;
            let column = |id: &'static str, label: &'static str, key: SortBy| {
                let arrow = if by == key {
                    if desc { " ▾" } else { " ▴" }
                } else {
                    ""
                };
                // A hover fill and a tooltip, so the captions read as sorts.
                div()
                    .id(id)
                    .px_1()
                    .rounded(px(3.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::get().hover).text_color(theme::get().text))
                    .tooltip(widgets::Tooltip::text(
                        // The texture column shows pixels but sorts by
                        // bytes; the tip says which.
                        if key == SortBy::Size {
                            "Sort by file size".to_owned()
                        } else {
                            format!("Sort by {}", label.to_lowercase())
                        },
                        "",
                        "Click again to reverse",
                    ))
                    .when(by == key, |d| d.text_color(theme::get().text))
                    // Right-aligned size columns put the arrow on their
                    // outer side, so the caption's end stays over its values.
                    .child(if key == SortBy::Size && !gridded {
                        match arrow.trim() {
                            "" => label.to_owned(),
                            a => format!("{a} {label}"),
                        }
                    } else {
                        format!("{label}{arrow}")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let (by, desc) = this.browser.sort;
                        this.browser.sort = (key, by == key && !desc);
                        this.browser.rebuild();
                        cx.notify();
                    }))
            };
            // In the grid the captions are only sorts, in one row.
            if gridded {
                return div()
                    .id("browser-sort-row")
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme::get().border)
                    .text_size(px(10.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::get().muted)
                    .child("Sort by")
                    .child(column("browser-sort-name", "Name", SortBy::Name))
                    .child(column("browser-sort-folder", "Folder", SortBy::Folder))
                    // "File size": a bare "Size" beside the S/M/L tile size
                    // read as the same thing.
                    .child(column("browser-sort-size", "File size", SortBy::Size))
                    // Tile size: small to scan a folder, large to tell
                    // near-identical maps apart without opening each.
                    .child(div().flex_1())
                    .child(
                        widgets::segmented_tips(
                            "browser-tile",
                            [
                                (TileSize::Small, "S".into(), "Small tiles"),
                                (TileSize::Medium, "M".into(), "Medium tiles"),
                                (TileSize::Large, "L".into(), "Large tiles"),
                            ],
                            &b.tile,
                            cx,
                            |this, size, _, _| this.browser.tile = size,
                        )
                        .font_weight(FontWeight::NORMAL),
                    );
            }
            div()
                .id("browser-columns")
                // Dragging the divider sets the Folder column's share: the
                // folder is what tells same-named files apart, so it should
                // get the room it needs. Row padding is 6 px left, 16 right;
                // the detail columns and their gaps take `detail_width`.
                .on_drag_move(
                    cx.listener(move |this, e: &DragMoveEvent<ColumnDrag>, _, cx| {
                        let b = e.bounds;
                        let inner = f32::from(b.size.width) - 22.;
                        let right = f32::from(b.right()) - 16. - detail_width;
                        let x = f32::from(e.event.position.x);
                        this.browser.folder_share =
                            Some(((right - x) / inner.max(1.)).clamp(0.15, 0.7));
                        cx.notify();
                    }),
                )
                .flex()
                .items_center()
                .h(px(22.))
                .pl(px(6.))
                .pr(px(16.))
                .gap_1()
                // The rows' own padding, with the rest of the indent as a
                // spacer: a share of the width is taken inside padding, so
                // more padding here put Folder out of line with its column.
                .child(div().w(px(lead - 7.)).flex_none())
                .border_b_1()
                .border_color(theme::get().border)
                .text_size(px(10.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::get().muted)
                // Pulled out by their padding, so the captions' text lines
                // up with the values under them; the hover fill overhangs.
                .child(column("browser-sort-name", "Name", SortBy::Name).flex_1().ml(px(-4.)))
                .when_some(share, |d, share| d.child(
                    div()
                        .w(relative(share))
                        .relative()
                        // The divider, on the column's left edge.
                        .child(
                            div()
                                .id("browser-folder-divider")
                                .absolute()
                                .left(px(-7.))
                                .top(px(-4.))
                                .h(px(22.))
                                .w(px(6.))
                                .flex()
                                .justify_center()
                                .cursor_col_resize()
                                .group("browser-divider")
                                .child(
                                    div()
                                        .w(px(1.))
                                        .h_full()
                                        .bg(theme::get().border)
                                        .group_hover("browser-divider", |d| {
                                            d.bg(theme::get().accent)
                                        }),
                                )
                                .on_drag(ColumnDrag, |_, _, _, cx| cx.new(|_| NoDragPreview)),
                        )
                        .child(column("browser-sort-folder", "Folder", SortBy::Folder)),
                ))
                // Textures: pixels and format as a caption (read from each
                // file's header as rows draw, so not sortable), then the
                // file size as its own sortable column; one meaning each.
                .when(show_pixels, |d| {
                    d.child(
                        div()
                            .w(px(84.))
                            .ml_1()
                            .flex()
                            .justify_end()
                            // Muted and plain, unlike the sorts beside it.
                            .child(
                                div()
                                    .id("browser-pixels-caption")
                                    // Nudged right without moving its column:
                                    // it ended ~5 px short of its values.
                                    .relative()
                                    .left(px(4.))
                                    .font_weight(FontWeight::NORMAL)
                                    .child("Pixels · format")
                                    .tooltip(widgets::Tooltip::text(
                                        "Pixels · format",
                                        "",
                                        "Read from each file’s header as rows draw, so not sortable",
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .w(px(60.))
                            .ml(px(8.))
                            .flex()
                            .justify_end()
                            .child(column("browser-sort-size", "Size", SortBy::Size).mr(px(-4.))),
                    )
                })
                .when(!show_pixels, |d| {
                    d.child(
                        div()
                            .w(px(84.))
                            .ml_1()
                            .flex()
                            .justify_end()
                            .child(if pictures {
                                // Read from headers as rows draw: not a sort.
                                div()
                                    .font_weight(FontWeight::NORMAL)
                                    .child("Pixels · format")
                                    .into_any_element()
                            } else {
                                column("browser-sort-size", "Size", SortBy::Size)
                                    .mr(px(-4.))
                                    .into_any_element()
                            }),
                    )
                })
        });
        // Textures by look: the same rows as tiles, pictures from the same
        // cache the list rows use.
        // A caption's room: the tile as the grid stretches it to fill the
        // row (its padding 8, gaps 6, scrollbar 12, as tree::Grid lays
        // out), less the tile's own padding, at the caption's 10 px.
        let tile_room = {
            let (w, t) = (b.list_width.get(), b.tile.px());
            let columns = (((w - 28.) + 6.) / (t + 6.)).floor().max(1.);
            let tile = ((w - 28. - 6. * (columns - 1.)) / columns).max(t);
            Room::measure(window, 10., tile - 12.)
        };
        let (auto, grid_light) = (b.backdrop == Backdrop::Auto, b.thumbs.clone());
        let grid_info = b.thumbs.clone();
        let large_tiles = b.tile != TileSize::Small;
        let grid_view = gridded.then(|| {
            let (grid_rows, grid_install, grid_picture) =
                (rows.clone(), install.clone(), picture.clone());
            let click = rows.clone();
            tree::Grid::new(
                "browser-grid",
                rows.len(),
                move |i| {
                    let Node::File(f) = grid_rows[i].node else {
                        return tree::Tile {
                            picture: None,
                            label: SharedString::default(),
                            detail: SharedString::default(),
                            selected: false,
                            tip: None,
                            backdrop: None,
                        };
                    };
                    let path = grid_install.file_path(f);
                    tree::Tile {
                        picture: grid_picture(i),
                        // Without the ".dds" every tile shares, and with
                        // break points after "_" and ".", so the label can
                        // wrap to the part that differs.
                        label: {
                            let name = grid_install.file_name(f);
                            // No break before the last "_" part: alone on
                            // the second line, "0" read as a stray digit.
                            let stem = name.strip_suffix(".dds").unwrap_or(name);
                            let (head, tail) = stem.rsplit_once('_').unwrap_or((stem, ""));
                            let breakable =
                                head.replace('_', "_\u{200b}").replace('.', ".\u{200b}");
                            if tail.is_empty() {
                                breakable.into()
                            } else {
                                format!("{breakable}_{tail}").into()
                            }
                        },
                        // Cut once after a "_", as the list's notes are.
                        // Medium and large tiles have the room for what a
                        // replacement must match, after the folder.
                        detail: {
                            let folder = cut_middle(&tile_note(path), "", &tile_room);
                            match (large_tiles, grid_info.borrow().info.get(&f)) {
                                (true, Some(info)) => format!("{folder} · {info}").into(),
                                _ => folder.into(),
                            }
                        },
                        selected: folder.is_none() && selected == Some(f),
                        // What a replacement must match, before the path:
                        // compared across tiles without selecting each.
                        tip: Some(
                            match grid_info.borrow().info.get(&f) {
                                Some(info) => format!(
                                    "{info} · {}\n{path}",
                                    human(grid_install.winner(f).raw_size)
                                ),
                                None => path.to_owned(),
                            }
                            .into(),
                        ),
                        backdrop: (auto && grid_light.borrow().light.contains(&f))
                            .then_some(rgb(LIGHT)),
                    }
                },
                move |this: &mut Editor, i, event, cx| {
                    if let Node::File(f) = click[i].node {
                        this.select_file(f, cx);
                        if event.click_count() == 2 {
                            this.open_selected(cx);
                        }
                    }
                },
            )
            .width(b.list_width.clone())
            .tile_size(b.tile.px())
            // The preview's backdrop: brown artwork wants Light, white
            // icons Dark, and a cropped strip's bands read as backdrop.
            .backdrop(rgb(match b.backdrop {
                Backdrop::Dark | Backdrop::Auto => DARK,
                Backdrop::Light => LIGHT,
                Backdrop::Checker => crate::colors::CHECKER[0],
            }))
            // The browser is a cached pane: a new width redraws it here.
            .on_resize(|this: &mut Editor, cx| this.notify_pane(crate::editor::Pane::Browser, cx))
            .highlight(b.query.clone())
            .render(window, cx)
            .flex_1()
            .min_h_0()
        });
        let pin_rows = rows.clone();
        let row_light_rows = rows.clone();
        // The Folder column's width in characters, from the list's width as
        // last laid out: the row less its padding, picture slot and detail
        // columns, times the column's share.
        // Characters the Folder column holds, at the row font's width for
        // a typical path: the 5.6 px guess was narrow by a tenth, so notes
        // overran the column and were cut again.
        let note_room = Room::measure(window, 11., folder_px - 16.);
        let (crumb_rows, crumb_install) = (rows.clone(), install.clone());
        let list = tree::List::new(
            "browser-rows",
            rows.len(),
            move |i| {
                let row = rows[i];
                let (label, detail, chosen) = match row.node {
                    Node::Folder(f) => (
                        // A content pack's code gets its kind beside it; the
                        // right column stays counts only.
                        {
                            let name = list_install.folder_name(f);
                            // The base game's folder named too, beside the
                            // packs it sits with.
                            match pack_name(name)
                                .or_else(|| (name == "base").then(|| "Base game".to_owned()))
                            {
                                Some(pack) => format!("{name} · {pack}"),
                                None => name.to_owned(),
                            }
                        },
                        // Direct contents, split so "12" is not read as the
                        // files below it.
                        folder_counts(&list_install, f, list_types, &list_kinds),
                        folder == Some(f),
                    ),
                    // The name whole in a flat list too, its folder muted
                    // after it (the note): a path cut in the middle lost
                    // the start of the name rows differ by.
                    // Textures listed alone drop the ".dds" every row
                    // shares; the footer says it once, as in the grid.
                    Node::File(f) => (
                        {
                            let name = list_install.file_name(f);
                            // In a Textures tree too, so the type reads the
                            // same in its three views.
                            let name = match name.strip_suffix(".dds") {
                                Some(stem) if list_types == Types::Textures => stem,
                                _ => name,
                            };
                            // ◆ after the name, as the filter box shows it: in
                            // front of a right-aligned size it moved with the
                            // digits and read as noise in a number column.
                            if list_install.layers(f).len() > 1 {
                                format!("{name} ◆")
                            } else {
                                name.to_owned()
                            }
                        },
                        // A texture's pixel size and format once its header
                        // is read; the bytes on hover-less rows until then.
                        // Textures: pixels and format once the header is
                        // read, then the bytes, as two columns.
                        // Narrow, textures keep pixels over bytes: nearly
                        // every map is 341.5 KB, and "512² · BC7" is what
                        // decides a replacement.
                        {
                            let info = || {
                                info_thumbs
                                    .borrow()
                                    .info
                                    .get(&f)
                                    .map_or_else(|| "…".to_owned(), |i| i.to_string())
                            };
                            let size = || human(list_install.winner(f).raw_size);
                            if show_pixels {
                                format!("{}\t{}", info(), size())
                            } else if pictures {
                                info()
                            } else {
                                size()
                            }
                        },
                        folder.is_none() && selected == Some(f),
                    ),
                };
                tree::Item {
                    depth: row.depth,
                    disclosure: row.disclosure,
                    label: label.into(),
                    detail: detail.into(),
                    selected: chosen,
                }
            },
            move |this: &mut Editor, i, event, cx| match click_rows[i].node {
                Node::Folder(f) => this.browser.toggle(f),
                Node::File(f) => {
                    this.select_file(f, cx);
                    if event.click_count() == 2 {
                        this.open_selected(cx);
                    }
                }
            },
        )
        .note(
            move |i| match (searching && share.is_some(), note_rows[i].node) {
                (true, Node::File(f)) => {
                    // A neighbour of the same name (the same file in two packs).
                    let name = note_install.file_name(f);
                    let twin = [i.wrapping_sub(1), i + 1]
                        .into_iter()
                        .filter_map(|j| note_rows.get(j))
                        .find_map(|r| match r.node {
                            Node::File(g) if g != f && note_install.file_name(g) == name => {
                                Some(note_install.file_path(g))
                            }
                            _ => None,
                        });
                    folder_note(note_install.file_path(f), &note_room, twin).map(SharedString::from)
                }
                _ => None,
            },
        )
        // The whole path on hover in a flat list, where the folder column
        // is cut; a patched file says what its ◆ means.
        .decorate(move |i, row| match tip_rows[i].node {
            Node::File(f) => {
                let patched = tip_install.layers(f).len() > 1;
                if !searching && !patched {
                    return row;
                }
                let path = if searching {
                    tip_install.file_path(f).to_owned()
                } else {
                    String::new()
                };
                row.tooltip(widgets::Tooltip::text(
                    if patched {
                        format!("◆ In {} archives", tip_install.layers(f).len())
                    } else {
                        String::new()
                    },
                    "",
                    // The preview's Archive layers line, in the same words.
                    match (path.is_empty(), patched) {
                        (false, true) => {
                            format!("{path}\nThe last one wins: this is the version the game loads")
                        }
                        (false, false) => path,
                        _ => "The last one wins: this is the version the game loads".to_owned(),
                    },
                ))
            }
            _ => row,
        })
        .icon({
            let picture = picture.clone();
            move |i| picture(i)
        })
        // Row pictures on the grid's and preview's backdrop, Auto's light
        // one per texture, so a file looks the same in all three.
        .icon_backdrop(rgb(match b.backdrop {
            Backdrop::Dark | Backdrop::Auto => DARK,
            Backdrop::Light => LIGHT,
            Backdrop::Checker => crate::colors::CHECKER[0],
        }))
        .icon_backdrop_for({
            let (rows, thumbs, auto) = (
                row_light_rows,
                b.thumbs.clone(),
                b.backdrop == Backdrop::Auto,
            );
            move |i| match rows[i].node {
                Node::File(f) if auto && thumbs.borrow().light.contains(&f) => Some(rgb(LIGHT)),
                _ => None,
            }
        })
        .glyph(move |i| {
            use gpui_editor_kit::icon::Icon;
            // A picture takes the glyph's place.
            if pictures && has_picture(i) {
                return None;
            }
            Some(match glyph_rows[i].node {
                Node::Folder(_) => Icon::FOLDER,
                Node::File(f) => {
                    let path = glyph_install.file_path(f);
                    match kind(path) {
                        "texture" | "image" => Icon::IMAGE,
                        "island" => Icon::ISLAND,
                        "ui" => Icon::LAYOUT,
                        _ if Types::Models.admits(path) => Icon::CUBE,
                        "text" => Icon::TEXT,
                        _ => Icon::FILE,
                    }
                }
            })
        })
        // Taller with pictures, so a 26 px texture reads and glyph rows
        // still waiting for theirs keep the names in line.
        // In the tree, the folders above the top row stay pinned, nearest
        // three: a pick revealed five levels down sat with no sign of
        // which branch it was in.
        .sticky(move |top| {
            if searching {
                return Vec::new();
            }
            let mut above = Vec::new();
            let mut depth = pin_rows.get(top).map_or(0, |r| r.depth);
            for j in (0..top).rev() {
                if depth == 0 {
                    break;
                }
                if pin_rows[j].depth < depth {
                    depth = pin_rows[j].depth;
                    above.push(j);
                }
            }
            above.truncate(3);
            above.reverse();
            above
        })
        // The ancestors above the pinned three, as one muted line.
        .sticky_crumb(move |first| match crumb_rows.get(first).map(|r| r.node) {
            Some(Node::Folder(f)) => crumb_install.parent(f).and_then(|p| {
                let path = crumb_install.folder_path(p);
                // No trailing "›": it read as clipped text; the indented
                // rows under it continue the path.
                (!path.is_empty()).then(|| path.replace('/', " › ").into())
            }),
            _ => None,
        })
        // Banded: at the half split a count or size sits far right of a
        // short name, and the band keeps the eye on its row.
        .banded()
        .guides()
        .note_share(share.unwrap_or(FOLDER_SHARE))
        .detail_columns(&[84., 60.])
        .row_height(if pictures {
            28.
        } else {
            gpui_editor_kit::widgets::TREE_ROW
        })
        .highlight(b.query.clone())
        .render(&b.tree, window, cx)
        .flex_1()
        .min_h_0();
        let list = grid_view.unwrap_or(list);
        if pictures {
            self.poll_row_thumbs(cx);
        }
        // A count only answers a search or filter; unfiltered, the tree's
        // own rows say enough and the archive totals are on the status bar.

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(header)
            .children(columns)
            .child({
                // The list area's width each frame, for the grid's columns.
                let (measured, view) = (b.list_width.clone(), window.current_view());
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        canvas(
                            // The columns follow the width: a pane is a
                            // cached view, so a new width redraws it here.
                            move |bounds, window, cx| {
                                let w = f32::from(bounds.size.width);
                                if (measured.get() - w).abs() > 0.5 {
                                    measured.set(w);
                                    window.refresh();
                                    cx.notify(view);
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(list)
            })
            .into_any_element()
    }

    /// The preview pane for a focused folder: its path, counts of what it
    /// holds directly by kind, and its subfolders as rows that open them.
    /// Direct contents only: totals over `data` would walk 429k files.
    fn folder_summary(
        &self,
        install: &Arc<Install>,
        folder: FolderId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let files = install.folder_files(folder);
        let subfolders = install.subfolders(folder);
        let mut kinds: Vec<(&'static str, usize)> = Vec::new();
        let mut bytes = 0u64;
        for &f in files {
            bytes += install.winner(f).raw_size;
            let k = kind(install.file_path(f));
            match kinds.iter_mut().find(|(name, _)| *name == k) {
                Some((_, n)) => *n += 1,
                None => kinds.push((k, 1)),
            }
        }
        kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
        let name = install.folder_name(folder).to_owned();
        let mut body = div()
            .id("folder-summary")
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .w_full()
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(info_row(
                "Direct contents",
                format!(
                    "{} · {}",
                    crate::project::plural(subfolders.len(), "folder", "folders"),
                    crate::project::plural(files.len(), "file", "files")
                ),
            ));
        if !files.is_empty() {
            body = body
                .child(info_row(
                    "Kinds",
                    kinds
                        .iter()
                        .map(|(k, n)| format!("{} {k}", crate::project::count(*n)))
                        .collect::<Vec<_>>()
                        .join(" · "),
                ))
                .child(info_row("Size", human(bytes)));
        }
        if !subfolders.is_empty() {
            body = body.child(div().pt_2().child(hint("Subfolders")));
            // A few to start from; the tree holds the rest.
            const SHOWN: usize = 8;
            for (n, &sub) in subfolders.iter().take(SHOWN).enumerate() {
                let label = install.folder_name(sub).to_owned();
                // As the tree counts it: "0 files" on a folder of folders
                // read as empty.
                let detail = folder_counts(install, sub, Types::All, &[]);
                body = body.child(
                    div()
                        .id(("folder-summary-sub", n))
                        .flex()
                        .justify_between()
                        .px_1()
                        .py(px(2.))
                        .rounded(px(3.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::get().hover))
                        .child(div().text_size(px(12.)).child(label))
                        .child(hint(detail))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.browser.toggle(sub);
                            cx.notify();
                        })),
                );
            }
            if subfolders.len() > SHOWN {
                body = body.child(hint(format!(
                    "… and {} more in the tree",
                    subfolders.len() - SHOWN
                )));
            }
        }
        body.into_any_element()
    }

    pub fn render_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // A new display scale reloads the texture at its texel size.
        let dpr = window.scale_factor();
        if (dpr - self.browser.dpr).abs() > 0.01 {
            self.browser.dpr = dpr;
            if self.browser.preview.is_some() {
                self.reload_preview(cx);
            }
        }
        for image in self.browser.retired.drain(..) {
            // A preview that finished after the selection moved never painted.
            let _ = window.drop_image(image);
        }
        let Some(install) = self.install.clone() else {
            return empty_pane("No install");
        };
        let Some(file) = self.browser.selected else {
            // A focused folder summarizes itself: what is in it, by kind,
            // and its subfolders to open. Two thirds of the window said
            // nothing until a file was found by drilling.
            // Only in the tree: in a flat list the focused folder is the
            // scope's, and its summary read as the wrong state beside a
            // list of the whole install.
            if let Some(folder) = self.browser.folder.filter(|_| !self.browser.flat()) {
                return self.folder_summary(&install, folder, cx);
            }
            // Named for the type being listed, so the line matches the list.
            let types = self.browser.types;
            let (one, _) = types.noun();
            // Short, the verbs on the line under it: as one sentence it
            // ran to the pane's edge at the half split.
            let title = match types {
                Types::All => "Select a file".to_owned(),
                _ => format!("Select a {}", one.split(" (").next().unwrap_or(one)),
            };
            use gpui_editor_kit::icon::Icon;
            let icon = match types {
                Types::All => Icon::FILE,
                Types::Textures => Icon::IMAGE,
                Types::Models => Icon::CUBE,
                Types::Islands => Icon::ISLAND,
                Types::Ui => Icon::LAYOUT,
                Types::Text => Icon::TEXT,
            };
            return widgets::empty_state_icon(icon, title)
                .child(hint("Preview it, extract it, or replace it in the mod."))
                // Only where such files are listed.
                .when(
                    matches!(types, Types::All | Types::Islands | Types::Ui),
                    |d| d.child(hint("Islands and UI screens open in their editors.")),
                )
                // Where a replacement goes, before the first one.
                .child(hint(format!(
                    "Replacements save into ‘{}’; the game’s archives stay unchanged.",
                    self.project.info.name
                )))
                // Where to start, as links in a sentence: the chips above the
                // tree are the control, these only name the common ways in.
                // Full buttons here were a second set of chips; a sentence
                // pointing at chips 800 px away gave directions instead.
                .when(types == Types::All, |d| {
                    let link = |t: Types, label: &'static str| {
                        div()
                            .id(label)
                            .text_color(theme::get().accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.browser.types = t;
                                this.browser.reveal_selected();
                                this.preview_first(cx);
                                this.notify_pane(crate::editor::Pane::Browser, cx);
                                cx.notify();
                            }))
                    };
                    d.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_center()
                            .gap_1()
                            .text_size(px(11.))
                            .child(div().text_color(theme::get().muted).child("List every"))
                            .child(link(Types::Textures, "texture"))
                            .child(div().text_color(theme::get().muted).child("·"))
                            .child(link(Types::Models, "model"))
                            .child(div().text_color(theme::get().muted).child("·"))
                            .child(link(Types::Islands, "island"))
                            .child(div().text_color(theme::get().muted).child("·"))
                            .child(link(Types::Ui, "UI screen")),
                    )
                })
                .into_any_element();
        };
        let path = install.file_path(file).to_owned();
        let layers = install.layers(file);
        let mut body = div()
            .id("preview")
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .w_full()
            // The file's name as the pane's subject, with what can be done
            // to it on the same row; under it, muted, where it lives and
            // what it came from. The full path as a 12 px first line named
            // no subject and cost a row.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(install.file_name(file).to_owned()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .gap_2()
                            // Only where there is an editor to open, named: on
                            // a texture "Open" did nothing.
                            // A model's next step is an island: arm Place with
                            // it there (or on the next island opened), as the
                            // Library's cards do.
                            .when_some(
                                match kind(&path) {
                                    "island" => Some("Open in Island editor"),
                                    "ui" => Some("Open in UI editor"),
                                    _ => None,
                                },
                                |d, label| {
                                    d.child(
                                        widgets::action(label, widgets::Tone::Secondary).on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.open_selected(cx);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                },
                            )
                            // One order for every file type, Extract, Replace
                            // in mod…, then a model's Place; only which one is
                            // filled changes. Swapping the order to keep the
                            // filled one last moved Extract 140 px between a
                            // texture and a Lua file.
                            .map(|d| {
                                // On read-only text the first step of an edit
                                // (the caption says so), so it is the filled one.
                                // Once this file is extracted the same button is
                                // the next step, Use in mod: the steps take turns
                                // in one place instead of a second action
                                // appearing under the path.
                                                                let extract = {
                                let tone = if matches!(
                                    self.browser.preview.as_ref().map(|p| &p.content),
                                    Some(Content::Text { .. })
                                ) {
                                    widgets::Tone::Primary
                                } else {
                                    widgets::Tone::Secondary
                                };
                                if self.browser.extracted.as_ref().is_some_and(|(f, _)| *f == file) {
                                    widgets::action("Use in mod", tone)
                                        .tooltip(widgets::Tooltip::text(
                                            "Use the extracted copy",
                                            "",
                                            "Stages the copy, as you left it, at this path in the mod",
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| this.use_extracted(cx)))
                                } else {
                                    widgets::action("Extract", tone)
                                        // Where it goes, before it goes there: a
                                        // copy to edit, outside the mod.
                                        .tooltip(widgets::Tooltip::text(
                                            "Extract a copy",
                                            "",
                                            format!(
                                                "To {}. Outside the mod: edit it there, then Use in mod",
                                                crate::project::short_path(
                                                    &self.config.join("extracted").join(&path)
                                                )
                                            ),
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.extract_selected();
                                            cx.notify();
                                        }))
                                }
                            };
                                // The one action that writes into the mod; named
                                // for where it writes: the game's archive stays.
                                                                let replace = widgets::action(
                                    "Replace in mod…",
                                    // Secondary where another step leads: a .cfg
                                    // model's Place, text's Extract.
                                    if match self.browser.preview.as_ref().map(|p| &p.content) {
                                        Some(Content::Text { .. }) => true,
                                        Some(Content::Model { .. }) => path.ends_with(".cfg"),
                                        _ => false,
                                    } {
                                        widgets::Tone::Secondary
                                    } else {
                                        widgets::Tone::Primary
                                    },
                                )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.replace_selected(cx);
                                    }));
                                d.child(extract).child(replace)
                            })
                            .when(path.ends_with(".cfg") && matches!(self.browser.preview.as_ref().map(|p| &p.content), Some(Content::Model { .. })), |d| {
                                let cfg = path.clone();
                                let open = self.island.is_some();
                                // Named for the island it lands on; with none
                                // open, the trip goes through the list.
                                let label = match &self.island {
                                    Some(i) => format!(
                                        "Place on {}",
                                        crate::island::island_title(&i.path)
                                    ),
                                    None => "Place on an island…".to_owned(),
                                };
                                d.child(
                                    // A model's primary step: Replace is for
                                    // a model made elsewhere.
                                    widgets::action(label, widgets::Tone::Primary)
                                    .tooltip(widgets::Tooltip::text(
                                        "Place on island",
                                        "",
                                        if open {
                                            "Switches to the island with the Place tool holding this model"
                                        } else {
                                            "Pick an island in the list; the Place tool will hold this model when it opens"
                                        },
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        match &mut this.island {
                                            Some(island) => {
                                                island.choose_prop(&cfg);
                                                this.switch(crate::editor::Workspace::Island);
                                            }
                                            None => {
                                                this.place_pending = Some(cfg.clone());
                                                this.show_pane(crate::editor::Pane::IslandList);
                                            }
                                        }
                                        cx.notify();
                                    })),
                                )
                            })
                    ),
            )
            // Folder, archive and (but for a texture, whose summary below
            // says it) size, cut in the middle; a click copies the whole
            // path, for ModOps and file names in a mod.
            .child({
                let archive = &install.archives()[install.winner(file).archive as usize];
                let folder = path.rsplit_once('/').map_or("", |(d, _)| d);
                // The folder, the archive and size, then the actions on it.
                let line = folder.to_owned();
                let size = (kind(&path) != "texture").then(|| human(install.winner(file).raw_size));
                let archive = archive.to_string();
                let (copied, button_copy) = (path.clone(), path.clone());
                let line_el = div()
                    .id("preview-path")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_middle()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme::get().text))
                    .tooltip(widgets::Tooltip::text("Copy path", "", path.clone()))
                    .child(line)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        this.browser.message = Some(("Copied the path".to_owned(), false));
                        cx.notify();
                    }));
                // The copy as a visible action too, right after what it
                // copies: the line's own click had no resting sign of it,
                // and at the pane's far edge it sat 650 px from the path.
                // What it is, muted, then what to do with it: links between
                // the path and its archive read as stray tokens.
                // The kit's middot between path, archive and size, so the
                // dots match every other line's.
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(theme::get().muted)
                            .child(line_el)
                            .child(widgets::middot())
                            .child(div().flex_shrink_0().child(archive))
                            .when_some(size, |d, size| {
                                d.child(widgets::middot())
                                    .child(div().flex_shrink_0().child(size))
                            }),
                    )
                    .child(
                        div()
                            .id("preview-copy-path")
                            .ml_2()
                            .flex_shrink_0()
                            .px_1()
                            .rounded(px(3.))
                            .text_size(px(11.))
                            .text_color(theme::get().accent)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::get().hover))
                            .child("Copy path")
                            .tooltip(widgets::Tooltip::text(
                                "Copy path",
                                "",
                                "The game-relative path, for ModOps and file names in a mod",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(button_copy.clone()));
                                this.browser.message = Some(("Copied the path".to_owned(), false));
                                cx.notify();
                            })),
                    )
                    // From a search or a flat list, the file among its
                    // neighbours in one click: its siblings (the other
                    // mips, the model's textures) were a new search away.
                    .when(self.browser.flat(), |d| {
                        // −1 px: the links' padding and the glyph's own
                        // bearings already space it as wide as a text dot.
                        d.child(widgets::middot().mx(px(-1.)).text_size(px(11.)))
                            .child(
                            div()
                                .id("preview-show-in-tree")
                                .flex_shrink_0()
                                .px_1()
                                .rounded(px(3.))
                                .text_size(px(11.))
                                .text_color(theme::get().accent)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::get().hover))
                                .child("Show in tree")
                                .tooltip(widgets::Tooltip::text(
                                    "Show in tree",
                                    "",
                                    "Clears the search and opens the folder tree at this file",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let b = &mut this.browser;
                                    if !b.query.is_empty() {
                                        b.back = Some((std::mem::take(&mut b.query), b.grid));
                                    }
                                    b.editing = None;
                                    b.overrides_only = false;
                                    b.tree_view = true;
                                    b.grid = false;
                                    b.reveal_selected();
                                    this.notify_pane(crate::editor::Pane::Browser, cx);
                                    cx.notify();
                                })),
                        )
                    })
            })
            // Where Extract put this file, until another is shown: the
            // status message alone was gone by the time it was wanted.
            .when_some(
                self.browser.extracted.clone().filter(|(f, _)| *f == file),
                |d, (_, target)| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(11.))
                            .child(div().text_color(theme::get().ok).child("✓ Extracted"))
                            .child(
                                div()
                                    .id("preview-extracted")
                                    .text_color(theme::get().accent)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .child("Show in folder")
                                    .tooltip(widgets::Tooltip::text(
                                        "Show in folder",
                                        "",
                                        target.display().to_string(),
                                    ))
                                    .on_click({
                                        let target = target.clone();
                                        move |_, _, cx| cx.reveal_path(&target)
                                    }),
                            )
                            // Open with the system's app for the type: the
                            // edit step of Extract, edit, Use in mod, without
                            // finding the copy in a file manager first.
                            .child(div().text_color(theme::get().muted).child("·"))
                            .child(
                                div()
                                    .id("preview-open-extracted")
                                    .text_color(theme::get().accent)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .child("Open copy")
                                    .tooltip(widgets::Tooltip::text(
                                        "Open copy",
                                        "",
                                        "Opens the extracted copy in your default app for the file type",
                                    ))
                                    .on_click(move |_, _, cx| cx.open_with_system(&target)),
                            )
                            // The next step, now that its button is there.
                            .child(
                                div()
                                    .text_color(theme::get().muted)
                                    .child("· edit it, then Use in mod"),
                            )
                            ,
                    )
                },
            );
        let staged = self.project.files.contains_key(&path);
        if layers.len() > 1 {
            // Folded to one line by default: open, five patch archives took
            // a quarter of the pane above the preview.
            let archive = |a: u16| {
                let name = &install.archives()[a as usize];
                name.strip_suffix(".rda").unwrap_or(name).to_owned()
            };
            let (heading, open) =
                widgets::collapsible("preview-layers", "Archive layers", false, window, cx);
            body = body.child(
                heading.child(
                    div()
                        .ml_2()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(theme::get().muted)
                        // The archive in use ends the path line above;
                        // folded, this says how many and which wins.
                        .child(widgets::meta_line(&format!(
                            "In {} archives · the last one wins",
                            layers.len()
                        ))),
                ),
            );
            if open {
                // One line each: the archive's name, never wrapped, its
                // size in a column, the loaded one marked.
                for (k, layer) in layers.iter().enumerate() {
                    let last = k + 1 == layers.len();
                    body = body.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl(px(14.))
                            .h(px(20.))
                            .text_size(px(11.))
                            .text_color(if last {
                                theme::get().text
                            } else {
                                theme::get().muted
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .child(archive(layer.archive)),
                            )
                            .when(last, |d| {
                                d.child(div().text_color(theme::get().accent).child("in use"))
                            })
                            .child(div().w(px(64.)).flex_none().text_right().child(
                                if layer.is_tombstone() {
                                    "deleted".into()
                                } else {
                                    human(layer.raw_size)
                                },
                            )),
                    );
                }
            }
        }
        if staged {
            body = body.child(
                div()
                    .text_color(theme::get().accent)
                    .text_size(px(11.))
                    .child("Staged in the mod project"),
            );
        }
        let preview = self.browser.preview.as_ref().filter(|p| p.file == file);
        // No "Preview" heading: the tab says it, and the image follows the
        // actions with a rule above.
        body = body.child(div().mt_1().border_t_1().border_color(theme::get().border));
        body = match preview.map(|p| (&p.content, p.size)) {
            None => body.child(div().text_color(theme::get().muted).child("Loading…")),
            Some((
                Content::Texture {
                    image,
                    summary,
                    mips,
                    mip,
                    dims,
                    light,
                },
                _,
            )) => body
                // What the image is, then how to look at it: one channel at
                // a time (normal and metal maps hide in them) and any mip.
                .child(
                    // The four groups apart by a rule with 12 px each side
                    // (gap_2 and the rule's margin).
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .flex_wrap()
                        // Fit's zoom in the facts, the one view whose scale no
                        // segment shows; at 1:1 or a scroll zoom the segment
                        // says it, and the facts said it twice.
                        .child({
                            let facts = if self.browser.native {
                                summary.to_string()
                            } else {
                                let fit = fit_scale((dims.0 >> *mip).max(1), window.scale_factor());
                                format!("{summary} · {:.0}%", fit * 100.)
                            };
                            div()
                                .text_size(px(11.))
                                .text_color(theme::get().muted)
                                .child(widgets::meta_line(&facts))
                        })
                        .child(div().flex_1())
                        // On what, and fitted or at its own pixels, beside
                        // the channel and mip: one row of view controls.
                        .child(widgets::segmented(
                            "preview-backdrop",
                            [
                                (Backdrop::Auto, "Auto".into()),
                                (Backdrop::Dark, "Dark".into()),
                                (Backdrop::Light, "Light".into()),
                                (Backdrop::Checker, "Checker".into()),
                            ],
                            &self.browser.backdrop,
                            cx,
                            // The grid's tiles follow it, in the other pane.
                            |this, b, _, cx| {
                                this.browser.backdrop = b;
                                this.notify_pane(crate::editor::Pane::Browser, cx);
                            },
                        ))
                        // Rules between the groups: one segmented style
                        // four times over read as one long control.
                        .child(widgets::toolbar_separator())
                        // Fit, 1:1 and the wheel's zoom, one control: the
                        // third segment always names a zoom (the live one, or
                        // the last the wheel set) and a click goes back to it.
                        // Split into a control and a filled readout, the
                        // readout looked pressed but did nothing. Each label
                        // keeps one width, so the right-aligned toolbar does
                        // not shift under the pointer.
                        .child({
                            let zoom = self.browser.zoom();
                            let mode = match (self.browser.native, zoom) {
                                (false, _) => 0u8,
                                (true, z) if (z - 1.).abs() < 0.01 => 1,
                                _ => 2,
                            };
                            // Until the wheel sets one, the step past Fit: a
                            // fixed 200% was Fit's own zoom on every icon up
                            // to 450 px, and the segment did nothing.
                            let fit = fit_scale((dims.0 >> *mip).max(1), window.scale_factor());
                            let wheel = if mode == 2 {
                                zoom
                            } else if self.browser.wheel_zoom > 0. {
                                self.browser.wheel_zoom
                            } else {
                                Browser::zoom_step(fit, true)
                            };
                            // A number only while it is the view: beside the
                            // facts' live zoom, a recalled "300%" under Fit
                            // read as a second current zoom. "Zoom" is about
                            // as wide, so the toolbar keeps its place.
                            let label = if mode == 2 {
                                format!("{:.0}%", wheel * 100.)
                            } else {
                                "Zoom".to_owned()
                            };
                            let label = if label.chars().count() < 4 {
                                format!("\u{2007}{label}")
                            } else {
                                label
                            };
                            widgets::segmented(
                                "preview-zoom",
                                [(0u8, "Fit".into()), (1, "1:1".into()), (2, label.into())],
                                &mode,
                                cx,
                                // Reloaded: the texture is enlarged for the view.
                                move |this, mode, _, cx| {
                                    let b = &mut this.browser;
                                    b.native = mode != 0;
                                    b.zoom = if mode == 2 { wheel } else { 1. };
                                    this.reload_preview(cx);
                                },
                            )
                            .tooltip(widgets::Tooltip::text(
                                "Zoom",
                                "Scroll",
                                "Fit, 1:1, or back to the last scroll zoom. Scroll over the picture to zoom from 25% to 800%",
                            ))
                        })
                        .child(widgets::toolbar_separator())
                        .child(widgets::segmented(
                            "preview-channel",
                            [
                                (Channel::Rgb, "RGB".into()),
                                (Channel::R, "R".into()),
                                (Channel::G, "G".into()),
                                (Channel::B, "B".into()),
                                (Channel::A, "A".into()),
                            ],
                            &self.browser.channel,
                            cx,
                            |this, channel, _, cx| {
                                this.browser.channel = channel;
                                this.reload_preview(cx);
                            },
                        ))
                        .child(widgets::toolbar_separator())
                        // Wide enough for "Mip 12 · 4096×4096": sized by its
                        // label, it moved the toolbar between textures.
                        .child(
                            widgets::dropdown(
                                "preview-mip",
                                (0..*mips)
                                    .map(|m| {
                                        let (w, h) = ((dims.0 >> m).max(1), (dims.1 >> m).max(1));
                                        (m, format!("Mip {m} · {w}×{h}").into())
                                    })
                                    .collect(),
                                mip,
                                window,
                                cx,
                                |this, m, _, cx| {
                                    this.browser.mip = Some(m);
                                    this.reload_preview(cx);
                                },
                            )
                            .min_w(px(150.)),
                        ),
                )
                .child({
                    let (w, h) = ((dims.0 >> *mip).max(1), (dims.1 >> *mip).max(1));
                    if self.browser.native {
                        // Its own pixels at the zoom, scrolled both ways
                        // when larger than the pane: edges and compression
                        // read there. The wheel zooms, as in image viewers;
                        // Shift-wheel and the scrollbars pan.
                        let z = self.browser.zoom();
                        // The same stage as Fit: to the pane's bottom, on
                        // the canvas colour, the picture centred both ways
                        // while it fits; larger, it scrolls. Sized by the
                        // picture alone, a zoom lost the stage and sat at
                        // the pane's top.
                        let [top, bottom] = self.preview_span_unscrolled();
                        let room = if bottom > top {
                            bottom - top - 16.
                        } else {
                            480.
                        };
                        div()
                            .relative()
                            .w_full()
                            .h(px(room.max(320.)))
                            .child(span_probe(&self.browser.preview_span, 0, window))
                            .child(backdrop(match (self.browser.backdrop, *light) {
                                (Backdrop::Auto, true) => Backdrop::Dark,
                                (b, l) => b.resolve(l),
                            }))
                            .child(
                                div()
                                    .id("preview-native")
                                    .absolute()
                                    .inset_0()
                                    .overflow_scroll()
                                    .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
                                        let dy = match e.delta {
                                            ScrollDelta::Pixels(p) => f32::from(p.y),
                                            ScrollDelta::Lines(p) => p.y,
                                        };
                                        if dy == 0. || e.modifiers.shift {
                                            return;
                                        }
                                        cx.stop_propagation();
                                        let b = &mut this.browser;
                                        b.zoom = Browser::zoom_step(b.zoom(), dy > 0.);
                                        b.wheel_zoom = b.zoom;
                                        this.reload_preview(cx);
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .min_w_full()
                                            .min_h_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                div()
                                                    .relative()
                                                    .flex_shrink_0()
                                                    .w(px(w as f32 * z))
                                                    .h(px(h as f32 * z))
                                                    .child(backdrop(
                                                        self.browser.backdrop.resolve(*light),
                                                    ))
                                                    .child(img(image.clone()).size_full()),
                                            ),
                                    ),
                            )
                            // How to move about, where the moving is, as the
                            // model preview's pill says it: on the zoom
                            // control's tooltip alone the wheel went unfound.
                            .child(
                                widgets::hint_pill("Scroll: zoom · Shift-scroll: pan")
                                    .absolute()
                                    .left_2()
                                    .bottom_2(),
                            )
                            .into_any_element()
                    } else {
                        // Fitted at a known size, up to twice its pixels and
                        // 900 px wide: sized by the pane alone it ran out of
                        // its backdrop, and a 96 px strip went soft.
                        // The backdrop spans the pane, to the controls'
                        // right edge; short of it, it read as a cut panel.
                        let scale = fit_scale(w, window.scale_factor());
                        let (fw, fh) = (w as f32 * scale, h as f32 * scale);
                        // To the pane's bottom (at least 320 px), the image
                        // centred in it: a 96 px strip at the top of an
                        // empty pane, or a canvas stopping short of the
                        // bottom, looked unfinished; image viewers give the
                        // canvas the room. Measured as drawn, so a frame
                        // after a resize settles.
                        // 16 px short of the pane's foot: at 8 the stage
                        // overran the pane's own padding by a few pixels and
                        // a scrollbar ran the pane's height with nothing to
                        // scroll.
                        let [top, bottom] = self.preview_span_unscrolled();
                        let room = if bottom > top {
                            bottom - top - 16.
                        } else {
                            480.
                        };
                        // The wheel here leaves Fit for a zoom one step from
                        // the fitted scale, as image viewers do.
                        div()
                            .id("preview-fit")
                            .relative()
                            .w_full()
                            .h(px(fh.max(room.max(320.))))
                            .on_scroll(cx.listener(move |this, e: &ScrollEvent, _, cx| {
                                let dy = match e.delta {
                                    ScrollDelta::Pixels(p) => f32::from(p.y),
                                    ScrollDelta::Lines(p) => p.y,
                                };
                                if dy == 0. {
                                    return;
                                }
                                cx.stop_propagation();
                                let b = &mut this.browser;
                                b.native = true;
                                b.zoom = Browser::zoom_step(scale, dy > 0.);
                                b.wheel_zoom = b.zoom;
                                this.reload_preview(cx);
                                cx.notify();
                            }))
                            .child(span_probe(&self.browser.preview_span, 0, window))
                            .flex()
                            .items_center()
                            .justify_center()
                            // Auto's light backdrop is a matte just around the
                            // image, on the dark canvas: across the whole
                            // pane it was the brightest surface in the app.
                            .child(backdrop(match (self.browser.backdrop, *light) {
                                (Backdrop::Auto, true) => Backdrop::Dark,
                                (b, l) => b.resolve(l),
                            }))
                            .child(
                                div()
                                    .when(self.browser.backdrop == Backdrop::Auto && *light, |d| {
                                        d.p(px(24.)).rounded(px(4.)).bg(rgb(LIGHT)).shadow_lg()
                                    })
                                    .child(
                                        img(image.clone())
                                            .w(px(fw))
                                            .max_w_full()
                                            .h(px(fh))
                                            .object_fit(ObjectFit::Contain),
                                    ),
                            )
                            // At Fit too, as the model preview shows its keys
                            // at rest: only once zoomed, the wheel stayed
                            // hidden until stumbled on.
                            .child(
                                widgets::hint_pill("Scroll: zoom · Shift-scroll: pan")
                                    .absolute()
                                    .left_2()
                                    .bottom_2(),
                            )
                            .into_any_element()
                    }
                }),
            Some((Content::Image(image), _)) => body.child(
                img(image.clone())
                    .max_w(px(1024.))
                    .w_full()
                    .object_fit(ObjectFit::Contain)
                    .bg(rgb(crate::colors::IMAGE_BG)),
            ),
            // The caption rides in the view's own row, beside Find.
            Some((Content::Text { code, summary }, _)) => {
                body.child(self.code_box(code.clone(), Some(summary.clone().into()), window, cx))
            }
            Some((Content::Hex(hex), size)) => body
                .child(info_row(
                    "Shown",
                    format!(
                        "first {} of {}",
                        human(HEX_BYTES.min(size) as u64),
                        human(size as u64)
                    ),
                ))
                .child(mono(hex.clone())),
            Some((
                Content::Model {
                    summary, source, ..
                },
                _,
            )) => {
                let source = source.clone().filter(|_| self.browser.model_source);
                let has_source = matches!(
                    self.browser.preview.as_ref().map(|p| &p.content),
                    Some(Content::Model {
                        source: Some(_),
                        ..
                    })
                );
                let body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::get().muted)
                                .child(widgets::meta_line(summary)),
                        )
                        .child(div().flex_1())
                        // A .cfg is also the XML a modder edits: both, one
                        // click apart.
                        .when(has_source, |d| {
                            d.child(widgets::segmented(
                                "preview-model-mode",
                                [(false, "Model".into()), (true, "Source".into())],
                                &self.browser.model_source,
                                cx,
                                |this, source, _, cx| {
                                    this.browser.model_source = source;
                                    this.notify_pane(crate::editor::Pane::Preview, cx);
                                },
                            ))
                        }),
                );
                match source {
                    Some(code) => body.child(self.code_box(code, None, window, cx)),
                    None => {
                        // To the pane's bottom, as the texture canvas is.
                        let [top, bottom] = self.preview_span_unscrolled();
                        let room = if bottom > top {
                            bottom - top - 8.
                        } else {
                            480.
                        };
                        body.child(
                            div()
                                .relative()
                                .w_full()
                                .h(px(room.max(320.)))
                                .rounded(px(3.))
                                .overflow_hidden()
                                .bg(rgb(gpui_editor_kit::model_view::BACKDROP))
                                .child(span_probe(&self.browser.preview_span, 0, window))
                                .child(self.browser.model.render(
                                    "preview-model-view",
                                    |e: &mut Editor| &mut e.browser.model,
                                    |e, cx| e.notify_pane(crate::editor::Pane::Preview, cx),
                                    window,
                                    cx,
                                ))
                                // How to look around, where the looking is, in
                                // the island viewport's key pill and its keys.
                                .child(
                                    widgets::hint_pill(
                                        "Left- or right-drag: orbit · Middle-drag: pan · Scroll: zoom · Double-click: frame",
                                    )
                                    .absolute()
                                    .left_2()
                                    .bottom_2(),
                                ),
                        )
                    }
                }
            }
            Some((Content::Island, _)) => body.child(
                div()
                    .text_color(theme::get().muted)
                    .child("Island archive. Open it to sculpt, paint and move props."),
            ),
            Some((Content::UiAsset(magic), _)) => {
                body.child(div().text_color(theme::get().muted).child(format!(
                    "Compiled Phoenix asset ({magic}). {}",
                    "Open it in the UI workspace."
                )))
            }
            Some((Content::Error(e), _)) => {
                body.child(div().text_color(theme::get().error).child(e.clone()))
            }
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::get().panel)
            .child(span_probe(&self.browser.preview_span, 1, window))
            // No header of its own: the tab already says Asset preview,
            // and the section below says Preview again.
            .child(
                gpui_editor_kit::widgets::vscroll(
                    "preview-scroll",
                    &self.scrolls.get("preview"),
                    body,
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }
}

impl Editor {
    /// Source text in a box to the pane's bottom, as the texture canvas
    /// is: the view scrolls its own lines, the pane only the header.
    fn code_box(
        &self,
        code: Arc<Code>,
        caption: Option<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) -> Div {
        let [top, bottom] = self.preview_span_unscrolled();
        let room = if bottom > top {
            bottom - top - 8.
        } else {
            480.
        };
        div()
            .relative()
            .w_full()
            .h(px(room.max(320.)))
            .overflow_hidden()
            .child(span_probe(&self.browser.preview_span, 0, window))
            .child(code_view("preview-code", code, caption, window, cx))
    }
}

fn mono(text: String) -> Div {
    div()
        .font_family(theme::MONO_FONT)
        .text_size(px(11.))
        .whitespace_normal()
        .child(text)
}

/// What a content-pack folder code is: `dlc03` is DLC 3, `cdlc01` cosmetic
/// DLC 1. The game data has no display names for them on disk.
pub(crate) fn pack_name(folder: &str) -> Option<String> {
    let (kind, n) = if let Some(n) = folder.strip_prefix("cdlc") {
        ("Cosmetic DLC", n)
    } else {
        ("DLC", folder.strip_prefix("dlc")?)
    };
    let n: u32 = n.parse().ok()?;
    Some(format!("{kind} {n}"))
}

#[cfg(test)]
mod tests {
    use super::{Room, folder_note, hex_dump, human, kind, owning_cfg, pack_name};

    /// A column `chars` wide where every char is 1 px.
    fn room(chars: usize) -> Room {
        Room {
            px: chars as f32,
            ascii: [1.; 128],
            average: 1.,
        }
    }

    #[test]
    fn hex_dump_pads_short_rows_and_masks_control_bytes() {
        let dump = hex_dump(b"PHXWI\x1c\x09\x01");
        assert_eq!(
            dump,
            "00000000  50 48 58 57 49 1c 09 01                          PHXWI...\n"
        );
    }

    #[test]
    fn kinds_follow_extension_and_ui_folder() {
        assert_eq!(kind("data/a/b.dds"), "texture");
        assert_eq!(kind("data/a/rdm/b_lod0.rdm"), "model");
        assert_eq!(
            owning_cfg("data/trees/olive/rdm/olive_01_lod2.rdm"),
            Some((
                "data/trees/olive/olive_01.cfg".to_owned(),
                "data/trees/olive/rdm/olive_01",
                2
            ))
        );
        assert_eq!(
            owning_cfg("data/rocks/rock_06_a.rdm"),
            Some((
                "data/rocks/rock_06_a.cfg".to_owned(),
                "data/rocks/rock_06_a",
                0
            ))
        );
        assert_eq!(kind("data/x/islands/i/i.a7m"), "island");
        assert_eq!(kind("data/ui/studio/generated/00214ce2-8775"), "ui");
        assert_eq!(kind("data/ui/fonts/a.ttf"), "binary");
        assert_eq!(human(1536), "1.5 KB");
    }

    #[test]
    fn one_channel_shows_as_opaque_grey() {
        use super::Channel;
        let texel = [10u8, 20, 30, 40];
        let mut all = texel.to_vec();
        Channel::Rgb.apply(&mut all);
        assert_eq!(all, texel);
        for (channel, v) in [
            (Channel::R, 10),
            (Channel::G, 20),
            (Channel::B, 30),
            (Channel::A, 40),
        ] {
            let mut px = texel.to_vec();
            channel.apply(&mut px);
            assert_eq!(px, [v, v, v, 255]);
        }
    }

    #[test]
    fn list_folders_fit_whole_or_name_the_telling_folder() {
        let note = |p| folder_note(p, &room(34), None).unwrap();
        assert_eq!(
            note("data/graphics/units/legionary_officer_lean_m_01/maps/a.dds"),
            "legionary_officer_lean_m_01"
        );
        // Both folders where the last one tells something.
        assert_eq!(
            note("data/props/trees/olive_trees/olive_01.cfg"),
            "trees/olive_trees"
        );
        // Too long for both: the folder the grid names, never cut inside.
        assert_eq!(
            note("data/graphics/units/colosseo_gladiator_muscular_m_01/maps/a.dds"),
            "colosseo_gladiator_muscular_m_01"
        );
        assert_eq!(note("data/a.dds"), "data");
        // Narrower than even that folder: cut once, in its middle, keeping
        // its end, so a cell holds one ellipsis.
        let cut = folder_note(
            "data/graphics/units/legionary_officer_lean_m_01/maps/a.dds",
            &room(20),
            None,
        )
        .unwrap();
        assert_eq!(cut, "legionary_…lean_m_01");
        assert!(cut.chars().count() <= 20);
        assert_eq!(cut.matches('…').count(), 1);
        let cut = folder_note(
            "data/units/calidus_legionary_lean_m_01/maps/a.dds",
            &room(27),
            None,
        )
        .unwrap();
        // Room for the name: whole, not cut.
        assert_eq!(cut, "calidus_legionary_lean_m_01");
    }

    #[test]
    fn same_named_neighbours_show_where_their_paths_part() {
        let (base, dlc) = (
            "data/base/graphics/trees/olive_trees/olive_01.cfg",
            "data/dlc01/graphics/trees/olive_trees/olive_01.cfg",
        );
        assert_eq!(
            folder_note(base, &room(34), Some(dlc)).unwrap(),
            "base/…/olive_trees"
        );
        assert_eq!(
            folder_note(dlc, &room(34), Some(base)).unwrap(),
            "dlc01/…/olive_trees"
        );
        // Narrow: the root that tells them apart stays, the leaf is cut.
        let narrow = folder_note(base, &room(14), Some(dlc)).unwrap();
        assert!(narrow.starts_with("base/"), "{narrow}");
        assert!(narrow.chars().count() <= 14, "{narrow}");
        // Neighbours whose notes already differ keep the usual note.
        assert_eq!(
            folder_note(base, &room(34), Some("data/x/rocks/olive_01.cfg")).unwrap(),
            "trees/olive_trees"
        );
    }

    #[test]
    fn clear_texels_take_their_neighbours_colour() {
        use super::bleed_clear;
        // 2×2: one red opaque texel, the rest clear black.
        let mut px = [[200, 10, 10, 255], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]];
        bleed_clear(px.as_flattened_mut(), 2, 2);
        assert_eq!(
            px,
            [
                [200, 10, 10, 255],
                [200, 10, 10, 0],
                [200, 10, 10, 0],
                [200, 10, 10, 0]
            ]
        );
    }

    #[test]
    fn dark_art_on_transparency_asks_for_a_light_backdrop() {
        use super::wants_light;
        // Brown strokes on clear texels: light. White icon: dark. Opaque
        // dark map (a normal map's shadows): no transparency, dark.
        let brown_on_clear = [
            [90, 60, 40, 255],
            [0, 0, 0, 0],
            [0, 0, 0, 0],
            [80, 55, 35, 255],
        ];
        let white_on_clear = [
            [250, 250, 250, 255],
            [0, 0, 0, 0],
            [0, 0, 0, 0],
            [240, 240, 240, 255],
        ];
        let opaque_dark = [[30, 30, 30, 255]; 4];
        assert!(wants_light(brown_on_clear.as_flattened()));
        assert!(!wants_light(white_on_clear.as_flattened()));
        assert!(!wants_light(opaque_dark.as_flattened()));
    }

    #[test]
    fn grey_thumbnails_stretch_to_the_full_range() {
        use super::stretch_grey;
        let mut px = vec![100, 100, 100, 255, 110, 110, 110, 255];
        assert!(stretch_grey(&mut px));
        assert_eq!(px, [0, 0, 0, 255, 255, 255, 255, 255]);
        let mut flat = vec![90, 90, 90, 255, 90, 90, 90, 255];
        assert!(!stretch_grey(&mut flat));
        assert_eq!(flat[0], 90);
    }

    #[test]
    fn one_channel_textures_show_grey() {
        use super::one_channel_as_grey;
        use gpui_editor_kit::texture::{Block, Format};
        let mut px = vec![200, 0, 0, 255];
        one_channel_as_grey(Format::Block(Block::Bc4), &mut px);
        assert_eq!(px, [200, 200, 200, 255]);
        let mut px = vec![200, 0, 0, 255];
        one_channel_as_grey(Format::Block(Block::Bc7), &mut px);
        assert_eq!(px, [200, 0, 0, 255]);
    }

    #[test]
    fn tile_notes_keep_the_folder_that_tells_files_apart() {
        use super::tile_note;
        // The unit's folder above the generic "maps", whole: the tile cuts it.
        assert_eq!(tile_note("data/x/caeso_unit/maps/a.dds"), "caeso_unit");
        assert_eq!(
            tile_note("data/x/calidus_auxilia_lean_m_01/maps/a.dds"),
            "calidus_auxilia_lean_m_01"
        );
        // A telling last folder stays itself.
        assert_eq!(
            tile_note("data/ui/city_status/00_cityicons/a.dds"),
            "00_cityicons"
        );
    }

    #[test]
    fn pack_codes_read_as_their_kind() {
        assert_eq!(pack_name("dlc03").as_deref(), Some("DLC 3"));
        assert_eq!(pack_name("cdlc01").as_deref(), Some("Cosmetic DLC 1"));
        assert_eq!(pack_name("data"), None);
        assert_eq!(pack_name("dlcx"), None);
    }
}
