// The placeable library: every prop model and every object the shipped
// islands use, with a shipped island to copy each object's record from.
// Built once from the install off the UI thread (a few seconds) and cached
// in the config folder, as are the thumbnails rendered for it.
use super::models::{self, Cache};
use anno_core::files::ModelLook;
use anno_core::game::IslandRules;
use anno_core::island::objects::Objects;
use anno_formats::{adf::Adf, rda::Archive, vfs::Install};
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    camera::{Camera, Extent},
    gpu::{Batch, Instances, Offscreen, Renderer, Scene},
    gpui::RenderImage,
    math,
    texture::Texture,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

/// The Library pane's lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Props,
    Decorations,
    Objects,
    /// Models the project and its dependencies carry.
    Mod,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PropEntry {
    /// `.cfg` path as the rd3d `PropGrid/FileNames` store it.
    pub path: String,
    /// Instances over all shipped islands.
    pub uses: u32,
    /// Median vertical scale of those instances: 117 places some tree
    /// models at three times their size, so a model's height alone says
    /// little about how tall it stands.
    pub scale: f32,
    /// Provinces whose shipped islands place it (`roman`, `colony01`, …),
    /// sorted. Shared library folders hide that Latium's maritime pines
    /// grow on no Albion island; this does not.
    pub regions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectEntry {
    pub guid: u32,
    /// A shipped island with one, to copy its record from.
    pub island: String,
    pub uses: u32,
}

/// A model placed through a generic object (`SimpleCfg` and the like) with
/// its own `DynamicVariation/ConfigFilename`: most shipped decoration.
#[derive(Clone, Debug, PartialEq)]
pub struct DecorEntry {
    pub config: String,
    /// The object asset carrying it.
    pub guid: u32,
    pub island: String,
    pub uses: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    /// Most used first.
    pub props: Vec<PropEntry>,
    pub objects: Vec<ObjectEntry>,
    pub decorations: Vec<DecorEntry>,
}

impl Catalog {
    /// Scans every shipped island's prop grid and objects.
    pub fn build(rules: &IslandRules, install: &Install) -> Result<Catalog> {
        let islands: Vec<String> = install
            .search(".a7m", 100_000)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .filter(|p| p.ends_with(".a7m") && p.contains("/islands/"))
            .collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = islands.len().div_ceil(threads).max(1);
        let scanned: Vec<Scan> = std::thread::scope(|scope| {
            let workers: Vec<_> = islands
                .chunks(chunk)
                .map(|paths| {
                    scope.spawn(move || {
                        paths
                            .iter()
                            .filter_map(|p| {
                                scan(install, p)
                                    .map_err(|e| eprintln!("catalog: {p}: {e:#}"))
                                    .ok()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().expect("catalog worker"))
                .collect()
        });
        // Per model: instances, their vertical scales, the provinces.
        let mut props: BTreeMap<String, (u32, Vec<f32>, Vec<String>)> = BTreeMap::new();
        let mut objects: HashMap<u32, ObjectEntry> = HashMap::new();
        let mut decorations: HashMap<String, DecorEntry> = HashMap::new();
        for s in scanned {
            for (config, (guid, n)) in s.decorations {
                decorations
                    .entry(config.clone())
                    .or_insert_with(|| DecorEntry {
                        config,
                        guid,
                        island: s.path.clone(),
                        uses: 0,
                    })
                    .uses += n;
            }
            let region = rules.island_region(&s.path).to_ascii_lowercase();
            for (path, (n, scales)) in s.props {
                let e = props.entry(path).or_default();
                e.0 += n;
                e.1.extend(scales);
                if !e.2.contains(&region) {
                    e.2.push(region.clone());
                }
            }
            for (guid, n) in s.objects {
                objects
                    .entry(guid)
                    .or_insert_with(|| ObjectEntry {
                        guid,
                        island: s.path.clone(),
                        uses: 0,
                    })
                    .uses += n;
            }
        }
        let mut props: Vec<PropEntry> = props
            .into_iter()
            .map(|(path, (uses, scales, mut regions))| {
                regions.sort_unstable();
                PropEntry {
                    path,
                    uses,
                    scale: median(scales),
                    regions,
                }
            })
            .collect();
        props.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.path.cmp(&b.path)));
        let mut objects: Vec<ObjectEntry> = objects.into_values().collect();
        objects.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.guid.cmp(&b.guid)));
        let mut decorations: Vec<DecorEntry> = decorations.into_values().collect();
        decorations.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.config.cmp(&b.config)));
        Ok(Catalog {
            props,
            objects,
            decorations,
        })
    }
}

impl Catalog {
    /// `module`'s catalog over what `install` ships. A game whose own codec
    /// reads its islands (2070) is scanned through it: every prop's model
    /// and every object's (by its asset in `assets`, the install's database)
    /// counts as a prop, since conversion places either kind by model.
    /// Others read their a7m prop grids and object stores (`build`).
    pub fn of(
        module: &dyn anno_core::game::Module,
        install: &Install,
        assets: Option<&anno_formats::assets::Assets>,
    ) -> Result<Catalog> {
        let islands = module.islands().context("this game has no islands")?;
        let Some(codec) = islands.codec().own() else {
            return Self::build(islands.rules(), install);
        };
        let shipped = islands.shipped(install, assets);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let chunk = shipped.len().div_ceil(threads).max(1);
        // Per island: model, instances and their vertical scales.
        let scanned: Vec<(&str, HashMap<String, (u32, Vec<f32>)>)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (shipped.chunks(chunk))
                .map(|list| {
                    scope.spawn(move || {
                        let mut out = Vec::new();
                        for s in list {
                            let island = match codec.read(install, &s.path, assets) {
                                Ok(island) => island,
                                Err(e) => {
                                    eprintln!("catalog: {}: {e:#}", s.path);
                                    continue;
                                }
                            };
                            let doc = &island.doc;
                            let mut models: HashMap<String, (u32, Vec<f32>)> = HashMap::new();
                            for p in doc.props.iter() {
                                if let Some(m) = doc.meshes.get(p.source.file as usize) {
                                    let e = models.entry(m.clone()).or_default();
                                    e.0 += 1;
                                    e.1.push(p.source.scale[1]);
                                }
                            }
                            for o in doc.objects.iter() {
                                let cfg = (assets.and_then(|a| a.get(o.guid)))
                                    .filter(|a| !a.cfgs.is_empty())
                                    .map(|a| a.cfgs[o.variation as usize % a.cfgs.len()].clone());
                                if let Some(cfg) = cfg {
                                    let e = models.entry(cfg.replace('\\', "/")).or_default();
                                    e.0 += 1;
                                    e.1.push(o.scale[1]);
                                }
                            }
                            out.push((s.region.as_str(), models));
                        }
                        out
                    })
                })
                .collect();
            (workers.into_iter())
                .flat_map(|w| w.join().expect("catalog worker"))
                .collect()
        });
        let mut props: BTreeMap<String, (u32, Vec<f32>, Vec<String>)> = BTreeMap::new();
        for (region, models) in scanned {
            for (path, (n, scales)) in models {
                let e = props.entry(path).or_default();
                e.0 += n;
                e.1.extend(scales);
                if !e.2.iter().any(|r| r == region) {
                    e.2.push(region.to_owned());
                }
            }
        }
        let mut props: Vec<PropEntry> = (props.into_iter())
            .map(|(path, (uses, scales, mut regions))| {
                regions.sort_unstable();
                PropEntry {
                    path,
                    uses,
                    scale: median(scales),
                    regions,
                }
            })
            .collect();
        props.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.path.cmp(&b.path)));
        Ok(Catalog {
            props,
            ..Catalog::default()
        })
    }
}

