// The placeable library: every prop model and every object the shipped
// islands use, with a shipped island to copy each object's record from.
// Built once from the install off the UI thread (a few seconds) and cached
// in the config folder, as are the thumbnails rendered for it.
use super::models::{self, Cache};
use anno_formats::{adf::Adf, objects::Objects, rda::Archive, vfs::Install};
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    camera::{Camera, Extent},
    gpu::{Batch, Instances, Offscreen, Renderer, Scene},
    gpui::RenderImage,
    math,
    texture::Texture,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

/// Bumped when the cache's shape or contents change.
const VERSION: u32 = 3;

/// The Library pane's lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Props,
    Decorations,
    Objects,
    /// Models the project and its dependencies carry.
    Mod,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropEntry {
    /// `.cfg` path as the rd3d `PropGrid/FileNames` store it.
    pub path: String,
    /// Instances over all shipped islands.
    pub uses: u32,
    /// Median vertical scale of those instances: 117 places some tree
    /// models at three times their size, so a model's height alone says
    /// little about how tall it stands.
    pub scale: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObjectEntry {
    pub guid: u32,
    /// A shipped island with one, to copy its record from.
    pub island: String,
    pub uses: u32,
}

/// A model placed through a generic object (`SimpleCfg` and the like) with
/// its own `DynamicVariation/ConfigFilename`: most shipped decoration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecorEntry {
    pub config: String,
    /// The object asset carrying it.
    pub guid: u32,
    pub island: String,
    pub uses: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    version: u32,
    /// Install file count when built; a patched install rebuilds it.
    stamp: usize,
    /// Most used first.
    pub props: Vec<PropEntry>,
    pub objects: Vec<ObjectEntry>,
    pub decorations: Vec<DecorEntry>,
}

impl Catalog {
    /// The cached catalog when it matches the install, else a fresh scan,
    /// written back to `cache`.
    pub fn load_or_build(install: &Install, cache: &Path) -> Result<Catalog> {
        if let Ok(bytes) = std::fs::read(cache)
            && let Ok(c) = serde_json::from_slice::<Catalog>(&bytes)
            && c.version == VERSION
            && c.stamp == install.file_count()
        {
            return Ok(c);
        }
        let c = Catalog::build(install)?;
        if let Some(dir) = cache.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(cache, serde_json::to_vec(&c)?)?;
        Ok(c)
    }

