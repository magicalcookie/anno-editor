// Owns turning a compiled widget tree into canvas nodes: the cross-file
// layout from `phx::layout` (anchors, stacks, pivots), image textures
// (ImageAsset → DDS, cut into nine when it has a slice centre) and label
// text; and the inverse, turning a canvas rectangle change back into
// Position/Scale edits; plus the static part of the game's
// compositing: mask providers clip the Masked siblings before them,
// BlendingMode add/multiply, UV transforms.
// Rotated images are drawn into their bounding box on the CPU (GPUI
// cannot rotate images); rotated labels draw unrotated.
use super::canvas::{Kind, Node, Slices};
use anno_formats::{
    phx::{self, Asset, Edit, Guid, Value, Widget, layout},
    vfs::Install,
};
use gpui_editor_kit::{
    gpui::RenderImage,
    texture::{Dds, Texture},
};
use std::{collections::HashMap, sync::Arc};

/// A hash map for the scene's own keys (guids, node indices, bake keys):
/// foldhash, several times cheaper than SipHash on them.
type FastMap<K, V> = HashMap<K, V, foldhash::fast::RandomState>;

/// UI textures decode at most this large; the canvas rarely shows them bigger.
const IMAGE_MIP: u32 = 1024;
/// What a label shows when its text only arrives with game data.
pub const BOUND_TEXT: &str = "(bound text)";
/// Labels have no stored size; this approximates one line of body text.
const LABEL_HEIGHT: f32 = 28.;

/// Nine-slice pieces, left-to-right, top-to-bottom.
type Pieces = Arc<[Arc<RenderImage>; 9]>;

/// An image asset resolved to pixels.
#[derive(Clone)]
pub struct Image {
    pub texture: Option<Arc<RenderImage>>,
    /// Nine-slice pieces and their margins in image pixels (l, t, r, b).
    pub slices: Option<(Pieces, [f32; 4])>,
    /// The ImageAsset's pixel size: the intrinsic size image widgets scale.
    pub size: [f32; 2],
    /// Every texel is transparent: placeholders and hit areas (`null_16`).
    /// Nothing to draw, and GPUI paints such images opaque white.
    pub blank: bool,
    /// The tinted, blend-adjusted pixels, for masks and CPU compositing.
    pub pixels: Option<Arc<Texture>>,
    /// The texture as stored, shared by every tint: what the GPU draws.
    pub source: Option<Arc<super::gpu::Source>>,
    /// The 9-slice centre in 0..1 when `slices` is set.
    pub centre: [[f32; 2]; 2],
    /// The art's own size: `size`, except for an image set by path, which
    /// keeps the stored asset's box but shows this (FillMode fits by it).
    pub art: [f32; 2],
}

/// Per-node layout facts for turning canvas edits into property edits and
/// for drawing how a widget is placed (`placement`).
#[derive(Clone, Copy, Default)]
pub struct Frame {
    /// Parent's world scale per axis.
    pub parent_scale: [f32; 2],
    /// Full world transforms (`layout::Placed::transform`) of the parent and
    /// of the node's own pixel space.
    pub parent_transform: [f32; 6],
    pub transform: [f32; 6],
    pub position: [f32; 3],
    pub pivot: [f32; 3],
    pub scale: [f32; 3],
    pub rotation: f32,
    pub intrinsic: [f32; 2],
    /// The node's local box (min x, min y, width, height) in its own pixels.
    pub local: [f32; 4],
    pub anchor: u32,
    /// The parent's Size the anchor picks a point of. None when the parent
    /// has none (a plain group): anchors add nothing there.
    pub anchor_size: Option<[f32; 2]>,
    /// The parent's layout component that places this widget itself, so
    /// Position (and anchor) are overridden: "StackLayout" or
    /// "BackgroundLayout".
    pub placed_by: Option<&'static str>,
    /// ProportionalPivotPoint ratio, when that component replaces PivotPoint.
    pub pivot_ratio: Option<[f32; 2]>,
    /// Position or size metrics cancel ancestor scale on this widget.
    pub metrics: bool,
    /// Spans of the stored values; absent ones cannot be edited in place
    /// (adding a property changes the presence mask). All `None` for
    /// widgets that come from another template file.
    pub position_span: Option<usize>,
    pub scale_span: Option<usize>,
    pub pivot_span: Option<usize>,
    pub rotation_span: Option<usize>,
    /// AnchorPoint span and its enum type crc.
    pub anchor_span: Option<(usize, u32)>,
    /// Values data bindings put over the stored ones; editing the stored
    /// value does not show while a binding overrides it.
    pub bound: phx::binding::Bound,
}

/// A decoded ImageAsset before tinting.
struct Decoded {
    texture: Option<Arc<Texture>>,
    /// `texture` as the GPU takes it.
    source: Option<Arc<super::gpu::Source>>,
    /// 9-slice centre in 0..1 of the image.
    uv: [[f32; 2]; 2],
    nine: bool,
    size: [f32; 2],
    blank: bool,
}

/// One of the game's UI fonts (`data/fonts/…`): its family name for GPUI,
/// its bytes to register, and advance widths to size labels by.
pub struct Font {
    pub family: gpui_editor_kit::gpui::SharedString,
    pub bytes: Arc<Vec<u8>>,
    /// Advance per em of the printable Latin range; others use `fallback`.
    advances: HashMap<char, f32>,
    fallback: f32,
    /// One line in ems: hhea ascent − descent + line gap (Albertus Nova
    /// titles 1.442, Lisboa Sans body 1.227). The archive header's hairline
    /// sits under a 29 px line of its 20 px title, as in game.
    line: f32,
}

impl Font {
    fn parse(bytes: Vec<u8>) -> Option<Font> {
        let face = ttf_parser::Face::parse(&bytes, 0).ok()?;
        let em = f32::from(face.units_per_em()).max(1.);
        let family = face
            .names()
            .into_iter()
            .filter(|n| {
                n.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY
                    || n.name_id == ttf_parser::name_id::FAMILY
            })
            .filter(|n| n.is_unicode())
            .min_by_key(|n| n.name_id != ttf_parser::name_id::TYPOGRAPHIC_FAMILY)
            .and_then(|n| n.to_string())?;
        let advance = |c: char| {
            let g = face.glyph_index(c)?;
            Some(f32::from(face.glyph_hor_advance(g)?) / em)
        };
        let advances: HashMap<char, f32> = (' '..='\u{24f}')
            .filter_map(|c| Some((c, advance(c)?)))
            .collect();
        let fallback = advance('n').unwrap_or(0.5);
        let line =
            f32::from(face.ascender()) - f32::from(face.descender()) + f32::from(face.line_gap());
        Some(Font {
            family: family.into(),
            bytes: Arc::new(bytes),
            advances,
            fallback,
            line: if line > 0. {
                line / em
            } else {
                layout::LINE_EM
            },
        })
    }

    /// Width of one line of `text` at `size` pixels per em.
    pub fn width(&self, text: &str, size: f32, letter_spacing: f32) -> f32 {
        let n = text.chars().count() as f32;
        text.chars()
            .map(|c| self.advances.get(&c).copied().unwrap_or(self.fallback))
            .sum::<f32>()
            * size
            + letter_spacing * (n - 1.).max(0.)
    }
}

/// Image and template assets resolved through the install, once per guid;
/// images are rendered once per (guid, ColorTint, BlendingMode).
#[derive(Default)]
pub struct Images {
    decoded: FastMap<Guid, Option<Arc<Decoded>>>,
    images: FastMap<(Guid, [u8; 3], u32), Image>,
    /// Textures by source path, for images a binding sets by path.
    by_path: FastMap<String, Option<Arc<Decoded>>>,
    /// By path, then (tint, blend): looked up by `&str`.
    path_images: FastMap<String, FastMap<([u8; 3], u32), Image>>,
    templates: FastMap<Guid, Option<Arc<Asset>>>,
    /// Screens whose references [`Images::load_references`] loaded.
    prefetched: std::collections::HashSet<Guid>,
    /// Fonts by LocalizedFontAsset guid.
    fonts: FastMap<Guid, Option<Arc<Font>>>,
    /// Fonts loaded since the canvas last registered them with GPUI.
    pub new_fonts: Vec<Arc<Font>>,
    /// What layouts keep from one to the next (per-template work, the
    /// last evaluations of each screen).
    layout: layout::Cache,
    /// Composites by [`bake_key`], with the textures the key names by
    /// address (held so no other texture takes it) and the layout that last
    /// used them. A relayout after an edit redoes only the bakes it changed.
    bakes: FastMap<u64, (Option<(Arc<RenderImage>, [f32; 4])>, Vec<Arc<Texture>>, u64)>,
    layouts: u64,
    /// What each widget's look needs from its file, per template; the
    /// file being laid out is read again each time, as the editor changes
    /// it.
    statics: FastMap<Guid, Arc<Vec<Static>>>,
    /// Per screen, most recent last: what its last layout made of each
    /// tree node (see [`Kept`]).
    kept: Vec<(Option<Guid>, Kept)>,
}

/// What the last layout of a screen made of each node of its tree, for the
/// next one to take over: a node the layout says is the same
/// (`layout::Tree::previous`) keeps its look while its tint and blend are
/// the same, and its canvas node while where it sits is too (a hover or an
/// edit changes a few of a screen's thousand).
#[derive(Default)]
struct Kept {
    /// The view options and localization it was made with.
    view: (bool, bool, bool, bool, usize, Option<(Guid, u64)>),
    /// Per tree node; real where `reach` is set and not taken since.
    looks: Vec<Look>,
    reach: Vec<u64>,
    drawn: Vec<(Option<[u8; 3]>, u32)>,
    /// The intrinsic size and origin the look gave the node.
    set: Vec<(Option<[f32; 2]>, [f32; 2])>,
    around: Vec<Option<Around>>,
    /// The canvas nodes in walk order, and per tree node its one's index
    /// (or `u32::MAX`).
    canvas: Vec<Option<Node>>,
    canvas_at: Vec<u32>,
    laid: Option<layout::Laid>,
}

/// Where a node sits: everything its canvas node takes from outside its
/// own tree node and look.
#[derive(Clone, Copy, PartialEq)]
struct Around {
    placed: layout::Placed,
    parent: Option<[f32; 6]>,
    parent_alpha: f32,
    shown: bool,
    depth: usize,
    clip: u64,
    /// The mask chain's [`Clip::rel`], and where the node sits from the
    /// chain's origin.
    clip_rel: u64,
    clip_at: [f32; 2],
}

/// Screens whose [`Kept`] a layout keeps: as many as the layout cache.
const KEPT_SCREENS: usize = 16;

/// Layouts a composite outlives unused: the game view lays out seven HUD
/// screens in a row, each must find its own again.
const KEPT_BAKES: u64 = 16;

impl Images {
    /// The instances of template `g` in the last layout of root `root`.
    pub fn instances_of(&self, root: Option<Guid>, g: Guid) -> Option<Vec<u64>> {
        Some(self.layout.tree(root)?.instances_of(g))
    }

    /// Forgets the last layout of root `g`, so the next one starts fresh:
    /// after a structural edit the old nodes no longer line up.
    /// What the image caches hold, for the profile log (round 12: two
    /// reviewers' sessions passed 3.5 GB): decoded textures with their GPU
    /// copies, tinted images (pixels and GPUI's copy), composites, and how
    /// many templates are read.
    pub fn footprint(&self) -> String {
        const MB: usize = 1 << 20;
        let render = |r: &RenderImage| {
            let s = r.size(0);
            s.width.0.max(0) as usize * s.height.0.max(0) as usize * 4
        };
        let decoded = self
            .decoded
            .values()
            .chain(self.by_path.values())
            .flatten()
            .map(|d| {
                let texture = d.texture.as_ref().map_or(0, |t| t.rgba.len());
                let source = d.source.as_ref().map_or(0, |s| {
                    // The source's pixels are often the texture itself.
                    let shared = d.texture.as_ref().is_some_and(|t| Arc::ptr_eq(t, &s.rgba));
                    (if shared { 0 } else { s.rgba.rgba.len() })
                        + s.bc
                            .as_ref()
                            .map_or(0, |c| c.mips.iter().map(Vec::len).sum())
                });
                texture + source
            })
            .sum::<usize>();
        let tinted: Vec<&Image> = self
            .images
            .values()
            .chain(self.path_images.values().flat_map(|m| m.values()))
            .collect();
        let tinted_bytes = tinted
            .iter()
            .map(|i| {
                i.texture.as_deref().map_or(0, render)
                    + i.pixels.as_ref().map_or(0, |t| t.rgba.len())
                    + i.slices
                        .as_ref()
                        .map_or(0, |(p, _)| p.iter().map(|r| render(r)).sum())
            })
            .sum::<usize>();
        let bakes = self
            .bakes
            .values()
            .filter_map(|b| b.0.as_ref())
            .map(|(r, _)| render(r))
            .sum::<usize>();
        format!(
            "ui images: decoded {} MB ({}), tinted {} MB ({}), bakes {} MB ({}), templates {}",
            decoded / MB,
            self.decoded.len() + self.by_path.len(),
            tinted_bytes / MB,
            tinted.len(),
            bakes / MB,
            self.bakes.len(),
            self.templates.len()
        )
    }

    pub fn forget(&mut self, g: Option<Guid>) {
        self.layout.forget(g);
        self.kept.retain(|(r, _)| *r != g);
        if let Some(g) = g {
            self.statics.remove(&g);
        }
    }

    pub fn get(
        &mut self,
        install: &Install,
        guid: Guid,
        tint: [u8; 3],
        blend: u32,
    ) -> Option<Image> {
        if let Some(image) = self.images.get(&(guid, tint, blend)) {
            return Some(image.clone());
        }
        let decoded = self
            .decoded
            .entry(guid)
            .or_insert_with(|| load_image(install, guid).map(Arc::new))
            .clone()?;
        let image = render(&decoded, tint, blend);
        self.images.insert((guid, tint, blend), image.clone());
        Some(image)
    }

    /// An image by its source path (`data\ui\…\icon.png`), as image
    /// bindings (builtin 42) set it: as the ImageAsset of that texture has
    /// it (size, 9-slice), else the whole texture at its pixel size.
    pub fn by_path(
        &mut self,
        install: &Install,
        path: &str,
        tint: [u8; 3],
        blend: u32,
    ) -> Option<Image> {
        if let Some(image) = self.path_image(path, tint, blend) {
            return Some(image.clone());
        }
        let decoded = self
            .by_path
            .entry(path.to_owned())
            .or_insert_with(|| load_path(install, path).map(Arc::new))
            .clone()?;
        let image = render(&decoded, tint, blend);
        self.path_images
            .entry(path.to_owned())
            .or_default()
            .insert((tint, blend), image.clone());
        Some(image)
    }

    fn path_image(&self, path: &str, tint: [u8; 3], blend: u32) -> Option<&Image> {
        self.path_images.get(path)?.get(&(tint, blend))
    }

    /// Whether [`Images::prefetch`] has nothing to do for this image.
    fn has(&self, src: Result<Guid, &str>, tint: [u8; 3], blend: u32) -> bool {
        match src {
            Ok(g) => self.images.contains_key(&(g, tint, blend)),
            Err(p) => self.path_image(p, tint, blend).is_some(),
        }
    }

    /// Decodes and tints, on every core, the images of `wanted` not loaded
    /// yet: (ImageAsset, or texture path as [`Images::by_path`] takes it;
    /// tint; blend). A screen opens with hundreds; one at a time, BC7
    /// decoding was a third of its first layout.
    fn prefetch(&mut self, install: &Install, wanted: &[(Result<Guid, &str>, [u8; 3], u32)]) {
        let mut missing: Vec<(Result<Guid, &str>, [u8; 3], u32)> = wanted
            .iter()
            .copied()
            .filter(|&(src, tint, blend)| !self.has(src, tint, blend))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        let mut sources: Vec<Result<Guid, &str>> = missing
            .iter()
            .map(|m| m.0)
            .filter(|src| match src {
                Ok(g) => !self.decoded.contains_key(g),
                Err(p) => !self.by_path.contains_key(*p),
            })
            .collect();
        sources.sort_unstable();
        sources.dedup();
        // Built here, not inside a job: its first use runs jobs of its own,
        // and pool workers that block on it while one builds it would wait
        // for jobs only they could run.
        if sources.iter().any(Result::is_err) {
            image_assets(install);
        }
        let loaded = parallel(&sources, |src| match src {
            Ok(g) => load_image(install, *g),
            Err(p) => load_path(install, p),
        });
        for (src, d) in sources.iter().zip(loaded) {
            let d = d.map(Arc::new);
            match src {
                Ok(g) => self.decoded.insert(*g, d),
                Err(p) => self.by_path.insert((*p).to_owned(), d),
            };
        }
        let jobs: Vec<_> = missing
            .iter()
            .filter_map(|&(src, tint, blend)| {
                let d = match src {
                    Ok(g) => self.decoded.get(&g),
                    Err(p) => self.by_path.get(p),
                };
                Some((src, tint, blend, d?.clone()?))
            })
            .collect();
        let rendered = parallel(&jobs, |(_, tint, blend, d)| render(d, *tint, *blend));
        for ((src, tint, blend, _), image) in jobs.into_iter().zip(rendered) {
            match src {
                Ok(g) => self.images.insert((g, tint, blend), image),
                Err(p) => self
                    .path_images
                    .entry(p.to_owned())
                    .or_default()
                    .insert((tint, blend), image),
            };
        }
    }

    /// The font a LocalizedFontAsset uses for English, loaded once.
    pub fn font(&mut self, install: &Install, localized: Guid) -> Option<Arc<Font>> {
        if let Some(f) = self.fonts.get(&localized) {
            return f.clone();
        }
        let font = self
            .asset(install, localized)
            .and_then(|lf| phx::localized_font(&lf))
            .and_then(|ft| self.asset(install, ft))
            .and_then(|ft| phx::font_file(&ft))
            .and_then(|path| {
                let path = path.replace('\\', "/");
                install
                    .read(&path)
                    .or_else(|_| install.read(&path.to_ascii_lowercase()))
                    .ok()
            })
            .and_then(Font::parse)
            .map(Arc::new);
        if let Some(f) = &font {
            self.new_fonts.push(f.clone());
        }
        self.fonts.insert(localized, font.clone());
        font
    }

    /// Any compiled UI asset by guid (container templates, text styles,
    /// image and video assets).
    /// Puts `asset` in place of the shipped template `guid`, for a
    /// template edited in place. Returns whether anything changed; the
    /// caller then lays the root out afresh (its kept tree was built from
    /// the old template).
    pub fn put(&mut self, guid: Guid, asset: &Asset) -> bool {
        if self
            .templates
            .get(&guid)
            .and_then(Option::as_ref)
            .is_some_and(|a| a.bytes() == asset.bytes())
        {
            return false;
        }
        self.templates.insert(guid, Some(Arc::new(asset.clone())));
        self.layout.forget(Some(guid));
        self.statics.remove(&guid);
        true
    }

    /// Template `guid` if it was read already; never reads.
    pub fn templates_loaded(&self, guid: Guid) -> Option<Arc<Asset>> {
        self.templates.get(&guid).cloned().flatten()
    }