/// Median of `v`, 1 when empty.
fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 1.;
    }
    let mid = v.len() / 2;
    *v.select_nth_unstable_by(mid, f32::total_cmp).1
}

struct Scan {
    path: String,
    /// Prop file: instances, their vertical scales.
    props: HashMap<String, (u32, Vec<f32>)>,
    objects: HashMap<u32, u32>,
    /// Config path: carrying guid, count.
    decorations: HashMap<String, (u32, u32)>,
}

/// Every model island `path` places, normalized: prop files and the
/// models decoration objects name.
pub fn island_models(install: &Install, path: &str) -> Result<Vec<String>> {
    let s = scan(install, path)?;
    let mut out: Vec<String> = (s.props.into_keys())
        .chain(s.decorations.into_keys())
        .map(|m| anno_formats::rda::normalize_path(&m).into_owned())
        .collect();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// One island's prop files with instance counts, and object guids with
/// counts. Reads only the two ADF trees, not the whole island model.
fn scan(install: &Install, path: &str) -> Result<Scan> {
    let archive = Archive::parse(install.read(path)?, path)?;
    let rd = Adf::parse(&archive.read_path("rd3d.data")?)?;
    let grid = rd
        .child_node(rd.root(), "PropGrid")
        .context("no PropGrid")?;
    let files: Vec<String> = rd
        .children(rd.child_node(grid, "FileNames").context("no FileNames")?)
        .map(|f| rd.read_utf16(f))
        .collect::<Result<_>>()?;
    let mut counts = vec![(0u32, Vec::new()); files.len()];
    if let Some(instances) = rd.child_node(grid, "Instances") {
        for inst in rd.children(instances) {
            if let Some(i) = rd.child_value(inst, "Index")
                && let Some(n) = counts.get_mut(rd.read_u32(i)? as usize)
            {
                n.0 += 1;
                let scale = rd.child_value(inst, "Scale").map(|v| rd.read_f32s::<3>(v));
                if let Some(Ok([_, y, _])) = scale {
                    n.1.push(y);
                }
            }
        }
    }
    let props = files
        .into_iter()
        .zip(counts)
        .filter(|(_, n)| n.0 > 0)
        .collect();
    let gamedata = Adf::parse(&archive.read_path("gamedata.data")?)?;
    let mut objects = HashMap::new();
    let mut decorations: HashMap<String, (u32, u32)> = HashMap::new();
    if let Ok(store) = Objects::read(&gamedata) {
        for o in &store.objects {
            match &o.config {
                Some(config) => {
                    decorations
                        .entry(config.replace('\\', "/"))
                        .or_insert((o.guid, 0))
                        .1 += 1
                }
                None => *objects.entry(o.guid).or_default() += 1,
            }
        }
    }
    Ok(Scan {
        path: path.to_owned(),
        props,
        objects,
        decorations,
    })
}

/// Thumbnail side in pixels, as cached and shown.
pub const THUMB: u32 = 96;
/// Bumped when thumbnails should re-render (camera, lighting, size). 6: the
/// game's model look, dye and glow as the Assets preview draws them.
const THUMB_VERSION: u32 = 6;

/// What a prop looks like, for matching props across games: measured from
/// its thumbnail and model bounds when the thumbnail renders, and cached
/// next to it (`<key>.look`, 56 bytes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// Model bounds in metres: x, height, z.
    pub size: [f32; 3],
    /// Mean CIELAB of the upper and lower half of the silhouette: a tree's
    /// crown and trunk, a rock's lit top and shaded foot.
    pub top: [f32; 3],
    pub bottom: [f32; 3],
    /// Silhouette height over width, as the thumbnail shows it.
    pub aspect: f32,
    /// Share of covered pixels on the silhouette's edge: leaves and
    /// branches are ragged (0.2 and up), rocks and buildings smooth.
    pub ragged: f32,
    /// The silhouette on an 8x8 grid over its own bounding box, row-major
    /// from the top, one bit per cell at least 40% covered.
    pub mask: u64,
}

const LOOK_MAGIC: [u8; 4] = *b"LK02";

impl Look {
    /// Measures a thumbnail; `None` when nothing is covered.
    pub fn measure(img: &image::RgbaImage, size: [f32; 3]) -> Option<Look> {
        let (w, h) = img.dimensions();
        let covered = |x: u32, y: u32| img.get_pixel(x, y).0[3] >= 128;
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        for y in 0..h {
            for x in 0..w {
                if covered(x, y) {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
                }
            }
        }
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let (bw, bh) = (x1 - x0, y1 - y0);
        let (mut area, mut edge) = (0u32, 0u32);
        for y in y0..y1 {
            for x in x0..x1 {
                if !covered(x, y) {
                    continue;
                }
                area += 1;
                let open = |dx: i32, dy: i32| {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    nx < 0
                        || ny < 0
                        || nx >= w as i32
                        || ny >= h as i32
                        || !covered(nx as u32, ny as u32)
                };
                if open(1, 0) || open(-1, 0) || open(0, 1) || open(0, -1) {
                    edge += 1;
                }
            }
        }
        let mut cells = [[0u32; 2]; 64];
        let mut halves = [([0f64; 3], 0u32); 2];
        for y in y0..y1 {
            for x in x0..x1 {
                let cell = (((y - y0) * 8 / bh) * 8 + (x - x0) * 8 / bw) as usize;
                cells[cell][1] += 1;
                if !covered(x, y) {
                    continue;
                }
                cells[cell][0] += 1;
                let half = &mut halves[usize::from((y - y0) * 2 >= bh)];
                let px = img.get_pixel(x, y).0;
                for (sum, &v) in half.0.iter_mut().zip(&px) {
                    *sum += f64::from(v);
                }
                half.1 += 1;
            }
        }
        let mask = (cells.iter().enumerate())
            .filter(|(_, c)| c[1] > 0 && c[0] * 5 >= c[1] * 2)
            .fold(0u64, |m, (i, _)| m | 1 << i);
        let mean = |(sum, n): ([f64; 3], u32)| -> [f32; 3] {
            let n = f64::from(n.max(1));
            lab(sum.map(|v| (v / n) as f32))
        };
        let top = mean(halves[0]);
        // A silhouette one row tall has no lower half: reuse the upper.
        let bottom = if halves[1].1 == 0 {
            top
        } else {
            mean(halves[1])
        };
        Some(Look {
            size,
            top,
            bottom,
            aspect: bh as f32 / bw as f32,
            ragged: edge as f32 / area.max(1) as f32,
            mask,
        })
    }

