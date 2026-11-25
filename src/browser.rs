// Owns the asset browser: the folder tree over the layered install, search,
// selection, and the preview pane (textures, text, hex, archive layers).
// Opening an island or a compiled UI asset hands off to its workspace.
use crate::editor::{Editor, empty_pane};
use anno_core::files::{self, Group, Kind, View as Shows};
use anno_core::game::Module;
use anno_formats::model::Model;
use anno_formats::vfs::{FileId, FolderId, Install, Node};
use gpui_editor_kit::{
    code::{Code, Language, code_view},
    field::{EditEvent, Filter, TextEdit, text_field},
    gpu::{self, Mesh, MeshData},
    gpui::{prelude::*, *},
    icon::Icon,
    math::{self, Mat4, Vec3},
    model_view::ModelView,
    texture, theme,
    tree::{self, Nav, Row, Tree},
    widgets::{self, hint, info_row},
};
use std::{borrow::Cow, sync::Arc};

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

    /// The chip that lists `group`.
    fn of(group: Group) -> Types {
        match group {
            Group::Textures => Types::Textures,
            Group::Models => Types::Models,
            Group::Islands => Types::Islands,
            Group::Ui => Types::Ui,
            Group::Text => Types::Text,
        }
    }

    /// Whether the chip lists a file of `kind`; All lists every file, of
    /// a kind or not.
    fn admits(self, kind: Option<&Kind>) -> bool {
        self == Types::All || kind.is_some_and(|k| Types::of(k.group) == self)
    }
}

/// The open game's file kinds, its own then `files::GENERIC`, as the list
/// classifies them: every file of the install (~440k on 117) goes through
/// `get` on the first type shown as a tree, so extensions are packed into
/// a `u64` and binary-searched instead of walking each kind's list.
/// Answers as `files::kind_of` does (tested).
#[derive(Default)]
struct Kinds {
    /// (packed extension, index into `list`), sorted and unique: the first
    /// kind to name an extension owns it.
    exts: Vec<(u64, u16)>,
    /// Folder markers in list order.
    folders: Vec<(&'static str, u16)>,
    list: Vec<&'static Kind>,
}

impl Kinds {
    fn new(module: &dyn Module) -> Kinds {
        let list: Vec<&'static Kind> = module.files().iter().chain(files::GENERIC).collect();
        let mut exts = Vec::new();
        let mut folders = Vec::new();
        for (i, k) in list.iter().enumerate() {
            exts.extend(k.exts.iter().filter_map(|e| Some((pack(e)?, i as u16))));
            folders.extend(k.folder.map(|f| (f, i as u16)));
        }
        // By extension, then list order: dedup keeps the earliest kind.
        exts.sort_unstable();
        exts.dedup_by_key(|(e, _)| *e);
        Kinds {
            exts,
            folders,
            list,
        }
    }

    fn get(&self, path: &str) -> Option<&'static Kind> {
        let by_ext = pack(files::extension(path))
            .and_then(|e| self.exts.binary_search_by_key(&e, |&(x, _)| x).ok())
            .map(|i| self.exts[i].1);
        let by_folder = (self.folders.iter())
            .find(|(f, _)| path.contains(f))
            .map(|&(_, i)| i);
        let i = match (by_ext, by_folder) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b)?,
        };
        Some(self.list[i as usize])
    }
}

/// An extension of up to eight bytes, lower-cased, as one integer; `None`
/// for longer ones, which no kind has, or none at all.
fn pack(ext: &str) -> Option<u64> {
    if ext.is_empty() || ext.len() > 8 {
        return None;
    }
    let mut b = [0u8; 8];
    b[..ext.len()].copy_from_slice(ext.as_bytes());
    Some(u64::from_le_bytes(b.map(|c| c.to_ascii_lowercase())))
}

/// Whether a previewed file offers "Place on an island": a drawn model the
/// island's Place tool takes (a `.cfg`), in a game that edits islands and
/// their objects.
fn offers_place(module: &dyn Module, path: &str, shown: Option<&Content>) -> bool {
    module.islands().is_some_and(|i| i.places_objects())
        && module.database().is_some()
        && files::kind_of(module, path).is_some_and(|k| k.id == files::CFG.id)
        && matches!(shown, Some(Content::Model { .. }))
}

/// The workspace that opens `path` in `module`: its kind's view, where the
/// game has that capability. `None` previews it in Assets.
pub(crate) fn editor_for(module: &dyn Module, path: &str) -> Option<crate::editor::Workspace> {
    use crate::editor::Workspace;
    match files::kind_of(module, path)?.view? {
        Shows::Island if module.islands().is_some() => Some(Workspace::Island),
        Shows::Map if module.islands().and_then(|i| i.maps()).is_some() => Some(Workspace::Map),
        Shows::Screen if module.ui().is_some() => Some(Workspace::Ui),
        _ => None,
    }
}