    pub fn asset(&mut self, install: &Install, guid: Guid) -> Option<Arc<Asset>> {
        if let Some(a) = self.templates.get(&guid) {
            return a.clone();
        }
        let asset = read_asset(install, guid).map(Arc::new);
        self.templates.insert(guid, asset.clone());
        if let Some(a) = &asset {
            phx::register_palette(a);
            self.palettes_of(install, a);
        }
        asset
    }

    /// Loads every asset `asset` refers to, and what those refer to in
    /// turn, a level at a time on all cores: a screen's first layout then
    /// finds its templates parsed instead of reading them one by one as it
    /// meets them (the construction menu: 420 of them, 15 levels deep).
    /// Font data stays out: FontDescriptor assets hold whole font files,
    /// which nothing here reads. Once per screen.
    pub fn load_references(&mut self, install: &Install, asset: &Asset) {
        const CLASS_FONT_DESCRIPTOR: u32 = phx::name_crc(b"FontDescriptorAsset");
        if !asset.guid().is_some_and(|g| self.prefetched.insert(g)) {
            return;
        }
        let refs = |a: &Asset| -> Vec<Guid> {
            a.cross_refs()
                .assets
                .into_iter()
                .filter(|&(class, ..)| class != CLASS_FONT_DESCRIPTOR)
                .map(|(_, g, _)| g)
                .collect()
        };
        let mut level = refs(asset);
        let mut loaded = Vec::new();
        loop {
            level.sort_unstable();
            level.dedup();
            level.retain(|g| !self.templates.contains_key(g));
            if level.is_empty() {
                break;
            }
            let read = parallel(&level, |&g| {
                let a = read_asset(install, g).map(Arc::new);
                let next = a.as_deref().map(refs).unwrap_or_default();
                (a, next)
            });
            let mut next = Vec::new();
            for (&g, (a, refs)) in level.iter().zip(read) {
                if let Some(a) = &a {
                    phx::register_palette(a);
                    loaded.push(a.clone());
                }
                self.templates.insert(g, a);
                next.extend(refs);
            }
            level = next;
        }
        // Widget trees once every palette they resolve against is in. An
        // asset no reference names (a native template building asks for)
        // is read here as building would.
        let templates = &self.templates;
        self.layout.warm(&loaded, &|g| match templates.get(&g) {
            Some(a) => a.clone(),
            None => read_asset(install, g).map(Arc::new),
        });
    }

    /// Loads (and so registers) the colour palettes `asset` refers to, so
    /// its ColorTypes resolve to palette colours.
    pub fn palettes_of(&mut self, install: &Install, asset: &Asset) {
        const CLASS_PALETTE: u32 = phx::name_crc(b"ColorPaletteAsset");
        for (class, guid, _) in asset.cross_refs().assets {
            if class == CLASS_PALETTE && !self.templates.contains_key(&guid) {
                self.asset(install, guid);
            }
        }
    }
}

/// A compiled UI asset by guid: templates are under `generated/`, image
/// assets under `images/`.
pub(super) fn read_asset(install: &Install, guid: Guid) -> Option<Asset> {
    let bytes = install
        .read(&format!("data/ui/studio/generated/{guid}"))
        .or_else(|_| install.read(&format!("data/ui/studio/images/{guid}")))
        .ok()?;
    Asset::parse(bytes).ok()
}

/// The texture of a UI source path. Sources are named `.png`; the game ships
/// `<stem>_0.dds`. Assets are authored against `data/ui/fhd/`, and the game
/// ships only the `4k` set (twice the pixels, same UV rects).
fn load_texture(install: &Install, path: &str) -> Option<super::gpu::Source> {
    let source = path.replace('\\', "/");
    let stem = source.rsplit_once('.').map_or(source.as_str(), |(s, _)| s);
    let lower = stem.to_ascii_lowercase();
    [stem, &lower]
        .iter()
        .flat_map(|stem| [stem.to_string(), stem.replacen("/fhd/", "/4k/", 1)])
        .find_map(|stem| install.read(&format!("{stem}_0.dds")).ok())
        .and_then(|b| {
            let dds = Dds::parse(&b).ok()?;
            Some(super::gpu::Source {
                rgba: Arc::new(dds.decode(&b, dds.mip_for(IMAGE_MIP)).ok()?),
                // Only the GPU reads the stored chain.
                bc: super::gpu::enabled()
                    .then(|| dds.compressed(&b).ok())
                    .flatten(),
            })
        })
}

/// How texture paths compare: `data/ui/…`, lower case, forward slashes.
fn path_key(path: &str) -> String {
    path.replace('\\', "/").to_ascii_lowercase()
}

/// Every ImageAsset of the install by its texture path ([`path_key`]),
/// read once per process (about 940 assets; one install per process).
/// Building it runs [`parallel`] jobs: first use must not be from one.
fn image_assets(install: &Install) -> &'static HashMap<String, Guid> {
    static INDEX: std::sync::OnceLock<HashMap<String, Guid>> = std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        let mut out = HashMap::new();
        let Some(folder) = install.folder("data/ui/studio/generated") else {
            return out;
        };
        // On every core: a screen's first image loads all wait on this.
        let found = parallel(install.folder_files(folder), |&f| {
            let bytes = install.read_file(f).ok()?;
            if !bytes.starts_with(b"PHXIM") {
                return None;
            }
            let asset = Asset::parse(bytes).ok()?;
            Some((path_key(&phx::image_info(&asset)?.path), asset.guid()?))
        });
        // In folder order, the first asset of a texture wins.
        for (path, g) in found.into_iter().flatten() {
            out.entry(path).or_insert(g);
        }
        out
    })
}

/// A texture by source path, as an ImageAsset with it has it: that knows
/// its design size (4k textures are larger than it) and 9-slice centre.
/// Else the whole texture at its pixel size.
fn load_path(install: &Install, path: &str) -> Option<Decoded> {
    if let Some(&guid) = image_assets(install).get(&path_key(path)) {
        return load_image(install, guid);
    }
    let source = Arc::new(load_texture(install, path)?);
    let texture = source.rgba.clone();
    Some(Decoded {
        blank: false,
        size: [texture.width as f32, texture.height as f32],
        texture: Some(texture),
        source: Some(source),
        uv: [[0., 0.], [1., 1.]],
        nine: false,
    })
}

fn load_image(install: &Install, guid: Guid) -> Option<Decoded> {
    let bytes = install
        .read(&format!("data/ui/studio/generated/{guid}"))
        .or_else(|_| install.read(&format!("data/ui/studio/images/{guid}")))
        .ok()?;
    let info = phx::image_info(&Asset::parse(bytes).ok()?)?;
    let source = load_texture(install, &info.path).map(Arc::new);
    let texture = source.as_ref().map(|s| s.rgba.clone());
    let blank = texture
        .as_ref()
        .is_some_and(|t| t.rgba.iter().skip(3).step_by(4).all(|&a| a == 0));
    Some(Decoded {
        blank,
        texture,
        source,
        uv: info.slice,
        // The slice rect is the stretchable centre; the full rect means a
        // plain image.
        nine: info.slice != [[0., 0.], [1., 1.]],
        size: info.size,
    })
}

/// BlendingMode values (enum `0x1bf32b74`). The draw's blend byte picks the
/// phoenix_gui.fx state block through `dword_1453F4050` (0 NoAlpha,
/// 1 Standard, 2 Add, 3 Multiply, 14 NoColorWrite for masks); widget value 1
/// means "the parent's".
const BLEND_INHERIT: u32 = 1;
const BLEND_ADD: u32 = 2;
const BLEND_MULTIPLY: u32 = 3;

/// ColorTint multiplies RGB, like the game's widget shader. GPUI only
/// blends source-over, so add and multiply are rewritten into source-over
/// texels that land close: additive `dst + s·a` becomes colour `s / m` at
/// alpha `a·m` (m = brightest channel), which brightens and barely darkens;
/// multiply `dst · s` becomes black at alpha `a·(1 − luma(s))`, exact for
/// grey sources.
fn shaded(t: &Texture, tint: [u8; 3], blend: u32) -> Texture {
    if tint == [255; 3] && blend != BLEND_ADD && blend != BLEND_MULTIPLY {
        return t.clone();
    }
    let mut out = t.clone();
    for px in out.rgba.as_chunks_mut::<4>().0 {
        for c in 0..3 {
            px[c] = (u16::from(px[c]) * u16::from(tint[c]) / 255) as u8;
        }
        match blend {
            BLEND_ADD => {
                let m = px[0].max(px[1]).max(px[2]);
                if m == 0 {
                    px[3] = 0;
                    continue;
                }
                for v in &mut px[..3] {
                    *v = (u16::from(*v) * 255 / u16::from(m)) as u8;
                }
                px[3] = (u16::from(px[3]) * u16::from(m) / 255) as u8;
            }
            BLEND_MULTIPLY => {
                let luma =
                    (u32::from(px[0]) * 54 + u32::from(px[1]) * 183 + u32::from(px[2]) * 19) >> 8;
                px[3] = (u32::from(px[3]) * (255 - luma) / 255) as u8;
                px[..3].fill(0);
            }
            _ => {}
        }
    }
    out
}

fn render(d: &Decoded, tint: [u8; 3], blend: u32) -> Image {
    let Some(full) = &d.texture else {
        return Image {
            texture: None,
            slices: None,
            size: d.size,
            blank: d.blank,
            pixels: None,
            source: None,
            centre: d.uv,
            art: d.size,
        };
    };
    let t = shaded(full, tint, blend);
    if !d.nine {
        return Image {
            texture: Some(t.render_image()),
            slices: None,
            size: d.size,
            blank: d.blank,
            pixels: Some(Arc::new(t)),
            source: d.source.clone(),
            centre: d.uv,
            art: d.size,
        };
    }
    let [[u0, v0], [u1, v1]] = d.uv;
    let cuts_u = [0., u0, u1, 1.];
    let cuts_v = [0., v0, v1, 1.];
    let pieces: [Arc<RenderImage>; 9] = std::array::from_fn(|i| {
        let (cx, cy) = (i % 3, i / 3);
        crop(
            &t,
            [[cuts_u[cx], cuts_v[cy]], [cuts_u[cx + 1], cuts_v[cy + 1]]],
        )
        .render_image()
    });
    let margins = [
        u0 * d.size[0],
        v0 * d.size[1],
        (1. - u1) * d.size[0],
        (1. - v1) * d.size[1],
    ];
    Image {
        texture: Some(t.render_image()),
        slices: Some((Arc::new(pieces), margins)),
        size: d.size,
        blank: d.blank,
        pixels: Some(Arc::new(t)),
        source: d.source.clone(),
        centre: d.uv,
        art: d.size,
    }
}

/// A UV rectangle of a texture as its own image.
fn crop(t: &Texture, uv: [[f32; 2]; 2]) -> Texture {
    let (w, h) = (t.width as f32, t.height as f32);
    let x0 = ((uv[0][0].clamp(0., 1.) * w) as u32).min(t.width - 1);
    let y0 = ((uv[0][1].clamp(0., 1.) * h) as u32).min(t.height - 1);
    let x1 = ((uv[1][0].clamp(0., 1.) * w).ceil() as u32).clamp(x0 + 1, t.width);
    let y1 = ((uv[1][1].clamp(0., 1.) * h).ceil() as u32).clamp(y0 + 1, t.height);
    if (x0, y0, x1, y1) == (0, 0, t.width, t.height) {
        return t.clone();
    }
    let mut rgba = Vec::with_capacity(((x1 - x0) * (y1 - y0) * 4) as usize);
    for y in y0..y1 {
        let row = (y * t.width) as usize * 4;
        rgba.extend_from_slice(&t.rgba[row + x0 as usize * 4..row + x1 as usize * 4]);
    }
    Texture::from_rgba(x1 - x0, y1 - y0, rgba).unwrap_or_else(|_| t.clone())
}

fn prop_span(widget: &Widget, name: &str) -> Option<usize> {
    widget
        .props
        .iter()
        .find(|p| p.name == Some(name) && !matches!(p.value, Value::Other | Value::Null))
        .map(|p| p.span)
}

/// `ReferenceSize` of a template root, if stored.
fn reference_size(asset: &Asset) -> Option<[f32; 2]> {
    let (root, _) = asset.objects().next()?;
    asset
        .object_props(root)
        .into_iter()
        .find_map(|p| match (p.name, p.value) {
            (Some("ReferenceSize"), Value::Vec2(v)) => Some(v),
            (Some("ReferenceSize"), Value::Vec3([x, y, _])) => Some([x, y]),
            _ => None,
        })
}

fn enum_prop(widget: &Widget, name: &str) -> u32 {
    widget
        .props
        .iter()
        .find_map(|p| match (p.name, &p.value) {
            (Some(n), Value::Enum { value, .. }) if n == name => Some(*value),
            _ => None,
        })
        .unwrap_or(0)
}

/// How to show a file: design view (faded-in widgets opaque), data
/// bindings evaluated, and a timeline clip sampled.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub design: bool,
    pub bindings: bool,
    pub play: Option<layout::Play>,
    /// Fake game data for template properties (the Preview data section).
    pub preview: layout::Preview,
    /// Opening clips applied at their end (`layout::Options::shown`).
    pub shown: bool,
    /// Interaction states held on instances (`layout::Options::sims`).
    pub sims: Vec<layout::Sim>,
    /// Plausible game values for properties nothing sets
    /// (`preview::sample_value`).
    pub sample: bool,
    /// The template instance being edited in place (its file and instance
    /// key): its widgets are the editable ones instead of the root file's.
    pub edit: Option<(Guid, u64)>,
    /// Instances to rebuild although nothing they are fed changed: those
    /// of a template edited in place (`layout::Options::stale`).
    pub stale: Vec<u64>,
}

/// One file instance's evaluated bindings: what each binding read and
/// wrote, and its template property values.
pub struct FileEval {
    pub trace: Arc<phx::binding::Traces>,
    inputs: Option<layout::Values>,
    outputs: Option<layout::Values>,
}

impl FileEval {
    /// A template property's value: what the instance's bindings wrote,
    /// else what its container passed in.
    pub fn value(&self, uid: u32) -> Option<&phx::DefaultValue> {
        [&self.outputs, &self.inputs]
            .into_iter()
            .find_map(|v| v.as_ref()?.get(&uid))
    }
}