    /// The same look at `scale` times its size.
    pub fn scaled(self, scale: f32) -> Look {
        Look {
            size: self.size.map(|v| v * scale),
            ..self
        }
    }

    fn to_bytes(self) -> Vec<u8> {
        let floats = (self.size.iter().chain(&self.top).chain(&self.bottom))
            .chain([&self.aspect, &self.ragged])
            .flat_map(|v| v.to_le_bytes());
        LOOK_MAGIC
            .into_iter()
            .chain(floats)
            .chain(self.mask.to_le_bytes())
            .collect()
    }

    fn from_bytes(b: &[u8]) -> Option<Look> {
        if b.len() != 56 || b[..4] != LOOK_MAGIC {
            return None;
        }
        let f = |i: usize| f32::from_le_bytes(b[4 + i * 4..8 + i * 4].try_into().unwrap());
        Some(Look {
            size: [f(0), f(1), f(2)],
            top: [f(3), f(4), f(5)],
            bottom: [f(6), f(7), f(8)],
            aspect: f(9),
            ragged: f(10),
            mask: u64::from_le_bytes(b[48..56].try_into().unwrap()),
        })
    }
}

/// sRGB 0..255 to CIELAB (D65), so color distances follow what eyes see.
fn lab(rgb: [f32; 3]) -> [f32; 3] {
    let lin = rgb.map(|v| {
        let v = v / 255.;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    });
    let xyz = [
        (0.4124 * lin[0] + 0.3576 * lin[1] + 0.1805 * lin[2]) / 0.9505,
        0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2],
        (0.0193 * lin[0] + 0.1192 * lin[1] + 0.9505 * lin[2]) / 1.089,
    ]
    .map(|t: f32| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16. / 116.
        }
    });
    [
        116. * xyz[1] - 16.,
        500. * (xyz[0] - xyz[1]),
        200. * (xyz[1] - xyz[2]),
    ]
}

/// Model thumbnails for the Library, rendered on a worker thread with its
/// own Vulkan device and cached as PNGs, keyed by cfg path.
pub struct Thumbs {
    images: HashMap<String, Option<Arc<RenderImage>>>,
    /// The same at a third the side, for rows and chips.
    small: HashMap<String, Arc<RenderImage>>,
    looks: HashMap<String, Look>,
    requested: HashSet<String>,
    send: mpsc::Sender<Vec<String>>,
    done: mpsc::Receiver<(String, Finished)>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The PNG cache, keyed like the worker keys it.
    dir: PathBuf,
}

// Dropping closes both queues and does not wait: the worker ends after the
// render it is on, when its answer finds nobody listening. Joining it here
// held the dropping thread, the UI's on a game switch, for that render:
// 0.45 s for a first one, which also builds its pipelines. Exit still
// waits for it (gpu::join_at_exit): a worker ending while exit tore the GPU
// driver down crashed the process.
impl Drop for Thumbs {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            gpui_editor_kit::gpu::join_at_exit(worker);
        }
    }
}

/// Where a test holds the worker inside a render: the worker says it got
/// there on the first and waits on the second, until a message comes or
/// the sender goes, so a failed test cannot strand it.
type RenderGate = (mpsc::Sender<()>, mpsc::Receiver<()>);

impl Thumbs {
    /// Starts the worker, drawing models with `look`; it exits when this
    /// is dropped.
    pub fn new(install: Arc<Install>, dir: PathBuf, look: ModelLook) -> Thumbs {
        Thumbs::spawn(install, dir, look, None)
    }

    fn spawn(
        install: Arc<Install>,
        dir: PathBuf,
        look: ModelLook,
        gate: Option<RenderGate>,
    ) -> Thumbs {
        let (send, requests) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let cache = dir.clone();
        let worker = std::thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || thumbnail_worker(&install, &dir, &look, requests, finished, gate))
            .expect("spawn thumbnail thread");
        Thumbs {
            images: HashMap::new(),
            small: HashMap::new(),
            looks: HashMap::new(),
            requested: HashSet::new(),
            send,
            done,
            worker: Some(worker),
            dir: cache,
        }
    }

    /// Forgets `cfg`'s thumbnail, in memory and on disk, so the next
    /// request renders it again: a model replaced in the mod.
    pub fn invalidate(&mut self, cfg: &str) {
        self.images.remove(cfg);
        self.small.remove(cfg);
        self.looks.remove(cfg);
        self.requested.remove(cfg);
        let file = self.dir.join(format!("{:016x}.png", thumb_key(cfg)));
        let _ = std::fs::remove_file(file.with_extension("look"));
        let _ = std::fs::remove_file(file);
    }

    /// The thumbnail of `cfg` once rendered; `None` while pending or when
    /// the model has nothing to draw.
    pub fn get(&self, cfg: &str) -> Option<Arc<RenderImage>> {
        self.images.get(cfg).cloned().flatten()
    }

    /// The thumbnail at row and chip size: thin stems kept visible. GPUI's
    /// scaling of the 96 px image averaged a marsh bush away to grey.
    pub fn get_small(&self, cfg: &str) -> Option<Arc<RenderImage>> {
        self.small.get(cfg).cloned()
    }

    /// What `cfg` looks like, once its thumbnail is in.
    pub fn look(&self, cfg: &str) -> Option<Look> {
        self.looks.get(cfg).copied()
    }

    /// Queues the cfgs not yet asked for. The latest call renders first, so
    /// what the pane shows now beats what it showed before a search.
    pub fn request<'a>(&mut self, cfgs: impl IntoIterator<Item = &'a str>) {
        let fresh: Vec<String> = cfgs
            .into_iter()
            .filter(|c| self.requested.insert((*c).to_owned()))
            .map(str::to_owned)
            .collect();
        if !fresh.is_empty() {
            let _ = self.send.send(fresh);
        }
    }

    /// Takes finished thumbnails; true if there were any.
    pub fn receive(&mut self) -> bool {
        let mut any = false;
        while let Ok((cfg, done)) = self.done.try_recv() {
            if let Some(look) = done.look {
                self.looks.insert(cfg.clone(), look);
            }
            if let Some(small) = done.small {
                self.small.insert(cfg.clone(), small);
            }
            self.images.insert(cfg, done.image);
            any = true;
        }
        any
    }

    pub fn pending(&self) -> bool {
        self.images.len() < self.requested.len()
    }

    /// Whether `cfg` was rendered and had nothing to draw, as against still
    /// pending: tiles say "No preview" then, where "…" read as loading that
    /// never ended.
    pub fn missing(&self, cfg: &str) -> bool {
        matches!(self.images.get(cfg), Some(None))
    }
}

/// A thumbnail ready to draw, both sizes.
struct Finished {
    image: Option<Arc<RenderImage>>,
    small: Option<Arc<RenderImage>>,
    look: Option<Look>,
}