    /// Scans every shipped island's prop grid and objects.
    pub fn build(install: &Install) -> Result<Catalog> {
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
        // Per model: instances and their vertical scales.
        let mut props: BTreeMap<String, (u32, Vec<f32>)> = BTreeMap::new();
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
            for (path, (n, scales)) in s.props {
                let e = props.entry(path).or_default();
                e.0 += n;
                e.1.extend(scales);
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
            .map(|(path, (uses, scales))| PropEntry {
                path,
                uses,
                scale: median(scales),
            })
            .collect();
        props.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.path.cmp(&b.path)));
        let mut objects: Vec<ObjectEntry> = objects.into_values().collect();
        objects.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.guid.cmp(&b.guid)));
        let mut decorations: Vec<DecorEntry> = decorations.into_values().collect();
        decorations.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.config.cmp(&b.config)));
        Ok(Catalog {
            version: VERSION,
            stamp: install.file_count(),
            props,
            objects,
            decorations,
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
    let archive = Archive::from_bytes(install.read(path)?, path)?;
    let rd = Adf::decode(&archive.read_path("rd3d.data")?)?;
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
    let gamedata = Adf::decode(&archive.read_path("gamedata.data")?)?;
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
/// Bumped when thumbnails should re-render (camera, lighting, size).
const THUMB_VERSION: u32 = 5;

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
    send: Option<mpsc::Sender<Vec<String>>>,
    done: mpsc::Receiver<(String, Option<image::RgbaImage>, Option<Look>)>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The PNG cache, keyed like the worker keys it.
    dir: PathBuf,
}

impl Drop for Thumbs {
    // Closing the queue ends the worker; wait so it frees its Vulkan device
    // before the process tears the driver down (at most one render).
    fn drop(&mut self) {
        self.send = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Thumbs {
    /// Starts the worker; it exits when this is dropped.
    pub fn start(install: Arc<Install>, dir: PathBuf) -> Thumbs {
        let (send, requests) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let cache = dir.clone();
        let worker = std::thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || thumbnail_worker(&install, &dir, requests, finished))
            .expect("spawn thumbnail thread");
        Thumbs {
            images: HashMap::new(),
            small: HashMap::new(),
            looks: HashMap::new(),
            requested: HashSet::new(),
            send: Some(send),
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
        if let (false, Some(send)) = (fresh.is_empty(), &self.send) {
            let _ = send.send(fresh);
        }
    }

    /// Takes finished thumbnails; true if there were any.
    pub fn receive(&mut self) -> bool {
        let mut any = false;
        while let Ok((cfg, img, look)) = self.done.try_recv() {
            // Models get the rim; island maps sit on their own sea blue.
            let model = look.is_some();
            if let Some(look) = look {
                self.looks.insert(cfg.clone(), look);
            }
            if let Some(img) = &img
                && let Some(small) = shrink_thin(img, 3, model)
            {
                self.small.insert(cfg.clone(), small);
            }
            let image = img.and_then(|img| {
                let (w, h) = img.dimensions();
                let mut raw = img.into_raw();
                if model {
                    halo(&mut raw, w, h);
                }
                Some(Texture::from_rgba(w, h, raw).ok()?.render_image())
            });
            self.images.insert(cfg, image);
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

fn thumbnail_worker(
    install: &Install,
    dir: &Path,
    requests: mpsc::Receiver<Vec<String>>,
    finished: mpsc::Sender<(String, Option<image::RgbaImage>, Option<Look>)>,
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
                        Some(img) => img.save(&file).map_err(anyhow::Error::from),
                        None => std::fs::write(&file, []).map_err(anyhow::Error::from),
                    };
                    made
                }
            };
            if finished.send((cfg, image, None)).is_err() {
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
                        Ok(target) => gpu = Some((target, Renderer::default())),
                        Err(e) => {
                            eprintln!("thumbnails: {e:#}");
                            broken = true;
                            continue;
                        }
                    }
                }
                let (target, renderer) = gpu.as_mut().expect("created above");
                let rendered = render_thumbnail(install, target, renderer, &cfg)
                    .map_err(|e| eprintln!("thumbnail {cfg}: {e:#}"))
                    .ok()
                    .flatten();
                let look = (rendered.as_ref()).and_then(|(img, size)| Look::measure(img, *size));
                let _ = std::fs::create_dir_all(dir);
                let saved = match (&rendered, look) {
                    (Some((img, _)), Some(look)) => img
                        .save(&file)
                        .and_then(|()| Ok(std::fs::write(&look_file, look.to_bytes())?))
                        .map_err(anyhow::Error::from),
                    _ if modded => Ok(()),
                    _ => std::fs::write(&file, []).map_err(anyhow::Error::from),
                };
                if let Err(e) = saved {
                    eprintln!("thumbnail cache {}: {e:#}", file.display());
                }
                (rendered.map(|r| r.0), look)
            }
        };
        if finished.send((cfg, image, look)).is_err() {
            break;
        }
    }
}

/// Bump when island map thumbnails change; model thumbnails keep theirs.
const MAP_VERSION: u32 = 4;