/// Lays out every widget, expanding containers from their template files
/// (`phx::layout`: anchors in the parent's Size, stacks, pivots, metrics).
/// `names` turns class crcs into labels; `texts` resolves LocalizationIds.
pub fn layout(
    asset: &Asset,
    install: &Install,
    images: &mut Images,
    texts: &HashMap<i64, String>,
    view: View,
    name: impl Fn(u32) -> String,
) -> (Vec<Node>, Vec<Frame>) {
    // An untyped file (a class version the schema lacks, as after a game
    // patch) has no widget tree: it opens read-only and draws nothing.
    if asset.widgets().is_empty() {
        return (Vec::new(), Vec::new());
    }
    if view.sample {
        super::preview::load_scene_content(install);
    }
    images.load_references(install, asset);
    images.palettes_of(install, asset);
    let design = view.design;
    let sample = view.sample;
    let opts = layout::Options {
        bindings: view.bindings,
        play: view.play,
        preview: view.preview,
        shown: view.shown,
        sims: view.sims,
        sample: view
            .sample
            .then_some(super::preview::sample_value as layout::Sampler),
        sample_generation: super::preview::game_data_generation(),
        stale: view.stale,
        ..Default::default()
    };
    // Bindings that read a label's size get it measured with its font, as
    // the label draws.
    let mut cache = std::mem::take(&mut images.layout);
    let cell = std::cell::RefCell::new(&mut *images);
    let mut tree = layout::Tree::build_cached(
        asset,
        &mut |g| cell.borrow_mut().asset(install, g),
        &mut |style, text| {
            let mut im = cell.borrow_mut();
            let s = im.asset(install, style).and_then(|a| phx::text_style(&a))?;
            let font = im.font(install, s.font?.id)?;
            let size = s.size.max(4.);
            Some(font.width(text, size, s.letter_spacing * size / 1000.))
        },
        &opts,
        &mut cache,
    );
    images.layout = cache;
    if tree.nodes.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // Which instance is edited decides which nodes are editable and have
    // edit frames: kept nodes from another scope would be wrong.
    let view_key = (
        design,
        sample,
        view.bindings,
        view.shown,
        texts.len(),
        view.edit,
    );
    let mut old = match images.kept.iter().position(|(r, _)| *r == asset.guid()) {
        Some(at) => images.kept.remove(at).1,
        None => Kept::default(),
    };
    if old.view != view_key {
        old = Kept::default();
    }
    let prev = tree.previous().to_vec();
    let was = |i: usize| match prev.get(i) {
        Some(&m) if m != layout::NOT_KEPT => Some(m as usize),
        _ => None,
    };
    let mut set: Vec<(Option<[f32; 2]>, [f32; 2])> = vec![(None, [0.; 2]); tree.nodes.len()];
    // Nodes whose look came over from the last layout.
    let mut same_look = vec![0u64; tree.nodes.len().div_ceil(64)];
    let files: Vec<&Asset> = std::iter::once(asset)
        .chain(tree.templates.iter().map(|t| &**t))
        .collect();
    // Per file: its widgets as the layout parsed them, and what each
    // widget's look needs from the file, kept per template across layouts
    // (a template expands hundreds of times). One lookup per distinct
    // file: `files` holds every asset for the whole loop, so addresses
    // stay unique.
    if let Some(g) = asset.guid() {
        images.statics.remove(&g);
    }
    let mut by_file: FastMap<usize, (Arc<layout::Parsed>, Arc<Vec<Static>>)> = FastMap::default();
    let (lists, statics): (Vec<_>, Vec<_>) = files
        .iter()
        .map(|f| {
            by_file
                .entry(*f as *const Asset as usize)
                .or_insert_with(|| {
                    let parsed = f
                        .guid()
                        .and_then(|g| images.layout.parsed(g))
                        .unwrap_or_else(|| Arc::new((f.widgets().to_vec(), Vec::new())));
                    let statics = match f.guid() {
                        Some(g) => match images.statics.get(&g) {
                            Some(s) => s.clone(),
                            None => {
                                let s = Arc::new(Static::of_file(f, &parsed.0, install, images));
                                images.statics.insert(g, s.clone());
                                s
                            }
                        },
                        None => Arc::new(Static::of_file(f, &parsed.0, install, images)),
                    };
                    (parsed, statics)
                })
                .clone()
        })
        .unzip();
    // The game sizes every scene root 1920×1080; other
    // templates lay out in their own ReferenceSize.
    let screen = reference_size(asset).unwrap_or([1920., 1080.]);

    // What each node draws; images fix their intrinsic size from the
    // decoded asset before the layout runs.
    let mut look: Vec<Look> = Vec::with_capacity(tree.nodes.len());
    // What the build gave the boxes images and labels size here, to hand
    // the tree back as built.
    let mut built: Vec<(usize, Option<[f32; 2]>, [f32; 2])> = Vec::with_capacity(tree.nodes.len());
    // ColorTint multiplies down the tree like alpha: the badge circle
    // tints its Stroke and Base containers and leaves their half-circle
    // and square images white. White means untinted.
    // The nodes the scene handles (`reach`): the root, the open file's
    // own widgets, and what sits under nothing hidden but them. Most of a
    // screen is templates under hidden widgets, expanded only for the
    // sizes and outputs bindings read (16k of the transfer screen's 17k
    // nodes); nothing of them draws. `down`: the reached nodes whose
    // children are reached too. Parents come before their children in the
    // node list.
    let count = tree.nodes.len();
    let mut reach = vec![0u64; count.div_ceil(64)];
    let mut down = vec![0u64; count.div_ceil(64)];
    reach[0] = 1;
    for i in 0..count {
        let n = &tree.nodes[i];
        if bit(&reach, i) && (i == 0 || tree.source[i].0 == 0 || n.geometry.visible) {
            down[i / 64] |= 1 << (i % 64);
            for &c in &n.children {
                reach[c / 64] |= 1 << (c % 64);
            }
        }
    }
    let keys = tree.instance_keys();
    // Whether node `i` is a widget of the file being edited: the root's
    // own, or those of the template instance edited in place.
    let edit = view.edit;
    let own = |i: usize| match edit {
        None => tree.source[i].0 == 0,
        Some((g, key)) => {
            let f = tree.source[i].0;
            f != 0
                && tree.templates[f - 1].guid() == Some(g)
                && tree.instance[i].and_then(|c| keys.get(&c)) == Some(&key)
        }
    };
    // Per node: the tint it draws with and its blend mode.
    let mut tints: Vec<[u8; 3]> = Vec::with_capacity(tree.nodes.len());
    let mut drawn: Vec<(Option<[u8; 3]>, u32)> = Vec::with_capacity(tree.nodes.len());
    for (i, &(file, k)) in tree.source.iter().enumerate() {
        if !bit(&reach, i) {
            tints.push([255; 3]);
            drawn.push((None, 0));
            continue;
        }
        let st = &statics[file][k];
        let mut tint = tree.nodes[i].geometry.tint;
        // Game code tints player-colour layers (the governor portrait's
        // disc); sample data shows the reference player's red.
        if sample && tint.is_none_or(|t| t == [255; 3]) && st.player_color {
            tint = Some(SAMPLE_PLAYER_COLOR);
        }
        let inherited = tree.parent[i].map_or([255; 3], |p| tints[p]);
        let own = tint.unwrap_or([255; 3]);
        let total: [u8; 3] =
            std::array::from_fn(|c| (u16::from(own[c]) * u16::from(inherited[c]) / 255) as u8);
        tints.push(total);
        if inherited != [255; 3] {
            tint = Some(total);
        }
        let blend = match st.blend {
            BLEND_INHERIT => tree.parent[i].map_or(0, |p| drawn[p].1),
            b => b,
        };
        drawn.push((tint, blend));
    }
    let wanted: Vec<_> = tree
        .source
        .iter()
        .enumerate()
        .filter(|&(i, _)| bit(&reach, i))
        // A look that comes over has its images.
        .filter(|&(i, _)| {
            !was(i).is_some_and(|m| {
                old.drawn.get(m) == Some(&drawn[i])
                    && old
                        .reach
                        .get(m / 64)
                        .is_some_and(|w| w >> (m % 64) & 1 == 1)
            })
        })
        .flat_map(|(i, &(file, k))| {
            let (tint, blend) = drawn[i];
            let tint = tint.unwrap_or([255; 3]);
            let image = statics[file][k].image.as_ref();
            let by_id = image
                .and_then(|im| im.asset)
                .map(|r| (Ok(r.id), tint, blend));
            let by_path = image
                .and_then(|im| {
                    tree.nodes[i]
                        .text
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .or(im.path.as_deref())
                })
                .map(|p| (Err(p), tint, blend));
            by_id.into_iter().chain(by_path)
        })
        .filter(|&(src, tint, blend)| !images.has(src, tint, blend))
        .collect();
    if !wanted.is_empty() {
        images.prefetch(install, &wanted);
    }
    for (i, &(file, k)) in tree.source.iter().enumerate() {
        if !bit(&reach, i) {
            look.push(Look::none());
            continue;
        }
        if let Some(m) = was(i)
            && old.drawn.get(m) == Some(&drawn[i])
            && old
                .reach
                .get(m / 64)
                .is_some_and(|w| w >> (m % 64) & 1 == 1)
        {
            // Taken: one new node per old one.
            old.reach[m / 64] &= !(1 << (m % 64));
            let l = std::mem::replace(&mut old.looks[m], Look::none());
            let node = &mut tree.nodes[i];
            built.push((i, node.intrinsic, node.origin));
            (node.intrinsic, node.origin) = old.set[m];
            set[i] = old.set[m];
            look.push(l);
            same_look[i / 64] |= 1 << (i % 64);
            continue;
        }
        let st = &statics[file][k];
        let (tint, blend) = drawn[i];
        let widget_image = st.image.clone();
        let image = widget_image
            .as_ref()
            .and_then(|im| im.asset)
            .and_then(|r| images.get(install, r.id, tint.unwrap_or([255; 3]), blend));
        // A binding set the image by path: its texture, at the widget's own
        // image size (the layout sized it by that). A path sets no new
        // ImageAsset, so the native box keeps the stored
        // asset's size: the governors bar's portrait mosaic stays 104 px.
        // A binding's path, else the one the widget stores.
        let path = widget_image.as_ref().and_then(|im| {
            tree.nodes[i]
                .text
                .as_deref()
                .filter(|p| !p.is_empty())
                .or(im.path.as_deref())
        });
        let image = match (&widget_image, path) {
            (Some(_), Some(path)) => images
                .by_path(install, path, tint.unwrap_or([255; 3]), blend)
                .map(|mut im| {
                    if let Some(own) = &image {
                        im.art = im.size;
                        im.size = own.size;
                    }
                    im
                })
                .or(image),
            _ => image,
        };
        // Inside an item a radial list placed with sample data (at any
        // depth of templates).
        let placed = |i: usize| {
            let mut at = tree.instance[i];
            for _ in 0..8 {
                let Some(n) = at else { return false };
                if tree.placed.contains(&n) {
                    return true;
                }
                at = tree.instance[n];
            }
            false
        };
        // A hover zone's own images only show the zone in the designer;
        // containers under it (a whole stat element that opens an infotip)
        // are content. Radial items the sample layout placed are not.
        let runtime = (st.radial_item && !placed(i))
            || (st.infotip_area && st.image.is_some())
            || st.drop_zone;
        // Stored label layout, with what bindings wrote per axis (its
        // strings are read below, from the stored label).
        let label = st.label.as_ref().map(|l| {
            let mut l = phx::LabelText {
                text: String::new(),
                displayed_text: String::new(),
                localization_id: String::new(),
                ..*l
            };
            let b = &tree.nodes[i].bound;
            for a in 0..2 {
                l.sizing[a] = b.sizing[a].unwrap_or(l.sizing[a]);
                l.alignment[a] = b.alignment[a].unwrap_or(l.alignment[a]);
                l.overflow[a] = b.overflow[a].unwrap_or(l.overflow[a]);
            }
            l.line_spacing = b.line_spacing.unwrap_or(l.line_spacing);
            l
        });
        // Label text: what a binding put there, the literal, then the
        // localized line, else a placeholder for game data.
        let bound_text = tree.nodes[i].text.clone();
        let bound_loc = tree.nodes[i]
            .loc
            .as_deref()
            .and_then(phx::localization_line_id)
            .and_then(|id| texts.get(&id).cloned());
        let text = st.label.as_ref().map(|l| {
            if let Some(t) = bound_loc.as_ref().or(bound_text.as_ref()) {
                return t.clone();
            }
            [&l.displayed_text, &l.text]
                .into_iter()
                .find(|t| !t.is_empty())
                .cloned()
                .or_else(|| {
                    phx::localization_line_id(&l.localization_id)
                        .and_then(|id| texts.get(&id).cloned())
                })
                .unwrap_or_else(|| BOUND_TEXT.into())
        });
        let style = st.style.clone();
        let font_size = style
            .as_ref()
            .map_or(LABEL_HEIGHT * 0.7, |s| s.size.max(4.));
        let font = st.font.clone();
        // LetterSpacing is tracking in thousandths of an em: shipped styles
        // use 0, 10 and 20 on 14–42 px fonts. Read as pixels, it widened
        // "19,743" by 100 px and the minimap's lode badges, sized from
        // their label, grew to 200 px discs.
        // TODO(alex): unit inferred from the shipped values, not the exe.
        let letter_spacing = style
            .as_ref()
            .map_or(0., |s| s.letter_spacing * font_size / 1000.);
        let mut measure = |t: &str| match &font {
            Some(f) => f.width(t, font_size, letter_spacing),
            None => t.chars().count().max(1) as f32 * font_size * 0.52,
        };
        // At least 1.25 em, what shipped body labels were sized with (their
        // font's own metrics say 1.227); taller fonts take their own.
        let line = font_size
            * font
                .as_ref()
                .map_or(layout::LINE_EM, |f| f.line.max(layout::LINE_EM));
        // The lines travel '\n'-joined; the box is used where the file
        // stores no DisplayedSize.
        let plane = label.as_ref().map_or([0.; 2], |l| {
            let b = &tree.nodes[i].bound.text_plane;
            [0, 1].map(|a| b[a].unwrap_or(l.text_plane[a]))
        });
        let (text, text_box) = match (&label, text) {
            (Some(l), Some(t)) => {
                let (mut lines, size) = layout::label_box(l, &t, line, plane, &mut measure);
                let text = if lines.len() == 1 {
                    lines.pop()
                } else {
                    Some(lines.join("\n"))
                };
                (text, Some(size))
            }
            (Some(l), None) => (
                None,
                Some(layout::label_box(l, "Text", line, plane, &mut measure).1),
            ),
            (None, t) => (t, None),
        };
        let node = &mut tree.nodes[i];
        built.push((i, node.intrinsic, node.origin));
        let kind = match (&image, &label) {
            (Some(img), _) => {
                node.intrinsic = Some(img.size);
                Kind::Image
            }
            (None, Some(_)) => {
                // Shipped labels store DisplayedSize, 0×0 on those whose text
                // only arrives at runtime; those and new ones get one line at
                // the style's font size until glyphs are measured.
                if node.intrinsic.is_none_or(|s| s[0] <= 0. || s[1] <= 0.) {
                    node.intrinsic = text_box;
                }
                if let (Some(l), Some(size)) = (&label, node.intrinsic) {
                    node.origin = layout::label_origin(l, plane, size);
                }
                Kind::Label
            }
            _ => Kind::Container,
        };
        set[i] = (node.intrinsic, node.origin);
        look.push(Look {
            font: font.map(|f| f.family.clone()),
            runtime,
            kind,
            image,
            text: text.map(Into::into),
            font_size,
            line_spacing: label.as_ref().map_or(1., |l| l.line_spacing),
            line,
            align: label.as_ref().map_or([0; 2], |l| l.alignment),
            overflow: label.as_ref().map_or(0, |l| l.overflow[0]),
            // ColorTint multiplies what a widget draws, a label's style
            // colour too: the governors bar tints its white-styled values
            // with `-Text_Color` (174, 160, 144).
            color: if kind == Kind::Label {
                match (style.as_ref().and_then(|s| s.color), tint) {
                    (Some(s), Some(t)) => Some(std::array::from_fn(|c| {
                        (u16::from(s[c]) * u16::from(t[c]) / 255) as u8
                    })),
                    (s, t) => s.or(t),
                }
            } else {
                tint
            },
            // ShadowDistance along ShadowAngle, degrees clockwise from +x
            // with y down: the HUD's 1.5 px at 55° falls below and right.
            // TODO(alex): direction read off the reference shot, not traced.
            shadow: style
                .as_ref()
                .filter(|_| kind == Kind::Label)
                .and_then(|s| {
                    let a = s.shadow_angle.to_radians();
                    Some(super::canvas::Shadow {
                        color: s.shadow?,
                        alpha: s.shadow_alpha,
                        offset: [s.shadow_distance * a.cos(), s.shadow_distance * a.sin()],
                    })
                }),
            mask: st.mask,
            mask_fx: st.mask_fx,
            masked: st.masked,
            backdrop: st.backdrop,
            address: st.address,
            uv: {
                // Stored UVOffset/UVScale, with what bindings wrote per axis.
                let (mut o, mut sc) =
                    widget_image.map_or(([0.; 2], [1.; 2]), |im| (im.uv_offset, im.uv_scale));
                let b = &tree.nodes[i].bound;
                for a in 0..2 {
                    o[a] = b.uv_offset[a].unwrap_or(o[a]);
                    sc[a] = b.uv_scale[a].unwrap_or(sc[a]);
                }
                (o, sc)
            },
            uv_rotation: st.image.as_ref().map_or(0., |im| im.uv_rotation),
            fill: st.image.as_ref().map_or(0, |im| im.fill),
        });
    }
    // From the last layout's: a node is as it was if the layout says so
    // and its look sized it the same.
    let laid = layout::evaluate_since(
        &tree.nodes,
        0,
        screen,
        &|i| bit(&down, i),
        &prev,
        old.laid.as_ref(),
        &|i| was(i).is_none_or(|m| old.set.get(m) != Some(&set[i])),
    );
    let placed = &laid.placed;
    let clips = clips(&tree, placed, &look, design, &down);

    // What the walk emits: about the nodes it reaches.
    let reached = reach.iter().map(|w| w.count_ones() as usize).sum();
    let mut nodes = Vec::with_capacity(reached);
    let mut frames = Vec::with_capacity(reached);
    // Composites to do once the node list stands: they are independent and
    // run on every core (the diplomacy screen bakes 273).
    let mut bakes: Vec<Bake> = Vec::new();
    // Pre-order: (tree node, depth, inherited visibility, inherited alpha).
    let mut chains: FastMap<usize, Arc<[(Guid, u64)]>> = FastMap::default();
    // Per canvas node, its tree node; per tree node, where it sat.
    let mut tree_of: Vec<usize> = Vec::with_capacity(reached);
    let mut around: Vec<Option<Around>> = vec![None; tree.nodes.len()];
    let mut stack = Vec::with_capacity(256);
    stack.push((0usize, 0usize, true, 1f32));
    while let Some((i, depth, shown, parent_alpha)) = stack.pop() {
        let (file, k) = tree.source[i];
        let widget = &lists[file].0[k];
        let n = &tree.nodes[i];
        let g = &n.geometry;
        let p = &placed[i];
        let l = &look[i];
        let root = i == 0;
        let blank = l.image.as_ref().is_some_and(|i| i.blank);
        // Mask providers only write stencil (sbPhoenix_NoColorWrite) and the
        // background-blur effect paints the blurred 3D scene through the
        // texture's alpha: neither shows anything the editor can draw.
        let visible =
        // IsHidden is the designer's eye toggle: node setup
        // never reads it, and the minimap ring's sector icons, hidden in
        // the files, show in game. Design view keeps hiding them.
            shown && (root || (g.visible && !(design && g.hidden) && l.mask & 1 == 0 && !blank && !l.backdrop));
        // Screens often start transparent and fade in by animation (the
        // main title's `Content` is stored at AlphaTint 0). Design view
        // shows such widgets as authored instead of as the first frame.
        // Effect images (glows, shines) also rest at 0 and only flash by
        // animation; they stay hidden so they do not cover the panel.
        let own_alpha = if design && g.alpha < 0.01 && l.kind != Kind::Image {
            1.
        } else {
            g.alpha
        };
        let alpha = if root { 1. } else { parent_alpha * own_alpha };
        let t = p.transform;
        let here = Around {
            placed: *p,
            // What 9-slice borders scale by (`border_scale`).
            parent: tree.parent[i].map(|q| placed[q].transform),
            parent_alpha,
            shown,
            depth,
            clip: clips[i].as_ref().map_or(0, |c| c.key),
            clip_rel: clips[i].as_ref().map_or(0, |c| c.rel),
            clip_at: clips[i].as_ref().map_or([0.; 2], |c| {
                [p.transform[4] - c.origin[0], p.transform[5] - c.origin[1]]
            }),
        };
        around[i] = Some(here);
        // The same node with the same look, in the same place: the same
        // canvas node. Moved but not turned or scaled, with no mask in the
        // way (a drag): the same, where it is now.
        let shifted = |o: &Around| {
            let lin = |t: &[f32; 6]| [t[0], t[1], t[2], t[3]];
            (here.clip == 0 && o.clip == 0
                || (here.clip_rel == o.clip_rel && here.clip_at == o.clip_at))
                && o.shown == here.shown
                && o.depth == here.depth
                && o.parent_alpha == here.parent_alpha
                && o.placed.local == here.placed.local
                && lin(&o.placed.transform) == lin(&here.placed.transform)
                && o.parent.map(|t| lin(&t)) == here.parent.map(|t| lin(&t))
        };
        if bit(&same_look, i)
            && let Some(m) = was(i)
            && let Some(Some(o)) = old.around.get(m)
            && let same = *o == here
            && (same || shifted(o))
            && let Some(node) = old
                .canvas_at
                .get(m)
                .and_then(|&c| old.canvas.get_mut(c as usize)?.take())
                .and_then(|mut node| {
                    if same {
                        return Some(node);
                    }
                    // Where it is now, clipped as the walk below clips: a
                    // composite covers its box within the chain's bounds,
                    // and moved with its masks it is the same image.
                    let r = p.rect;
                    let d = [r.min[0] - node.rect[0], r.min[1] - node.rect[1]];
                    node.rect = [r.min[0], r.min[1], r.size()[0], r.size()[1]];
                    if let Some(s) = &node.sprite {
                        node.sprite = Some(Arc::new(s.moved(d)));
                    }
                    let bounds = clips[i].as_deref().map(Clip::bounds);
                    if node.baked {
                        node.clip = Some(match bounds {
                            Some(b) => intersect(node.rect, b)?,
                            None => node.rect,
                        });
                    } else if node.clip.is_some() {
                        node.clip = Some(intersect(node.rect, bounds?)?);
                    }
                    Some(node)
                })
        {
            tree_of.push(i);
            nodes.push(node);
            frames.push(if own(i) && n.item == 0 {
                frame(&tree, placed, i, Some(widget), root, screen)
            } else {
                Frame {
                    bound: n.bound,
                    ..Frame::default()
                }
            });
            if bit(&down, i) {
                for &c in n.children.iter().rev() {
                    stack.push((c, depth + 1, visible, alpha));
                }
            }
            continue;
        }
        let world_scale = [t[0].hypot(t[1]), t[2].hypot(t[3])];
        let border = border_scale(&tree, placed, i);
        let r = p.rect;
        let slices = l
            .image
            .as_ref()
            .and_then(|i| i.slices.clone())
            .map(|(pieces, m)| Slices {
                pieces,
                margins: [
                    m[0] * border[0],
                    m[1] * border[1],
                    m[2] * border[0],
                    m[3] * border[1],
                ],
            });
        let rect = [r.min[0], r.min[1], r.size()[0], r.size()[1]];
        let mut node = Node {
            widget: k,
            // Sample list items repeat their prototype widget.
            foreign: !own(i) || n.item > 0,
            item: n.item,
            depth,
            name: statics[file][k]
                .name
                .clone()
                .unwrap_or_else(|| name(widget.class).into()),
            kind: if root { Kind::Container } else { l.kind },
            rect,
            image: l.image.as_ref().and_then(|i| i.texture.clone()),
            slices,
            text: l.text.clone(),
            font: l.font.clone(),
            text_size: l.font_size * world_scale[1],
            // As `layout::label_box` stacks them.
            line_height: l.line * l.line_spacing.max(0.1) * world_scale[1],
            align: l.align,
            overflow: l.overflow,
            color: l.color,
            shadow: l.shadow.map(|s| super::canvas::Shadow {
                offset: s.offset.map(|o| o * world_scale[1]),
                ..s
            }),
            opacity: alpha,
            visible: visible && alpha > 0.004,
            clip: None,
            baked: false,
            sprite: None,
            runtime: l.runtime,
            // The same for every node of an instance: worked out once.
            tree: i,
            clip_by: usize::MAX,
            owners: match tree.instance[i].filter(|_| file != 0) {
                None => Arc::default(),
                Some(first) => chains
                    .entry(first)
                    .or_insert_with(|| {
                        let mut chain = Vec::new();
                        let mut at = Some(first);
                        let mut f = file;
                        while let (Some(inst), true) = (at, f != 0) {
                            if let (Some(g), Some(&key)) =
                                (tree.templates[f - 1].guid(), keys.get(&inst))
                            {
                                chain.push((g, key));
                            }
                            f = tree.source[inst].0;
                            at = tree.instance[inst];
                        }
                        chain.into()
                    })
                    .clone(),
            },
        };
        if clips[i].as_deref().is_some_and(Clip::hides) {
            node.visible = false;
        }
        // A mask that misses the widget entirely is placed by a binding at
        // runtime (the quest tracker's `ui_mask` sits at x −545 until the
        // list scrolls in); clipping by its stored box would hide content
        // the game shows, so such a chain is ignored.
        let clip = clips[i]
            .as_deref()
            .filter(|c| intersect(rect, c.bounds()).is_some() || rect[2] <= 0. || rect[3] <= 0.);
        if node.visible && !root && super::gpu::enabled() {
            let (tint, blend) = drawn[i];
            node.sprite = sprite(l, t, p.local, border, tint, blend, alpha, clip).map(Arc::new);
            // What a composite would have covered: the inspector and the
            // placement overlay show it, and a chain that misses the box
            // shows nothing of it.
            if node.sprite.is_some()
                && let Some(c) = clip
            {
                match intersect(rect, c.bounds()) {
                    Some(region) => node.clip = Some(region),
                    None => node.visible = false,
                }
            }
        }
        let uv_moved = l.uv != ([0.; 2], [1.; 2]) || l.uv_rotation != 0.;
        // GPUI cannot rotate images, so rotated ones (their own rotation or
        // an ancestor's) are drawn into their bounding box on the CPU.
        let rotated = t[1].abs() > 1e-4 || t[2].abs() > 1e-4;
        if node.sprite.is_none() && node.visible && !root && (clip.is_some() || uv_moved || rotated)
        {
            let boxed = BoxMap::new(t, p.local);
            let pixels = l.image.as_ref().and_then(|i| i.pixels.clone());
            match (pixels, clip.map(Clip::bounds)) {
                // Clipped or UV-shifted textures are composited on the CPU
                // into the visible part of the box.
                (Some(pixels), bounds) => match boxed {
                    Some(boxed) => bakes.push(Bake {
                        node: nodes.len(),
                        pixels,
                        map: Mapping::of(l, border),
                        rect,
                        boxed,
                        clip,
                        bounds,
                    }),
                    None => node.visible = false,
                },
                // Labels and untextured boxes clip to the mask's box.
                (None, Some(bounds)) => match intersect(rect, bounds) {
                    Some(region) => node.clip = Some(region),
                    None if l.kind == Kind::Container => {}
                    None => node.visible = false,
                },
                (None, None) => {}
            }
        }
        tree_of.push(i);
        nodes.push(node);
        // Edit frames place and move the open file's widgets; of the rest
        // the inspector shows only what bindings put there.
        frames.push(if own(i) && n.item == 0 {
            frame(&tree, placed, i, Some(widget), root, screen)
        } else {
            Frame {
                bound: n.bound,
                ..Frame::default()
            }
        });
        if bit(&down, i) {
            for &c in n.children.iter().rev() {
                stack.push((c, depth + 1, visible, alpha));
            }
        }
    }
    images.layouts += 1;
    let now = images.layouts;
    let keys: Vec<u64> = bakes.iter().map(bake_key).collect();
    let todo: Vec<usize> = (0..bakes.len())
        .filter(|&i| !images.bakes.contains_key(&keys[i]))
        .collect();
    let done = parallel(&todo, |&i| {
        let j = &bakes[i];
        bake(&j.pixels, &j.map, j.rect, &j.boxed, j.clip, j.bounds)
            .map(|(t, region)| (t.render_image(), region))
    });
    for (&i, baked) in todo.iter().zip(done) {
        let j = &bakes[i];
        let mut pins = vec![j.pixels.clone()];
        let mut at = j.clip;
        while let Some(c) = at {
            pins.extend(c.run.iter().filter_map(|l| l.texture.clone()));
            at = c.outer.as_deref();
        }
        images.bakes.insert(keys[i], (baked, pins, now));
    }
    let baked: Vec<_> = keys
        .iter()
        .map(|k| {
            let e = images.bakes.get_mut(k).expect("baked above");
            e.2 = now;
            e.0.clone()
        })
        .collect();
    images.bakes.retain(|_, e| now - e.2 < KEPT_BAKES);
    for (job, baked) in bakes.iter().zip(baked) {
        let node = &mut nodes[job.node];
        match baked {
            Some((image, region)) => {
                node.image = Some(image);
                node.slices = None;
                node.clip = Some(region);
                node.baked = true;
            }
            None => node.visible = false,
        }
    }
    for (i, intrinsic, origin) in built {
        tree.nodes[i].intrinsic = intrinsic;
        tree.nodes[i].origin = origin;
    }
    for (c, &i) in tree_of.iter().enumerate() {
        nodes[c].tree = i;
        nodes[c].clip_by = clips[i]
            .as_deref()
            .and_then(|k| k.run.first())
            .map_or(usize::MAX, |l| l.by);
    }
    let canvas: Vec<Option<Node>> = nodes.iter().cloned().map(Some).collect();
    let mut canvas_at = vec![u32::MAX; tree.nodes.len()];
    for (c, &i) in tree_of.iter().enumerate() {
        canvas_at[i] = c as u32;
    }
    layout::drop_later(std::mem::take(&mut old));
    if images.kept.len() >= KEPT_SCREENS {
        layout::drop_later(images.kept.remove(0));
    }
    images.kept.push((
        asset.guid(),
        Kept {
            view: view_key,
            looks: look,
            reach,
            drawn,
            set,
            around,
            canvas,
            canvas_at,
            laid: Some(laid),
        },
    ));
    images.layout.keep(tree);
    (nodes, frames)
}