impl Finished {
    /// Made on the worker: the rim and the shrink touch every pixel, and a
    /// pane of cached thumbnails arrives in one frame.
    fn new(img: Option<image::RgbaImage>, look: Option<Look>) -> Finished {
        // Models get the rim; island maps sit on their own sea blue.
        let model = look.is_some();
        let small = img.as_ref().and_then(|img| shrink_thin(img, 3, model));
        let image = img.and_then(|img| {
            let (w, h) = img.dimensions();
            let mut raw = img.into_raw();
            if model {
                halo(&mut raw, w, h);
            }
            Some(Texture::from_rgba(w, h, raw).ok()?.render_image())
        });
        Finished { image, small, look }
    }
}

fn thumbnail_worker(
    install: &Install,
    dir: &Path,
    look: &ModelLook,
    requests: mpsc::Receiver<Vec<String>>,
    finished: mpsc::Sender<(String, Finished)>,
    gate: Option<RenderGate>,
) {
    // Batches newest last; work takes the front of the newest.
    let mut batches: Vec<VecDeque<String>> = Vec::new();
    // Created on the first cache miss: most sessions only read PNGs.
    let mut gpu: Option<(Offscreen, Renderer)> = None;
    let mut broken = false;
    loop {
        batches.retain(|b| !b.is_empty());
        if batches.is_empty() {
            match requests.recv() {
                Ok(b) => batches.push(b.into()),
                Err(_) => break,
            }
        }
        batches.extend(requests.try_iter().map(VecDeque::from));
        let Some(cfg) = batches.last_mut().and_then(VecDeque::pop_front) else {
            continue;
        };
        // Islands: their map image, cropped to the land; no GPU and no look.
        // Keyed apart from models so their cache can change on its own.
        let map = cfg.ends_with(".a7m");
        let file = dir.join(format!(
            "{:016x}.png",
            thumb_key(&if map {
                format!("map{MAP_VERSION}:{cfg}")
            } else {
                cfg.clone()
            })
        ));
        if map {
            // A mod's island is redrawn as it is edited: its map read
            // fresh, never from the cache, where a picture taken while it
            // was an early draft stayed for good.
            let modded = install.mod_layer(&cfg).is_some();
            let cached = if modded {
                Err(())
            } else {
                image::open(&file).map_err(|_| ())
            };
            let image = match cached {
                Ok(img) => Some(img.to_rgba8()),
                Err(()) if !modded && std::fs::metadata(&file).is_ok_and(|m| m.len() == 0) => None,
                Err(()) if modded => map_thumbnail(install, &cfg),
                Err(()) => {
                    let made = map_thumbnail(install, &cfg);
                    let _ = std::fs::create_dir_all(dir);
                    let _ = match &made {
                        Some(img) => save_png(img, &file),
                        None => write_atomic(&file, &[]).map_err(anyhow::Error::from),
                    };
                    made
                }
            };
            if finished.send((cfg, Finished::new(image, None))).is_err() {
                break;
            }
            continue;
        }
        let look_file = file.with_extension("look");
        // A mod's own or a dependency's model can change under the same
        // path: one ported while its meshes were still being written left a
        // cached miss, and its tile stayed blank for good. Misses are kept
        // on disk for the game's files only.
        let modded = install.mod_layer(&cfg).is_some();
        let cached = image::open(&file)
            .ok()
            .map(|img| img.to_rgba8())
            .and_then(|img| {
                // Thumbnails from before looks were measured render again.
                let look = Look::from_bytes(&std::fs::read(&look_file).ok()?)?;
                Some((img, look))
            });
        let (image, look) = match cached {
            Some((img, look)) => (Some(img), Some(look)),
            // A cached miss is an empty file: the model has nothing to draw.
            None if !modded && std::fs::metadata(&file).is_ok_and(|m| m.len() == 0) => (None, None),
            None if broken => (None, None),
            None => {
                if gpu.is_none() {
                    match Offscreen::new(THUMB * 2, THUMB * 2) {
                        Ok(target) => {
                            // The families the look's meshes draw in.
                            let renderer = Renderer::with_families(look.families());
                            gpu = Some((target, renderer))
                        }
                        Err(e) => {
                            eprintln!("thumbnails: {e:#}");
                            broken = true;
                            continue;
                        }
                    }
                }
                // Exit waits for a render that has its Busy; past exit's
                // start there are no more.
                let Some(_busy) = gpui_editor_kit::gpu::busy() else {
                    break;
                };
                let (target, renderer) = gpu.as_mut().expect("created above");
                if let Some((reached, go)) = &gate {
                    let _ = reached.send(());
                    let _ = go.recv();
                }
                let rendered = render_thumbnail(install, target, renderer, &cfg, look)
                    .map_err(|e| eprintln!("thumbnail {cfg}: {e:#}"))
                    .ok()
                    .flatten();
                let look = (rendered.as_ref()).and_then(|(img, size)| Look::measure(img, *size));
                let _ = std::fs::create_dir_all(dir);
                let saved = match (&rendered, look) {
                    (Some((img, _)), Some(look)) => save_png(img, &file)
                        .and_then(|()| Ok(write_atomic(&look_file, &look.to_bytes())?)),
                    _ if modded => Ok(()),
                    _ => write_atomic(&file, &[]).map_err(anyhow::Error::from),
                };
                if let Err(e) = saved {
                    eprintln!("thumbnail cache {}: {e:#}", file.display());
                }
                (rendered.map(|r| r.0), look)
            }
        };
        if finished.send((cfg, Finished::new(image, look))).is_err() {
            break;
        }
    }
    // Freeing the target and renderer is GPU work too: while exit waits,
    // or not at all once it has begun.
    match gpui_editor_kit::gpu::busy() {
        Some(_busy) => drop(gpu),
        None => std::mem::forget(gpu),
    }
}

/// Bump when island map thumbnails change; model thumbnails keep theirs.
const MAP_VERSION: u32 = 5;

/// An island's minimap picture cut to the land with a small margin, square,
/// at twice the list's 30 px icon. Sea is transparent so the row shows
/// through; `None` without a picture or without land above the sea.
fn map_thumbnail(install: &Install, path: &str) -> Option<image::RgbaImage> {
    map_image(install, path, 60)
}

/// `path`'s map image cut to its land as [`map_thumbnail`] does, at
/// `edge` px a side: the Inspector's preview while an island is chosen.
pub fn map_preview(
    install: &Install,
    path: &str,
    edge: u32,
) -> Option<Arc<gpui_editor_kit::gpui::RenderImage>> {
    let img = map_image(install, path, edge)?;
    let (w, h) = img.dimensions();
    Some(
        Texture::from_rgba(w, h, img.into_raw())
            .ok()?
            .render_image(),
    )
}

fn map_image(install: &Install, path: &str, edge: u32) -> Option<image::RgbaImage> {
    cut(&land_image(install, path, edge)?, edge)
}