/// A model's cfg, or a prop's prp: what the Model workspace opens.
pub(crate) fn is_model(path: &str) -> bool {
    path.ends_with(".cfg") || path.ends_with(".prp")
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
    module: &dyn Module,
    install: &Install,
    file: FileId,
    side: u32,
) -> (Option<Arc<RenderImage>>, Option<SharedString>, bool) {
    let path = install.file_path(file);
    let Some(kind) = files::kind_of(module, &path) else {
        return (None, None, false);
    };
    let Some(bytes) = install.read_file(file).ok() else {
        return (None, None, false);
    };
    let (mut flat, mut light) = (false, false);
    let (decoded, size, format) = match kind.view {
        Some(Shows::Texture) => {
            let Ok(dds) = texture::Dds::parse(&bytes) else {
                return (None, None, false);
            };
            let format = match dds.format {
                texture::Format::Block(b) => format!("{b:?}"),
                f => format!("{f:?}"),
            }
            .to_uppercase();
            let decoded = dds.decode(&bytes, dds.mip_for(side)).ok().map(|mut i| {
                one_channel_as_grey(dds.format, &mut i.rgba);
                // A thumbnail only: heights and masks often use a narrow
                // band, and grey at one level read as an unloaded tile. The
                // preview shows the true values.
                if dds.format == texture::Format::Block(texture::Block::Bc4) {
                    flat = !stretch_grey(&mut i.rgba);
                }
                i
            });
            // As the preview writes it, so one size reads one way.
            (decoded, format!("{}×{}", dds.width, dds.height), format)
        }
        // 2070's masks, and cursors.
        Some(Shows::Picture) => match picture(kind, &bytes) {
            Ok((i, _)) => {
                let size = format!("{}×{}", i.width, i.height);
                let ext = files::extension(&path);
                (Some(i), size, ext.to_uppercase())
            }
            Err(_) => return (None, None, false),
        },
        _ => return (None, None, false),
    };
    let image = decoded.map(|mut i| {
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
    /// The open game's kinds, shared with the list's row closures.
    kinds: Arc<Kinds>,
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

    pub fn attach(&mut self, install: &Arc<Install>, module: &dyn Module) {
        self.install = Some(install.clone());
        self.kinds = Arc::new(Kinds::new(module));
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
        let mut kinds = vec![0u8; 1];
        let mut counts = [0usize; 6];
        for f in (0..install.file_count() as u32).map(FileId) {
            let Some(kind) = self.kinds.get(&install.file_path(f)) else {
                continue;
            };
            let t = Types::of(kind.group);
            let bits = t.bit();
            counts[t as usize] += 1;
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
                && types.admits(self.kinds.get(&install.file_path(f)))
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
                if !self.flat() && self.types.admits(self.kinds.get(&install.file_path(file))) =>
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
        if !self.types.admits(self.kinds.get(path)) {
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
fn folder_counts(
    install: &Install,
    table: &Kinds,
    f: FolderId,
    types: Types,
    kinds: &[u8],
) -> String {
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
                .filter(|&&file| types.admits(table.get(&install.file_path(file))))
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
    module: &dyn Module,
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
    let kind = files::kind_of(module, &path);
    let content = match kind.and_then(|k| k.view.map(|v| (k, v))) {
        Some((kind, view @ (Shows::Texture | Shows::Picture))) => match (|| -> anyhow::Result<_> {
            // The facts in one line, as the list names them; the raw DXGI
            // code was engine detail beside them.
            let (mut image, dims, mips, mip, facts) = if view == Shows::Picture {
                let (image, facts) = picture(kind, &bytes)?;
                let dims = (image.width, image.height);
                (image, dims, 1, 0, facts)
            } else {
                let dds = texture::Dds::parse(&bytes)?;
                // The asked-for mip, else the largest within 1024 px.
                let mip = mip.map_or_else(|| dds.mip_for(1024), |m| m.min(dds.mips - 1));
                let mut image = dds.decode(&bytes, mip)?;
                one_channel_as_grey(dds.format, &mut image.rgba);
                let format = match dds.format {
                    texture::Format::Block(b) => format!("{b:?}").to_uppercase(),
                    f => format!("{f:?}"),
                };
                let facts = format!("{format} · {} mips", dds.mips);
                (image, (dds.width, dds.height), dds.mips, mip, facts)
            };
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
            Ok(Content::Texture {
                light,
                image: image.render_image(),
                mips,
                mip,
                dims,
                summary: format!("{}×{} · {facts} · {}", dims.0, dims.1, human(size as u64)),
            })
        })() {
            Ok(content) => content,
            Err(e) => Content::Error(format!("{e:#}")),
        },
        // Without the capability, the file shows as its bytes.
        Some((_, Shows::Island)) if module.islands().is_some() => Content::Island,
        Some((_, Shows::Model)) => load_model(module, install, &path, &bytes),
        Some((kind, Shows::Image)) => Content::Image(Arc::new(Image::from_bytes(
            if kind.id == "png" {
                ImageFormat::Png
            } else {
                ImageFormat::Jpeg
            },
            bytes,
        ))),
        Some((_, Shows::Screen)) if module.ui().is_some() => {
            Content::UiAsset(String::from_utf8_lossy(&bytes[..bytes.len().min(5)]).into_owned())
        }
        _ => match readable(&bytes) {
            Some(text) => text_content(&path, text, size),
            _ => Content::Hex(hex_dump(&bytes[..bytes.len().min(HEX_BYTES)])),
        },
    };
    Preview {
        file,
        size,
        content,
    }
}

/// A picture of `kind` and what it is: the game's own format through the
/// kind's decoder, else a Windows cursor (`.cur`) or animated one (`.ani`),
/// of which the first frame shows. Those read through the `image` crate,
/// which only the shell links.
fn picture(kind: &Kind, bytes: &[u8]) -> anyhow::Result<(texture::Texture, String)> {
    use anyhow::{Context, ensure};
    if let Some(decode) = kind.decode {
        return decode(bytes);
    }
    let cursor = |bytes: &[u8]| -> anyhow::Result<texture::Texture> {
        let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Ico)?;
        let image = image.to_rgba8();
        texture::Texture::from_rgba(image.width(), image.height(), image.into_raw())
    };
    // A cursor's first directory entry holds its hotspot where an icon's
    // holds planes and bit depth.
    let hotspot = |bytes: &[u8]| -> String {
        let at = |i: usize| {
            bytes
                .get(i..i + 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
        };
        match (at(2), at(10), at(12)) {
            (Some(2), Some(x), Some(y)) => format!("cursor, hotspot {x}, {y}"),
            _ => "icon".to_owned(),
        }
    };
    match kind.id {
        "cur" => Ok((cursor(bytes)?, hotspot(bytes))),
        "ani" => {
            // RIFF "ACON": a `LIST` of type `fram` holds one `icon` chunk
            // per frame, each a whole .cur or .ico file.
            ensure!(
                bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"ACON",
                "not an animated cursor"
            );
            fn chunks(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
                std::iter::from_fn(move || {
                    let id: [u8; 4] = data.get(..4)?.try_into().ok()?;
                    let len = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?) as usize;
                    let body = data.get(8..8 + len)?;
                    // Chunks start on even offsets.
                    data = data.get(8 + len + (len & 1)..).unwrap_or_default();
                    Some((id, body))
                })
            }
            let frames: Vec<&[u8]> = chunks(&bytes[12..])
                .filter(|(id, body)| id == b"LIST" && body.starts_with(b"fram"))
                .flat_map(|(_, body)| chunks(&body[4..]))
                .filter(|(id, _)| id == b"icon")
                .map(|(_, body)| body)
                .collect();
            let first = frames.first().context("animated cursor has no frames")?;
            let facts = format!(
                "animated {} · {} · first shown",
                hotspot(first),
                crate::project::plural(frames.len(), "frame", "frames"),
            );
            Ok((cursor(first)?, facts))
        }
        _ => anyhow::bail!("no picture decoder for {}", kind.label),
    }
}

/// A texture's or picture's texels as PNG: mip `mip` (a texture's; a
/// picture has one) in `channel`, unenlarged, so what the preview shows is
/// what a paint program opens. Pixels only: DDS out stays a byte copy
/// (Extract), since nothing here encodes block formats.
fn png_of(
    module: &dyn Module,
    install: &Install,
    file: FileId,
    mip: u32,
    channel: Channel,
) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    let path = install.file_path(file);
    let kind = files::kind_of(module, &path);
    let bytes = install.read_file(file)?;
    let mut image = match (kind, kind.and_then(|k| k.view)) {
        (_, Some(Shows::Texture)) => {
            let dds = texture::Dds::parse(&bytes)?;
            let mut image = dds.decode(&bytes, mip.min(dds.mips - 1))?;
            one_channel_as_grey(dds.format, &mut image.rgba);
            image
        }
        (Some(kind), Some(Shows::Picture)) => picture(kind, &bytes)?.0,
        _ => anyhow::bail!("{path} is not a texture or picture"),
    };
    channel.apply(&mut image.rgba);
    let (w, h) = (image.width, image.height);
    let image = image::RgbaImage::from_raw(w, h, image.rgba).context("texels short of the size")?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    Ok(png.into_inner())
}

/// The drawn parts (mesh and its place in model space) as one binary glTF:
/// positions, normals and UVs, one mesh per part, baked into model space
/// and mirrored on X into glTF's right-handed frame (Anno's models are
/// left-handed; the winding flips with them). Geometry only: no materials,
/// textures, levels or animation.
fn glb_of(parts: &[(Arc<Mesh>, Mat4)]) -> anyhow::Result<Vec<u8>> {
    use serde_json::{Value, json};
    let mut bin: Vec<u8> = Vec::new();
    let (mut views, mut accessors, mut meshes, mut nodes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // A buffer view and its accessor over `bytes` (4-aligned: f32 and u32).
    let mut push = |bytes: &[u8], count: usize, ty: &str, component: u32, extra: Value| {
        let view = json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len()});
        bin.extend_from_slice(bytes);
        views.push(view);
        let mut accessor = json!({"bufferView": views.len() - 1, "count": count, "type": ty, "componentType": component});
        if let (Value::Object(a), Value::Object(e)) = (&mut accessor, extra) {
            a.extend(e);
        }
        accessors.push(accessor);
        accessors.len() - 1
    };
    const FLOAT: u32 = 5126;
    const UINT: u32 = 5125;
    for (mesh, local) in parts {
        // Decoded: loaded meshes keep their geometry packed (`compact`).
        let data = mesh.geometry();
        if data.indices.is_empty() {
            continue;
        }
        let mirror = |v: Vec3| [-v[0], v[1], v[2]];
        let positions: Vec<Vec3> = (data.positions.iter())
            .map(|&p| mirror(math::transform(local, p)))
            .collect();
        let [lo, hi] = math::bounds(positions.iter().copied());
        let floats = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
        let mut attributes = serde_json::Map::new();
        let at = push(
            &floats(positions.as_flattened()),
            positions.len(),
            "VEC3",
            FLOAT,
            json!({"min": lo, "max": hi}),
        );
        attributes.insert("POSITION".into(), at.into());
        if data.normals.len() == positions.len() {
            // Directions: the matrix without its translation.
            let turn =
                |n: Vec3| math::sub(math::transform(local, n), math::transform(local, [0.; 3]));
            let normals: Vec<Vec3> = (data.normals.iter())
                .map(|&n| mirror(math::normalize(turn(n)).unwrap_or([0., 1., 0.])))
                .collect();
            let at = push(
                &floats(normals.as_flattened()),
                normals.len(),
                "VEC3",
                FLOAT,
                json!({}),
            );
            attributes.insert("NORMAL".into(), at.into());
        }
        if data.uvs.len() == positions.len() {
            let at = push(
                &floats(data.uvs.as_flattened()),
                data.uvs.len(),
                "VEC2",
                FLOAT,
                json!({}),
            );
            attributes.insert("TEXCOORD_0".into(), at.into());
        }
        let indices: Vec<u8> = (data.indices.as_chunks::<3>().0.iter())
            .flat_map(|&[a, b, c]| [a, c, b])
            .flat_map(u32::to_le_bytes)
            .collect();
        let at = push(
            &indices,
            data.indices.len() / 3 * 3,
            "SCALAR",
            UINT,
            json!({}),
        );
        meshes.push(json!({"primitives": [{"attributes": attributes, "indices": at, "mode": 4}]}));
        nodes.push(json!({"mesh": meshes.len() - 1}));
    }
    anyhow::ensure!(!meshes.is_empty(), "nothing to export");
    let doc = json!({
        "asset": {"version": "2.0", "generator": "Anno editor"},
        "scene": 0,
        "scenes": [{"nodes": (0..nodes.len()).collect::<Vec<_>>()}],
        "nodes": nodes,
        "meshes": meshes,
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{"byteLength": bin.len()}],
    });
    let mut text = serde_json::to_vec(&doc)?;
    // Chunks are 4-aligned: JSON pads with spaces, the binary with zeros.
    text.resize(text.len().next_multiple_of(4), b' ');
    bin.resize(bin.len().next_multiple_of(4), 0);
    let total = 12 + 8 + text.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(total)?.to_le_bytes());
    for (chunk, kind) in [(&text, b"JSON"), (&bin, b"BIN\0")] {
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(chunk);
    }
    Ok(out)
}

/// Where a PNG export of `path` goes under the extract folder: beside the
/// extracted original, named for the mip and channel when not the first
/// and all of them, so exports of one texture don't overwrite each other.
fn png_name(path: &str, mip: u32, channel: Channel) -> String {
    let mut name = path.to_owned();
    if mip > 0 {
        name += &format!(".mip{mip}");
    }
    name += match channel {
        Channel::Rgb => "",
        Channel::R => ".r",
        Channel::G => ".g",
        Channel::B => ".b",
        Channel::A => ".a",
    };
    name + ".png"
}

/// The `.cfg` files that may draw an `.rdm`, likeliest first: the one named
/// after its family beside its `rdm/` folder (`…/olive_trees/rdm/
/// olive_01_lod2.rdm` → `…/olive_trees/olive_01.cfg`), then that folder's
/// other cfgs, which is where a part of a several-part model finds its
/// owner (2070's `…_podest_lod0.rdm`).
fn owning_cfgs(install: &Install, path: &str) -> Vec<String> {
    let Some((dir, _)) = path.rsplit_once('/') else {
        return Vec::new();
    };
    let family = files::mesh_stem(path);
    let name = &family[dir.len() + 1..];
    let dir = dir.strip_suffix("/rdm").unwrap_or(dir);
    let mut cfgs = vec![format!("{dir}/{name}.cfg")];
    let others = install.list(dir).into_iter();
    let others = others.filter(|p| p.ends_with(".cfg") && *p != cfgs[0]);
    // A folder of hundreds of props would parse each; the owner is almost
    // always the first.
    let others: Vec<String> = others.take(64).collect();
    cfgs.extend(others);
    cfgs
}

/// A file dragged out of the Asset browser, by virtual path.
#[derive(Clone, Debug)]
pub struct AssetDrag(pub String);

impl Editor {
    /// The picture of the texture at `path` as the browser's rows show it,
    /// asked for when not decoded yet (it arrives on a later frame).
    pub(crate) fn texture_thumb(
        &self,
        path: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        let f = self.install.as_ref()?.file(path)?;
        let mut t = self.browser.thumbs.borrow_mut();
        if let Some(image) = t.done.get(&f) {
            return image.clone();
        }
        t.wanted.push(f);
        drop(t);
        self.poll_row_thumbs(cx);
        None
    }
}

/// The look the model preview draws `module`'s models in, its meshes in
/// the families the model view then draws with: the game's own (2070's
/// building family, 117's and 1800's a7m model family), glowing by the
/// look's strength.
fn preview_look(module: &dyn Module) -> files::ModelLook {
    module.model_look()
}

/// A `.cfg` as the island draws it, textured, nested props and all. A bare
/// `.rdm` borrows the materials of the `.cfg` part it is a level of, else
/// shows as grey clay. A `.cfg` that is not a model shows as text.
fn load_model(module: &dyn Module, install: &Install, path: &str, bytes: &[u8]) -> Content {
    let text = || readable(bytes);
    let cache = crate::island::models::Cache::new(&preview_look(module));
    let mut borrowed = None;
    let parts: Vec<(Arc<Mesh>, Mat4)> = if path.ends_with(".rdm") {
        let rdm = match anno_formats::rdm::Rdm::parse(bytes) {
            Ok(rdm) => rdm,
            Err(e) => return Content::Error(format!("{e:#}")),
        };
        // The owner's part with this file as its mesh: the full mesh or
        // any shipped level, each drawn with the part's materials.
        let family = files::mesh_stem(path);
        // A parse, not the kind's `deps`: those also name animation meshes,
        // and skip a mesh whose name has a space (`wooden _beam_ornament`).
        let owner = owning_cfgs(install, path).into_iter().find_map(|cfg| {
            let parsed = Model::parse(&install.read(&cfg).ok()?).ok()?;
            (parsed.parts.iter())
                .any(|p| files::mesh_stem(&p.rdm) == family)
                .then_some(cfg)
        });
        let textured = owner.and_then(|cfg| {
            let mut model = Model::load(install, &cfg).ok()?;
            let part = (model.parts.drain(..)).find(|p| files::mesh_stem(&p.rdm) == family)?;
            model.parts.push(anno_formats::model::Part {
                rdm: path.to_owned(),
                mesh: Some(rdm.clone()),
                ..part
            });
            model.props.clear();
            model.decals.clear();
            let pieces = crate::island::models::build(install, &cache, &model, 1., false);
            borrowed = Some(cfg);
            Some(
                pieces
                    .into_iter()
                    .map(|p| (p.mesh, p.local))
                    .collect::<Vec<_>>(),
            )
        });
        match textured {
            Some(parts) if !parts.is_empty() => parts,
            _ => {
                borrowed = None;
                clay(&rdm)
                    .into_iter()
                    .map(|m| (m, math::IDENTITY))
                    .collect()
            }
        }
    } else {
        match crate::island::models::load(install, &cache, path, 1.) {
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
            .map(|t| Arc::new(Code::new(&t.text, Language::Xml))),
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

/// A file's head decoded for the text view, and how.
struct Readable<'a> {
    text: Cow<'a, str>,
    /// What the view shows differently from the bytes, for the summary.
    note: Option<&'static str>,
}

/// The first `TEXT_BYTES` as text, a character cut at the end dropped;
/// `None` for binary. UTF-8 as is; UTF-16 by its byte order mark (2070's
/// loca files); anything else past ASCII as Windows-1252 (2070's German
/// comments in scripts and shaders). Binary values 2070's serializer
/// writes into XML (`CDATA[<u32 length><bytes>]`: islands, AI layouts,
/// scenario maps, 2070 and 1800 `.fc` files) show as their numbers.
fn readable(bytes: &[u8]) -> Option<Readable<'_>> {
    let head = &bytes[..bytes.len().min(TEXT_BYTES)];
    if let Some(units) = head.strip_prefix(b"\xff\xfe") {
        let units = units
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c));
        let text: String = char::decode_utf16(units)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        return (!text.contains('\0')).then_some(Readable {
            text: Cow::Owned(text),
            note: None,
        });
    }
    let (head, blobs) = match cdata_values(head) {
        Some(text) => (Cow::Owned(text), true),
        None => (Cow::Borrowed(head), false),
    };
    let binary = |b: &[u8]| {
        let control = b
            .iter()
            .filter(|&&c| c < 0x20 && !b"\t\n\r\x0c".contains(&c));
        b.contains(&0) || control.count() * 100 > b.len()
    };
    if binary(&head) {
        return None;
    }
    let text = match head {
        Cow::Borrowed(b) => match std::str::from_utf8(b) {
            Ok(t) => Some(Cow::Borrowed(t)),
            // Cut inside a character at the end: the rest is fine.
            Err(e) if e.error_len().is_none() => std::str::from_utf8(&b[..e.valid_up_to()])
                .ok()
                .map(Cow::Borrowed),
            Err(_) => None,
        },
        Cow::Owned(b) => String::from_utf8(b).ok().map(Cow::Owned),
    };
    Some(match text {
        Some(text) => Readable {
            text,
            note: blobs.then_some("binary values shown as numbers"),
        },
        None => Readable {
            text: Cow::Owned(windows_1252(&bytes[..bytes.len().min(TEXT_BYTES)])),
            note: Some("Windows-1252"),
        },
    })
}