/// What the bindings of one file instance of the last layout of `asset`
/// did: its traces and template values, by instance key
/// (`canvas::Node::owners`, 0 for the file itself). Worked out when the
/// editor shows them, not per layout.
pub fn file_eval(images: &Images, asset: &Asset, key: u64) -> Option<FileEval> {
    let tree = images.layout.tree(asset.guid())?;
    let (inputs, outputs) = tree.template_values(key);
    Some(FileEval {
        trace: tree.traces.get(&key)?.clone(),
        inputs,
        outputs,
    })
}

/// One image to composite on the CPU, see [`bake`].
struct Bake<'a> {
    node: usize,
    pixels: Arc<Texture>,
    map: Mapping,
    rect: [f32; 4],
    boxed: BoxMap,
    clip: Option<&'a Clip>,
    bounds: Option<[f32; 4]>,
}

/// Everything [`bake`] reads of a job; textures by address.
fn bake_key(j: &Bake) -> u64 {
    use std::hash::Hasher;
    let mut h = key_hasher();
    hash_mapping(&mut h, &j.map);
    hash_floats(&mut h, &j.rect);
    hash_boxmap(&mut h, &j.boxed);
    hash_floats(&mut h, &j.bounds.unwrap_or([f32::NAN; 4]));
    h.write_u64(Arc::as_ptr(&j.pixels) as usize as u64);
    h.write_u64(j.clip.map_or(0, |c| c.key));
    h.finish()
}

/// What hashes bake and clip keys: our own data, not an attacker's, so
/// foldhash, fixed so keys hold for the process. Textures go by address;
/// the image cache holds them.
fn key_hasher() -> impl std::hash::Hasher {
    std::hash::BuildHasher::build_hasher(&foldhash::fast::FixedState::with_seed(0))
}

fn hash_floats(h: &mut impl std::hash::Hasher, xs: &[f32]) {
    xs.iter().for_each(|x| h.write_u32(x.to_bits()));
}

fn hash_mapping(h: &mut impl std::hash::Hasher, m: &Mapping) {
    match m.nine {
        Some((b, c)) => {
            hash_floats(h, &b);
            hash_floats(h, &[c[0][0], c[0][1], c[1][0], c[1][1]]);
        }
        None => hash_floats(h, &[f32::NAN]),
    }
    hash_floats(h, &[m.uv.0[0], m.uv.0[1], m.uv.1[0], m.uv.1[1]]);
    hash_floats(h, &m.rotation);
    hash_floats(h, &m.px);
    h.write_u8(u8::from(m.clamp));
}

fn hash_boxmap(h: &mut impl std::hash::Hasher, b: &BoxMap) {
    hash_floats(h, &b.inv);
    hash_floats(h, &b.min);
    hash_floats(h, &b.scale);
    hash_floats(h, &b.size);
}

/// [`Clip::key`] of a run over `outer`.
fn clip_key(run: &[MaskLayer], outer: Option<&Clip>) -> u64 {
    use std::hash::Hasher;
    let mut h = key_hasher();
    h.write_usize(run.len());
    for l in run {
        hash_floats(&mut h, &l.rect);
        hash_boxmap(&mut h, &l.boxed);
        hash_mapping(&mut h, &l.map);
        hash_floats(&mut h, &[l.alpha, l.edge.0, l.edge.1]);
        h.write_u8(
            u8::from(l.invert)
                | u8::from(l.intersect) << 1
                | u8::from(l.soft) << 2
                | u8::from(l.exclusive) << 3,
        );
        h.write_u64(
            l.texture
                .as_ref()
                .map_or(0, |t| Arc::as_ptr(t) as usize as u64),
        );
    }
    h.write_u64(outer.map_or(0, |o| o.key));
    h.finish() | 1
}

/// [`Clip::rel`]: [`clip_key`] with world positions taken from `origin`,
/// so a chain moved as a whole keys the same.
fn clip_rel(run: &[MaskLayer], outer: Option<&Clip>, origin: [f32; 2]) -> u64 {
    use std::hash::Hasher;
    let mut h = key_hasher();
    h.write_usize(run.len());
    for l in run {
        let r = l.rect;
        hash_floats(&mut h, &[r[0] - origin[0], r[1] - origin[1], r[2], r[3]]);
        let b = &l.boxed;
        hash_floats(&mut h, &b.inv[..4]);
        hash_floats(&mut h, &apply(b.inv, origin));
        hash_floats(&mut h, &b.min);
        hash_floats(&mut h, &b.scale);
        hash_floats(&mut h, &b.size);
        hash_mapping(&mut h, &l.map);
        hash_floats(&mut h, &[l.alpha, l.edge.0, l.edge.1]);
        h.write_u8(
            u8::from(l.invert)
                | u8::from(l.intersect) << 1
                | u8::from(l.soft) << 2
                | u8::from(l.exclusive) << 3,
        );
        h.write_u64(
            l.texture
                .as_ref()
                .map_or(0, |t| Arc::as_ptr(t) as usize as u64),
        );
    }
    if let Some(o) = outer {
        h.write_u64(o.rel);
        hash_floats(&mut h, &[o.origin[0] - origin[0], o.origin[1] - origin[1]]);
    }
    h.finish() | 1
}

/// Runs `f` over `jobs` on the shared worker pool and returns the results
/// in job order.
fn parallel<T: Sync, R: Send>(jobs: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use rayon::prelude::*;
    jobs.par_iter().map(&f).collect()
}

/// What a tree node draws, resolved before the layout runs.
/// One bit per node.
fn bit(set: &[u64], i: usize) -> bool {
    set[i / 64] >> (i % 64) & 1 == 1
}

struct Look {
    /// GPUI family of the label's game font.
    font: Option<gpui_editor_kit::gpui::SharedString>,
    runtime: bool,
    kind: Kind,
    image: Option<Image>,
    text: Option<gpui_editor_kit::gpui::SharedString>,
    font_size: f32,
    line_spacing: f32,
    /// One line's height at LineSpacing 1, design pixels.
    line: f32,
    align: [u32; 2],
    overflow: u32,
    color: Option<[u8; 3]>,
    /// A label's TextStyle drop shadow, offset in the label's own pixels.
    shadow: Option<super::canvas::Shadow>,
    /// MaskMode: odd values provide a mask (1 hard, 3/5 hard with chain
    /// control, 7 soft alpha); 0 draws normally.
    mask: u32,
    mask_fx: Option<MaskFx>,
    /// Joins the mask run that follows it among its siblings.
    masked: bool,
    /// Carries the background-blur effect: draws the blurred scene.
    backdrop: bool,
    /// Texture addressing, unnamed image enum `b01b49aa` (see ADDRESS_*).
    address: u32,
    /// UVOffset, UVScale.
    uv: ([f32; 2], [f32; 2]),
    /// UVRotation in degrees.
    uv_rotation: f32,
    /// FillMode (see `phx::WidgetImage::fill`).
    fill: u32,
}

impl Look {
    /// For a node the scene does not reach.
    fn none() -> Look {
        Look {
            font: None,
            runtime: false,
            kind: Kind::Container,
            image: None,
            text: None,
            font_size: 0.,
            line_spacing: 1.,
            line: 0.,
            align: [0; 2],
            overflow: 0,
            color: None,
            shadow: None,
            mask: 0,
            mask_fx: None,
            masked: false,
            backdrop: false,
            address: 0,
            uv: ([0.; 2], [1.; 2]),
            uv_rotation: 0.,
            fill: 0,
        }
    }
}

/// What a widget's look takes from its file alone, the same in every
/// instance of the file.
struct Static {
    /// The widget's name, shared by every node of it.
    name: Option<Arc<str>>,
    blend: u32,
    image: Option<phx::WidgetImage>,
    /// Has the radial item / hover zone (`InfoTipArea`) component, itself
    /// or (for top-level widgets) on the template root.
    radial_item: bool,
    infotip_area: bool,
    /// Carries a DropZone component (or, at the top level, its template
    /// root does): drawn only while a drag is on.
    drop_zone: bool,
    label: Option<phx::LabelText>,
    style: Option<phx::TextStyle>,
    font: Option<Arc<Font>>,
    mask: u32,
    mask_fx: Option<MaskFx>,
    masked: bool,
    backdrop: bool,
    address: u32,
    player_color: bool,
}

impl Static {
    fn of_file(
        f: &Asset,
        widgets: &[Widget],
        install: &Install,
        images: &mut Images,
    ) -> Vec<Static> {
        let root_components = widgets.first().map(|r| f.components(r)).unwrap_or_default();
        widgets
            .iter()
            .map(|widget| {
                let own = f.components(widget);
                let comps: Vec<&layout::Component> = if widget.parent == Some(0) {
                    own.iter().chain(&root_components).collect()
                } else {
                    own.iter().collect()
                };
                let has = |id: &str| {
                    comps
                        .iter()
                        .any(|c| c.template.is_some_and(|g| g.to_string() == id))
                };
                let effects = f.effects(widget);
                let mask_fx = effects
                    .iter()
                    .find(|e| e.template.is_some_and(|g| g.to_string() == EFFECT_MASK));
                let label = f.label(widget);
                let style = label
                    .as_ref()
                    .and_then(|l| l.style)
                    .and_then(|r| images.asset(install, r.id))
                    .and_then(|a| phx::text_style(&a));
                let font = style
                    .as_ref()
                    .and_then(|s| s.font)
                    .and_then(|r| images.font(install, r.id));
                Static {
                    name: widget.name.as_deref().map(Arc::from),
                    blend: enum_prop(widget, "BlendingMode"),
                    image: f.widget_image(widget),
                    radial_item: has(COMPONENT_RADIAL_ITEM),
                    infotip_area: has(COMPONENT_INFOTIP_AREA),
                    drop_zone: has(COMPONENT_DROP_ZONE),
                    label,
                    style,
                    font,
                    mask: enum_prop(widget, "MaskMode"),
                    mask_fx: mask_fx.map(|e| MaskFx {
                        threshold: fx_f32(e.get(MASK_ALPHA_THRESHOLD)),
                        gradient: fx_f32(e.get(MASK_ALPHA_GRADIENT)),
                        // Only InverseThreshold inverts. ExclusiveMask is set
                        // on plain union masks too (the governors bar's
                        // corner masks) and inverting those filled the band's
                        // corners.
                        invert: matches!(
                            e.get(MASK_INVERSE_THRESHOLD),
                            Some(phx::DefaultValue::Bool(1))
                        ),
                        enabled: !matches!(e.get(MASK_ENABLED), Some(phx::DefaultValue::Bool(0))),
                        exclusive: matches!(
                            e.get(MASK_EXCLUSIVE),
                            Some(phx::DefaultValue::Bool(1))
                        ),
                    }),
                    masked: bool_prop(widget, "Masked"),
                    backdrop: effects.iter().any(|e| {
                        e.template
                            .is_some_and(|g| g.to_string() == EFFECT_BACKGROUND_BLUR)
                    }),
                    address: widget
                        .props
                        .iter()
                        .find_map(|p| match (p.crc, &p.value) {
                            (PROP_ADDRESS, Value::Enum { value, .. }) => Some(*value),
                            _ => None,
                        })
                        .unwrap_or(1),
                    player_color: widget
                        .name
                        .as_deref()
                        .is_some_and(|n| n.starts_with("PlayerColor")),
                }
            })
            .collect()
    }
}

/// Mask effect (`05b51850…`, on most mask providers) overrides.
#[derive(Clone, Copy)]
struct MaskFx {
    threshold: Option<f32>,
    gradient: Option<f32>,
    invert: bool,
    /// The effect's Enabled (on unless the widget turns it off): a mask
    /// image carrying an enabled one is a soft mask.
    enabled: bool,
    /// ExclusiveMask: outside its box a soft mask leaves coverage alone,
    /// where otherwise nothing shows.
    exclusive: bool,
}

fn fx_f32(v: Option<&phx::DefaultValue>) -> Option<f32> {
    match v {
        Some(phx::DefaultValue::F32(bits)) => Some(f32::from_bits(*bits)),
        _ => None,
    }
}