/// `full` cut to its opaque part, square, `edge` px a side.
fn cut(full: &image::RgbaImage, edge: u32) -> Option<image::RgbaImage> {
    let (w, h) = full.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (x, y, p) in full.enumerate_pixels() {
        if p.0[3] >= 128 {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    if x0 > x1 {
        return None;
    }
    // Square around the land's centre, 6 % margin, clear past the
    // picture's edge: minimap pictures are cut to the island, rarely
    // square, and a crop clamped to one was stretched square. Magnified 2x
    // at most, so a deco rock of a few texels stays a small rock instead
    // of a blur filling the tile.
    let side = (((x1 - x0).max(y1 - y0) as f32 * 1.12) as u32 + 1).max(edge / 2);
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    let mut crop = image::RgbaImage::new(side, side);
    let at = |c: u32| i64::from(side / 2) - i64::from(c);
    image::imageops::replace(&mut crop, full, at(cx), at(cy));
    // The filter weighs colour as it is: premultiplied, so the transparent
    // sea's black stays out of the coast instead of rimming it dark.
    for p in crop.pixels_mut() {
        let a = u32::from(p.0[3]);
        for c in 0..3 {
            p.0[c] = ((u32::from(p.0[c]) * a + 127) / 255) as u8;
        }
    }
    let mut out = image::imageops::resize(&crop, edge, edge, image::imageops::FilterType::Triangle);
    for p in out.pixels_mut() {
        let a = u32::from(p.0[3]);
        for c in 0..3 {
            if let Some(v) = (u32::from(p.0[c]) * 255 + a / 2).checked_div(a) {
                p.0[c] = v.min(255) as u8;
            }
        }
    }
    Some(out)
}

/// `path`'s minimap picture, `_gamedata/<name>/gamemapimage_0.dds`, sea
/// transparent, at the first mip no more than twice `edge` (512 at
/// least: a deco rock is a few texels of its picture). It is the
/// game's own cut: inland lakes, rivers and dark forest stay, shallows
/// go. Every shipped island has one and saves rewrite it with the others
/// (new::map_images). `mapimage_0.dds` is no use here: its sea is opaque,
/// and some shipped ones are stale, patched with blocks of an old render.
pub(crate) fn land_image(install: &Install, path: &str, edge: u32) -> Option<image::RgbaImage> {
    let bytes = install
        .read(&format!(
            "{}/gamemapimage_0.dds",
            super::doc::island_data_dir(path).ok()?
        ))
        .ok()?;
    let dds = gpui_editor_kit::texture::Dds::parse(&bytes).ok()?;
    // Mip 0 of a continental island's 1792 px picture made the hovered
    // preview decode and resize four times the texels; mip 1 is plenty.
    let image = dds.decode(&bytes, dds.mip_for((edge * 2).max(512))).ok()?;
    let mut img = image::RgbaImage::from_raw(image.width, image.height, image.rgba)?;
    land_alpha(&mut img);
    Some(img)
}

/// Clears what is not land from a minimap picture: Anno 1800's drop
/// shadow, pure black with its alpha fading out from the coast and opaque
/// right at it, and the near-black blocks some DLC 3 quadrant pictures
/// have where they were never drawn. Both touch the clear sea; dark land
/// does not, so the sea grows from the clear texels through dark ones
/// only. Black inside the land stays, as the game draws it: a long strip
/// across the Arctic's colony03_a03_01.
fn land_alpha(img: &mut image::RgbaImage) {
    // Below this in every channel a texel is shadow, if the sea reaches it.
    const DARK: u8 = 8;
    let (w, h) = (img.width() as usize, img.height() as usize);
    let texels = img.as_chunks_mut::<4>().0;
    let dark = |t: &[u8; 4]| t[0].max(t[1]).max(t[2]) < DARK;
    let clear = |t: &[u8; 4]| t[3] < 128;
    // The shadow's faint outer ring is sea outright.
    for t in texels.iter_mut() {
        if clear(t) && dark(t) {
            t[3] = 0;
        }
    }
    let around = |i: usize| {
        let (x, y) = (i % w, i / w);
        [
            (x > 0).then(|| i - 1),
            (x + 1 < w).then(|| i + 1),
            (y > 0).then(|| i - w),
            (y + 1 < h).then(|| i + w),
        ]
        .into_iter()
        .flatten()
    };
    // Texel indices fit u32: the kit caps a DDS side at MAX_SIDE.
    let mut stack: Vec<u32> = (0..texels.len())
        .filter(|&i| {
            let t = &texels[i];
            !clear(t) && dark(t) && around(i).any(|j| clear(&texels[j]))
        })
        .map(|i| i as u32)
        .collect();
    // Cleared when pushed, so each texel goes on the stack once.
    stack.iter().for_each(|&i| texels[i as usize][3] = 0);
    while let Some(i) = stack.pop() {
        for j in around(i as usize) {
            if !clear(&texels[j]) && dark(&texels[j]) {
                texels[j][3] = 0;
                stack.push(j as u32);
            }
        }
    }
}

/// FNV-1a over the version and path: stable across runs and Rust releases.
fn thumb_key(cfg: &str) -> u64 {
    THUMB_VERSION
        .to_le_bytes()
        .iter()
        .chain(cfg.to_ascii_lowercase().as_bytes())
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        })
}