/// An island's map image (`_gamedata/<name>/mapimage_0.dds`) cut to the
/// land with a small margin, square, at twice the list's 30 px icon. Sea is
/// transparent so the row shows through; `None` without a map image.
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
    let bytes = install
        .read(&format!(
            "{}/mapimage_0.dds",
            super::doc::island_data_dir(path).ok()?
        ))
        .ok()?;
    let dds = gpui_editor_kit::texture::Dds::parse(&bytes).ok()?;
    let image = dds.decode(&bytes, 0).ok()?;
    let (w, h) = (image.width, image.height);
    let mask = land_mask(&image.rgba, w, h);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if mask.contains((y * w + x) as usize) {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
    }
    if x0 > x1 {
        return None;
    }
    // Square around the land's centre, 6 % margin.
    let side = ((x1 - x0).max(y1 - y0) as f32 * 1.12) as u32 + 1;
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    let mut rgba = image.rgba;
    for (i, texel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        texel[3] = if mask.contains(i) { 255 } else { 0 };
    }
    let full = image::RgbaImage::from_raw(w, h, rgba)?;
    let left = cx
        .saturating_sub(side / 2)
        .min(w.saturating_sub(side.min(w)));
    let top = cy
        .saturating_sub(side / 2)
        .min(h.saturating_sub(side.min(h)));
    let crop = image::imageops::crop_imm(&full, left, top, side.min(w), side.min(h)).to_image();
    Some(image::imageops::resize(
        &crop,
        edge,
        edge,
        image::imageops::FilterType::Triangle,
    ))
}

/// Which texels of a map image are land. The image covers the sea floor
/// too, opaque, and its colour varies by pack: dark blue-grey in the base
/// game, teal in DLC 3, with baked noise and streaks in some DLC 1 maps.
/// Land is lit, not blue, and unlike the sea at the image's border; of
/// that, only sizeable connected pieces count, so speckle and stray strips
/// in the sea drop out.
fn land_mask(rgba: &[u8], w: u32, h: u32) -> Mask {
    let n = (w * h) as usize;
    let at = |i: usize| -> [i32; 4] { std::array::from_fn(|c| i32::from(rgba[i * 4 + c])) };
    // The sea's colour: the per-channel median of the opaque border
    // texels. A median, since some maps have a black corner.
    let mut border: [Vec<i32>; 3] = Default::default();
    for y in 0..h {
        for x in 0..w {
            if x > 1 && y > 1 && x + 2 < w && y + 2 < h {
                continue;
            }
            let t = at((y * w + x) as usize);
            if t[3] >= 128 {
                (0..3).for_each(|c| border[c].push(t[c]));
            }
        }
    }
    let sea = (!border[0].is_empty()).then(|| {
        border.map(|mut v| {
            let mid = v.len() / 2;
            *v.select_nth_unstable(mid).1
        })
    });
    // Per texel: NOT_LAND, or land not yet given a piece, or its piece.
    const NOT_LAND: u32 = u32::MAX;
    const UNSEEN: u32 = u32::MAX - 1;
    let mut piece: Vec<u32> = (0..n)
        .map(|i| {
            let [r, g, b, a] = at(i);
            // With a transparent sea (the editor's own map images) alpha
            // marks the land; the brightness test, made for the game's
            // sandy maps, kept a dark green island's lit rim only.
            let lit = a >= 128 && (sea.is_none() || (r + g + b) > 190 && b <= r.max(g) + 12);
            let unlike_sea =
                sea.is_none_or(|s| (r - s[0]).abs() + (g - s[1]).abs() + (b - s[2]).abs() > 60);
            if lit && unlike_sea { UNSEEN } else { NOT_LAND }
        })
        .collect();
    // Connected pieces (4-neighbour), kept when at least a tenth of the
    // largest: an island's islets stay, the sea's specks go.
    let mut sizes: Vec<usize> = Vec::new();
    let mut stack = Vec::new();
    for seed in 0..n {
        if piece[seed] != UNSEEN {
            continue;
        }
        let id = sizes.len() as u32;
        let mut size = 0;
        piece[seed] = id;
        stack.push(seed);
        while let Some(i) = stack.pop() {
            size += 1;
            let (x, y) = (i as u32 % w, i as u32 / w);
            let around = [
                (x > 0).then(|| i - 1),
                (x + 1 < w).then(|| i + 1),
                (y > 0).then(|| i - w as usize),
                (y + 1 < h).then(|| i + w as usize),
            ];
            for j in around.into_iter().flatten() {
                if piece[j] == UNSEEN {
                    piece[j] = id;
                    stack.push(j);
                }
            }
        }
        sizes.push(size);
    }
    let keep = sizes.iter().max().map_or(0, |m| m / 10).max(1);
    let mut out = Mask(vec![0; n.div_ceil(64)]);
    for (i, &p) in piece.iter().enumerate() {
        if p != NOT_LAND && sizes[p as usize] >= keep {
            out.0[i / 64] |= 1 << (i % 64);
        }
    }
    out
}