fn bool_prop(widget: &Widget, name: &str) -> bool {
    widget
        .props
        .iter()
        .any(|p| p.name == Some(name) && p.value == Value::Bool(true))
}

/// What player-colour layers show with sample data (player 1's red).
const SAMPLE_PLAYER_COLOR: [u8; 3] = [140, 28, 36];

/// ComponentTemplate of radial menu items (custom properties Position,
/// Rotation, AngleSize, SkipAnimations, DisableWobbling): the game spreads
/// them round a circle at runtime (minimap sectors, construction radial),
/// so their stored placement is meaningless on its own.
const COMPONENT_RADIAL_ITEM: &str = "5065a655-d1cc-4a8a-824e-d167d02099ec";

/// ComponentTemplate `InfoTipArea`: the hover zone that opens an infotip.
/// Its template's images (`LayoutArea`, a blue `Preview`) only show the
/// zone in the designer.
const COMPONENT_INFOTIP_AREA: &str = "41f69505-a029-4bdb-bd17-6ab85420c93f";

/// ComponentTemplate DropZone (Enabled, AllowedDragType, IsValidDropTarget,
/// IsCurrentDropTarget, DropSuccessful). The construction bar's pin slots
/// stack three of them (Entry, Pin, PinMaterial) over every slot; none of
/// their bindings hides them, and in game they only show while a building
/// is dragged.
/// TODO(alex): which native call toggles them is not read from the exe.
const COMPONENT_DROP_ZONE: &str = "7831a47e-2d9c-4945-afda-0ab73f55074b";

// EffectTemplate guids (`PHXEF`, no names in the asset; read from usage:
// every carrier of the blur is named `*_Blur`, the mask effect's custom
// properties are Enabled/AlphaThreshold/InverseThreshold/AlphaGradient/
// ExclusiveMask).
const EFFECT_BACKGROUND_BLUR: &str = "d0fb16e6-f688-4b53-b5a8-b1ecf0959f94";
const EFFECT_MASK: &str = "05b51850-11dd-4bb1-9cbe-dd1b228db6d7";
const MASK_ALPHA_THRESHOLD: u32 = 663_379_121;
const MASK_INVERSE_THRESHOLD: u32 = 599_953_202;
const MASK_ALPHA_GRADIENT: u32 = 642_946_916;
const MASK_ENABLED: u32 = 716_655_252;
const MASK_EXCLUSIVE: u32 = 757_680_268;

/// Unnamed ImageWidgetInstance enum `b01b49aa` @0xfc: texture addressing,
/// passed unchanged to the node (`phx_image_node_set_texture_state`, +304)
/// and the draw's texture list. Only 2 repeats: every pattern that tiles at
/// the texture's density (UVScale bound to its own Scale: the bars' marble,
/// button textures) stores 2. 1, the default, clamps to the edge texels:
/// the archive header's hairline (UVScale 11) shows its motif once with the
/// tile's line smeared out to both sides, as in game. 0 (masks, gradients,
/// 9-slices) stays in 0–1 in every shipped use.
/// TODO(alex): the shader's PAM order is clamp 0 / wrap 1 / clip 2; where
/// the widget values are remapped (`PackTextureInformation`) is not found.
const PROP_ADDRESS: u32 = 0xb01b_49aa;
const ADDRESS_WRAP: u32 = 2;
/// Stencil masks pass where AlphaTint × texel alpha ≥ this: the alpha test
/// the mask draw sets up by default (0.25) unless the mask
/// effect overrides AlphaThreshold.
const MASK_ALPHA_TEST: f32 = 0.25;
/// Soft masks (MaskMode 7) fade over AlphaThreshold ± AlphaGradient; these
/// are the mask effect's defaults (threshold, gradient), as the draw item's
/// own SoftMaskParams carries them.
const SOFT_MASK_DEFAULT: (f32, f32) = (0.5, 0.05);
/// Composited textures are at most this many pixels on a side.
const BAKE_MAX: f32 = 512.;

/// How a box maps to texture UV: stretched or 9-sliced with fixed
/// borders; then UVScale, UVRotation, UVOffset and addressing.
struct Mapping {
    /// Border widths in design pixels (l, t, r, b) and the UV centre.
    nine: Option<([f32; 4], [[f32; 2]; 2])>,
    uv: ([f32; 2], [f32; 2]),
    /// UVRotation's cosine and sine.
    rotation: [f32; 2],
    /// The image's pixel size (ImageAsset `Size`): UVOffset's unit.
    px: [f32; 2],
    clamp: bool,
}

impl Mapping {
    fn of(l: &Look, border: [f32; 2]) -> Mapping {
        let image = l.image.as_ref();
        Mapping {
            nine: image.and_then(|i| {
                let (_, m) = i.slices.as_ref()?;
                Some((
                    [
                        m[0] * border[0],
                        m[1] * border[1],
                        m[2] * border[0],
                        m[3] * border[1],
                    ],
                    i.centre,
                ))
            }),
            uv: l.uv,
            rotation: {
                let r = l.uv_rotation.to_radians();
                [r.cos(), r.sin()]
            },
            px: image.map_or([1.; 2], |i| i.size),
            clamp: l.address != ADDRESS_WRAP,
        }
    }

    /// UV of `p` (design pixels, relative to the box's top-left) in a box of
    /// `size`.
    fn uv(&self, p: [f32; 2], size: [f32; 2]) -> [f32; 2] {
        let mut uv = [0.; 2];
        for a in 0..2 {
            uv[a] = if let Some((m, c)) = self.nine {
                // Borders keep their width; they shrink together when the
                // box is smaller than both (as the canvas draws them).
                let (lo, hi) = (m[a], m[a + 2]);
                let k = (size[a] / (lo + hi).max(1e-3)).min(1.);
                let (lo, hi) = (lo * k, hi * k);
                if p[a] < lo {
                    p[a] / lo.max(1e-3) * c[0][a]
                } else if p[a] > size[a] - hi {
                    c[1][a] + (p[a] - (size[a] - hi)) / hi.max(1e-3) * (1. - c[1][a])
                } else {
                    c[0][a] + (p[a] - lo) / (size[a] - lo - hi).max(1e-3) * (c[1][a] - c[0][a])
                }
            } else {
                p[a] / size[a].max(1e-3)
            };
            // UVScale pivots on the texture's centre: the governors bar
            // shows each half of its corner mask with scale ±0.5.
            uv[a] = (uv[a] - 0.5) * self.uv.1[a];
        }
        // UVRotation turns about the centre too, in degrees, so the image
        // turns counter-clockwise on screen: the vote panel's Up arrow is
        // the right-pointing `icon_arrow` at 90 (its Down one adds a 180
        // widget rotation). Then UVOffset, in image pixels (32 on that
        // 128 px corner mask).
        let [c, s] = self.rotation;
        let turned = [c * uv[0] - s * uv[1], s * uv[0] + c * uv[1]];
        for a in 0..2 {
            uv[a] = turned[a] + 0.5 + self.uv.0[a] / self.px[a].max(1e-3);
        }
        uv
    }

    fn sample(&self, t: &Texture, uv: [f32; 2]) -> [u8; 4] {
        t.sample(self.address(t, uv))
    }

    /// [`Mapping::sample`]'s alpha alone, for masks (the same filter, a
    /// quarter of the work).
    fn alpha(&self, t: &Texture, uv: [f32; 2]) -> u8 {
        let uv = self.address(t, uv);
        let (w, h) = (t.width as i32, t.height as i32);
        let x = uv[0].rem_euclid(1.) * w as f32 - 0.5;
        let y = uv[1].rem_euclid(1.) * h as f32 - 0.5;
        let (ix, iy) = (x.floor() as i32, y.floor() as i32);
        let at = |x: i32, y: i32| {
            let i = (y.rem_euclid(h) as usize * w as usize + x.rem_euclid(w) as usize) * 4;
            f32::from(t.rgba[i + 3])
        };
        let (u, v) = (x - x.floor(), y - y.floor());
        let top = at(ix, iy) * (1. - u) + at(ix + 1, iy) * u;
        let bottom = at(ix, iy + 1) * (1. - u) + at(ix + 1, iy + 1) * u;
        (top * (1. - v) + bottom * v).round() as u8
    }

    /// Clamp addressing keeps the filter off the far edge's texels.
    fn address(&self, t: &Texture, uv: [f32; 2]) -> [f32; 2] {
        if !self.clamp {
            return uv;
        }
        let half = [0.5 / t.width as f32, 0.5 / t.height as f32];
        [
            uv[0].clamp(half[0], 1. - half[0]),
            uv[1].clamp(half[1], 1. - half[1]),
        ]
    }
}

/// The scale 9-slice borders draw at: the parent's world scale. An image's
/// own Scale is its size (Phoenix images have no other), so it stretches
/// the centre and keeps the borders: the governors bar's 300 px hairline
/// (a 40 px ornament texture at Scale 7.5) keeps its 18 px ornament.
fn border_scale(tree: &layout::Tree, placed: &[layout::Placed], i: usize) -> [f32; 2] {
    tree.parent[i]
        .and_then(|p| placed.get(p))
        .map_or([1.; 2], |p| {
            let t = p.transform;
            [t[0].hypot(t[1]), t[2].hypot(t[3])]
        })
}

/// One mask provider: its world box, what decides coverage inside it.
struct MaskLayer {
    /// World bounds, for the chain's clip box.
    rect: [f32; 4],
    /// World pixels into the mask's own box, rotation included.
    boxed: BoxMap,
    /// None: the box itself (no texture, or a transparent placeholder the
    /// game swaps at runtime).
    texture: Option<Arc<Texture>>,
    /// `texture` as stored, untinted, for the GPU.
    source: Option<Arc<super::gpu::Source>>,
    map: Mapping,
    alpha: f32,
    /// Coverage = smoothstep(edge.0, edge.1, alpha × texel alpha); a hard
    /// mask has edge.0 == edge.1.
    edge: (f32, f32),
    invert: bool,
    /// MaskMode bit 2: intersects with the masks before it in the chain.
    intersect: bool,
    /// A soft mask: multiplies into the chain's coverage instead of joining
    /// its run's union (phoenix_gui.fx `ProcessSoftMasks`).
    soft: bool,
    /// See [`MaskFx::exclusive`].
    exclusive: bool,
    /// The tree index of the widget that provides it.
    by: usize,
}

impl MaskLayer {
    /// Covers nothing: stands for a run of hidden masks.
    fn nothing() -> MaskLayer {
        MaskLayer {
            rect: [0.; 4],
            boxed: BoxMap {
                inv: [1., 0., 0., 1., 0., 0.],
                min: [0.; 2],
                scale: [1.; 2],
                size: [0.; 2],
            },
            texture: None,
            source: None,
            map: Mapping {
                nine: None,
                uv: ([0.; 2], [1.; 2]),
                rotation: [1., 0.],
                px: [1.; 2],
                clamp: true,
            },
            alpha: 0.,
            edge: (MASK_ALPHA_TEST, MASK_ALPHA_TEST),
            invert: false,
            intersect: false,
            soft: false,
            exclusive: false,
            by: usize::MAX,
        }
    }

    fn coverage(&self, p: [f32; 2]) -> f32 {
        let Some(b) = self.boxed.local(p) else {
            return if self.soft && self.exclusive { 1. } else { 0. };
        };
        let c = self.cover(b);
        if self.invert { 1. - c } else { c }
    }

    // Coverage at `b`, a point inside the mask's own box.
    fn cover(&self, b: [f32; 2]) -> f32 {
        if let Some(t) = &self.texture {
            let a =
                self.alpha * f32::from(self.map.alpha(t, self.map.uv(b, self.boxed.size))) / 255.;
            let (lo, hi) = self.edge;
            if hi <= lo {
                if a >= lo { 1. } else { 0. }
            } else {
                let k = ((a - lo) / (hi - lo)).clamp(0., 1.);
                k * k * (3. - 2. * k)
            }
        } else if self.alpha >= self.edge.0 || self.edge.0 <= 0. {
            1.
        } else {
            0.
        }
    }
}

/// The mask chain a widget draws through: one run of sibling mask
/// providers, then whatever masks its parent draws through.
struct Clip {
    /// Nearest (last in array order) first, as the chain is walked.
    run: Vec<MaskLayer>,
    outer: Option<Arc<Clip>>,
    /// See [`Clip::bounds`]; asked for twice per node drawn through it.
    bounds: [f32; 4],
    /// Hash of all of it, the outer chain's key included; never 0.
    key: u64,
    /// Where it is (its first mask's corner), and its key from there: the
    /// same for the chain moved as a whole (a drag).
    origin: [f32; 2],
    rel: u64,
}

impl Clip {
    /// Draws through a run of hidden masks (here or further out): nothing
    /// of it shows.
    fn hides(&self) -> bool {
        let empty = |l: &MaskLayer| l.alpha == 0. && l.texture.is_none() && l.rect == [0.; 4];
        (!self.run.is_empty() && self.run.iter().all(empty))
            || self.outer.as_deref().is_some_and(Clip::hides)
    }

    fn coverage(&self, p: [f32; 2]) -> f32 {
        let mut own: Option<f32> = None;
        let mut soft = 1.;
        for l in &self.run {
            let c = l.coverage(p);
            if l.soft {
                soft *= c;
                continue;
            }
            own = Some(match own {
                None => c,
                Some(a) if l.intersect => a.min(c),
                Some(a) => a.max(c),
            });
        }
        let own = own.unwrap_or(1.) * soft;
        if own == 0. {
            return 0.;
        }
        own * self.outer.as_ref().map_or(1., |o| o.coverage(p))
    }

    /// A chain: `run`, then what its parent draws through.
    fn new(run: Vec<MaskLayer>, outer: Option<Arc<Clip>>) -> Clip {
        let any_invert = run.iter().any(|l| l.invert && !l.soft);
        let stencil: Vec<&MaskLayer> = run.iter().filter(|l| !l.soft).collect();
        let mut own = if any_invert || stencil.is_empty() {
            [-1e6, -1e6, 2e6, 2e6]
        } else {
            stencil.iter().skip(1).fold(stencil[0].rect, |a, l| {
                if l.intersect {
                    intersect(a, l.rect).unwrap_or([a[0], a[1], 0., 0.])
                } else {
                    union(a, l.rect)
                }
            })
        };
        // A soft mask that is not exclusive shows nothing outside its box.
        for l in run.iter().filter(|l| l.soft && !l.exclusive) {
            own = intersect(own, l.rect).unwrap_or([own[0], own[1], 0., 0.]);
        }
        let bounds = match &outer {
            Some(o) => intersect(own, o.bounds).unwrap_or([own[0], own[1], 0., 0.]),
            None => own,
        };
        let key = clip_key(&run, outer.as_deref());
        let origin = run.first().map_or_else(
            || outer.as_ref().map_or([0.; 2], |o| o.origin),
            |l| [l.rect[0], l.rect[1]],
        );
        let rel = clip_rel(&run, outer.as_deref(), origin);
        Clip {
            run,
            outer,
            bounds,
            key,
            origin,
            rel,
        }
    }

    /// Box outside which coverage is zero (x, y, w, h).
    fn bounds(&self) -> [f32; 4] {
        self.bounds
    }
}

/// A soft mask's fade: coverage = smoothstep(t − g, t + g, alpha) with
/// both ends kept in [0, 1] and g at least 0.001 (phoenix_gui.fx
/// `ProcessSoftMasks`).
fn soft_edge(threshold: Option<f32>, gradient: Option<f32>) -> (f32, f32) {
    let t = threshold.unwrap_or(SOFT_MASK_DEFAULT.0).clamp(0., 1.);
    let g = gradient.unwrap_or(SOFT_MASK_DEFAULT.1).clamp(0.001, 1.);
    ((t - g).max(0.), (t + g).min(1.))
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> Option<[f32; 4]> {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

fn union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].min(b[0]);
    let y0 = a[1].min(b[1]);
    let x1 = (a[0] + a[2]).max(b[0] + b[2]);
    let y1 = (a[1] + a[3]).max(b[1] + b[3]);
    [x0, y0, x1 - x0, y1 - y0]
}

/// Which mask chain every tree node draws through.
///
/// Per parent, in array order: a run of
/// consecutive mask providers is moved in front of the Masked siblings that
/// precede it, back to the last sibling that is neither (or the previous
/// run), and those siblings, with their subtrees, draw through it. The
/// run's first mask links to the parent's chain, so chains nest. Unmasked
/// siblings draw through the parent's chain only.
fn clips(
    tree: &layout::Tree,
    placed: &[layout::Placed],
    look: &[Look],
    design: bool,
    down: &[u64],
) -> Vec<Option<Arc<Clip>>> {
    let mut out: Vec<Option<Arc<Clip>>> = vec![None; tree.nodes.len()];
    let mut stack = vec![(0usize, 1f32)];
    while let Some((i, alpha)) = stack.pop() {
        if !bit(down, i) {
            continue;
        }
        let inherited = out[i].clone();
        let children = &tree.nodes[i].children;
        let mut run: Option<Arc<Clip>> = None;
        let mut layers: Vec<MaskLayer> = Vec::new();
        let mut in_run = false;
        for &c in children.iter().rev() {
            let l = &look[c];
            let g = &tree.nodes[c].geometry;
            if l.mask & 1 == 1 {
                if !in_run {
                    layers.clear();
                    in_run = true;
                }
                // Hidden masks draw nothing into the stencil; skip them.
                // IsHidden only counts in design view, as for drawing.
                if g.visible && !(design && g.hidden) {
                    let r = placed[c].rect;
                    let t = placed[c].transform;
                    let border = border_scale(tree, placed, c);
                    // Node setup stores MaskMode 1 as 7, and
                    // a mode-7 image carrying an enabled mask effect draws as
                    // a soft mask; one without stays a
                    // stencil mask. Shipped mode-7 widgets carry no effect
                    // and keep the effect's defaults.
                    let fx = l.mask_fx;
                    let soft = (l.mask == 7 && fx.is_none_or(|f| f.enabled))
                        || (l.mask == 1 && fx.is_some_and(|f| f.enabled));
                    let threshold = fx.and_then(|f| f.threshold);
                    let edge = if soft {
                        soft_edge(threshold, fx.and_then(|f| f.gradient))
                    } else {
                        let t = threshold.unwrap_or(MASK_ALPHA_TEST);
                        (t, t)
                    };
                    let own_alpha = if design && g.alpha < 0.01 {
                        1.
                    } else {
                        g.alpha
                    };
                    // A degenerate (zero-scale) mask covers nothing.
                    let Some(boxed) = BoxMap::new(t, placed[c].local) else {
                        continue;
                    };
                    layers.push(MaskLayer {
                        rect: [r.min[0], r.min[1], r.size()[0], r.size()[1]],
                        boxed,
                        texture: l
                            .image
                            .as_ref()
                            .filter(|i| !i.blank)
                            .and_then(|i| i.pixels.clone()),
                        source: l
                            .image
                            .as_ref()
                            .filter(|i| !i.blank && i.pixels.is_some())
                            .and_then(|i| i.source.clone()),
                        map: Mapping::of(l, border),
                        alpha: alpha * own_alpha,
                        edge,
                        invert: fx.is_some_and(|f| f.invert),
                        intersect: !layers.is_empty() && l.mask & 2 == 2,
                        soft,
                        exclusive: fx.is_some_and(|f| f.exclusive),
                        by: c,
                    });
                }
                continue;
            }
            if in_run {
                in_run = false;
                // A run whose masks are all hidden writes no stencil, so
                // what it masks shows nowhere (an empty list's frame).
                if layers.is_empty() {
                    layers.push(MaskLayer::nothing());
                }
                run = if layers.is_empty() {
                    inherited.clone()
                } else {
                    Some(Arc::new(Clip::new(
                        std::mem::take(&mut layers),
                        inherited.clone(),
                    )))
                };
            }
            if l.masked {
                out[c] = run.clone().or_else(|| inherited.clone());
            } else {
                run = None;
                out[c] = inherited.clone();
            }
            stack.push((c, alpha * g.alpha));
        }
    }
    out
}