/// `cfg` from a three-quarter view, fitted to its bounds, dyed and glowing
/// as `look` says; `None` when it has no drawable piece.
pub(crate) fn render_thumbnail(
    install: &Install,
    target: &mut Offscreen,
    renderer: &Renderer,
    cfg: &str,
    look: &ModelLook,
) -> Result<Option<(image::RgbaImage, [f32; 3])>> {
    // A fresh texture cache per model keeps the worker's memory flat over
    // thousands of models; repeats within one model still share.
    let pieces = models::load(install, &Cache::new(look), cfg, 1.)?;
    if pieces.is_empty() {
        return Ok(None);
    }
    let [lo, hi] = math::bounds(
        pieces
            .iter()
            .flat_map(|p| math::corners(p.mesh.bounds()).map(|c| math::transform(&p.local, c))),
    );
    let center: [f32; 3] = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
    renderer.set_scene(&Scene {
        batches: pieces
            .iter()
            .map(|p| {
                Batch::new(
                    p.mesh.clone(),
                    Arc::new(Instances {
                        matrices: vec![p.local],
                        ids: vec![None],
                    }),
                )
            })
            .collect(),
        // Brighter than the default scene light: thin dark foliage has to
        // read at 96 px on a grey card. More fill from below and the sky
        // than the island's: marsh bushes and sparse grass still read as
        // specks at 0.95 / 0.6.
        light: gpui_editor_kit::gpu::Light {
            sun_color: [0.6; 3],
            sky: [1.2; 3],
            ground: [0.85; 3],
            ..Default::default()
        },
        glow: look.glow(&look.scene_glow()),
        ..Default::default()
    });
    let camera = Camera {
        // As the island views: models are left-handed game data too.
        mirror: true,
        yaw: 0.6,
        tilt: 0.95,
        ..Default::default()
    };
    // Fit the box as seen by this camera: its corners' widest offset from
    // the centre across or down the screen. The box is symmetric about the
    // centre, so the image is too.
    let (right, down) = (camera.right(), camera.down());
    let reach = math::corners([lo, hi])
        .iter()
        .map(|&c| {
            let d = math::sub(c, center);
            math::dot(d, right).abs().max(math::dot(d, down).abs())
        })
        .fold(0.01, f32::max);
    let mut extent = Extent {
        center,
        size: reach * 2.,
    };
    let mut pixels = target.render(renderer, camera, extent)?;
    // Bounds can also understate: a mesh whose stored bounds miss part of
    // it (a wall lamp's bracket) ran off the tile's top edge. Drawn pixels
    // on the border mean more lies outside; widen and render again.
    let side = THUMB * 2;
    for _ in 0..2 {
        match drawn_box(&pixels, side) {
            Some([x0, y0, x1, y1]) if x0 == 0 || y0 == 0 || x1 == side || y1 == side => {
                extent.size *= 1.5;
                pixels = target.render(renderer, camera, extent)?;
            }
            _ => break,
        }
    }
    // Bounds overstate sparse models (a marsh bush is a few stems in a wide
    // box) and they came out as a speck. Frame what was drawn instead and
    // render again; 0.92 is the share of the side `Camera::scale` fills.
    if let Some([x0, y0, x1, y1]) = drawn_box(&pixels, side) {
        let fit = side as f32 * 0.92;
        let fill = ((x1 - x0).max(y1 - y0) as f32 / fit).max(0.05);
        if fill < 0.85 {
            let per_px = extent.size / fit;
            let half = side as f32 / 2.;
            let (dx, dy) = (
                ((x0 + x1) as f32 / 2. - half) * per_px,
                ((y0 + y1) as f32 / 2. - half) * per_px,
            );
            extent = Extent {
                center: std::array::from_fn(|i| center[i] + right[i] * dx + down[i] * dy),
                size: extent.size * fill * 1.04,
            };
            pixels = target.render(renderer, camera, extent)?;
        }
    }
    Ok(Some((
        downsample(&pixels, THUMB * 2),
        std::array::from_fn(|i| hi[i] - lo[i]),
    )))
}

/// Pixel box `[x0, y0, x1, y1]` (end exclusive) of the drawn texels of a
/// square RGBA render, `None` when nothing was drawn.
fn drawn_box(pixels: &[u8], side: u32) -> Option<[u32; 4]> {
    let mut b = [u32::MAX, u32::MAX, 0, 0];
    for (i, px) in pixels.as_chunks::<4>().0.iter().enumerate() {
        if px[3] > 0 {
            let (x, y) = (i as u32 % side, i as u32 / side);
            b = [b[0].min(x), b[1].min(y), b[2].max(x + 1), b[3].max(y + 1)];
        }
    }
    (b[2] > 0).then_some(b)
}

/// A one-texel dark rim around what a render drew, so any model reads on
/// the one mid-grey tile: sparse pale foliage had faded into it, and
/// picking a dark or light tile per picture made a grid of tiles flip
/// between the two at random.
fn halo(rgba: &mut [u8], w: u32, h: u32) {
    let (w, h) = (w as usize, h as usize);
    let alpha: Vec<u8> = rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    for y in 0..h {
        for x in 0..w {
            if alpha[y * w + x] >= 64 {
                continue;
            }
            let (mut strongest, mut drawn) = (0u8, 0);
            for ny in y.saturating_sub(1)..(y + 2).min(h) {
                for nx in x.saturating_sub(1)..(x + 2).min(w) {
                    let a = alpha[ny * w + nx];
                    strongest = strongest.max(a);
                    drawn += usize::from(a >= 128);
                }
            }
            // A gap between leaves (drawn on most sides) stays open: filled,
            // sparse foliage read as a scribble. The rim follows how solid
            // its neighbour is, so soft edges stay soft.
            if drawn == 0 || drawn >= 5 {
                continue;
            }
            let rim = (u32::from(strongest) * 45 / 100) as u8;
            rgba[(y * w + x) * 4..][..4].copy_from_slice(&[14, 16, 20, rim]);
        }
    }
}

/// `img` shrunk by `k` per side for row-sized pictures: colour averaged over
/// a block's drawn texels, coverage doubled so a one-texel stem in a 3x3
/// block still shows instead of fading to the tile behind.
fn shrink_thin(img: &image::RgbaImage, k: u32, rim: bool) -> Option<Arc<RenderImage>> {
    let (w, h) = img.dimensions();
    let (sw, sh) = (w / k, h / k);
    let mut out = Vec::with_capacity((sw * sh * 4) as usize);
    for y in 0..sh {
        for x in 0..sw {
            let (mut rgb, mut alpha, mut weight) = ([0u32; 3], 0u32, 0u32);
            for dy in 0..k {
                for dx in 0..k {
                    let p = img.get_pixel(x * k + dx, y * k + dy).0;
                    let a = u32::from(p[3]);
                    alpha += a;
                    weight += a;
                    for c in 0..3 {
                        rgb[c] += u32::from(p[c]) * a;
                    }
                }
            }
            let n = weight.max(1);
            out.extend([
                (rgb[0] / n) as u8,
                (rgb[1] / n) as u8,
                (rgb[2] / n) as u8,
                (alpha * 2 / (k * k)).min(255) as u8,
            ]);
        }
    }
    if rim {
        halo(&mut out, sw, sh);
    }
    Some(Texture::from_rgba(sw, sh, out).ok()?.render_image())
}

/// Halves a square RGBA render. The renderer has no antialiasing, so the
/// thumbnail averages 2x2; a texel counts as covered when drawn at all,
/// keeping cutout leaves from darkening against the transparent clear.
fn downsample(pixels: &[u8], side: u32) -> image::RgbaImage {
    let half = side / 2;
    let side = side as usize;
    image::RgbaImage::from_fn(half, half, |x, y| {
        let (mut rgb, mut covered) = ([0u32; 3], 0u32);
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let i = ((y as usize * 2 + dy) * side + x as usize * 2 + dx) * 4;
            if pixels[i + 3] > 0 {
                covered += 1;
                for c in 0..3 {
                    rgb[c] += u32::from(pixels[i + c]);
                }
            }
        }
        let n = covered.max(1);
        image::Rgba([
            (rgb[0] / n) as u8,
            (rgb[1] / n) as u8,
            (rgb[2] / n) as u8,
            (covered * 255 / 4) as u8,
        ])
    })
}

/// Writes `bytes` beside `path` and renames them over it. Exit may end the
/// process while a thumbnail worker writes (the kit's exit hook), and a torn
/// file would stay in the cache: an empty one means "no thumbnail", a cut-off
/// PNG fails to load on every later open.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("writing");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `img` as a PNG at `path`, written as `write_atomic` does.
fn save_png(img: &image::RgbaImage, path: &std::path::Path) -> anyhow::Result<()> {
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)?;
    Ok(write_atomic(path, png.get_ref())?)
}