/// `bytes` with each of the serializer's `>CDATA[<u32 length><bytes>]`
/// blobs written out as numbers; `None` when there are none. Up to 16
/// words show as floats when every one is a plausible float (positions,
/// 1800's dummies), else as signed integers (2070's fixed point, 4096 to
/// the metre); longer blobs (grids) show their size and first bytes.
fn cdata_values(bytes: &[u8]) -> Option<Vec<u8>> {
    const OPEN: &[u8] = b">CDATA[";
    let mut out = Vec::new();
    let mut at = 0;
    let mut found = false;
    while let Some(i) = bytes[at..].windows(OPEN.len()).position(|w| w == OPEN) {
        let start = at + i + OPEN.len();
        let blob = (bytes.get(start..start + 4))
            .map(|n| u32::from_le_bytes([n[0], n[1], n[2], n[3]]) as usize)
            .and_then(|n| Some((n, bytes.get(start + 4..start + 4 + n)?)))
            .filter(|(n, _)| bytes.get(start + 4 + n) == Some(&b']'));
        out.extend_from_slice(&bytes[at..start]);
        at = start;
        let Some((n, blob)) = blob else { continue };
        found = true;
        at = start + 4 + n;
        let words = blob.as_chunks::<4>().0;
        let floats = words.iter().map(|w| f32::from_le_bytes(*w));
        let plausible = |v: f32| v == 0. || (v.is_finite() && (1e-4..1e7).contains(&v.abs()));
        let shown = if n % 4 == 0 && n <= 64 && n > 0 && floats.clone().all(plausible) {
            floats.map(|v| v.to_string()).collect::<Vec<_>>().join(" ")
        } else if n % 4 == 0 && n <= 64 {
            (words.iter().map(|w| i32::from_le_bytes(*w).to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            let hex: Vec<String> = blob.iter().take(16).map(|b| format!("{b:02x}")).collect();
            let more = if n > 16 { " …" } else { "" };
            format!(
                "{} bytes: {}{more}",
                crate::project::count(n),
                hex.join(" ")
            )
        };
        out.extend_from_slice(shown.as_bytes());
    }
    out.extend_from_slice(&bytes[at..]);
    found.then_some(out)
}

/// Windows-1252, which differs from Latin-1 in 0x80..0x9f.
fn windows_1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    (bytes.iter())
        .map(|&b| match b {
            0x80..=0x9f => HIGH[b as usize - 0x80],
            _ => b as char,
        })
        .collect()
}

fn text_content(path: &str, text: Readable, size: usize) -> Content {
    let language = Language::guess(path, &text.text);
    let code = Code::new(&text.text, language);
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
    if size > TEXT_BYTES {
        summary += &format!(
            " · first {} of {}",
            human(TEXT_BYTES as u64),
            human(size as u64)
        );
    }
    if let Some(note) = text.note {
        summary += &format!(" · {note}");
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
    /// Sorts every file by name off the UI thread, ~60 ms for 440k files,
    /// so the first search or filter does not wait for it. One asked for
    /// before this lands sorts on the spot, as before.
    pub(crate) fn order_names(&mut self, cx: &mut Context<Self>) {
        let Some(install) = self.browser.install.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let names = cx
                .background_spawn({
                    let install = install.clone();
                    async move { NameOrder::new(&install) }
                })
                .await;
            this.update(cx, |this, _| {
                let b = &mut this.browser;
                // Another install attached meanwhile, or a list sorted first.
                let same = b.install.as_ref().is_some_and(|i| Arc::ptr_eq(i, &install));
                if !same || b.names.is_some() {
                    return;
                }
                b.names = Some(names);
            })
            .ok();
        })
        .detach();
    }

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
        let module = self.module;
        cx.spawn(async move |this, cx| {
            let preview = cx
                .background_executor()
                .spawn(
                    async move { load_preview(module, &install, file, mip, channel, dpr, native) },
                )
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
                    // By day, in the game's first player colour, in the
                    // families the look's meshes draw in.
                    let look = preview_look(&*this.module);
                    let families = look.families();
                    if !Arc::ptr_eq(&this.browser.model.families(), &families) {
                        this.browser.model = ModelView::with_families(families);
                    }
                    this.browser.model.glow = look.glow(&look.scene_glow());
                    // Anno's models are left-handed, as the island views.
                    // The preview keeps its parts for glTF export.
                    this.browser.model.show(parts.clone(), true);
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

    pub(crate) fn open_selected(&mut self, cx: &mut Context<Self>) {
        let (Some(install), Some(file)) = (self.install.clone(), self.browser.selected) else {
            return;
        };
        let path = install.file_path(file).to_owned();
        // Islands, maps, screens and models open in their workspace,
        // through the one dispatch every Open uses; nothing else has an
        // editor, and the preview is the view.
        let model = is_model(&path) && self.supports(crate::editor::Workspace::Model);
        if model || editor_for(self.module, &path).is_some() {
            self.open_path(&path, cx);
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

    /// Writes the shown texture or picture, at its mip and channel, as a
    /// PNG under the extract folder. A re-export overwrites: the PNG is
    /// derived, not a copy someone edits in place of the original.
    fn export_png(&mut self) {
        let (Some(install), Some(file)) = (self.install.clone(), self.browser.selected) else {
            return;
        };
        let Some(Content::Texture { mip, .. }) = self.browser.preview.as_ref().map(|p| &p.content)
        else {
            return;
        };
        let channel = self.browser.channel;
        let path = install.file_path(file);
        let target = (self.config.join("extracted")).join(png_name(&path, *mip, channel));
        let result = png_of(self.module, &install, file, *mip, channel).and_then(|png| {
            std::fs::create_dir_all(target.parent().expect("extracted paths have a parent"))?;
            std::fs::write(&target, png)?;
            Ok(())
        });
        match result {
            Ok(()) => self.message(format!("Exported to {}", target.display()), false),
            Err(e) => self.message(format!("PNG export failed: {e:#}"), true),
        }
    }

    /// Writes the shown model's drawn parts as `<path>.glb` under the
    /// extract folder (`glb_of`), overwriting an earlier export.
    fn export_gltf(&mut self) {
        let (Some(install), Some(file)) = (self.install.clone(), self.browser.selected) else {
            return;
        };
        let Some(Content::Model { parts, .. }) = self.browser.preview.as_ref().map(|p| &p.content)
        else {
            return;
        };
        let path = install.file_path(file);
        let target = (self.config.join("extracted")).join(format!("{path}.glb"));
        let result = glb_of(parts).and_then(|glb| {
            std::fs::create_dir_all(target.parent().expect("extracted paths have a parent"))?;
            std::fs::write(&target, glb)?;
            Ok(())
        });
        match result {
            Ok(()) => self.message(format!("Exported to {}", target.display()), false),
            Err(e) => self.message(format!("glTF export failed: {e:#}"), true),
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
        let module = self.module;
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
                // Across cores: 44 diffuse maps took 18 ms one after
                // another (45 ms cold), 7 ms in parallel.
                let decoded = cx
                    .background_spawn(async move {
                        use rayon::prelude::*;
                        batch
                            .into_par_iter()
                            .map(|f| (f, decode_thumb(module, &install, f, side)))
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
        let list_table = b.kinds.clone();
        let info_thumbs = b.thumbs.clone();
        let click_rows = rows.clone();
        // A mark per row for what it is, matching the type filter.
        let glyph_rows = rows.clone();
        let (glyph_install, glyph_kinds) = (install.clone(), b.kinds.clone());
        let (note_rows, note_install) = (rows.clone(), install.clone());
        // Texture rows in a flat list carry their picture: sibling textures
        // share names and sizes, and only the image tells them apart.
        let pictures = searching && b.types == Types::Textures;
        let (thumb_rows, thumb_install, thumbs) = (rows.clone(), install.clone(), b.thumbs.clone());
        let thumb_kinds = b.kinds.clone();
        let picture = std::rc::Rc::new(move |i: usize| -> Option<Arc<RenderImage>> {
            let Node::File(f) = thumb_rows[i].node else {
                return None;
            };
            let view = (thumb_kinds.get(&thumb_install.file_path(f))).and_then(|k| k.view);
            if !pictures || !matches!(view, Some(Shows::Texture | Shows::Picture)) {
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
                            let dir = install.folder_path(install.file_folder(f));
                            let dir = Some(dir).filter(|d| !d.is_empty())?;
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
                            let folder = cut_middle(&tile_note(&path), "", &tile_room);
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
                                    human(grid_install.winner(f).raw_size.into())
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
                        folder_counts(&list_install, &list_table, f, list_types, &list_kinds),
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
                            let size = || human(list_install.winner(f).raw_size.into());
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
                    folder_note(&note_install.file_path(f), &note_room, twin.as_deref())
                        .map(SharedString::from)
                }
                _ => None,
            },
        )
        // The whole path on hover in a flat list, where the folder column
        // is cut; a patched file says what its ◆ means.
        .decorate(move |i, row| match tip_rows[i].node {
            Node::File(f) => {
                // Files drag into the Model workspace: a prop or effect onto
                // the model, any file onto a path field.
                let path = tip_install.file_path(f);
                let name: SharedString = path.rsplit('/').next().unwrap_or(&path).to_owned().into();
                let row = row.on_drag(AssetDrag(path), move |_, _, _, cx| {
                    cx.new(|_| widgets::DragLabel(name.clone()))
                });
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
                    match glyph_kinds
                        .get(&glyph_install.file_path(f))
                        .map(|k| k.group)
                    {
                        Some(Group::Textures) => Icon::IMAGE,
                        Some(Group::Islands) => Icon::ISLAND,
                        Some(Group::Ui) => Icon::LAYOUT,
                        Some(Group::Models) => Icon::CUBE,
                        Some(Group::Text) => Icon::TEXT,
                        None => Icon::FILE,
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
            bytes += u64::from(install.winner(f).raw_size);
            let k = (self.browser.kinds.get(&install.file_path(f))).map_or("other", |k| k.label);
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
                let detail = folder_counts(install, &self.browser.kinds, sub, Types::All, &[]);
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
                    |d| {
                        // Only the kinds this game has an editor for.
                        let mut kinds = Vec::new();
                        if self.module.islands().is_some() {
                            kinds.push("Islands");
                        }
                        if self.maps().is_some() {
                            kinds.push("maps");
                        }
                        if self.module.ui().is_some() {
                            kinds.push("UI screens");
                        }
                        // "Islands" or "UI screens" leads, capitalised.
                        let said = match kinds.as_slice() {
                            [] => String::new(),
                            [one] => format!("{one} open in their editor."),
                            [rest @ .., last] => {
                                format!("{} and {last} open in their editors.", rest.join(", "))
                            }
                        };
                        d.when(!said.is_empty(), |d| d.child(hint(said)))
                    },
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
        let place = offers_place(
            self.module,
            &path,
            self.browser.preview.as_ref().map(|p| &p.content),
        );
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
                                match editor_for(self.module, &path) {
                                    Some(crate::editor::Workspace::Island) => Some("Open in Island editor"),
                                    Some(crate::editor::Workspace::Map) => Some("Open in Map editor"),
                                    Some(crate::editor::Workspace::Ui) => Some("Open in UI editor"),
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
                                        Some(Content::Model { .. }) => place,
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
                            .when(place, |d| {
                                let cfg = path.clone();
                                let open = self.island.is_some();
                                // Named for the island it lands on; with none
                                // open, the trip goes through the list.
                                let label = match &self.island {
                                    Some(i) => format!(
                                        "Place on {}",
                                        crate::island::island_title(self.asset_words(), &i.path)
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
                let view = self.browser.kinds.get(&path).and_then(|k| k.view);
                let size = (!matches!(view, Some(Shows::Texture | Shows::Picture))).then(|| human(install.winner(file).raw_size.into()));
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
        let staged = self.project.has_file(&path);
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
                                    human(layer.raw_size.into())
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
                        )
                        .child(widgets::toolbar_separator())
                        // Beside the mip and channel it takes them from.
                        .child(
                            widgets::action("Export PNG", widgets::Tone::Secondary)
                                .tooltip(widgets::Tooltip::text(
                                    "Export as PNG",
                                    "",
                                    format!(
                                        "The mip and channel shown, at their own size, to {}",
                                        crate::project::short_path(
                                            &self.config.join("extracted").join(png_name(
                                                &path,
                                                *mip,
                                                self.browser.channel
                                            ))
                                        )
                                    ),
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_png();
                                    cx.notify();
                                })),
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
                // A cfg with nothing to draw (an effect, a light, sound
                // anchors) still edits in the Model workspace.
                let edit_model = (self.browser.preview.as_ref())
                    .and_then(|p| self.install.as_ref().map(|i| i.file_path(p.file)))
                    .filter(|p| is_model(p))
                    .filter(|_| self.supports(crate::editor::Workspace::Model));
                body.when_some(edit_model, |d, path| {
                    d.child(
                        div().flex().justify_end().child(
                            widgets::action("Edit model", widgets::Tone::Primary)
                                .tooltip(widgets::Tooltip::text(
                                    "Edit model",
                                    "Ctrl 5",
                                    "Opens it in the Model workspace; saving puts the edited copy in the mod",
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_model(&path, cx);
                                    cx.notify();
                                })),
                        ),
                    )
                })
                .child(self.code_box(code.clone(), Some(summary.clone().into()), window, cx))
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
                // A cfg or prp opens in the Model workspace where the game edits.
                let edit_model = (self.browser.preview.as_ref())
                    .and_then(|p| self.install.as_ref().map(|i| i.file_path(p.file)))
                    .filter(|p| is_model(p))
                    .filter(|_| self.supports(crate::editor::Workspace::Model));
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
                        .when_some(edit_model, |d, path| {
                            d.child(
                                widgets::action("Edit model", widgets::Tone::Primary)
                                    .tooltip(widgets::Tooltip::text(
                                        "Edit model",
                                        "Ctrl 5",
                                        "Opens it in the Model workspace; saving puts the edited copy in the mod",
                                    ))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_model(&path, cx);
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(
                            widgets::action("Export glTF", widgets::Tone::Secondary)
                                .tooltip(widgets::Tooltip::text(
                                    "Export as glTF",
                                    "",
                                    "The drawn meshes, without materials, as a .glb beside the extracted files",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_gltf();
                                    cx.notify();
                                })),
                        )
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
    use super::{
        Browser, Content, Install, Kinds, Room, Types, folder_note, hex_dump, human, pack_name,
        readable,
    };
    use anno_core::files::{self, Group, Kind, View};
    use anno_core::game::{Game, Module, Profile};
    use anno_core::text::GameText;
    use anno_formats::rda::{PackOptions, Packer};
    use std::sync::Arc;

    /// A game that claims `.dds` as a picture of its own and lists nothing
    /// else: what a game kind over a generic one looks like.
    struct Claims;
    static CLAIMS: Claims = Claims;
    static CLAIMED: &[Kind] = &[Kind {
        id: "own-dds",
        label: "Own texture",
        group: Group::Textures,
        exts: &["dds"],
        folder: None,
        view: Some(View::Picture),
        decode: None,
        deps: None,
    }];
    impl Module for Claims {
        fn game(&self) -> Game {
            Game::Anno117
        }
        fn profile(&self) -> &'static Profile {
            anno117::MODULE.profile()
        }
        fn text(&self) -> &'static GameText {
            anno117::MODULE.text()
        }
        fn card_art(&self) -> &'static [u8] {
            &[]
        }
        fn accent(&self) -> [u32; 2] {
            [0; 2]
        }
        fn files(&self) -> &'static [Kind] {
            CLAIMED
        }
    }

    fn id(module: &dyn Module, path: &str) -> Option<&'static str> {
        files::kind_of(module, path).map(|k| k.id)
    }

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
        let m = &anno117::MODULE;
        assert_eq!(id(m, "data/a/b.dds"), Some("dds"));
        assert_eq!(id(m, "data/a/B.DDS"), Some("dds"));
        assert_eq!(id(m, "data/a/rdm/b_lod0.rdm"), Some("rdm"));
        assert_eq!(id(m, "data/x/islands/i/i.a7m"), Some("a7m"));
        assert_eq!(
            id(m, "data/ui/studio/generated/00214ce2-8775"),
            Some("screen")
        );
        assert_eq!(id(m, "data/ui/fonts/a.ttf"), None);
        // A dot in a folder name is no extension.
        assert_eq!(id(m, "data/v1.2/readme"), None);
        assert_eq!(human(1536), "1.5 KB");
    }

    // 2070's masks, islands and text formats are 2070's; 1800's props and
    // the generated UI folder are no 2070 file. Before, one list of every
    // game's extensions ran for all of them.
    #[test]
    fn each_game_lists_only_its_own_kinds() {
        let (m117, m1800, m2070) = (&anno117::MODULE, &anno1800::MODULE, &anno2070::MODULE);
        let view = |m: &dyn Module, path| files::kind_of(m, path).and_then(|k| k.view);
        let abm = "data/graphics/ui/camctrl/circle.abm";
        assert_eq!(view(m2070, abm), Some(View::Picture));
        assert_eq!(id(m117, abm), None);
        let prp = "data/graphics/props/grass/grass_01.prp";
        assert_eq!(view(m1800, prp), Some(View::Model));
        assert_eq!(id(m117, prp), None);
        assert_eq!(id(m2070, prp), None);
        let screen = "data/ui/studio/generated/00214ce2-8775";
        assert_eq!(view(m117, screen), Some(View::Screen));
        assert_eq!(view(m1800, screen), Some(View::Screen));
        assert_eq!(id(m2070, screen), None);
        // 2070's islands open in the island editor, its worlds in the map
        // editor, as 117's and 1800's templates do.
        let isd = "data/levels/islands/normal/n_l01.isd";
        let k = files::kind_of(m2070, isd).unwrap();
        assert_eq!((k.group, k.view), (Group::Islands, Some(View::Island)));
        assert_eq!(
            super::editor_for(m2070, isd),
            Some(crate::editor::Workspace::Island)
        );
        assert_eq!(super::editor_for(m117, isd), None);
        let maps = |m: &dyn Module| {
            (m.islands().and_then(|i| i.maps())).map(|_| crate::editor::Workspace::Map)
        };
        let www = "data/levels/scenarios/singleplayer/06.www";
        assert_eq!(view(m2070, www), Some(View::Map));
        assert_eq!(super::editor_for(m2070, www), maps(m2070));
        for path in ["data/x/t/t.a7t", "data/x/t/t.a7tinfo"] {
            assert_eq!(view(m117, path), Some(View::Map), "{path}");
            assert_eq!(view(m1800, path), Some(View::Map), "{path}");
            assert_eq!(super::editor_for(m117, path), maps(m117), "{path}");
        }
        assert_eq!(id(m117, isd), None);
        assert_eq!(id(m2070, "data/gui/x.tpl"), Some("text"));
        assert_eq!(id(m1800, "data/gui/x.tpl"), None);
        // Generic kinds belong to no game's list.
        for m in [m117 as &dyn Module, m1800, m2070] {
            for k in m.files() {
                for g in files::GENERIC {
                    assert!(
                        !k.exts.iter().any(|e| g.exts.contains(e)),
                        "{} lists .{:?}, a generic kind's",
                        k.id,
                        k.exts
                    );
                }
            }
            assert_eq!(id(m, "data/x.dds"), Some("dds"));
            assert_eq!(id(m, "data/x.cur"), Some("cur"));
        }
    }

    #[test]
    fn a_game_kind_wins_over_a_generic_one() {
        let k = files::kind_of(&CLAIMS, "data/x/a_0.dds").unwrap();
        assert_eq!((k.id, k.view), ("own-dds", Some(View::Picture)));
        assert_eq!(
            Kinds::new(&CLAIMS).get("data/x/a_0.dds").unwrap().id,
            "own-dds"
        );
        // What it doesn't claim stays generic.
        assert_eq!(id(&CLAIMS, "data/x/a.png"), Some("png"));
    }

    // A game the registry doesn't know, in an install of its own: the list
    // holds its kinds and the generic ones, each chip lists what they claim
    // (not 117's cfg or screen beside them), its picture decodes through
    // its own reader, its mesh shows as text, and staged into one of its
    // projects the mesh is a model with the files it names.
    #[test]
    fn the_browser_lists_previews_and_stages_a_skeleton_install() {
        use crate::games::fixtures::{SKELETON, SKELETON_FILES, install};
        use anno_formats::vfs::Node;
        let dir = tempfile::tempdir().unwrap();
        let install = Arc::new(install(&dir.path().join("game"), SKELETON_FILES));
        let mut b = Browser::default();
        b.attach(&install, &SKELETON);
        let ids: Vec<&str> = b.kinds.list.iter().map(|k| k.id).collect();
        let generic = files::GENERIC.iter().map(|k| k.id);
        assert_eq!(
            ids,
            ["ski", "skm", "skt"]
                .into_iter()
                .chain(generic)
                .collect::<Vec<_>>()
        );
        let listed = |b: &mut Browser, types| {
            b.types = types;
            b.rebuild();
            let paths: Vec<String> = (b.rows.iter())
                .map(|r| match r.node {
                    Node::File(f) => install.file_path(f).to_string(),
                    Node::Folder(_) => panic!("a flat list holds files"),
                })
                .collect();
            paths
        };
        assert_eq!(listed(&mut b, Types::Models), ["content/meshes/rock.skm"]);
        assert_eq!(
            listed(&mut b, Types::Textures),
            ["content/textures/rock.skt"]
        );
        assert!(listed(&mut b, Types::Ui).is_empty());
        assert_eq!(
            listed(&mut b, Types::Text),
            ["skeleton/captions.xml", "skeleton/library.xml"]
        );

        let preview = |path| {
            let file = install.file(path).unwrap();
            super::load_preview(
                &SKELETON,
                &install,
                file,
                None,
                super::Channel::Rgb,
                1.,
                None,
            )
            .content
        };
        let Content::Texture { dims, summary, .. } = preview("content/textures/rock.skt") else {
            panic!("the picture didn't decode");
        };
        assert_eq!(dims, (2, 1));
        assert!(summary.contains("skeleton text picture"), "{summary}");
        let Content::Text { summary, .. } = preview("content/meshes/rock.skm") else {
            panic!("the mesh isn't text");
        };
        assert!(summary.contains("lines"), "{summary}");

        let folder = dir.path().join("project");
        let mut project =
            crate::project::Project::create(&folder, "Rocks", "rocks", &SKELETON).unwrap();
        for path in ["content/meshes/rock.skm", "content/textures/rock.skt"] {
            project.stage(path, install.read(path).unwrap());
        }
        // The texture's mips are staged too, as an import would.
        project.stage("content/textures/rock_0.dds", b"mip".to_vec());
        let models = project.models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].cfg, "content/meshes/rock.skm");
        assert_eq!(
            models[0].files,
            ["content/meshes/rock.skm", "content/textures/rock_0.dds"]
        );
    }

    // The tree's per-folder bits and per-type counts come from the packed
    // extension table; they must say what the per-file lookup says, for
    // every game, on a layout with folder markers, upper case, long and
    // missing extensions.
    #[test]
    fn the_folder_index_agrees_with_the_per_file_lookup() {
        let mut paths: Vec<String> = Vec::new();
        let modules: [&'static dyn Module; 4] = [
            &anno117::MODULE,
            &anno1800::MODULE,
            &anno2070::MODULE,
            &CLAIMS,
        ];
        for m in modules {
            for k in m.files().iter().chain(files::GENERIC) {
                for e in k.exts {
                    paths.push(format!("data/{}/a.{e}", k.id));
                    paths.push(format!("data/{}/upper/B.{}", k.id, e.to_uppercase()));
                }
            }
        }
        paths.extend(
            [
                "data/ui/studio/generated/00214ce2-8775",
                "data/ui/studio/generated/b.dds",
                "data/ui/studio/generated/c.xml",
                "data/ui/fonts/a.ttf",
                "data/v1.2/readme",
                "data/x/a.extension",
                "data/x/noext",
            ]
            .map(String::from),
        );
        paths.sort();
        paths.dedup();
        let dir = tempfile::tempdir().unwrap();
        let mut pack = Packer::new(PackOptions::default());
        for p in &paths {
            pack.add(p, b"x").unwrap();
        }
        let archives = dir.path().join("maindata");
        std::fs::create_dir(&archives).unwrap();
        std::fs::write(archives.join("data0.rda"), pack.encode().unwrap()).unwrap();
        let install = Arc::new(Install::open(dir.path(), &[]).unwrap());
        for m in modules {
            let mut b = Browser::default();
            b.attach(&install, m);
            b.index_folder_kinds(&install);
            let mut counts = [0usize; 6];
            for f in (0..install.file_count() as u32).map(anno_formats::vfs::FileId) {
                let path = install.file_path(f);
                let want = files::kind_of(m, &path);
                let got = b.kinds.get(&path);
                assert_eq!(got.map(|k| k.id), want.map(|k| k.id), "{path}");
                let Some(k) = want else { continue };
                let t = Types::of(k.group);
                counts[t as usize] += 1;
                let folder = install.file_folder(f);
                assert!(b.folder_kinds[folder.0 as usize] & t.bit() != 0, "{path}");
            }
            assert_eq!(b.kind_counts, counts);
        }
        // The shipped installs too, where present: every file, no reads.
        for (var, m) in [
            ("ANNO117_DIR", &anno117::MODULE as &'static dyn Module),
            ("ANNO1800_DIR", &anno1800::MODULE),
        ] {
            let Some(dir) = anno_formats::vfs::install_dir(var) else {
                continue;
            };
            let install = Install::open(dir, m.profile().archive_families).unwrap();
            let kinds = Kinds::new(m);
            for f in (0..install.file_count() as u32).map(anno_formats::vfs::FileId) {
                let path = install.file_path(f);
                assert_eq!(
                    kinds.get(&path).map(|k| k.id),
                    files::kind_of(m, &path).map(|k| k.id),
                    "{path}"
                );
            }
        }
    }

    #[test]
    fn models_offer_place_where_islands_keep_placed_objects() {
        let model = Content::Model {
            parts: Vec::new(),
            summary: String::new(),
            source: None,
        };
        let cfg = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
        assert!(super::offers_place(&anno2070::MODULE, cfg, Some(&model)));
        assert!(super::offers_place(&anno117::MODULE, cfg, Some(&model)));
        assert!(super::offers_place(&anno1800::MODULE, cfg, Some(&model)));
        // A cfg that showed as text, a bare mesh, and a game with screens
        // but no islands.
        assert!(!super::offers_place(
            &anno117::MODULE,
            cfg,
            Some(&Content::Hex(String::new()))
        ));
        assert!(!super::offers_place(
            &anno117::MODULE,
            "data/x/rdm/a.rdm",
            Some(&model)
        ));
        let screens = &crate::games::fixtures::PartialGame::Screens;
        assert!(!super::offers_place(screens, cfg, Some(&model)));
    }

    // A picture exports as what the preview shows, at its own size: a
    // 2070 mask's set bits white, its clear ones clear; its alpha channel
    // alone as grey. A texture exports one mip (where an install is set).
    #[test]
    fn textures_and_pictures_export_as_png() {
        use super::{Channel, png_name, png_of};
        let mut mask = b"ABM\x01\x0a\x00\x00\x00\x02\x00\x00\x00".to_vec();
        mask.extend([0b0000_0001, 0b0000_0010, 0b0000_0010, 0]);
        let dir = tempfile::tempdir().unwrap();
        let archives = dir.path().join("maindata");
        std::fs::create_dir(&archives).unwrap();
        let mut pack = Packer::new(PackOptions::default());
        pack.add("data/gui/mask.abm", &mask).unwrap();
        pack.add("data/gui/notes.txt", b"text").unwrap();
        std::fs::write(archives.join("data0.rda"), pack.encode().unwrap()).unwrap();
        let install = Install::open(dir.path(), &[]).unwrap();
        let file = install.file("data/gui/mask.abm").unwrap();
        let png = png_of(&anno2070::MODULE, &install, file, 0, Channel::Rgb).unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (10, 2));
        assert_eq!(image.get_pixel(9, 0).0, [255; 4]);
        assert_eq!(image.get_pixel(1, 0).0, [0; 4]);
        let png = png_of(&anno2070::MODULE, &install, file, 0, Channel::A).unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.get_pixel(1, 0).0, [0, 0, 0, 255]);
        // The same file is no picture to a game that doesn't list it.
        assert!(png_of(&anno117::MODULE, &install, file, 0, Channel::Rgb).is_err());
        let text = install.file("data/gui/notes.txt").unwrap();
        assert!(png_of(&anno2070::MODULE, &install, text, 0, Channel::Rgb).is_err());
        assert_eq!(
            png_name("data/a_0.dds", 0, Channel::Rgb),
            "data/a_0.dds.png"
        );
        assert_eq!(
            png_name("data/a_0.dds", 2, Channel::A),
            "data/a_0.dds.mip2.a.png"
        );
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let file = (0..install.file_count() as u32)
            .map(anno_formats::vfs::FileId)
            .find(|&f| install.file_path(f).ends_with("_0.dds"))
            .unwrap();
        let bytes = install.read_file(file).unwrap();
        let dds = gpui_editor_kit::texture::Dds::parse(&bytes).unwrap();
        let mip = dds.mips.min(2) - 1;
        let png = png_of(&anno117::MODULE, &install, file, mip, Channel::Rgb).unwrap();
        let image = image::load_from_memory(&png).unwrap();
        assert_eq!(
            (image.width(), image.height()),
            ((dds.width >> mip).max(1), (dds.height >> mip).max(1))
        );
    }

    // Needs Anno 1800 (ANNO1800_DIR): a prop file draws in 3D, as the island
    // draws it, where it once showed as XML.
    #[test]
    fn an_1800_prp_previews_as_a_model() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let install = Install::open(dir, families).unwrap();
        let file = (0..install.file_count() as u32)
            .map(anno_formats::vfs::FileId)
            .find(|&f| install.file_path(f).ends_with(".prp"))
            .expect("1800 ships props");
        let preview = super::load_preview(
            &anno1800::MODULE,
            &install,
            file,
            None,
            super::Channel::Rgb,
            1.,
            None,
        );
        match preview.content {
            Content::Model { parts, summary, .. } => assert!(!parts.is_empty(), "{summary}"),
            Content::Error(e) => panic!("{}: {e}", install.file_path(file)),
            _ => panic!("{}: not a model", install.file_path(file)),
        }
    }

    // UTF-16 by its mark, Windows-1252 where UTF-8 fails, and NULs or
    // control bytes as binary.
    #[test]
    fn text_reads_in_each_encoding_the_games_use() {
        let utf16: Vec<u8> = b"\xff\xfe"
            .iter()
            .copied()
            .chain("Grüße\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let t = readable(&utf16).unwrap();
        assert_eq!((t.text.as_ref(), t.note), ("Grüße\r\n", None));
        let t = readable(b"// wird einmal f\xfcr \x80 5").unwrap();
        assert_eq!(t.text, "// wird einmal für € 5");
        assert_eq!(t.note, Some("Windows-1252"));
        assert_eq!(readable(b"plain").unwrap().note, None);
        assert!(readable(b"PHX\x00\x01").is_none());
        assert!(readable(b"\x89PNG\r\n\x1a\n\x02\x03\x04\x05").is_none());
    }

    // 2070's XML binary blobs: three floats as floats, fixed point as
    // integers, a grid by its size and first bytes; a `]` the length does
    // not reach is left as written.
    #[test]
    fn binary_cdata_shows_as_numbers() {
        let mut xml = b"<P>CDATA[\x0c\x00\x00\x00".to_vec();
        for v in [844.5f32, 11.25, -744.] {
            xml.extend(v.to_le_bytes());
        }
        xml.extend(b"]</P>\r\n<Q>CDATA[\x08\x00\x00\x00");
        for v in [18432i32, -4096] {
            xml.extend(v.to_le_bytes());
        }
        xml.extend(b"]</Q>\r\n<G>CDATA[\x48\x00\x00\x00");
        xml.extend(0u8..72);
        xml.extend(b"]</G><T>CDATA[short]</T>");
        let t = readable(&xml).unwrap();
        assert_eq!(
            t.text,
            "<P>CDATA[844.5 11.25 -744]</P>\r\n<Q>CDATA[18432 -4096]</Q>\r\n\
             <G>CDATA[72 bytes: 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f …]</G>\
             <T>CDATA[short]</T>"
        );
        assert_eq!(t.note, Some("binary values shown as numbers"));
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

    fn install_2070() -> Option<Install> {
        use crate::games::GameExt;
        let dir = anno_formats::vfs::install_dir("ANNO2070_DIR")?;
        let families = anno_core::game::Game::Anno2070.profile().archive_families;
        Some(Install::open(&dir, families).unwrap())
    }

    fn preview(install: &Install, path: &str) -> Content {
        let file = install.file(path).unwrap_or_else(|| panic!("no {path}"));
        super::load_preview(
            &anno2070::MODULE,
            install,
            file,
            None,
            super::Channel::Rgb,
            1.,
            None,
        )
        .content
    }

    // Needs Anno 2070 (ANNO2070_DIR): a building's cfg previews as its
    // meshes in place, the way a 117 or 1800 cfg does, its parts textured
    // with normal maps, its ground decal under it.
    #[test]
    fn a_2070_cfg_previews_as_its_meshes() {
        let Some(install) = install_2070() else {
            return;
        };
        let path = "addondata/graphics/buildings/others/public/underwater_markethouse_small_others/underwater_markethouse_small_others.cfg";
        let Content::Model { parts, summary, .. } = preview(&install, path) else {
            panic!("not a model");
        };
        assert!(parts.len() >= 2, "{summary}");
        assert!(summary.contains("meshes"), "{summary}");
        // The building has a normal map; its podest does not.
        assert!(parts.iter().all(|(m, _)| m.data().texture.is_some()));
        let bumped = parts.iter().filter(|(m, _)| m.data().normal_map.is_some());
        assert_eq!(bumped.count(), 1, "{summary}");
        let path = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
        let Content::Model { parts, .. } = preview(&install, path) else {
            panic!("not a model");
        };
        let decals = parts
            .iter()
            .filter(|(m, _)| !matches!(m.data().material, super::gpu::Material::Lit));
        assert_eq!(decals.count(), 1);
    }

    // Needs Anno 2070: every shipped level of a mesh takes its cfg's
    // textures, and so does a part's mesh of a several-part model, named
    // after the part, from the cfg in the folder above. A big-endian mesh
    // previews too.
    #[test]
    fn a_2070_rdm_borrows_its_parts_materials() {
        let Some(install) = install_2070() else {
            return;
        };
        let bakery = "data/graphics/buildings/ecos/production/bakery_ecos/rdm/bakery_ecos";
        let podest = "addondata/graphics/buildings/others/public/underwater_markethouse_small_others/rdm/underwater_markethouse_small_others_podest_lod0.rdm";
        for (path, cfg) in [
            (format!("{bakery}_lod0.rdm"), "bakery_ecos.cfg"),
            (format!("{bakery}_lod1.rdm"), "bakery_ecos.cfg"),
            (format!("{bakery}_lod2.rdm"), "bakery_ecos.cfg"),
            (podest.to_owned(), "underwater_markethouse_small_others.cfg"),
        ] {
            let Content::Model { parts, summary, .. } = preview(&install, &path) else {
                panic!("{path}: not a model");
            };
            assert!(
                summary.contains(&format!("materials from {cfg}")),
                "{summary}"
            );
            assert!(parts.iter().all(|(m, _)| m.data().texture.is_some()));
        }
        let path = "data/graphics/buildings/tycoons/props/submarine/rdm/submarine.rdm";
        let Content::Model { summary, .. } = preview(&install, path) else {
            panic!("{path}: not a model");
        };
        assert!(
            summary.contains("materials from submarine.cfg"),
            "{summary}"
        );
    }

    // Needs Anno 2070: its UTF-16 loca text, its Windows-1252 AI scripts,
    // its worlds, its masks and its cursors all
    // preview as what they are.
    #[test]
    fn every_kind_of_2070_file_previews() {
        let Some(install) = install_2070() else {
            return;
        };
        let text = |path: &str, note: Option<&str>| match preview(&install, path) {
            Content::Text { summary, code } => {
                assert!(code.line_count() > 1, "{path}");
                if let Some(note) = note {
                    assert!(summary.contains(note), "{path}: {summary}");
                }
            }
            _ => panic!("{path}: not text"),
        };
        text("data/loca/eng/txt/credits.txt", None);
        text("data/ai/autorun.csl", Some("Windows-1252"));
        // An island opens in the island editor rather than previewing.
        assert!(matches!(
            preview(
                &install,
                "addondata/levels/islands/third parties/u_x03_rift.isd"
            ),
            Content::Island
        ));
        text("data/levels/scenarios/singleplayer/06.www", None);
        text("data/config/ai/eco_harbour6x3_1.rsd", Some("binary values"));
        for (path, facts) in [
            ("data/graphics/ui/camctrl/circle.abm", "64×64 · 1-bit mask"),
            ("data/graphics/ui/mousepointer/move.cur", "cursor, hotspot"),
            ("data/graphics/ui/mousepointer/follow.ani", "frames"),
        ] {
            match preview(&install, path) {
                Content::Texture { summary, .. } => assert!(summary.contains(facts), "{summary}"),
                Content::Error(e) => panic!("{path}: {e}"),
                _ => panic!("{path}: not a picture"),
            }
        }
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

    /// How many pixels of two renders of one view differ by more than a
    /// tenth of white, summed over the channels.
    fn changed_pixels(a: &[u8], b: &[u8]) -> usize {
        (a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0))
            .filter(|(a, b)| (0..3).map(|i| a[i].abs_diff(b[i]) as u32).sum::<u32>() > 76)
            .count()
    }

    // Needs 117 and a Vulkan device: a tile oven's fire, which glows by
    // day, glows in its Library thumbnail as in the Assets preview, both in
    // the game's look. Thumbnails drew a plain look before: the kit's glow
    // strength and 1800's colour rule. Glow pixels are those that change
    // when the look's glow goes to 0. (117's lit windows light only by
    // night, and previews show the day.)
    #[test]
    fn a_library_thumbnail_glows_like_the_preview() {
        use gpui_editor_kit::camera::{Camera, Extent};
        use gpui_editor_kit::gpu::{Batch, Glow, Instances, Light, Offscreen, Renderer, Scene};
        use gpui_editor_kit::math;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let module = &anno117::MODULE;
        let cfg = "data/base/graphics/roman/buildings/production/construction/tiles/oven2.cfg";
        let look = module.model_look();
        let dark = files::ModelLook { glow: 0., ..look };
        let side = crate::island::catalog::THUMB * 2;
        let mut target = Offscreen::new(side, side).unwrap();

        // The preview: the browser's parts, lit and glowing as the model
        // view draws them.
        let bytes = install.read(cfg).unwrap();
        let Content::Model { parts, .. } = super::load_model(module, &install, cfg, &bytes) else {
            panic!("{cfg} previews as a model");
        };
        let [lo, hi] = math::bounds(
            (parts.iter())
                .flat_map(|(m, l)| math::corners(m.bounds()).map(|c| math::transform(l, c))),
        );
        let mut preview = |glow: Glow| {
            let renderer = Renderer::with_families(look.families());
            renderer.set_scene(&Scene {
                batches: (parts.iter())
                    .map(|(mesh, local)| {
                        let at = Instances {
                            matrices: vec![*local],
                            ids: vec![None],
                        };
                        Batch::new(mesh.clone(), Arc::new(at))
                    })
                    .collect(),
                light: Light {
                    sun_color: [0.62; 3],
                    sky: [0.55; 3],
                    ground: [0.32; 3],
                    ..Default::default()
                },
                glow,
                ..Default::default()
            });
            let camera = Camera {
                yaw: 0.6,
                tilt: 0.95,
                mirror: true,
                ..Default::default()
            };
            let extent = Extent {
                center: std::array::from_fn(|i| (lo[i] + hi[i]) / 2.),
                size: (0..3).map(|i| hi[i] - lo[i]).fold(0.01, f32::max) * 1.2,
            };
            target.render(&renderer, camera, extent).unwrap()
        };
        let glow = |look: &files::ModelLook| look.glow(&look.scene_glow());
        let shown = changed_pixels(&preview(glow(&look)), &preview(glow(&dark)));

        let mut thumb = |look: &files::ModelLook| {
            let renderer = Renderer::with_families(look.families());
            let rendered = crate::island::catalog::render_thumbnail(
                &install,
                &mut target,
                &renderer,
                cfg,
                look,
            );
            rendered.unwrap().expect("a thumbnail").0.into_raw()
        };
        let lit = thumb(&look);
        let library = changed_pixels(&lit, &thumb(&dark));
        // The thumbnail takes the look it is given, not a plain one.
        let plain = changed_pixels(&lit, &thumb(&files::ModelLook::PLAIN));
        eprintln!("glow pixels: preview {shown}, thumbnail {library}; plain differs in {plain}");
        assert!(shown >= 100, "the preview glows in {shown} pixels");
        assert!(library >= 25, "the thumbnail glows in {library} pixels");
        assert!(plain >= 10, "a plain look changes {plain} thumbnail pixels");
    }

    // Needs 2070 and a Vulkan device: a 2070 building previews in 3D from
    // its cfg, big-endian meshes and DDS textures, in 2070's look: its parts
    // as recorded (mesh count and a digest of the drawn geometry and
    // placements), dressed in 2070's building family, and its windows
    // glowing where the normal map's blue says by the look's strength,
    // which a look without glow leaves dark.
    #[test]
    fn a_2070_building_previews_in_its_own_look() {
        use gpui_editor_kit::camera::{Camera, Extent};
        use gpui_editor_kit::gpu::{Batch, Instances, Light, Offscreen, Renderer, Scene};
        use gpui_editor_kit::math;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let module = &anno2070::MODULE;
        let install = Install::open(&dir, module.profile().archive_families).unwrap();
        let cfg = "data/graphics/buildings/techs/production/carbon_factory_techs/carbon_factory_techs.cfg";
        // A rock's meshes are big-endian, which the preview reads sniffed.
        let rock = "data/graphics/landscape/stones/north/nature_stones/n_nature_stone_big_01.cfg";
        let rock_bytes = install.read(rock).unwrap();
        let rdm = &anno_formats::model::Model::parse(&rock_bytes)
            .unwrap()
            .parts[0]
            .rdm;
        assert_eq!(install.read(rdm).unwrap()[..4], *b"RDM\0");
        let Content::Model { parts, .. } = super::load_model(module, &install, rock, &rock_bytes)
        else {
            panic!("{rock} previews as a model");
        };
        assert!(parts.iter().all(|(m, _)| m.triangles() > 0) && !parts.is_empty());

        let bytes = install.read(cfg).unwrap();
        let Content::Model { parts, summary, .. } =
            super::load_model(module, &install, cfg, &bytes)
        else {
            panic!("{cfg} previews as a model");
        };
        let mut h = 0xcbf29ce484222325u64;
        let mut eat = |bits: u32| {
            for b in bits.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100000001b3);
            }
        };
        for (mesh, local) in &parts {
            let g = mesh.geometry();
            g.positions
                .iter()
                .flatten()
                .chain(g.uvs.iter().flatten())
                .for_each(|v| eat(v.to_bits()));
            g.indices.iter().for_each(|&i| eat(i));
            local.iter().for_each(|v| eat(v.to_bits()));
        }
        eprintln!("{summary}: {} meshes, digest {h:016x}", parts.len());
        assert_eq!((parts.len(), h), (2, 0x20aa_9b89_9333_500b), "{summary}");
        // In 2070's own family set: its building family, not the kit's.
        use anno_render::a2070_buildings::{NAME, families};
        let building = families().id(NAME);
        assert!(
            parts
                .iter()
                .any(|(m, _)| Some(m.geometry().family) == building),
            "no part draws in the building family"
        );

        let [lo, hi] = math::bounds(
            (parts.iter())
                .flat_map(|(m, l)| math::corners(m.bounds()).map(|c| math::transform(l, c))),
        );
        let mut target = Offscreen::new(256, 256).unwrap();
        let mut render = |look: files::ModelLook| {
            let renderer = Renderer::with_families(look.families());
            renderer.set_scene(&Scene {
                batches: (parts.iter())
                    .map(|(mesh, local)| {
                        let at = Instances {
                            matrices: vec![*local],
                            ids: vec![None],
                        };
                        Batch::new(mesh.clone(), Arc::new(at))
                    })
                    .collect(),
                light: Light {
                    sun_color: [0.62; 3],
                    sky: [0.55; 3],
                    ground: [0.32; 3],
                    ..Default::default()
                },
                glow: look.glow(&look.scene_glow()),
                ..Default::default()
            });
            let camera = Camera {
                yaw: 0.6,
                tilt: 0.95,
                mirror: true,
                ..Default::default()
            };
            let extent = Extent {
                center: std::array::from_fn(|i| (lo[i] + hi[i]) / 2.),
                size: (0..3).map(|i| hi[i] - lo[i]).fold(0.01, f32::max) * 1.2,
            };
            target.render(&renderer, camera, extent).unwrap()
        };
        let look = module.model_look();
        let glowing = changed_pixels(
            &render(look),
            &render(files::ModelLook { glow: 0., ..look }),
        );
        eprintln!("{glowing} glowing pixels");
        assert!(glowing >= 50, "{glowing} glowing pixels");
    }

    // A model's drawn parts exported as glTF read back through the kit's
    // glTF reader (the `gltf` crate): every triangle, placed by its part's
    // matrix, mirrored on X and wound the other way. The shipped olive tree
    // too, when 117 is installed.
    #[test]
    fn exported_gltf_reads_back_as_the_drawn_triangles() {
        use gpui_editor_kit::gpu::{Mesh, MeshData};
        use gpui_editor_kit::heightfield::files::load_mesh;
        use gpui_editor_kit::math;
        let dir = tempfile::tempdir().unwrap();
        let read = |parts: &[(Arc<Mesh>, math::Mat4)]| {
            let file = dir.path().join("model.glb");
            std::fs::write(&file, super::glb_of(parts).unwrap()).unwrap();
            load_mesh(&file, false).unwrap()
        };
        let triangle = Arc::new(
            Mesh::new(MeshData {
                positions: vec![[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
                normals: vec![[0., 0., 1.]; 3],
                uvs: vec![[0., 0.], [1., 0.], [0., 1.]],
                indices: vec![0, 1, 2],
                ..Default::default()
            })
            .unwrap(),
        );
        let parts = [
            (triangle.clone(), math::IDENTITY),
            (triangle, math::translate([5., 0., 2.])),
        ];
        let (positions, indices) = read(&parts);
        assert_eq!(indices.len(), 6);
        let corner = |t: usize, k: usize| positions[indices[t * 3 + k] as usize];
        // Each part's mesh in the reader's order; find the moved one by X.
        let moved = (0..2).find(|&t| corner(t, 0)[0] < -1.).unwrap();
        assert_eq!(
            [corner(moved, 0), corner(moved, 1), corner(moved, 2)],
            [[-5., 0., 2.], [-5., 1., 2.], [-6., 0., 2.]]
        );
        // Nothing to draw is an error, not an empty file.
        assert!(super::glb_of(&[]).is_err());

        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip the shipped model: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let cfg = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";
        let bytes = install.read(cfg).unwrap();
        let Content::Model { parts, .. } =
            super::load_model(&anno117::MODULE, &install, cfg, &bytes)
        else {
            panic!("{cfg} previews as a model");
        };
        let drawn: usize = parts.iter().map(|(m, _)| m.triangles() * 3).sum();
        let (positions, indices) = read(&parts);
        assert_eq!(indices.len(), drawn);
        let [lo, hi] = math::bounds(positions.iter().copied());
        let [want_lo, want_hi] = math::bounds(parts.iter().flat_map(|(m, l)| {
            m.geometry().positions.into_iter().map(|p| {
                let p = math::transform(l, p);
                [-p[0], p[1], p[2]]
            })
        }));
        for i in 0..3 {
            assert!((lo[i] - want_lo[i]).abs() < 1e-3 && (hi[i] - want_hi[i]).abs() < 1e-3);
        }
    }
}