/// What the GPU draws for an image node (see `ui::gpu`): its untinted
/// texture through its box and UV mapping, tint, alpha and blend, and the
/// mask chain `clip`, nearest run first. None for what it does not draw
/// (no texture, placeholders, a degenerate box).
#[allow(clippy::too_many_arguments)]
fn sprite(
    l: &Look,
    t: [f32; 6],
    local: layout::Rect,
    border: [f32; 2],
    tint: Option<[u8; 3]>,
    blend: u32,
    alpha: f32,
    clip: Option<&Clip>,
) -> Option<super::gpu::Sprite> {
    let image = l.image.as_ref().filter(|i| !i.blank)?;
    let texture = image.source.clone()?;
    let quad = |b: &BoxMap, m: &Mapping| super::gpu::Quad {
        inv: b.inv,
        min: b.min,
        scale: b.scale,
        size: b.size,
        px: m.px,
        uv_offset: m.uv.0,
        uv_scale: m.uv.1,
        rotation: m.rotation,
        nine: m.nine,
        // A wrapping image whose UVs stay inside the texture shows only the
        // filter bleeding the far edge in, which mips widen into lines along
        // its box (16 px icons from 128 px art); the game's draw clean.
        clamp: m.clamp || inside_texture(m),
    };
    let mut masks = Vec::new();
    let mut at = clip;
    let mut level = 0;
    while let Some(c) = at {
        for m in &c.run {
            masks.push(super::gpu::Mask {
                quad: quad(&m.boxed, &m.map),
                texture: m.source.clone(),
                alpha: m.alpha,
                edge: m.edge,
                invert: m.invert,
                intersect: m.intersect,
                soft: m.soft,
                exclusive: m.exclusive,
                level,
                by: m.by,
            });
        }
        level += 1;
        at = c.outer.as_deref();
    }
    let local = fitted(l.fill, image.art, t, local);
    Some(super::gpu::Sprite {
        quad: quad(&BoxMap::new(t, local)?, &Mapping::of(l, border)),
        texture,
        transform: t,
        local: [local.min[0], local.min[1], local.max[0], local.max[1]],
        tint: tint.unwrap_or([255; 3]),
        alpha,
        blend,
        masks,
    })
}

/// The part of an image's box (own pixels, transform `t`) it draws in:
/// all of it, or for FillMode 1 (fit) the centred box that keeps the art's
/// aspect on screen (one axis scaled down, centred).
/// TODO(alex): FillMode 2 (cover, an extra crop pass) still stretches.
fn fitted(fill: u32, art: [f32; 2], t: [f32; 6], local: layout::Rect) -> layout::Rect {
    let s = local.size();
    let scale = [t[0].hypot(t[1]), t[2].hypot(t[3])];
    let drawn = [s[0] * scale[0], s[1] * scale[1]];
    if fill != 1 || art[0] <= 0. || art[1] <= 0. || drawn[0] <= 0. || drawn[1] <= 0. {
        return local;
    }
    let aspect = art[0] / art[1];
    let mut size = s;
    if drawn[0] / drawn[1] > aspect {
        size[0] = drawn[1] * aspect / scale[0];
    } else {
        size[1] = drawn[0] / aspect / scale[1];
    }
    let min = [
        local.min[0] + (s[0] - size[0]) * 0.5,
        local.min[1] + (s[1] - size[1]) * 0.5,
    ];
    layout::Rect {
        min,
        max: [min[0] + size[0], min[1] + size[1]],
    }
}

/// Whether `m`'s UVs stay within 0..1: no rotation, UVScale at most 1 and
/// UVOffset within what that leaves.
fn inside_texture(m: &Mapping) -> bool {
    m.rotation == [1., 0.]
        && (0..2).all(|a| {
            let s = m.uv.1[a].abs();
            let o = m.uv.0[a] / m.px[a].max(1e-3);
            s <= 1. && o.abs() <= (1. - s) * 0.5 + 1e-4
        })
}

/// How world pixels map into a widget's own box: the inverse of its
/// transform, then the box offset, in unrotated design pixels (own pixels ×
/// world scale) so 9-slice borders and tiles keep their drawn size.
pub struct BoxMap {
    inv: [f32; 6],
    min: [f32; 2],
    scale: [f32; 2],
    /// The box size in design pixels.
    pub size: [f32; 2],
}

impl BoxMap {
    pub fn new(t: [f32; 6], local: layout::Rect) -> Option<BoxMap> {
        let scale = [t[0].hypot(t[1]), t[2].hypot(t[3])];
        let s = local.size();
        Some(BoxMap {
            inv: inverse(t)?,
            min: local.min,
            scale,
            size: [s[0] * scale[0], s[1] * scale[1]],
        })
    }

    /// An unrotated box at `rect`, in design pixels.
    #[cfg(test)]
    fn axis(rect: [f32; 4]) -> BoxMap {
        BoxMap {
            inv: [1., 0., 0., 1., -rect[0], -rect[1]],
            min: [0.; 2],
            scale: [1.; 2],
            size: [rect[2], rect[3]],
        }
    }

    /// Where world point `p` falls in the box (design pixels from its
    /// top-left), or None outside it.
    fn local(&self, p: [f32; 2]) -> Option<[f32; 2]> {
        let q = apply(self.inv, p);
        let b = [
            (q[0] - self.min[0]) * self.scale[0],
            (q[1] - self.min[1]) * self.scale[1],
        ];
        (b[0] >= 0. && b[1] >= 0. && b[0] <= self.size[0] && b[1] <= self.size[1]).then_some(b)
    }
}

pub fn apply(t: [f32; 6], p: [f32; 2]) -> [f32; 2] {
    [
        t[0] * p[0] + t[2] * p[1] + t[4],
        t[1] * p[0] + t[3] * p[1] + t[5],
    ]
}

pub fn inverse(t: [f32; 6]) -> Option<[f32; 6]> {
    let det = t[0] * t[3] - t[1] * t[2];
    if det.abs() < 1e-9 {
        return None;
    }
    let (a, b, c, d) = (t[3] / det, -t[1] / det, -t[2] / det, t[0] / det);
    Some([a, b, c, d, -(a * t[4] + c * t[5]), -(b * t[4] + d * t[5])])
}

/// Renders the part of a textured box that `clip` lets through (all of it
/// without one), at most BAKE_MAX on a side. `rect` is the box's world
/// bounds, `boxed` maps world pixels into it (rotation included). Returns
/// the texture and the region it covers; None when nothing is visible.
fn bake(
    pixels: &Texture,
    map: &Mapping,
    rect: [f32; 4],
    boxed: &BoxMap,
    clip: Option<&Clip>,
    bounds: Option<[f32; 4]>,
) -> Option<(Texture, [f32; 4])> {
    let region = match bounds {
        Some(b) => intersect(rect, b)?,
        None => rect,
    };
    // Design pixels are the game's at 1080p; finer only zooms in sharper.
    let scale = (BAKE_MAX / region[2].max(region[3])).min(1.);
    let w = ((region[2] * scale).ceil() as u32).max(1);
    let h = ((region[3] * scale).ceil() as u32).max(1);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            let p = [
                region[0] + (x as f32 + 0.5) / w as f32 * region[2],
                region[1] + (y as f32 + 0.5) / h as f32 * region[3],
            ];
            // Masks only where the image is.
            let px = match boxed.local(p) {
                Some(b) => {
                    let cover = clip.map_or(1., |c| c.coverage(p));
                    if cover > 0. {
                        let mut px = map.sample(pixels, map.uv(b, boxed.size));
                        px[3] = (f32::from(px[3]) * cover) as u8;
                        px
                    } else {
                        [0; 4]
                    }
                }
                None => [0; 4],
            };
            any |= px[3] > 0;
            rgba.extend_from_slice(&px);
        }
    }
    let texture = Texture::from_rgba(w, h, rgba).ok()?;
    any.then_some((texture, region))
}

// The edit frame of node `i`: its and its parent's world transforms and the
// stored values `edits_for` and `placement` turn canvas drags into.
fn frame(
    tree: &layout::Tree,
    placed: &[layout::Placed],
    i: usize,
    own: Option<&Widget>,
    root: bool,
    screen: [f32; 2],
) -> Frame {
    let n = &tree.nodes[i];
    let g = &n.geometry;
    let (pos, piv, sc) = if root {
        ([0.; 3], [0.; 3], [1.; 3])
    } else {
        (g.position, g.pivot, g.scale)
    };
    let pt = tree.parent[i].map_or([1., 0., 0., 1., 0., 0.], |p| placed[p].transform);
    let parent_scale = [pt[0].hypot(pt[1]), pt[2].hypot(pt[3])];
    let t = placed[i].transform;
    let b = placed[i].local;
    let parent = tree.parent[i].map(|p| &tree.nodes[p]);
    // The root is sized by the screen; others by their own Size (0 = none).
    let anchor_size = match tree.parent[i] {
        Some(0) => Some(screen),
        Some(p) => tree.nodes[p].size,
        None => None,
    }
    .filter(|s| s[0] > 0. || s[1] > 0.);
    let placed_by = parent.and_then(|p| {
        p.components.iter().find_map(|c| match c {
            layout::LayoutComponent::Stack { .. } => Some("StackLayout"),
            // Only the first child is moved over the others.
            layout::LayoutComponent::Background { .. }
                if tree.nodes[tree.parent[i].unwrap()].children.first() == Some(&i) =>
            {
                Some("BackgroundLayout")
            }
            _ => None,
        })
    });
    let pivot_ratio = n.components.iter().find_map(|c| match c {
        layout::LayoutComponent::ProportionalPivot(r) => Some([r[0], r[1]]),
        _ => None,
    });
    let span = |name: &str| own.and_then(|w| prop_span(w, name));
    Frame {
        parent_scale,
        parent_transform: pt,
        transform: t,
        position: pos,
        pivot: piv,
        scale: sc,
        rotation: if root { 0. } else { g.rotation },
        intrinsic: n.intrinsic.unwrap_or_default(),
        local: [b.min[0], b.min[1], b.size()[0], b.size()[1]],
        anchor: g.anchor,
        anchor_size,
        placed_by,
        pivot_ratio,
        metrics: g.anchor_metrics != [0; 2] || g.resize_metrics != [0; 2] || g.aspect_ratio != 0,
        position_span: span("Position"),
        scale_span: span("Scale"),
        pivot_span: span("PivotPoint"),
        rotation_span: span("RotationAngle"),
        bound: n.bound,
        anchor_span: own.and_then(|w| {
            w.props.iter().find_map(|p| match (p.name, &p.value) {
                (Some("AnchorPoint"), Value::Enum { ty, .. }) => Some((p.span, *ty)),
                _ => None,
            })
        }),
    }
}