#[cfg(test)]
mod tests {
    use super::{Catalog, THUMB, Thumbs, cut, land_alpha, thumb_key};
    use anno_core::files::ModelLook;
    use anno_formats::vfs::Install;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn catalog_has_shipped_props_and_waterfalls() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let t = std::time::Instant::now();
        let c = Catalog::build(crate::games::rules(crate::games::of(&install)), &install).unwrap();
        eprintln!(
            "catalog: {} props, {} objects, {} decorations in {:?}",
            c.props.len(),
            c.objects.len(),
            c.decorations.len(),
            t.elapsed()
        );
        assert!(c.props.len() > 100);
        assert!(c.props.windows(2).all(|w| w[0].uses >= w[1].uses));
        assert!(c.props.iter().all(|p| p.path.ends_with(".cfg")));
        assert!(
            c.decorations.len() > 100,
            "{} decorations",
            c.decorations.len()
        );
        // The waterfall props of roman_island_medium_01.
        for guid in [51202, 51204] {
            let o = c
                .objects
                .iter()
                .find(|o| o.guid == guid)
                .expect("waterfall");
            assert!(o.island.ends_with(".a7m"));
        }
    }

    // Needs the game and a Vulkan device.
    #[test]
    fn thumbnails_render_once_then_come_from_the_cache() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Arc::new(Install::open(dir, families).unwrap());
        let cache = tempfile::tempdir().unwrap();
        let tree = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";
        let missing = "data/base/graphics/does/not/exist.cfg";
        let wait = |thumbs: &mut Thumbs| {
            let started = std::time::Instant::now();
            while thumbs.pending() {
                assert!(started.elapsed().as_secs() < 60, "thumbnails timed out");
                std::thread::sleep(std::time::Duration::from_millis(20));
                thumbs.receive();
            }
        };
        let mut thumbs = Thumbs::new(install.clone(), cache.path().to_owned(), ModelLook::PLAIN);
        thumbs.request([tree, missing]);
        wait(&mut thumbs);
        assert!(thumbs.get(tree).is_some());
        assert!(thumbs.get(missing).is_none());

        let file = cache.path().join(format!("{:016x}.png", thumb_key(tree)));
        let png = image::open(&file).unwrap().to_rgba8();
        assert_eq!(png.dimensions(), (THUMB, THUMB));
        let covered = png.pixels().filter(|p| p.0[3] > 0).count();
        let share = covered as f32 / (THUMB * THUMB) as f32;
        // A fitted tree fills a good part of the frame but not all of it.
        assert!((0.1..0.9).contains(&share), "covered {share}");
        assert_eq!(png.get_pixel(0, 0).0[3], 0, "transparent background");

        // A second session reads the PNG instead of rendering.
        let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
        let mut again = Thumbs::new(install, cache.path().to_owned(), ModelLook::PLAIN);
        again.request([tree]);
        wait(&mut again);
        assert!(again.get(tree).is_some());
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), stamp);
    }

    // Needs the game and a Vulkan device: dropping the thumbnails while
    // the worker renders hands back without waiting for the render. The
    // render is held on a gate, so the drop lands mid-render for certain,
    // and a drop that joined the worker could not return before it opens.
    // A wall-clock bound on the drop failed in loaded suite runs.
    #[test]
    fn dropping_thumbnails_mid_render_does_not_wait_for_it() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Arc::new(Install::open(dir, families).unwrap());
        let cache = tempfile::tempdir().unwrap();
        let tree = "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg";
        let (reached, rendering) = mpsc::channel();
        let (go, held) = mpsc::channel();
        let mut thumbs = Thumbs::spawn(
            install,
            cache.path().to_owned(),
            ModelLook::PLAIN,
            Some((reached, held)),
        );
        thumbs.request([tree]);
        rendering
            .recv_timeout(Duration::from_secs(120))
            .expect("the worker never started the render");
        let (dropped, returned) = mpsc::channel();
        std::thread::spawn(move || {
            drop(thumbs);
            let _ = dropped.send(());
        });
        let returned = returned.recv_timeout(Duration::from_secs(30));
        let _ = go.send(());
        assert!(returned.is_ok(), "drop waited for the render");
    }

    // Every shipped island's map thumbnail on contact sheets, to eyeball
    // the sea cut: per island its minimap picture made opaque, the same
    // with what the cut clears tinted magenta, and the thumbnail over a
    // list row's grey. `ANNO117_DIR=… ANNO1800_DIR=… ISLAND_THUMBS=<dir>
    // cargo test island_thumbnail_sheets -- --ignored --nocapture`; each
    // sheet-NN.png has a sheet-NN.txt naming its tiles in reading order.
    #[test]
    #[ignore]
    fn island_thumbnail_sheets() {
        const S: u32 = 128;
        const COLS: u32 = 4;
        const ROWS: u32 = 8;
        let Some(out) = std::env::var_os("ISLAND_THUMBS").map(std::path::PathBuf::from) else {
            eprintln!("skip: ISLAND_THUMBS unset");
            return;
        };
        std::fs::create_dir_all(&out).unwrap();
        let mut islands = Vec::new();
        for game in [
            anno_core::game::Game::Anno117,
            anno_core::game::Game::Anno1800,
        ] {
            let profile = crate::games::profile(game);
            let Some(dir) = anno_formats::vfs::install_dir(profile.env_var) else {
                continue;
            };
            let install = Arc::new(Install::open(dir, profile.archive_families).unwrap());
            let mut paths: Vec<String> = install
                .search(".a7m", 100_000)
                .into_iter()
                .map(|id| install.file_path(id).to_owned())
                .filter(|p| p.ends_with(".a7m") && p.contains("/islands/"))
                .collect();
            paths.sort();
            islands.extend(paths.into_iter().map(|p| (install.clone(), p)));
        }
        let (tile_w, tile_h) = (3 * S + 12, S + 6);
        let mut spent = std::time::Duration::ZERO;
        for (page, chunk) in islands.chunks((COLS * ROWS) as usize).enumerate() {
            let mut sheet = image::RgbaImage::from_pixel(
                COLS * tile_w,
                ROWS * tile_h,
                [16, 16, 20, 255].into(),
            );
            let mut names = String::new();
            for (k, (install, path)) in chunk.iter().enumerate() {
                names.push_str(&format!("{k:2} {path}\n"));
                let (tx, ty) = (
                    (k as u32 % COLS) * tile_w + 3,
                    (k as u32 / COLS) * tile_h + 3,
                );
                let started = std::time::Instant::now();
                let land = super::land_image(install, path, S);
                let thumb = super::map_image(install, path, S);
                spent += started.elapsed();
                let Some(land) = land else {
                    continue;
                };
                let mut orig = land.clone();
                orig.pixels_mut().for_each(|p| p.0[3] = 255);
                let mut cut = orig.clone();
                for (p, l) in cut.pixels_mut().zip(land.pixels()) {
                    // Magenta over what the cut makes transparent.
                    let t = 255 - u32::from(l.0[3]);
                    let m = [255, 0, 255];
                    (0..3).for_each(|c| {
                        p.0[c] = ((u32::from(p.0[c]) * (510 - t) + m[c] * t) / 510) as u8
                    });
                }
                let fit = |i: &image::RgbaImage| {
                    image::imageops::resize(i, S, S, image::imageops::FilterType::Triangle)
                };
                image::imageops::overlay(&mut sheet, &fit(&orig), i64::from(tx), i64::from(ty));
                image::imageops::overlay(
                    &mut sheet,
                    &fit(&cut),
                    i64::from(tx + S + 3),
                    i64::from(ty),
                );
                let mut row = image::RgbaImage::from_pixel(S, S, [44, 46, 52, 255].into());
                if let Some(t) = thumb {
                    image::imageops::overlay(&mut row, &t, 0, 0);
                }
                image::imageops::overlay(
                    &mut sheet,
                    &row,
                    i64::from(tx + 2 * (S + 3)),
                    i64::from(ty),
                );
                stamp(&mut sheet, tx + 2, ty + 2, k as u32);
            }
            sheet
                .save(out.join(format!("sheet-{page:02}.png")))
                .unwrap();
            std::fs::write(out.join(format!("sheet-{page:02}.txt")), names).unwrap();
        }
        eprintln!("{} islands, cut + thumbnail {spent:?}", islands.len());
    }

    /// Writes `n` in a 3x5 pixel font, doubled, white on black.
    fn stamp(img: &mut image::RgbaImage, x: u32, y: u32, n: u32) {
        const DIGITS: [u16; 10] = [
            0o75557, 0o22222, 0o71747, 0o71717, 0o55711, 0o74717, 0o74757, 0o71111, 0o75757,
            0o75717,
        ];
        let text = n.to_string();
        let w = text.len() as u32 * 8 + 2;
        for dy in 0..12 {
            for dx in 0..w {
                img.put_pixel(x + dx, y + dy, [0, 0, 0, 255].into());
            }
        }
        for (d, ch) in text.bytes().enumerate() {
            let bits = DIGITS[usize::from(ch - b'0')];
            for row in 0..5 {
                for col in 0..3 {
                    if bits >> ((4 - row) * 3 + (2 - col)) & 1 == 1 {
                        for (sx, sy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            let px = x + 2 + d as u32 * 8 + col * 2 + sx;
                            img.put_pixel(px, y + 1 + row * 2 + sy, [255, 255, 255, 255].into());
                        }
                    }
                }
            }
        }
    }

    /// A `w` x `h` minimap picture of clear sea, `paint` filling texels.
    fn picture(w: u32, h: u32, paint: impl Fn(u32, u32) -> Option<[u8; 4]>) -> image::RgbaImage {
        image::RgbaImage::from_fn(w, h, |x, y| paint(x, y).unwrap_or([0, 0, 0, 0]).into())
    }

    #[test]
    fn minimap_cut_keeps_every_opaque_texel_on_a_clear_sea() {
        // The editor's own picture (new::map_images): dark green land, the
        // sea fully clear but still coloured.
        let mut img = picture(20, 20, |x, y| {
            let land = (5..15).contains(&x) && (5..15).contains(&y);
            Some(if land {
                [50, 60, 30, 255]
            } else {
                [58, 66, 34, 0]
            })
        });
        land_alpha(&mut img);
        assert!(
            [(5, 5), (10, 10), (14, 14)]
                .iter()
                .all(|&(x, y)| img.get_pixel(x, y).0[3] == 255)
        );
        assert_eq!(img.get_pixel(0, 0).0[3], 0, "sea");
        // Cut to the land: the 10 texel square fills all but the margin.
        let thumb = cut(&img, 12).unwrap();
        assert_eq!(thumb.get_pixel(6, 6).0, [50, 60, 30, 255]);
    }

    #[test]
    fn minimap_cut_keeps_lakes_and_dark_forest_and_drops_the_shadow() {
        // Anno 1800's picture: a 20x20 sand island holding a lake and a
        // near-black forest, ringed by its drop shadow, pure black with
        // alpha fading over 8 texels from opaque at the coast.
        let lake = [30, 60, 80, 255];
        let forest = [2, 5, 1, 255];
        let mut img = picture(40, 40, |x, y| {
            let off = |v: u32| 10u32.saturating_sub(v).max(v.saturating_sub(29));
            let out = off(x).max(off(y));
            match out {
                0 if (14..20).contains(&x) && (14..20).contains(&y) => Some(lake),
                0 if (22..26).contains(&x) && (22..26).contains(&y) => Some(forest),
                0 => Some([200, 170, 110, 255]),
                1..=8 => Some([0, 0, 0, (255 - (out - 1) * 30) as u8]),
                _ => None,
            }
        });
        land_alpha(&mut img);
        let alpha = |x: u32, y: u32| img.get_pixel(x, y).0[3];
        assert_eq!(alpha(16, 16), 255, "lake");
        assert_eq!(alpha(23, 23), 255, "dark forest");
        assert_eq!(alpha(10, 20), 255, "coast");
        assert_eq!(alpha(9, 20), 0, "shadow at the coast");
        assert_eq!(alpha(5, 20), 0, "shadow further out");
        assert_eq!(alpha(2, 20), 0, "shadow's faint rim");
        let thumb = cut(&img, 40).unwrap();
        let (w, _) = thumb.dimensions();
        // The cut follows the land, not the shadow: the island spans the
        // middle 1/1.12 of the picture.
        assert_eq!(thumb.get_pixel(0, w / 2).0[3], 0);
        assert_eq!(thumb.get_pixel(w / 2, w / 2).0[3], 255);
        // Coast texels keep the sand's colour: the clear black sea is not
        // mixed into them.
        for p in thumb.pixels().filter(|p| (1..255).contains(&p.0[3])) {
            let rgb = p.0;
            assert!(rgb[0] > 150 && rgb[1] > 120, "darkened coast {rgb:?}");
        }
    }

    #[test]
    fn minimap_cut_keeps_the_shape_of_wide_and_tiny_land() {
        // A 90x30 picture, all land: the game cuts its pictures to the
        // island, so they are rarely square.
        let wide = picture(90, 30, |_, _| Some([200, 170, 110, 255]));
        let thumb = cut(&wide, 60).unwrap();
        let rows = (0..60)
            .filter(|&y| thumb.get_pixel(30, y).0[3] >= 128)
            .count();
        // 30 of 90 texels tall, less the margin: a third, not stretched.
        assert!((16..22).contains(&rows), "{rows} opaque rows");
        // A 4x4 rock in a 200 px picture is magnified 2x at most: 8x8 of
        // the 60 px tile, not all of it.
        let rock = picture(200, 200, |x, y| {
            ((98..102).contains(&x) && (98..102).contains(&y)).then_some([120, 110, 90, 255])
        });
        let thumb = cut(&rock, 60).unwrap();
        let covered = thumb.pixels().filter(|p| p.0[3] >= 128).count();
        assert!((16..=100).contains(&covered), "{covered} texels");
    }
}