/// One bit per texel.
struct Mask(Vec<u64>);

impl Mask {
    fn contains(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
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

/// `cfg` from a three-quarter view, fitted to its bounds; `None` when it
/// has no drawable piece.
fn render_thumbnail(
    install: &Install,
    target: &mut Offscreen,
    renderer: &Renderer,
    cfg: &str,
) -> Result<Option<(image::RgbaImage, [f32; 3])>> {
    // A fresh texture cache per model keeps the worker's memory flat over
    // thousands of models; repeats within one model still share.
    let pieces = models::load(install, &Cache::default(), cfg, 1.)?;
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

#[cfg(test)]
mod tests {
    use super::{Catalog, THUMB, Thumbs, land_mask, thumb_key};
    use anno_formats::vfs::Install;
    use std::sync::Arc;

    #[test]
    fn catalog_has_shipped_props_and_waterfalls() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let t = std::time::Instant::now();
        let c = Catalog::build(&install).unwrap();
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
        // The cache round-trips.
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("catalog.json");
        std::fs::write(&cache, serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(Catalog::load_or_build(&install, &cache).unwrap(), c);
    }

    // Needs the game and a Vulkan device.
    #[test]
    fn thumbnails_render_once_then_come_from_the_cache() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Arc::new(Install::open(dir).unwrap());
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
        let mut thumbs = Thumbs::start(install.clone(), cache.path().to_owned());
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
        let mut again = Thumbs::start(install, cache.path().to_owned());
        again.request([tree]);
        wait(&mut again);
        assert!(again.get(tree).is_some());
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), stamp);
    }

    #[test]
    fn land_mask_takes_every_opaque_texel_on_a_transparent_sea() {
        // The editor's own map image: dark green land, sea fully clear.
        let (w, h) = (20u32, 20u32);
        let mut rgba = [58u8, 66, 34, 0].repeat((w * h) as usize);
        for y in 5..15 {
            for x in 5..15 {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[50, 60, 30, 255]);
            }
        }
        let mask = land_mask(&rgba, w, h);
        let at = |x: u32, y: u32| mask.contains((y * w + x) as usize);
        assert!(at(5, 5) && at(10, 10) && at(14, 14), "dark land");
        assert!(!at(0, 0), "sea");
    }

    #[test]
    fn land_mask_drops_the_sea_and_its_specks() {
        // 40x40 of opaque teal sea (DLC 3's), a 20x20 sand island, and a
        // 2x2 sand speck in a corner.
        let (w, h) = (40u32, 40u32);
        let mut rgba = [20u8, 90, 80, 255].repeat((w * h) as usize);
        let sand = |rgba: &mut Vec<u8>, x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&[200, 170, 110, 255]);
        };
        for y in 10..30 {
            for x in 10..30 {
                sand(&mut rgba, x, y);
            }
        }
        for (x, y) in [(3, 3), (4, 3), (3, 4), (4, 4)] {
            sand(&mut rgba, x, y);
        }
        let mask = land_mask(&rgba, w, h);
        let at = |x: u32, y: u32| mask.contains((y * w + x) as usize);
        assert!(at(15, 15) && at(29, 29));
        assert!(!at(0, 0), "sea");
        assert!(!at(3, 3), "speck");
    }
}