/// Edits that move/resize a widget so its world box becomes `after`.
/// Returns why not when the needed property is absent in the file.
pub fn edits_for(
    frame: &Frame,
    before: [f32; 4],
    after: [f32; 4],
) -> Result<Vec<Edit>, &'static str> {
    let mut edits = Vec::new();
    let mut scale = frame.scale;
    let resized = (after[2] - before[2]).abs() > 0.01 || (after[3] - before[3]).abs() > 0.01;
    let t = frame.transform;
    if resized && (t[1].abs() > 1e-4 || t[2].abs() > 1e-4) {
        return Err("Rotated widgets resize through Scale or Size in the inspector");
    }
    if resized {
        if frame.intrinsic[0] <= 0. || frame.intrinsic[1] <= 0. {
            return Err("This widget has no intrinsic size; resize its children instead");
        }
        let Some(span) = frame.scale_span else {
            return Err("Scale is not stored for this widget, so it cannot be resized in place");
        };
        scale[0] = after[2] / (frame.parent_scale[0] * frame.intrinsic[0]);
        scale[1] = after[3] / (frame.parent_scale[1] * frame.intrinsic[1]);
        edits.push(Edit::Value {
            span,
            value: Value::Vec3(scale),
        });
    }
    // The drag's world delta, in the parent's pixels (through its rotation
    // and scale), plus what a new Scale moves the top-left about the pivot.
    let d = [after[0] - before[0], after[1] - before[1]];
    let dp = match inverse(frame.parent_transform) {
        Some(inv) => [inv[0] * d[0] + inv[2] * d[1], inv[1] * d[0] + inv[3] * d[1]],
        None => [
            d[0] / frame.parent_scale[0].max(1e-6),
            d[1] / frame.parent_scale[1].max(1e-6),
        ],
    };
    let position = [
        frame.position[0] + dp[0] + (scale[0] - frame.scale[0]) * frame.pivot[0],
        frame.position[1] + dp[1] + (scale[1] - frame.scale[1]) * frame.pivot[1],
        frame.position[2],
    ];
    if (position[0] - frame.position[0]).abs() > 1e-3
        || (position[1] - frame.position[1]).abs() > 1e-3
    {
        let Some(span) = frame.position_span else {
            return Err("Position is not stored for this widget, so it cannot be moved in place");
        };
        edits.push(Edit::Value {
            span,
            value: Value::Vec3(position),
        });
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support::{QUEST_TRACKER, TECH_TREE, install, path};
    use anno_formats::phx::{Edit, Value};

    /// Generated UI file `file` as stored.
    fn read(install: &Install, file: &str) -> Asset {
        Asset::parse(install.read(&path(file)).unwrap()).unwrap()
    }

    /// `asset` laid out in `view`, with fresh images and no game texts.
    fn lay(install: &Install, asset: &Asset, view: View) -> (Vec<Node>, Vec<Frame>) {
        let name = |c: u32| format!("{c:08x}");
        layout(
            asset,
            install,
            &mut Images::default(),
            &HashMap::new(),
            view,
            name,
        )
    }

    /// Design view with bindings run.
    fn design() -> View {
        View {
            design: true,
            bindings: true,
            ..Default::default()
        }
    }

    /// Bindings run, opening clips at their end, sample data: a screen as
    /// the game shows it.
    fn game() -> View {
        View {
            bindings: true,
            shown: true,
            sample: true,
            ..Default::default()
        }
    }

    /// `file` laid out as the game shows it.
    fn screen(install: &Install, file: &str) -> Vec<Node> {
        lay(install, &read(install, file), game()).0
    }

    /// The nodes called `name`.
    fn named<'a>(nodes: &'a [Node], name: &str) -> Vec<&'a Node> {
        nodes.iter().filter(|n| *n.name == *name).collect()
    }

    /// The visible nodes called `name`.
    fn shown<'a>(nodes: &'a [Node], name: &str) -> Vec<&'a Node> {
        nodes
            .iter()
            .filter(|n| n.visible && *n.name == *name)
            .collect()
    }

    /// The first node called `name` and the nodes inside it.
    fn under<'a>(nodes: &'a [Node], name: &str) -> (&'a Node, &'a [Node]) {
        let at = nodes
            .iter()
            .position(|n| *n.name == *name)
            .unwrap_or_else(|| panic!("no {name}"));
        let depth = nodes[at].depth;
        let end = nodes[at + 1..]
            .iter()
            .position(|n| n.depth <= depth)
            .map_or(nodes.len(), |e| at + 1 + e);
        (&nodes[at], &nodes[at + 1..end])
    }

    /// How many rows list `list` shows: its visible items' distinct tops.
    fn items(nodes: &[Node], list: &str) -> usize {
        let (at, inside) = under(nodes, list);
        let mut tops: Vec<i32> = inside
            .iter()
            .filter(|n| n.depth == at.depth + 1 && n.visible)
            .map(|n| n.rect[1] as i32)
            .collect();
        tops.sort();
        tops.dedup();
        tops.len()
    }

    // Needs the game: a tech-tree template lays out inside 1920×1080 with
    // textured images from the 4k set.
    #[test]
    fn tech_tree_template_lays_out_with_textures() {
        let Some(install) = install() else { return };
        let asset = read(&install, TECH_TREE);
        let (nodes, frames) = lay(&install, &asset, design());
        // Its own widgets, plus those expanded from container templates.
        let own = nodes.iter().filter(|n| !n.foreign).count();
        assert_eq!(own, asset.widgets().len());
        assert!(nodes.len() > own, "containers expand their templates");
        assert_eq!(nodes.len(), frames.len());
        assert!(
            nodes.iter().any(|n| n.slices.is_some()),
            "nine-slice frames"
        );
        // The root takes the template's stored ReferenceSize.
        assert_eq!(nodes[0].rect, [0., 0., 1920., 980.]);
        let textured = nodes.iter().filter(|n| n.image.is_some()).count();
        assert!(textured >= 30, "{textured} textured images");
        assert!(nodes.iter().all(|n| n.rect.iter().all(|v| v.is_finite())));
    }

    // Needs the game: the main title lays out as in the game (menu
    // bottom-left, logo top-centre), and every own widget's frame inverts
    // its own rectangle to "no edit".
    #[test]
    fn main_title_matches_game_layout() {
        let Some(install) = install() else { return };
        let (nodes, frames) = lay(&install, &read(&install, "maintitlescene"), design());
        // Labels draw in the game's fonts (data/fonts, by TextStyle →
        // LocalizedFont → FontAsset).
        assert!(
            nodes.iter().any(|n| n.text.is_some() && n.font.is_some()),
            "no label has a game font"
        );
        let menu = named(&nodes, "MainTitle_BtnContainer")[0].rect;
        assert!(
            (menu[1] + menu[3] - 1080.).abs() < 1. && menu[0] < 0.,
            "{menu:?}"
        );
        let logo = named(&nodes, "Logo")[0].rect;
        assert!(
            (logo[0] + logo[2] * 0.5 - 960.).abs() < 1. && logo[1] < 100.,
            "{logo:?}"
        );
        for (n, f) in nodes.iter().zip(&frames) {
            if !n.foreign && n.rect[2] > 0. && n.rect[3] > 0. && f.scale[0] != 0. {
                let edits = edits_for(f, n.rect, n.rect).unwrap();
                assert!(edits.is_empty(), "{}: {edits:?}", n.name);
            }
        }
    }

    fn solid(w: u32, h: u32, alpha: impl Fn(u32) -> u8) -> Texture {
        let rgba = (0..w * h)
            .flat_map(|i| [255, 255, 255, alpha(i % w)])
            .collect();
        Texture::from_rgba(w, h, rgba).unwrap()
    }

    fn stretch() -> Mapping {
        Mapping {
            nine: None,
            uv: ([0.; 2], [1.; 2]),
            rotation: [1., 0.],
            px: [1.; 2],
            clamp: true,
        }
    }

    // The governors bar's corner pieces show one half of a 128 px texture
    // each: scale ±0.5 pivots on the texture's centre, the offset is in
    // image pixels (Right_Mask: 32, 0.5 → [0.5, 1]; Left_Mask: 32, −0.5 →
    // [1, 0.5], the mirrored right half).
    // The vote panel's Up arrow (`11245941…`) is the right-pointing
    // `icon_arrow` texture at UVRotation 90: the top of its box must show
    // the arrow's tip, uv (1, 0.5), and the bottom its open end.
    #[test]
    fn uv_rotation_turns_the_image_counter_clockwise() {
        let r = 90f32.to_radians();
        let map = Mapping {
            nine: None,
            uv: ([0.; 2], [1.; 2]),
            rotation: [r.cos(), r.sin()],
            px: [512.; 2],
            clamp: true,
        };
        let at = |x: f32, y: f32| {
            map.uv([x, y], [64., 64.])
                .map(|v| (v * 100.).round() / 100.)
        };
        assert_eq!(at(32., 0.), [1., 0.5]);
        assert_eq!(at(32., 64.), [0., 0.5]);
        assert_eq!(at(0., 32.), [0.5, 0.]);
    }

    #[test]
    fn uv_scale_pivots_on_the_centre_and_offsets_in_pixels() {
        let map = |offset: f32, scale: f32| Mapping {
            nine: None,
            uv: ([offset, 0.], [scale, 1.]),
            rotation: [1., 0.],
            px: [128., 32.],
            clamp: true,
        };
        let u = |m: &Mapping, x: f32| m.uv([x, 0.], [64., 32.])[0];
        let right = map(32., 0.5);
        assert_eq!((u(&right, 0.), u(&right, 64.)), (0.5, 1.));
        let left = map(32., -0.5);
        assert_eq!((u(&left, 0.), u(&left, 64.)), (1., 0.5));
        let stroke_left = map(-32., 0.5);
        assert_eq!((u(&stroke_left, 0.), u(&stroke_left, 64.)), (0., 0.5));
    }

    fn layer(rect: [f32; 4], texture: Option<Texture>) -> MaskLayer {
        MaskLayer {
            rect,
            boxed: BoxMap::axis(rect),
            texture: texture.map(Arc::new),
            source: None,
            map: stretch(),
            alpha: 1.,
            edge: (MASK_ALPHA_TEST, MASK_ALPHA_TEST),
            invert: false,
            intersect: false,
            soft: false,
            exclusive: false,
            by: usize::MAX,
        }
    }

    // A masked image only draws inside its mask: the box mask cuts the
    // composited region, the textured mask's transparent half covers nothing.
    #[test]
    fn masked_image_draws_only_inside_the_mask() {
        let content = solid(4, 4, |_| 255);
        let masked = |rect: [f32; 4], clip: &Clip| {
            let boxed = BoxMap::axis(rect);
            bake(
                &content,
                &stretch(),
                rect,
                &boxed,
                Some(clip),
                Some(clip.bounds()),
            )
        };
        let boxed = Clip::new(vec![layer([0., 0., 10., 10.], None)], None);
        let (t, region) = masked([0., 0., 20., 20.], &boxed).unwrap();
        assert_eq!(region, [0., 0., 10., 10.]);
        assert!(t.rgba.chunks(4).all(|p| p[3] == 255));

        // Left texel opaque, right texel clear: only the left half passes
        // the alpha test.
        let half = Clip::new(
            vec![layer(
                [0., 0., 20., 20.],
                Some(solid(2, 1, |x| if x == 0 { 255 } else { 0 })),
            )],
            None,
        );
        let (t, _) = masked([0., 0., 20., 20.], &half).unwrap();
        let alpha = |x: u32| t.rgba[(x * 4 + 3) as usize];
        assert_eq!(alpha(1), 255);
        assert_eq!(alpha(t.width - 2), 0);

        // A run nested in a parent chain is limited by both.
        let nested = Clip::new(vec![layer([5., 0., 20., 20.], None)], Some(Arc::new(boxed)));
        assert_eq!(nested.bounds(), [5., 0., 5., 10.]);
        assert!(masked([30., 30., 5., 5.], &nested).is_none());
    }

    // A 10×4 image, left half opaque, turned 90° clockwise about its
    // origin (y down: x' = −y, y' = x) fills a 4×10 box to the left of the
    // origin, opaque in its top half.
    #[test]
    fn rotated_images_bake_into_their_bounding_box() {
        let image = solid(10, 4, |x| if x < 5 { 255 } else { 0 });
        let t = [0., 1., -1., 0., 0., 0.];
        let local = layout::Rect {
            min: [0.; 2],
            max: [10., 4.],
        };
        let boxed = BoxMap::new(t, local).unwrap();
        assert_eq!(boxed.size, [10., 4.]);
        let (baked, region) =
            bake(&image, &stretch(), [-4., 0., 4., 10.], &boxed, None, None).unwrap();
        assert_eq!(region, [-4., 0., 4., 10.]);
        let alpha = |x: u32, y: u32| baked.rgba[((y * baked.width + x) * 4 + 3) as usize];
        let (w, h) = (baked.width, baked.height);
        assert_eq!(alpha(w / 2, h / 5), 255, "top half opaque");
        assert_eq!(alpha(w / 2, h * 4 / 5), 0, "bottom half clear");
    }

    // A 10×10 box mask turned 45° about its centre is a diamond: its
    // centre passes, the corners of its bounding box do not.
    #[test]
    fn rotated_masks_clip_to_their_turned_shape() {
        let (s, c) = std::f32::consts::FRAC_PI_4.sin_cos();
        // T(5,5) · R(45°) · T(−5,−5)
        let t = [c, s, -s, c, 5. - 5. * c + 5. * s, 5. - 5. * s - 5. * c];
        let local = layout::Rect {
            min: [0.; 2],
            max: [10., 10.],
        };
        let r = 10. * std::f32::consts::SQRT_2;
        let mut mask = layer([5. - r / 2., 5. - r / 2., r, r], None);
        mask.boxed = BoxMap::new(t, local).unwrap();
        assert_eq!(mask.coverage([5., 5.]), 1.);
        assert_eq!(mask.coverage([5. - r / 2. + 0.5, 5. - r / 2. + 0.5]), 0.);
        assert_eq!(
            mask.coverage([5., 5. - r / 2. + 0.5]),
            1.,
            "the diamond's top tip"
        );
    }

    #[test]
    fn additive_and_multiply_become_source_over() {
        let t = Texture::from_rgba(2, 1, vec![0, 0, 0, 255, 128, 64, 0, 255]).unwrap();
        // Add: black adds nothing, so it turns clear; colour keeps its hue.
        let add = shaded(&t, [255; 3], BLEND_ADD);
        assert_eq!(add.rgba[3], 0);
        assert_eq!(&add.rgba[4..8], &[255, 127, 0, 128]);
        // Multiply: black darkens fully, white would not darken at all.
        let mul = shaded(&t, [255; 3], BLEND_MULTIPLY);
        assert_eq!(&mul.rgba[..4], &[0, 0, 0, 255]);
        assert!(mul.rgba[7] < 255 && mul.rgba[4..7] == [0, 0, 0]);
    }

    // Needs the game: the object menu's backdrop (`null_16.png`, a white
    // square carrying the background-blur effect) stays undrawn, and the
    // portrait's Masked textures are cut to their mask runs.
    #[test]
    fn object_menu_hides_the_blur_backdrop_and_masks_the_portrait() {
        let Some(install) = install() else { return };
        let asset = read(&install, "00dc61ad-46ce-4c71-883a-b72d873ce380");
        let nodes = lay(&install, &asset, View::default()).0;
        let backdrop = named(&nodes, "null_16.png")
            .into_iter()
            .find(|n| n.rect[1] < -800.)
            .expect("the OM_BG_DS backdrop");
        assert!(!backdrop.visible, "white box above the panel");
        // Nothing visible spans the 800 px above the panel any more.
        assert!(
            !nodes
                .iter()
                .any(|n| n.visible && n.image.is_some() && n.rect[1] < -700. && n.rect[3] > 700.),
            "a tall image above the panel"
        );
        for name in ["GoldenTexture", "Rotational_Shine", "BG_Shimmer"] {
            let n = named(&nodes, name)[0];
            assert!(n.clip.is_some() || !n.visible, "{name} is masked");
        }
    }

    #[test]
    fn soft_masks_fade_around_their_threshold() {
        let near =
            |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6;
        // The mask effect's defaults: 0.5 ± 0.05.
        assert!(near(soft_edge(None, None), (0.45, 0.55)));
        // The water warning's Mask_Line (threshold 0, gradient 1) clamps at 0.
        assert!(near(soft_edge(Some(0.), Some(1.)), (0., 1.)));
        // A zero gradient still fades over 0.002.
        assert!(near(soft_edge(Some(0.3), Some(0.)), (0.299, 0.301)));
    }

    // Needs the game: in the quest tracker the 256 px marble under each
    // entry header is cut to MASK_MARBLE's 53 px strip, while the list
    // itself (Masked under the binding-placed `ui_mask`) still draws.
    #[test]
    fn quest_tracker_marble_is_cut_to_the_header() {
        let Some(install) = install() else { return };
        let nodes = lay(&install, &read(&install, QUEST_TRACKER), View::default()).0;
        let marbles = shown(&nodes, "bg_marble");
        assert!(!marbles.is_empty());
        for n in marbles {
            let c = n.clip.expect("clipped");
            assert!(c[3] <= 54., "{c:?}");
        }
        assert!(!shown(&nodes, "quest_base_bg").is_empty());
    }

    // Soft masks (MaskMode 1 with the mask effect, as node setup stores it)
    // multiply into the coverage. An exclusive one leaves pixels outside its
    // box alone, so a run of them over adjacent boxes reads as their union
    // (the governors bar's cap and middle); one that is not hides outside.
    #[test]
    fn soft_masks_multiply_and_exclusive_ones_skip_outside() {
        let soft = |rect, exclusive| MaskLayer {
            soft: true,
            exclusive,
            ..layer(rect, None)
        };
        let run = Clip::new(
            vec![
                soft([0., 0., 10., 10.], true),
                soft([10., 0., 10., 10.], true),
            ],
            None,
        );
        for p in [[5., 5.], [15., 5.], [30., 5.]] {
            assert_eq!(run.coverage(p), 1., "{p:?}");
        }
        let lone = Clip::new(vec![soft([0., 0., 10., 10.], false)], None);
        assert_eq!(lone.coverage([5., 5.]), 1.);
        assert_eq!(lone.coverage([30., 5.]), 0.);
        assert_eq!(lone.bounds(), [0., 0., 10., 10.]);
    }

    // Needs the game: the archives HUD, authored and as the game shows it.
    #[test]
    fn archives_lay_out_as_in_game() {
        let Some(install) = install() else { return };
        let asset = read(&install, "hudarchivesscene");
        let authored = lay(
            &install,
            &asset,
            View {
                sample: false,
                ..game()
            },
        )
        .0;
        let nodes = lay(&install, &asset, game()).0;
        // The archive buttons store IsLocked=true, but a binding from the
        // game's unlock state feeds it, so sample data drops it. The lock
        // atom is an enum widget whose widgets all belong to the locked
        // style; unlocked, none of them draw.
        let atom = "5c4869bd-a878-457c-93c4-2370acc08e20";
        let locks = |nodes: &[Node]| {
            shown(nodes, "Icon_01")
                .iter()
                .filter(|n| n.owners[0].0.to_string() == atom)
                .count()
        };
        assert!(locks(&authored) > 0, "authored state shows the locks");
        assert_eq!(locks(&nodes), 0);
        // With sample data a list template (here an archive tab group)
        // shows several items stamped from its one prototype, stacked;
        // without, only the prototype.
        assert_eq!(items(&authored, "MetaBtns_02"), 1);
        assert_eq!(
            items(&nodes, "MetaBtns_02"),
            3,
            "three items at three heights"
        );
        // The column's buttons pick their first / middle / last look from
        // the list's per-item DataSourceIndex, so each group reads as one
        // strip with dividers between its buttons only.
        let count = |name| shown(&nodes, name).len();
        let (first, middle, last) = (count("Base_Left"), count("Base"), count("Base_Right"));
        assert!(
            first > 0 && middle > 0 && last > 0,
            "{first} {middle} {last}"
        );
        // One divider per button but the last of each group.
        assert_eq!(count("Divider"), first + middle);
        // The tabs get their size through a chain the templates compute
        // (list length from the column width, items from the length, the
        // tab from a scaling tool's OutputValue-), which takes the size
        // rounds and template outputs to settle. In the reference shot
        // they are about 45 × 50.
        let tabs = shown(&nodes, "ArchiveBtns");
        assert!(!tabs.is_empty());
        for t in tabs {
            let [_, _, w, h] = t.rect;
            assert!(
                (35. ..55.).contains(&w) && (40. ..60.).contains(&h),
                "{:?}",
                t.rect
            );
        }
        // Game strings nothing fills read as "": the count badges stay
        // hidden.
        assert!(
            !shown(&nodes, "DropShadow").iter().any(|n| n.rect[0] < 200.),
            "archive badges without a count are hidden"
        );
    }

    // Needs the game: the governors bar as the game shows it.
    #[test]
    fn governors_bar_lays_out_as_in_game() {
        let Some(install) = install() else { return };
        let nodes = screen(&install, "hudgovernorsbarscene");
        // The island name reaches its text input through a two-way binding
        // (`-Text` → Text) and the TextInput type's native DisplayedText;
        // the label's stored "WWWW…" is only a sizing placeholder.
        let input = named(&nodes, "ui_textInput");
        assert!(!input.is_empty());
        for n in input {
            let t = n.text.as_deref().unwrap_or("");
            assert!(!t.is_empty() && !t.starts_with("WWW"), "{t:?}");
        }
        // The top band takes its width from a Vector2 input written one
        // axis at a time (UI scale 1 → 1.11 × 1200); the reference
        // screenshot shows it across x 330..1590.
        let band = shown(&nodes, "HUDGovernorsBar_Top_BG")[0].rect;
        assert!(band[2] > 1100. && band[2] < 1400., "{band:?}");
        assert!((band[0] + band[2] / 2. - 960.).abs() < 5.);
        // The second row shows its own island's needs (not the "other
        // party" view a data-change clip swaps to), and its sample entries
        // sit side by side without overlapping.
        let (row, inside) = under(&nodes, "Right_OptionalNeeds");
        assert!(row.visible && row.opacity > 0.9);
        let mut xs: Vec<f32> = inside
            .iter()
            .filter(|n| n.depth == row.depth + 1 && n.visible)
            .map(|n| n.rect[0])
            .collect();
        xs.sort_by(f32::total_cmp);
        assert!(xs.len() >= 2, "{xs:?}");
        for w in xs.windows(2) {
            assert!(w[1] - w[0] > 40., "entries overlap: {xs:?}");
        }
        // Stat labels get their text through nested providers.
        let labels = shown(&nodes, "H02");
        assert!(!labels.is_empty());
        assert!(
            labels.iter().any(|n| n.text.as_deref() == Some("19,743")),
            "{:?}",
            labels.iter().map(|n| &n.text).collect::<Vec<_>>()
        );
        // Bindings that size things from a label's text get a size: the
        // bar splits its stats strip in three (at interface scale 1.0) and
        // centres icon + value in each slot from the label's DisplayedSize.
        let slots = shown(&nodes, "HUDGovernorsBar_IconText_Global");
        assert_eq!(slots.len(), 3);
        for n in slots {
            assert!(n.rect[2] > 100., "slot {:?}", n.rect);
        }
    }

    // Needs the game: the construction menu as the game shows it.
    #[test]
    fn construction_menu_lays_out_as_in_game() {
        let Some(install) = install() else { return };
        let asset = read(&install, "constructionmenuscene");
        let (mut nodes, frames) = lay(&install, &asset, game());
        // Sample data stands in only for what a parent feeds from game
        // data. The round buttons keep their `-ShowDeco` default (off)
        // unless the instance turns it on: in game only the left main
        // button has its gold ring.
        let rings = shown(&nodes, "deco_circle_gold_btn_92.png");
        assert_eq!(rings.len(), 1);
        assert!(rings[0].rect[0] < 960. && rings[0].rect[2] > 80.);
        // The round main button shows (button data's IsVisible samples on)
        // and sits on screen: its ProportionalPivotPoint x is bound per
        // axis (-Mirror), and writing that axis must not reset the other.
        let main: Vec<_> = shown(&nodes, "Btn_Main")
            .into_iter()
            .filter(|n| n.rect[2] > 70. && n.rect[1] > 900.)
            .collect();
        assert!(!main.is_empty());
        for n in main {
            let [_, y, _, h] = n.rect;
            assert!(y + h <= 1081., "on screen: {:?}", n.rect);
        }
        // A pin slot's GameObjectLoader has no object with sample data, so
        // it shows none of its variants (Unit, Constr.), is zero wide, and
        // the slot shows its empty-slot square instead.
        assert!(named(&nodes, "Constr.").iter().all(|n| !n.visible));
        assert!(!shown(&nodes, "ConstrMenuPinSlot_Core").is_empty());
        // The 300 px blue placeholder disc is hidden by EventToFalse on the
        // root's "shown" event.
        let disc = named(&nodes, "circle120x120.png");
        assert!(!disc.is_empty() && disc.iter().all(|n| !n.visible));
        // The pin slots are a Grid of 99 columns and 1 row, so its sample
        // entries make one row: each slot is 48×75; three in a row, not a
        // 225 px column.
        let pins = named(&nodes, "HUDPinsArray");
        assert!(!pins.is_empty());
        for n in &pins {
            let [_, _, w, h] = n.rect;
            assert!(h < 100. && w > 140., "a row, not a column: {:?}", n.rect);
        }
        // Scroll bars start hidden (the pins list fits), and the pins list
        // then gives its slots its whole height instead of leaving 8.5 px
        // for a bar.
        assert!(named(&nodes, "ScrollBar_DS").iter().all(|n| !n.visible));
        let list = pins[0].rect;
        let slot = named(&nodes, "ConstrMenuPinSlotContainer")[0].rect;
        assert!(
            (slot[3] - list[3]).abs() < 0.5,
            "slot {slot:?} list {list:?}"
        );
        // The bars' marble Pattern binds UVScale to its own Scale, which
        // its bound Size sets, so the 256 px tile repeats at its density
        // across the band (not the 7.1 stored for the wider governors bar).
        let i = (0..nodes.len())
            .find(|&i| *nodes[i].name == *"Pattern" && nodes[i].visible && nodes[i].rect[2] > 1000.)
            .unwrap();
        let repeats = frames[i].bound.uv_scale[0].unwrap();
        let want = nodes[i].rect[2] / 256.;
        assert!((repeats - want).abs() < 0.05, "{repeats} vs {want}");
        // Pin slots stack drop zones the game shows only while dragging:
        // runtime helpers.
        crate::ui::states::apply(&mut nodes, &[], &Default::default(), false);
        let zones: Vec<usize> = (0..nodes.len())
            .filter(|&i| *nodes[i].name == *"DropZone_DS_Entry")
            .collect();
        assert!(!zones.is_empty());
        for i in zones {
            let d = nodes[i].depth;
            let drawn = nodes[i + 1..]
                .iter()
                .take_while(|n| n.depth > d)
                .filter(|n| n.visible && n.image.is_some())
                .count();
            assert_eq!(drawn, 0, "drop zone {:?} draws", nodes[i].rect);
        }
    }

    // Needs the game: the transfer-goods popup opens when the root's SHOWN
    // event Starts its transition template, whose vec3 track ends with
    // opacity (z) 1.
    #[test]
    fn screens_open_through_their_clips() {
        let Some(install) = install() else { return };
        let transfer = screen(&install, "transfergoodsscene");
        let content = named(&transfer, "Content")[0];
        assert!(
            content.visible && content.opacity > 0.99,
            "{:?}",
            content.opacity
        );
        // A close button's template writes its icon into a field of the
        // inner button's data input (`-IconData…`), which the inner
        // template reads back along the same path; the popup shows the
        // close cross, not a sample icon. The button's icon is inside a
        // clip, so composited; the cross is near-white where the sample
        // coin is gold.
        let close = shown(&transfer, "BtnCloseS1_DS")
            .first()
            .expect("the close button")
            .rect;
        let inside = |r: [f32; 4]| {
            r[0] >= close[0] && r[1] >= close[1] && r[0] + r[2] <= close[0] + close[2] + 0.5
        };
        let icon = transfer
            .iter()
            .find(|n| n.visible && &*n.name == "2DHolder_AspectRatio" && inside(n.rect))
            .and_then(|n| n.image.as_ref())
            .expect("the close button's icon");
        let (mut sum, mut n) = ([0f32; 3], 0f32);
        for p in icon.as_bytes(0).unwrap().chunks(4).filter(|p| p[3] > 200) {
            for c in 0..3 {
                sum[c] += f32::from(p[c]);
            }
            n += 1.;
        }
        // RenderImage pixels are BGRA.
        let [b, g, r] = sum.map(|v| v / n.max(1.));
        let shows_cross = n > 0. && r > 150. && (r - b).abs() < 40. && (g - b).abs() < 40.;
        assert!(shows_cross);

        // SHOWN Starts the diplomacy screen's `Default` clip (alpha 0), the
        // scene transition's Show then Starts `OpeningAnim`; the object
        // menus open on Show alone.
        let diplomacy = screen(&install, "diplomacyscene");
        let menu = screen(&install, "omgenericscene");
        for (file, nodes) in [("diplomacyscene", &diplomacy), ("omgenericscene", &menu)] {
            let c = named(nodes, "SceneContainer")[0];
            assert!(c.visible && c.opacity > 0.99, "{file}: {}", c.opacity);
        }
        // Diplomacy's participant spokes turn their own template root.
        let mut at: Vec<[i32; 2]> = named(&diplomacy, "DiplomacyRelationshipLine")
            .iter()
            .map(|n| [n.rect[0].round() as i32, n.rect[1].round() as i32])
            .collect();
        let all = at.len();
        at.sort();
        at.dedup();
        assert!(all > 1 && at.len() == all, "spokes at {at:?}");
    }

    // Needs the game: a texture a binding sets by path draws as the
    // ImageAsset of that texture has it: the hairline ornament is a 40×12
    // 9-slice (its 4k texture is 160×48), not a stretched whole texture.
    #[test]
    fn textures_by_path_take_their_image_asset() {
        let Some(install) = install() else { return };
        let image = Images::default()
            .by_path(
                &install,
                "data\\ui\\fhd\\base\\ds\\deco\\deco_roman_1side_40x12.png",
                [255; 3],
                0,
            )
            .unwrap();
        assert_eq!(image.size, [40., 12.]);
        assert!(image.slices.is_some());
    }

    // Needs the game: the minimap as the game shows it, and in design view.
    #[test]
    fn minimap_lays_out_as_in_game() {
        let Some(install) = install() else { return };
        let asset = read(&install, "minimapscene");
        let nodes = lay(&install, &asset, game()).0;
        // The ring's sectors (its 18 entries from the MinimapScene assets:
        // day/night, 4 speeds, 3 controls, 2 lodes, 8 fertilities) each get
        // their slice. The angle reaches the sector drawing through its
        // animator: the parent writes the animator's own component
        // (AngleSize), and the animator settles its output to it. Before,
        // the drawing kept its stored 93° and every sector was a 254 px
        // wide square.
        assert_eq!(shown(&nodes, "Image_").len(), 18);
        // Fertilities draw no button tile.
        let sectors = shown(&nodes, "Base_Sq");
        assert_eq!(sectors.len(), 10);
        // A slice of a radius-175 ring is a box under 200 px either side,
        // turned.
        for n in &sectors {
            assert!(n.rect[2] < 200. && n.rect[3] < 200., "{:?}", n.rect);
        }
        // Opening Starts a button's Selected clip only when it is selected
        // (`IfTrueAssign [Shown, IsSelected]`): unselected sectors keep
        // their blue-grey base colour.
        for n in &sectors {
            let [r, _, b] = n.color.unwrap();
            assert!(b > r, "sector base {:?}", n.color);
        }
        // The ring places its sectors in the wheel's groups (day/night,
        // speed, controls, lodes, fertilities) from one step before 138°:
        // each sector turns once (no two share a spot) and the four speed
        // buttons centre on the bottom, as in game. The ring's centre is
        // the backing disc's: the list's own box follows whichever badges
        // show.
        let ring = named(&nodes, "backing_stroke")[0].rect;
        let mid = [ring[0] + ring[2] / 2., ring[1] + ring[3] / 2.];
        let angles: Vec<f32> = named(&nodes, "Image_")
            .iter()
            .map(|n| {
                let c = [n.rect[0] + n.rect[2] / 2., n.rect[1] + n.rect[3] / 2.];
                (c[0] - mid[0])
                    .atan2(mid[1] - c[1])
                    .to_degrees()
                    .rem_euclid(360.)
            })
            .collect();
        let mut sorted = angles.clone();
        sorted.sort_by(f32::total_cmp);
        assert!(
            sorted.windows(2).all(|w| w[1] - w[0] > 10.),
            "sector angles {angles:?}"
        );
        let speed = angles[1..5].iter().sum::<f32>() / 4.;
        assert!((speed - 180.).abs() < 10., "sector angles {angles:?}");
        // The wind arrows store their texture as a path on top of the
        // empty `null_16` placeholder asset: they draw it.
        let wind = named(&nodes, "wind_direction")[0];
        assert!(wind.visible && wind.image.is_some());
        // IsHidden only hides in design view.
        let icons = |nodes: &[Node]| shown(nodes, "2DHolder_AspectRatio").len();
        assert!(icons(&nodes) >= 6);
        let designed = lay(
            &install,
            &asset,
            View {
                design: true,
                ..game()
            },
        )
        .0;
        assert_eq!(icons(&designed), 0);
        // Bindings that size things from a label's text get a size: the
        // lode badge is its label + 18.
        let badges = shown(&nodes, "Badge_01");
        assert!(!badges.is_empty());
        for n in &badges {
            assert!(n.rect[2] < 90., "badge {:?}", n.rect);
        }
        // Each count sits inside its disc: the label overflows its 0×0
        // TextPlane centred, the disc's art is centred on its origin.
        let counts = shown(&nodes, "text_lode_01");
        assert_eq!(counts.len(), badges.len());
        let centre = |r: [f32; 4]| [r[0] + r[2] / 2., r[1] + r[3] / 2.];
        for (t, b) in counts.iter().zip(&badges) {
            let (c, d) = (centre(t.rect), centre(b.rect));
            assert!(
                (c[0] - d[0]).hypot(c[1] - d[1]) < b.rect[2] / 2.,
                "count {:?} outside badge {:?}",
                t.rect,
                b.rect
            );
        }
    }

    // Needs the game: ColorTint carries down to children. The badge
    // circle is a tinted Stroke container over white images.
    #[test]
    fn color_tint_carries_down_to_children() {
        let Some(install) = install() else { return };
        let circle = screen(&install, "8b03dd9e-5ae9-486e-84e4-4b1d32fe3251");
        let stroke = named(&circle, "Stroke")[0].color;
        assert_eq!(named(&circle, "HalfCircle_right")[0].color, stroke);
    }

    // Needs the game: layouts keep evaluations for the next one; after an
    // edit to the open file, a relayout must match laying the edited file
    // out from scratch.
    #[test]
    fn relayout_after_an_edit_matches_a_fresh_layout() {
        let Some(install) = install() else { return };
        let asset = read(&install, "hudgovernorsbarscene");
        let look = |nodes: &[Node]| {
            nodes
                .iter()
                .map(|n| (n.rect.map(f32::to_bits), n.visible, n.text.clone()))
                .collect::<Vec<_>>()
        };
        let name = |c: u32| format!("{c:08x}");
        let mut warm = Images::default();
        let (before, frames) = layout(&asset, &install, &mut warm, &HashMap::new(), game(), name);
        // Move the first widget the file stores a Position for.
        let span = frames.iter().find_map(|f| f.position_span).unwrap();
        let anno_formats::phx::Record::Value(Value::Vec3(p)) = asset.spans()[span].record else {
            panic!("position is a Vec3")
        };
        let moved = asset
            .apply(&Edit::Value {
                span,
                value: Value::Vec3([p[0] + 40., p[1] + 7., p[2]]),
            })
            .unwrap()
            .asset;
        let again = layout(&moved, &install, &mut warm, &HashMap::new(), game(), name).0;
        let fresh = lay(&install, &moved, game()).0;
        assert!(look(&again) != look(&before), "the edit moves something");
        assert!(look(&again) == look(&fresh));
    }

    // Needs the game: hovering relays out with a simulated state on one
    // instance, starting from the last layout's tree; moving the hover
    // from one button to another, and off again, must land where a fresh
    // layout does.
    #[test]
    fn hover_relayout_matches_a_fresh_layout() {
        let Some(install) = install() else { return };
        let asset = read(&install, "hudarchivesscene");
        let view = |sims: Vec<layout::Sim>| View { sims, ..game() };
        let look = |nodes: &[Node]| {
            nodes
                .iter()
                .map(|n| {
                    (
                        n.rect.map(f32::to_bits),
                        n.visible,
                        n.opacity.to_bits(),
                        n.text.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let name = |c: u32| format!("{c:08x}");
        let mut warm = Images::default();
        let (before, _) = layout(
            &asset,
            &install,
            &mut warm,
            &HashMap::new(),
            view(vec![]),
            name,
        );
        // Instances whose template has a hover clip, innermost owner first.
        let mut hovers: Vec<layout::Sim> = Vec::new();
        for &(g, inst) in before.iter().flat_map(|n| n.owners.iter()) {
            let Some(tpl) = warm.asset(&install, g) else {
                continue;
            };
            let clip = tpl.clips().iter().position(|c| {
                ["Enter", "MouseEnter", "OnMouseEnter", "Hover"]
                    .iter()
                    .any(|h| c.name.eq_ignore_ascii_case(h))
                    && !c.tracks.is_empty()
            });
            if let Some(clip) = clip {
                let sim = layout::Sim {
                    file: Some(g),
                    instance: Some(inst),
                    clip,
                };
                if !hovers.contains(&sim) {
                    hovers.push(sim);
                }
            }
        }
        assert!(hovers.len() >= 2, "two hoverable instances");
        let mut changed = false;
        for sims in [vec![hovers[0]], vec![hovers[1]], vec![]] {
            let again = layout(
                &asset,
                &install,
                &mut warm,
                &HashMap::new(),
                view(sims.clone()),
                name,
            )
            .0;
            changed |= look(&again) != look(&before);
            let fresh = lay(&install, &asset, view(sims.clone())).0;
            assert!(look(&again) == look(&fresh), "relayout with {sims:?}");
        }
        assert!(changed, "a hover shows");
    }

    // Needs the game: binding evaluation reruns only bindings whose sources
    // changed; it must land where running every binding on every sweep
    // does. These screens have places with several writers, readers
    // between them, vector component axes and bound text.
    #[test]
    fn worklist_evaluation_matches_full_sweeps() {
        let Some(install) = install() else { return };
        for file in [
            "hippodromeeventtestracescene",
            "diplomacyscene",
            "omgenericscene",
        ] {
            let asset = read(&install, file);
            let run = |full: bool| {
                phx::binding::set_full_sweeps(full);
                let nodes = lay(&install, &asset, game()).0;
                phx::binding::set_full_sweeps(false);
                nodes
                    .iter()
                    .map(|n| {
                        (
                            n.rect.map(f32::to_bits),
                            n.visible,
                            n.opacity.to_bits(),
                            n.text.clone(),
                            n.color,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert!(run(false) == run(true), "{file}");
        }
    }

    // Needs the game: a centred label sized to its text sits in the middle
    // of its 120 px TextPlane, where its plane-sized sibling is centred.
    #[test]
    fn content_sized_labels_align_inside_their_plane() {
        let Some(install) = install() else { return };
        let asset = read(&install, "0247744c-0eac-4447-8ea8-b02afe6360be");
        let nodes = lay(&install, &asset, View::default()).0;
        let centre = |name: &str| {
            let r = named(&nodes, name)[0].rect;
            r[0] + r[2] / 2.
        };
        let (a, b) = (centre("txt_statusName"), centre("txt_newStatus"));
        assert!((a - b).abs() < 1., "{a} vs {b}");
    }

    // Needs the game: a quest objective (HorizontalOverflow Wrap in a
    // 100 px TextPlane) breaks into lines.
    #[test]
    fn wrapping_labels_break_inside_their_text_plane() {
        let Some(install) = install() else { return };
        let asset = read(&install, "07d841cc-f3ec-499b-a749-94246e7dd983");
        let nodes = lay(&install, &asset, View::default()).0;
        let text = named(&nodes, "B2")[0].text.as_deref().unwrap();
        assert!(text.lines().count() > 2, "{text:?}");
    }

    // Needs the game: the archive popup (`e5be560d…`, the notification
    // archive's panel) shows the line its instance stores the id of in its
    // header ("!Archive" is the designer's fallback), and stays visible
    // though nothing reports its displayed text yet.
    #[test]
    fn archive_header_reads_its_localized_title() {
        let Some(install) = install() else { return };
        let texts = install
            .read(install.game().profile().texts_path)
            .map(|b| phx::parse_texts(&String::from_utf8_lossy(&b)))
            .unwrap();
        let asset = read(&install, "e5be560d-d94b-4f25-ad7a-13118b1c2c13");
        let name = |c: u32| format!("{c:08x}");
        let (nodes, _) = layout(
            &asset,
            &install,
            &mut Images::default(),
            &texts,
            game(),
            name,
        );
        let (header, inside) = under(&nodes, "HeaderS2_DS");
        assert!(header.visible);
        let label = inside
            .iter()
            .find(|n| *n.name == *"BaseText" && n.visible)
            .unwrap();
        assert_eq!(label.text.as_deref(), Some("Archive"));
        // Albertus Nova lines are 1.442 em: the 20 px title's box is 29 px
        // tall, so the hairline stacked under it hides behind the tabs
        // but for its tips, as in game.
        assert!(
            (label.rect[3] - 28.84).abs() < 0.5,
            "title box {:?}",
            label.rect
        );
        let hairline = inside.iter().find(|n| *n.name == *"Centre").unwrap();
        assert!(hairline.rect[1] >= label.rect[1] + label.rect[3] - 0.5);
    }

    // Needs the game: the object menu hangs off the screen's bottom-right
    // corner by a pivot its controller reads from the background's object
    // output (width, and a height the controller itself writes back into
    // it).
    #[test]
    fn object_menu_sits_inside_the_screen() {
        let Some(install) = install() else { return };
        let nodes = screen(&install, "omproductionscene");
        let r = named(&nodes, "SceneContainer")[0].rect;
        // 300 wide (the background's `_Width_In`), right edge on the
        // controller's position, 10 px in from the screen's edge.
        assert!(
            (r[0] - 1610.).abs() < 1. && (r[2] - 300.).abs() < 1.,
            "rect {r:?}"
        );
    }

    // Needs the game: the quest tracker as the game shows it.
    #[test]
    fn quest_tracker_lays_out_as_in_game() {
        let Some(install) = install() else { return };
        let nodes = screen(&install, "questtrackerscene");
        // The tracked quest's objective text comes through a list's item
        // data and two text templates, so only the widgets around it
        // (`Objectives_Container`) say it is an objective; sample data then
        // shows a sentence, not a figure.
        let (_, list) = under(&nodes, "Objectives_List");
        let texts: Vec<&str> = list
            .iter()
            .filter(|n| n.visible)
            .filter_map(|n| n.text.as_deref())
            .collect();
        assert!(
            texts.iter().any(|t| t.starts_with("Use a street")),
            "{texts:?}"
        );
        // The patched objective binds a 1.2 LineSpacing to its label: its
        // wrapped lines sit 1.25 em × 1.2 apart, as its box is sized.
        let body = list
            .iter()
            .find(|n| n.visible && n.text.as_deref().is_some_and(|t| t.starts_with("Use")))
            .unwrap();
        assert!(
            (body.line_height / body.text_size - 1.5).abs() < 0.01,
            "{} / {}",
            body.line_height,
            body.text_size
        );
        // The tracked quest's header shows one title: the IconText the
        // 2026-11-05 patch added beside it has nothing bound, so it stays
        // empty as in game.
        let (_, content) = under(&nodes, "Group_Content");
        let titles: Vec<&str> = content
            .iter()
            .filter(|n| n.visible)
            .filter_map(|n| n.text.as_deref())
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(titles.len(), 1, "{titles:?}");
        // Sample data leaves lists of events (untracked quests, buffs)
        // empty.
        assert_eq!(items(&nodes, "UntrackedQuest_List"), 0);
    }

    #[test]
    fn moves_and_resizes_invert_the_layout_transform() {
        let frame = Frame {
            parent_scale: [2., 2.],
            position: [10., 10., 0.],
            pivot: [32., 32., 0.],
            scale: [0.5, 0.5, 1.],
            intrinsic: [64., 64.],
            position_span: Some(1),
            scale_span: Some(2),
            ..Frame::default()
        };
        // World box: offset 100 + 2·(10 − 0.5·32) = 88; size 2·0.5·64 = 64.
        let before = [88., 38., 64., 64.];
        let moved = edits_for(&frame, before, [98., 38., 64., 64.]).unwrap();
        assert!(
            matches!(&moved[..], [Edit::Value { span: 1, value: Value::Vec3(p) }] if (p[0] - 15.).abs() < 1e-4 && p[1] == 10.)
        );
        let grown = edits_for(&frame, before, [88., 38., 128., 64.]).unwrap();
        assert!(
            matches!(&grown[0], Edit::Value { span: 2, value: Value::Vec3(s) } if (s[0] - 1.).abs() < 1e-4 && s[1] == 0.5)
        );
        let fixed = Frame {
            scale_span: None,
            ..frame
        };
        assert!(edits_for(&fixed, before, [88., 38., 100., 64.]).is_err());
    }
}
