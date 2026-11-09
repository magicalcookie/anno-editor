// World Machine link: reads a WM export folder (16-bit heightfield, RGBA
// splatmaps, colormap) into an island, and re-reads it when WM writes again.
// The work runs off the UI thread and lands as one undo step.
pub mod template;

use anno_formats::island::{self, Grid};
use anno_formats::tint::TintMap;
use anyhow::{Context as _, Result, ensure};
use gpui_editor_kit::heightfield::Grid as FieldGrid;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How an island takes a World Machine export. Saved per island with the
/// project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub folder: PathBuf,
    /// Metres between heightfield 0 and 1.
    pub range: f32,
    /// Heightfield value of sea level; WM's default water level is 0.1.
    pub sea: f32,
    /// Material id per channel (R, G, B, A) of each splatmap, in the order
    /// [`scan`] finds them; `None` leaves a channel out.
    pub channels: Vec<[Option<u8>; 4]>,
    /// How much of the colormap's variation becomes ground tint, 0..1.
    pub tint: f32,
    /// Re-import when the export files change.
    pub watch: bool,
}

impl Link {
    pub fn new(folder: PathBuf) -> Self {
        Link {
            folder,
            range: 100.,
            sea: 0.1,
            channels: Vec::new(),
            tint: 0.5,
            watch: true,
        }
    }
}

/// The files of a WM export, found by name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exports {
    pub height: Option<PathBuf>,
    pub splats: Vec<PathBuf>,
    pub color: Option<PathBuf>,
}

impl Exports {
    /// Latest write time of the files, to notice a new WM build.
    pub fn stamp(&self) -> Option<SystemTime> {
        self.height
            .iter()
            .chain(&self.splats)
            .chain(&self.color)
            .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .max()
    }

    pub fn is_empty(&self) -> bool {
        self.height.is_none() && self.splats.is_empty() && self.color.is_none()
    }
}

/// PNG exports in `folder` by WM's default output names: "Height",
/// "Splatmap", "Colormap". The newest file of each kind wins.
pub fn scan(folder: &Path) -> Exports {
    let mut files: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
        .map(|p| {
            let t = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (p, t)
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let named = |word: &str| -> Vec<PathBuf> {
        files
            .iter()
            .filter(|(p, _)| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.to_ascii_lowercase().contains(word))
            })
            .map(|(p, _)| p.clone())
            .collect()
    };
    // Several worlds may export into one folder; the splatmaps of one
    // build are the ones named like the newest ("<project> Splatmap…").
    let prefix = |p: &PathBuf| {
        let n = p
            .file_name()
            .map_or(String::new(), |n| n.to_string_lossy().to_lowercase());
        n.find("splat").map_or(n.clone(), |i| n[..i].to_owned())
    };
    let mut splats = named("splat");
    if let Some(newest) = splats.first().map(prefix) {
        splats.retain(|p| prefix(p) == newest);
    }
    splats.sort();
    Exports {
        height: named("height").into_iter().next(),
        splats,
        color: named("colormap").into_iter().next(),
    }
}

fn material_name(file: &str) -> &str {
    let file = file.rsplit('/').next().unwrap_or(file);
    file.split_once('_')
        .filter(|(prefix, _)| prefix.len() == 1 || matches!(*prefix, "tropical" | "colony02"))
        .map_or(file, |(_, name)| name)
}

/// Default channel materials for a WM default splatmap (R eroded ridges, G
/// flat rock, B scree, A the base cover), picked from the island's palette
/// by file name.
pub fn default_channels(materials: &[(u8, String)]) -> [Option<u8>; 4] {
    // Names must start with the word: "grass_01" must not pick
    // "underwatergrass_01".
    let find = |words: &[&str]| {
        words.iter().find_map(|w| {
            materials
                .iter()
                .find(|(_, file)| material_name(file).starts_with(w))
                .map(|(id, _)| *id)
        })
    };
    [
        find(&[
            "cliff_limestone",
            "cliff_blocky",
            "cliff_sharp",
            "cliff",
            "mountain",
        ]),
        find(&["layeredstone", "granite", "rocks_sharp", "rock", "stone"]),
        find(&["gravel", "gravle", "beach"]),
        find(&[
            "grass_01_dummy",
            "grass_01.",
            "grass_01",
            "grass",
            "gras",
            "snow",
        ]),
    ]
}

/// The Anno template an export came from, by its file name (templates are
/// written as "<island> <template>.tmd", and World Machine names exports
/// after the project). Their heights expect `template::SEA` and
/// `template::RANGE`, and their splatmap channels are R cliff, G rock,
/// B beach band, A gentle ground.
pub fn template_of(exports: &Exports) -> Option<template::Template> {
    let name = exports
        .height
        .as_ref()?
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    template::Template::ALL.into_iter().find(|t| {
        let label = t.label().replace(" & ", " and ").to_lowercase();
        name.contains(&format!("{label} height"))
    })
}

/// Everything an import changes, computed off the UI thread.
pub struct Imported {
    pub heights: Option<Vec<f32>>,
    pub materials: Option<(Grid<u32>, Grid<u32>)>,
    pub tint: Option<TintMap>,
    pub stamp: Option<SystemTime>,
}

/// What the island looks like to an import.
#[derive(Clone, Copy)]
pub struct Target {
    pub field: FieldGrid,
    /// Level-0 alpha map size; id blocks are half each way.
    pub alphas: [usize; 2],
    pub world: [u32; 2],
}

/// Reads the export and converts it for `target`.
pub fn load(link: &Link, exports: &Exports, target: Target) -> Result<Imported> {
    let heights = exports
        .height
        .as_deref()
        .map(|p| heights(p, link, target.field))
        .transpose()?;
    let splats: Vec<(image::RgbaImage, [Option<u8>; 4])> = exports
        .splats
        .iter()
        .zip(&link.channels)
        .filter(|(_, ch)| ch.iter().any(Option::is_some))
        .map(|(p, ch)| {
            let img = image::open(p)
                .with_context(|| format!("reading {}", p.display()))?
                .to_rgba8();
            Ok((img, *ch))
        })
        .collect::<Result<_>>()?;
    let materials = (!splats.is_empty()).then(|| materials(&splats, target.alphas));
    let tint = match (&exports.color, link.tint > 0.) {
        (Some(p), true) => Some(tint(p, link.tint, target.world)?),
        _ => None,
    };
    Ok(Imported {
        heights,
        materials,
        tint,
        stamp: exports.stamp(),
    })
}

/// Heights in metres on the island's sample grid: the image spans the
/// island, value `sea` lands on 0 m, and 1 is `range` metres above 0.
fn heights(path: &Path, link: &Link, grid: FieldGrid) -> Result<Vec<f32>> {
    let img = image::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .to_luma16();
    let (iw, ih) = img.dimensions();
    ensure!(iw > 1 && ih > 1, "{} is too small", path.display());
    let value = |x: u32, y: u32| f32::from(img.get_pixel(x, y).0[0]) / 65535.;
    let sample = |u: f32, v: f32| {
        let (fx, fy) = (u * (iw - 1) as f32, v * (ih - 1) as f32);
        let (x0, y0) = (fx.floor() as u32, fy.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(iw - 1), (y0 + 1).min(ih - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let top = value(x0, y0) * (1. - tx) + value(x1, y0) * tx;
        let bottom = value(x0, y1) * (1. - tx) + value(x1, y1) * tx;
        top * (1. - ty) + bottom * ty
    };
    Ok((0..grid.depth)
        .flat_map(|z| (0..grid.width).map(move |x| (x, z)))
        .map(|(x, z)| {
            // Image row 0 is north, the island's far z edge.
            let (u, v) = (
                x as f32 / (grid.width - 1) as f32,
                1. - z as f32 / (grid.depth - 1) as f32,
            );
            (sample(u, v) - link.sea) * link.range
        })
        .collect())
}

/// Level-0 material maps from splat weights: each 2x2 id block takes the
/// six materials weighing most over its texels, and each texel scales its
/// weights so the strongest is 31, as on shipped islands (the shader
/// renormalizes).
fn materials(
    splats: &[(image::RgbaImage, [Option<u8>; 4])],
    alphas: [usize; 2],
) -> (Grid<u32>, Grid<u32>) {
    let [aw, ah] = alphas;
    let (bw, bh) = (aw.div_ceil(2), ah.div_ceil(2));
    // Material weights at an alpha texel, summed over channels sharing one.
    let weights = |x: usize, y: usize| -> Vec<(u8, f32)> {
        // Image row 0 is north: the last alpha row.
        let (u, v) = (
            (x as f32 + 0.5) / aw as f32,
            1. - (y as f32 + 0.5) / ah as f32,
        );
        let mut out: Vec<(u8, f32)> = Vec::with_capacity(8);
        for (img, channels) in splats {
            let px = bilinear_rgba(img, u, v);
            for (c, id) in channels.iter().enumerate() {
                let Some(id) = *id else { continue };
                match out.iter_mut().find(|(m, _)| *m == id) {
                    Some((_, w)) => *w += px[c],
                    None => out.push((id, px[c])),
                }
            }
        }
        out
    };
    let mut ids = vec![0u32; bw * bh];
    let mut alpha = vec![0u32; aw * ah];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let rows = bh.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for ((block_rows, id_chunk), alpha_chunk) in (0..bh)
            .step_by(rows)
            .zip(ids.chunks_mut(rows * bw))
            .zip(alpha.chunks_mut(rows * 2 * aw))
        {
            let weights = &weights;
            scope.spawn(move || {
                for (r, id_row) in id_chunk.chunks_mut(bw).enumerate() {
                    let by = block_rows + r;
                    for (bx, word) in id_row.iter_mut().enumerate() {
                        let texels: Vec<(usize, usize)> = [(0, 0), (1, 0), (0, 1), (1, 1)]
                            .iter()
                            .map(|(dx, dy)| (bx * 2 + dx, by * 2 + dy))
                            .filter(|&(x, y)| x < aw && y < ah)
                            .collect();
                        let per_texel: Vec<Vec<(u8, f32)>> =
                            texels.iter().map(|&(x, y)| weights(x, y)).collect();
                        let mut totals: Vec<(u8, f32)> = Vec::new();
                        for (id, w) in per_texel.iter().flatten() {
                            match totals.iter_mut().find(|(m, _)| m == id) {
                                Some((_, t)) => *t += w,
                                None => totals.push((*id, *w)),
                            }
                        }
                        totals.sort_by(|a, b| b.1.total_cmp(&a.1));
                        totals.truncate(6);
                        // Unused slots get spare ids, never repeats: the
                        // game draws repeated ids as hard-edged patches.
                        let block = island::distinct_ids(std::array::from_fn(|k| {
                            totals
                                .get(k)
                                .map_or(totals.first().map_or(0, |t| t.0), |t| t.0)
                                & 31
                        }));
                        *word = island::pack_slots(block);
                        for (&(x, y), texel) in texels.iter().zip(&per_texel) {
                            let w: [f32; 6] = std::array::from_fn(|k| {
                                totals.get(k).map_or(0., |&(id, _)| {
                                    texel.iter().find(|(m, _)| *m == id).map_or(0., |t| t.1)
                                })
                            });
                            let max = w.iter().copied().fold(0., f32::max);
                            let slots: [u8; 6] = if max <= 0. {
                                [31, 0, 0, 0, 0, 0]
                            } else {
                                w.map(|v| (v / max * 31.).round() as u8)
                            };
                            let local = (y - block_rows * 2) * aw + x;
                            alpha_chunk[local] = island::pack_slots(slots);
                        }
                    }
                }
            });
        }
    });
    (
        Grid {
            width: bw,
            height: bh,
            data: ids,
        },
        Grid {
            width: aw,
            height: ah,
            data: alpha,
        },
    )
}

fn bilinear_rgba(img: &image::RgbaImage, u: f32, v: f32) -> [f32; 4] {
    let (iw, ih) = img.dimensions();
    let (fx, fy) = (u * iw as f32 - 0.5, v * ih as f32 - 0.5);
    let (x0, y0) = (fx.floor().max(0.) as u32, fy.floor().max(0.) as u32);
    let (x1, y1) = ((x0 + 1).min(iw - 1), (y0 + 1).min(ih - 1));
    let (x0, y0) = (x0.min(iw - 1), y0.min(ih - 1));
    let (tx, ty) = (
        (fx - x0 as f32).clamp(0., 1.),
        (fy - y0 as f32).clamp(0., 1.),
    );
    let p = |x, y| img.get_pixel(x, y).0.map(|c| f32::from(c) / 255.);
    let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
    std::array::from_fn(|i| {
        (a[i] * (1. - tx) + b[i] * tx) * (1. - ty) + (c[i] * (1. - tx) + d[i] * tx) * ty
    })
}

/// Ground tint from a colormap. WM colors are albedo, the game's tint
/// multiplies the textures, so the tint is the colormap relative to its
/// mean, mixed in by `strength`.
fn tint(path: &Path, strength: f32, world: [u32; 2]) -> Result<TintMap> {
    let img = image::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .to_rgb32f();
    let (iw, ih) = img.dimensions();
    let n = (iw * ih) as f32;
    let mean = img
        .pixels()
        .fold([0f32; 3], |m, p| std::array::from_fn(|i| m[i] + p.0[i] / n))
        .map(|m| m.max(1e-3));
    let size = [world[0] as f32, world[1] as f32];
    Ok(TintMap::from_fn(super::tint::WORK_LEVEL, world, |x, z| {
        let px = ((x / size[0] * iw as f32) as u32).min(iw - 1);
        // Image row 0 is north, at z = size.
        let py = (((1. - z / size[1]) * ih as f32) as u32).min(ih - 1);
        let c = img.get_pixel(px, py).0;
        std::array::from_fn(|i| (1. + strength * (c[i] / mean[i] - 1.)).clamp(0.5, 2.))
    }))
}

// --- island side -------------------------------------------------------------

use super::IslandEditor;
use crate::editor::Editor;
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    texture::Texture,
    theme,
    widgets::{Tooltip, button, checkbox, hint},
};
use std::sync::Arc;

impl IslandEditor {
    pub fn wm_link(&self) -> Option<&Link> {
        self.listing.world_machine.as_ref()
    }

    /// Changes the link as an unsaved edit; Save keeps it with the project.
    pub fn edit_wm_link(&mut self, f: impl FnOnce(&mut Link)) {
        let Some(mut link) = self.listing.world_machine.clone() else {
            return;
        };
        f(&mut link);
        self.set_listing(|l| l.world_machine = Some(link));
    }

    /// An import to run off the UI thread: always when `force`, else only
    /// when watching and the export changed since the last import.
    pub fn wm_job(
        &mut self,
        force: bool,
    ) -> Option<impl FnOnce() -> Result<Imported> + Send + 'static> {
        if self.wm_busy {
            return None;
        }
        let link = self.wm_link()?.clone();
        let exports = scan(&link.folder);
        if exports.is_empty() {
            if force {
                self.say(
                    format!("No World Machine export in {}", link.folder.display()),
                    true,
                );
            }
            return None;
        }
        // Watching starts after the first import, which the user asks for.
        if !force && (!link.watch || self.wm_stamp.is_none() || exports.stamp() <= self.wm_stamp) {
            return None;
        }
        let target = Target {
            field: self.doc.field.grid,
            alphas: self
                .doc
                .materials
                .as_ref()
                .map_or([0, 0], |m| m.alphas.size()),
            world: self.world(),
        };
        self.wm_busy = true;
        Some(move || load(&link, &exports, target))
    }

    /// Lands an import as one undo step: heights, material maps, tint; on
    /// islands made here also land, active area and buildable areas.
    pub fn apply_wm(&mut self, imported: Result<Imported>) {
        self.wm_busy = false;
        let imported = match imported {
            Ok(i) => i,
            Err(e) => return self.say(format!("World Machine import failed: {e:#}"), true),
        };
        self.wm_stamp = imported.stamp;
        let before = self.doc.clone();
        let mut done = Vec::new();
        if let Some(heights) = imported.heights {
            let [lo, hi] = self.doc.field.limits;
            let field = Arc::make_mut(&mut self.doc.field);
            for (to, from) in field.heights.iter_mut().zip(heights) {
                *to = from.clamp(lo, hi);
            }
            done.push("heights");
        }
        if let (Some((ids, alphas)), Some(m)) = (imported.materials, &mut self.doc.materials) {
            m.ids = super::doc::Layer::new(&ids);
            m.alphas = super::doc::Layer::new(&alphas);
            self.ground_stale = true;
            self.splat_stale = true;
            done.push("materials");
        }
        if let Some(tint) = imported.tint {
            self.doc.tint = Some(Arc::new(tint));
            self.layers.tint = true;
            done.push("tint");
        }
        if self.is_new && self.doc.grids.is_some() {
            let environment = self.land_environment();
            super::new::derive_land(&mut self.doc, environment);
            self.derive_areas_from_slope();
            done.push("land and buildable areas");
        }
        self.sync_land();
        self.checkpoint_owned(before);
        self.rebuild_tiles(None);
        self.say(
            format!("World Machine: imported {}", done.join(", ")),
            false,
        );
    }
}

/// Splat channel previews for the dialog, per splat file: four grayscale
/// thumbnails.
pub struct Previews {
    pub stamp: Option<SystemTime>,
    pub folder: PathBuf,
    pub exports: Exports,
    pub splats: Vec<[Arc<RenderImage>; 4]>,
    pub height: Option<Arc<RenderImage>>,
}

/// Thumbnail side in pixels.
const THUMB: u32 = 64;

fn previews(folder: &Path) -> Previews {
    let exports = scan(folder);
    let thumb = |path: &Path| -> Option<image::RgbaImage> {
        let img = image::open(path).ok()?;
        Some(image::imageops::resize(
            &img.to_rgba8(),
            THUMB,
            THUMB,
            image::imageops::FilterType::Triangle,
        ))
    };
    let gray = |img: &image::RgbaImage, c: usize| -> Arc<RenderImage> {
        let rgba = img
            .pixels()
            .flat_map(|p| [p.0[c], p.0[c], p.0[c], 255])
            .collect();
        Texture::from_rgba(THUMB, THUMB, rgba)
            .expect("thumbnail size")
            .render_image()
    };
    let splats = exports
        .splats
        .iter()
        .filter_map(|p| thumb(p))
        .map(|img| std::array::from_fn(|c| gray(&img, c)))
        .collect();
    let height = exports.height.as_deref().and_then(|p| {
        let img = image::open(p).ok()?.to_luma16();
        let small =
            image::imageops::resize(&img, THUMB, THUMB, image::imageops::FilterType::Triangle);
        let (lo, hi) = small
            .pixels()
            .fold((u16::MAX, 0), |(a, b), p| (a.min(p.0[0]), b.max(p.0[0])));
        let span = f32::from(hi.saturating_sub(lo).max(1));
        let rgba = small
            .pixels()
            .flat_map(|p| {
                let v = (f32::from(p.0[0] - lo) / span * 255.) as u8;
                [v, v, v, 255]
            })
            .collect();
        Some(Texture::from_rgba(THUMB, THUMB, rgba).ok()?.render_image())
    });
    Previews {
        stamp: exports.stamp(),
        folder: folder.to_owned(),
        exports,
        splats,
        height,
    }
}

/// Where World Machine saves by default.
fn default_folder() -> Option<PathBuf> {
    // `home_dir` also finds the profile folder on Windows, where HOME is unset.
    let dir = std::env::home_dir()?.join("Documents/WorldMachine");
    dir.is_dir().then_some(dir)
}

// --- dialog ------------------------------------------------------------------

impl Editor {
    /// Terrain → World Machine…: links the open island to an export folder,
    /// starting from World Machine's default folder.
    pub fn open_wm_dialog(&mut self) {
        let Some(island) = &mut self.island else {
            return self.message("Open an island first", true);
        };
        if island.wm_link().is_none() {
            let folder = default_folder().unwrap_or_default();
            let mut link = Link::new(folder.clone());
            let materials: Vec<(u8, String)> = island
                .palette
                .as_ref()
                .map(|p| {
                    p.materials
                        .iter()
                        .map(|(&id, m)| (id, m.file.clone()))
                        .collect()
                })
                .unwrap_or_default();
            link.channels = scan(&folder)
                .splats
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    if i == 0 {
                        default_channels(&materials)
                    } else {
                        [None; 4]
                    }
                })
                .collect();
            island.set_listing(|l| l.world_machine = Some(link));
        }
        self.wm_dialog = true;
        self.refresh_wm_previews();
    }

    fn refresh_wm_previews(&mut self) {
        let Some(link) = self.island.as_ref().and_then(|i| i.wm_link()) else {
            return;
        };
        let stale = self
            .wm_previews
            .as_ref()
            .is_none_or(|p| p.folder != link.folder || p.stamp != scan(&link.folder).stamp());
        if stale {
            let p = previews(&link.folder);
            // New splat files get empty channel rows.
            let count = p.exports.splats.len();
            if let Some(i) = &mut self.island {
                i.edit_wm_link(|l| l.channels.resize(count.max(l.channels.len()), [None; 4]));
            }
            self.wm_previews = Some(p);
        }
    }

    pub fn import_wm(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.island.as_mut().and_then(|i| i.wm_job(true)) else {
            return;
        };
        self.message("World Machine: importing…", false);
        self.spawn_wm(job, cx);
    }

    /// Re-imports a watched link whose export changed; called every second.
    pub fn poll_wm(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.island.as_mut().and_then(|i| i.wm_job(false)) else {
            return;
        };
        self.message("World Machine wrote new files: importing…", false);
        self.spawn_wm(job, cx);
    }

    fn spawn_wm(
        &mut self,
        job: impl FnOnce() -> Result<Imported> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let imported = cx.background_executor().spawn(async move { job() }).await;
            this.update(cx, |this, cx| {
                if let Some(i) = &mut this.island {
                    i.apply_wm(imported);
                }
                this.refresh_wm_previews();
                this.drain_messages();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Sets the link to what Anno templates are built for: sea level,
    /// range, and the 20 splatmap channels to palette materials by role
    /// (`template::CHANNELS`).
    fn apply_template_settings(&mut self) {
        let Some(island) = &mut self.island else {
            return;
        };
        let materials: Vec<(u8, String)> = island
            .palette
            .as_ref()
            .map(|p| {
                p.materials
                    .iter()
                    .map(|(&id, m)| (id, m.file.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let channels = template::channel_materials(self.game, &materials);
        let set = channels.iter().flatten().filter(|c| c.is_some()).count();
        island.edit_wm_link(|l| {
            l.sea = template::SEA;
            l.range = template::RANGE;
            l.channels = channels;
        });
        self.message(
            format!(
                "Template settings: sea 0.2, 100 m, {set} of 20 splat channels matched to palette materials. Import now applies them"
            ),
            false,
        );
    }

    /// Writes `template` as a World Machine world into the link's folder,
    /// named after the island, and sets sea level and range to the ones the
    /// template is built for.
    fn write_wm_template(&mut self, t: template::Template) {
        let Some(island) = &mut self.island else {
            return;
        };
        let Some(folder) = island.wm_link().map(|l| l.folder.clone()) else {
            return;
        };
        let stem = std::path::Path::new(&island.path)
            .file_stem()
            .map_or("island".into(), |s| s.to_string_lossy().into_owned());
        let file = folder.join(format!("{stem} {}.tmd", t.label().replace(" & ", " and ")));
        // A new seed each time: the same template, a different island.
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(1, |d| d.as_secs() as u32);
        let written = template::world(t, seed).and_then(|b| {
            std::fs::create_dir_all(&folder)
                .and(std::fs::write(&file, b))
                .map_err(Into::into)
        });
        match written {
            Ok(()) => {
                self.apply_template_settings();
                self.message(
                    format!(
                        "Wrote {}. Open it in World Machine and Build; Height and Splatmap land in this folder",
                        file.display()
                    ),
                    false,
                );
            }
            Err(e) => self.message(format!("Template not written: {e:#}"), true),
        }
    }

    fn pick_wm_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("World Machine export folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| {
                if let Some(i) = &mut this.island {
                    i.edit_wm_link(|l| l.folder = folder);
                }
                this.refresh_wm_previews();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn render_wm_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if !self.wm_dialog {
            return None;
        }
        let island = self.island.as_ref()?;
        let link = island.wm_link()?;
        let p = self.wm_previews.as_ref();
        let small = |label: String, on: bool| button(label, on).text_size(px(11.));
        let name = |path: &Option<PathBuf>| {
            path.as_ref()
                .and_then(|p| p.file_name())
                .map_or("missing".to_owned(), |n| n.to_string_lossy().into_owned())
        };
        let materials: Vec<(u8, String)> = island
            .palette
            .as_ref()
            .map(|p| {
                p.materials
                    .iter()
                    .map(|(&id, m)| {
                        let file = m.file.rsplit('/').next().unwrap_or(&m.file);
                        (
                            id,
                            file.trim_end_matches(".xml")
                                .trim_end_matches("_dummy")
                                .to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let thumb = |id: u8| {
            island
                .thumbs
                .iter()
                .find(|(t, _)| *t == id)
                .map(|(_, i)| i.clone())
        };

        // Folder.
        let folder = div()
            .id("wm-folder")
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(theme::get().border)
                    .rounded_sm()
                    .text_size(px(11.))
                    .child(link.folder.display().to_string()),
            )
            .child(
                small("Choose…".into(), false).on_click(cx.listener(|this, _, _, cx| {
                    this.pick_wm_folder(cx);
                    cx.notify();
                })),
            );

        // Templates: a ready-made WM world per island style.
        let mut templates = div()
            .id("wm-templates")
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(hint("New World Machine world:".to_owned()));
        for t in template::Template::ALL {
            templates = templates.child(
                small(t.label().into(), false)
                    .tooltip(Tooltip::text(
                        t.label(),
                        "",
                        "Writes a .tmd into the export folder: island shape, shelf, plateau, erosion, Height and Splatmap outputs wired for this importer. Open it in World Machine and Build.",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.write_wm_template(t);
                        cx.notify();
                    })),
            );
        }

        // Height.
        let step = |f: fn(&mut Link, f32), by: f32, label: &'static str| {
            small(label.into(), false).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(i) = &mut this.island {
                    i.edit_wm_link(|l| f(l, by));
                }
                cx.notify();
            }))
        };
        let height_row = div()
            .id("wm-height")
            .flex()
            .items_center()
            .gap_2()
            .children(
                p.and_then(|p| p.height.clone())
                    .map(|h| img(h).size(px(56.)).rounded_sm()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_size(px(12.)).child(format!(
                        "Height: {}",
                        name(&p.and_then(|p| p.exports.height.clone()))
                    )))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(hint("Range".to_owned()))
                            .child(step(
                                |l, d| l.range = (l.range + d).clamp(10., 600.),
                                -10.,
                                "−",
                            ))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .child(format!("{:.0} m", link.range)),
                            )
                            .child(step(
                                |l, d| l.range = (l.range + d).clamp(10., 600.),
                                10.,
                                "+",
                            ))
                            .child(hint("  Sea level".to_owned()))
                            .child(step(|l, d| l.sea = (l.sea + d).clamp(0., 1.), -0.01, "−"))
                            .child(div().text_size(px(11.)).child(format!("{:.2}", link.sea)))
                            .child(step(|l, d| l.sea = (l.sea + d).clamp(0., 1.), 0.01, "+")),
                    )
                    .child(hint(format!(
                        "0 → {:.0} m, 1 → {:.0} m, sea level at {:.2}",
                        -link.sea * link.range,
                        (1. - link.sea) * link.range,
                        link.sea
                    ))),
            );

        // An Anno template's export needs its own sea level, range and
        // channel meanings; say so, and set them in one click.
        let template = p.and_then(|p| template_of(&p.exports));
        let template_row =
            template.map(|t| {
                let matches = (link.sea - template::SEA).abs() < 1e-3
                    && (link.range - template::RANGE).abs() < 1e-3;
                let mut row = div()
                    .id("wm-template")
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .rounded_sm()
                    .bg(theme::get().input)
                    .child(div().flex_1().min_w_0().child(hint(format!(
                        "{} template: 20 splat channels in five images (cliffs, rock, rivers, coast, grass kinds){}",
                        t.label(),
                        if matches {
                            ""
                        } else {
                            ". Its heights expect sea 0.2 and 100 m"
                        }
                    ))));
                row = row.child(small("Use template settings".into(), !matches).flex_none().on_click(
                    cx.listener(|this, _, _, cx| {
                        this.apply_template_settings();
                        cx.notify();
                    }),
                ));
                row
            });

        // Splat channels.
        let mut splats = div().flex().flex_col().gap_1();
        let files = p.map_or(Vec::new(), |p| p.exports.splats.clone());
        if files.is_empty() {
            splats = splats.child(hint("No splatmap in the folder".to_owned()));
        }
        for (s, file) in files.iter().enumerate() {
            // Template exports have a fixed layout: name each image's theme.
            let theme = template
                .and_then(|_| template::IMAGES.get(s))
                .map_or(String::new(), |t| format!(" · {t}"));
            splats = splats.child(div().text_size(px(12.)).child(format!(
                "Splatmap: {}{theme}",
                file.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())
            )));
            let mut grid = div().flex().flex_wrap().gap_2();
            for c in 0..4 {
                let chosen = link.channels.get(s).and_then(|ch| ch[c]);
                let label = chosen
                    .and_then(|id| materials.iter().find(|(m, _)| *m == id))
                    .map_or("off".to_owned(), |(id, n)| format!("{id} {n}"));
                // Cycle through off and the palette.
                let cycle = |delta: i32| {
                    let ids: Vec<Option<u8>> = std::iter::once(None)
                        .chain(materials.iter().map(|(id, _)| Some(*id)))
                        .collect();
                    cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            if let Some(i) = &mut this.island {
                                i.edit_wm_link(|l| {
                                    if l.channels.len() <= s {
                                        l.channels.resize(s + 1, [None; 4]);
                                    }
                                    let at = ids
                                        .iter()
                                        .position(|v| *v == l.channels[s][c])
                                        .unwrap_or(0);
                                    let n = ids.len() as i32;
                                    l.channels[s][c] =
                                        ids[((at as i32 + delta).rem_euclid(n)) as usize];
                                });
                            }
                            cx.notify();
                        },
                    )
                };
                let channel = p.and_then(|p| p.splats.get(s)).map(|t| t[c].clone());
                grid = grid.child(
                    div()
                        .id(("wm-channel", s * 4 + c))
                        .w(px(410.))
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .w(px(if template.is_some() { 100. } else { 12. }))
                                .text_size(px(11.))
                                .child(
                                    match template.and_then(|_| template::CHANNELS.get(s * 4 + c)) {
                                        Some((role, _)) => {
                                            format!("{} {role}", ["R", "G", "B", "A"][c])
                                        }
                                        None => ["R", "G", "B", "A"][c].to_owned(),
                                    },
                                ),
                        )
                        .children(channel.map(|t| img(t).size(px(40.)).rounded_sm()))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::get().muted)
                                .child("→"),
                        )
                        .children(
                            chosen
                                .and_then(thumb)
                                .map(|t| img(t).size(px(40.)).rounded_sm()),
                        )
                        .child(small("◀".into(), false).on_click(cycle(-1)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(label),
                        )
                        .child(small("▶".into(), false).on_click(cycle(1))),
                );
            }
            splats = splats.child(grid);
        }

        // Tint and watching.
        let mut tint = div().id("wm-tint").flex().items_center().gap_1().child(
            div().text_size(px(12.)).child(format!(
                "Colormap: {}",
                name(&p.and_then(|p| p.exports.color.clone()))
            )),
        );
        for (v, label) in [(0., "Off"), (0.25, "25%"), (0.5, "50%"), (1., "100%")] {
            tint = tint.child(
                small(label.into(), (link.tint - v).abs() < 0.01)
                    .tooltip(Tooltip::text(
                        "Tint",
                        "",
                        "How much of the colormap’s variation tints the ground textures",
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(i) = &mut this.island {
                            i.edit_wm_link(|l| l.tint = v);
                        }
                        cx.notify();
                    })),
            );
        }
        let watch = checkbox(
            "Re-import when World Machine writes the files",
            link.watch,
            "checked every second",
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(i) = &mut this.island {
                i.edit_wm_link(|l| l.watch = !l.watch);
            }
            cx.notify();
        }));

        let scroll = self.scrolls.get("wm-dialog");
        let card = gpui_editor_kit::widgets::dialog("World Machine")
            .id("wm-dialog")
            .w(px(880.))
            .max_h(px(860.))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .gap_2()
            .child(hint(
                "Imports the export folder into this island: heights, the splatmap’s channels as ground materials, the colormap as tint."
                    .to_owned(),
            ))
            .child(folder)
            .child(templates)
            .children(template_row)
            .child(height_row)
            .child(splats)
            .child(tint)
            .child(watch)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(button("Close", false).on_click(cx.listener(|this, _, _, cx| {
                        this.wm_dialog = false;
                        cx.notify();
                    })))
                    .child(
                        button(if island.wm_busy { "Importing…" } else { "Import now" }, true)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.import_wm(cx);
                                cx.notify();
                            })),
                    ),
            );
        Some(gpui_editor_kit::widgets::modal(
            "world-machine",
            // The card sizes itself (up to its max height); the scrollbar
            // lies over its right edge.
            div()
                .relative()
                .child(card)
                .child(gpui_editor_kit::widgets::scrollbar("wm-dialog", &scroll)),
        ))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the GPUI prelude there has its own `test` macro.
    use super::{
        FieldGrid, Link, Target, default_channels, heights, island, load, materials, scan,
    };
    use std::path::PathBuf;

    fn splat(w: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> image::RgbaImage {
        image::RgbaImage::from_fn(w, w, |x, y| image::Rgba(f(x, y)))
    }

    #[test]
    fn splat_channels_become_six_slot_material_maps() {
        // Left half all R (material 4), right half 50/50 G and B (7, 9).
        let img = splat(64, |x, _| {
            if x < 32 {
                [255, 0, 0, 0]
            } else {
                [0, 128, 127, 0]
            }
        });
        let (ids, alphas) = materials(&[(img, [Some(4), Some(7), Some(9), None])], [128, 128]);
        // Six different ids in every block, as shipped maps have.
        for &w in &ids.data {
            let mut b = island::unpack_slots(w).to_vec();
            b.sort();
            b.dedup();
            assert_eq!(b.len(), 6, "block {:?}", island::unpack_slots(w));
        }
        assert_eq!((ids.width, ids.height, alphas.width), (64, 64, 128));
        let at = |x: usize, y: usize| {
            let block = island::unpack_slots(ids.get(x / 2, y / 2));
            let w = island::unpack_slots(alphas.get(x, y));
            let mut by_id: Vec<(u8, u8)> = (0..6)
                .filter(|&k| w[k] > 0)
                .map(|k| (block[k], w[k]))
                .collect();
            by_id.sort();
            by_id.dedup_by_key(|e| e.0);
            by_id
        };
        assert_eq!(at(10, 10), [(4, 31)]);
        let right = at(100, 10);
        assert_eq!(right.len(), 2, "{right:?}");
        assert!(right.iter().any(|&(id, w)| id == 7 && w == 31));
        assert!(right.iter().any(|&(id, w)| id == 9 && w >= 30));
    }

    #[test]
    fn more_than_six_materials_keep_the_strongest() {
        // Eight channels over two splats, each weighing more than the next.
        let a = splat(4, |_, _| [80, 60, 40, 30]);
        let b = splat(4, |_, _| [20, 10, 8, 2]);
        let (ids, _) = materials(
            &[
                (a, [Some(1), Some(2), Some(3), Some(4)]),
                (b, [Some(5), Some(6), Some(7), Some(8)]),
            ],
            [8, 8],
        );
        let mut block = island::unpack_slots(ids.get(1, 1));
        block.sort();
        assert_eq!(block, [1, 2, 3, 4, 5, 6]);
    }

    // An image's top rows are north. Anno's +z is north, so they become
    // the island's high-z edge, where the game (and the editor's north-up
    // views) show them; mapping row 0 to z = 0 mirrored islands in game.
    #[test]
    fn image_tops_land_on_the_north_edge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Height.png");
        // 16x16, bright in the top four rows.
        let img = image::ImageBuffer::<image::Luma<u16>, _>::from_fn(16, 16, |_, y| {
            image::Luma([if y < 4 { 60_000 } else { 0 }])
        });
        img.save(&path).unwrap();
        let grid = FieldGrid {
            width: 33,
            depth: 33,
            spacing: 1.,
            origin: [0.; 2],
        };
        let h = heights(&path, &Link::new(dir.path().to_owned()), grid).unwrap();
        let at = |z: usize| h[z * grid.width + 16];
        assert!(at(32) > 50., "north edge high: {}", at(32));
        assert!(at(0) < 0., "south edge sea: {}", at(0));
    }

    #[test]
    fn template_exports_are_recognised_by_name() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "Anno Plateau and cliffs Height-1025.png",
            "Anno Plateau and cliffs Splatmap 1025.png",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        assert_eq!(
            super::template_of(&scan(dir.path())),
            Some(super::template::Template::Plateau)
        );
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other.path().join("New Project Height Output-1025.png"),
            b"x",
        )
        .unwrap();
        assert_eq!(super::template_of(&scan(other.path())), None);
    }

    #[test]
    fn scans_the_world_machine_default_names() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "New Project Height Output-1025.png",
            "New Project Splatmap 1025.png",
            "New Project Colormap only 1025.png",
            "New Project Material Output 1025_diffuse.png",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let found = scan(dir.path());
        let name = |p: &Option<PathBuf>| p.as_ref().unwrap().file_name().unwrap().to_owned();
        assert_eq!(name(&found.height), "New Project Height Output-1025.png");
        assert_eq!(name(&found.color), "New Project Colormap only 1025.png");
        assert_eq!(found.splats.len(), 1);

        // A template's build lands next to the old export: only its own
        // splatmaps count, both of them.
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        for name in [
            "island Plateau Splatmap 769.png",
            "island Plateau Splatmap B 769.png",
        ] {
            let f = std::fs::File::create(dir.path().join(name)).unwrap();
            f.set_modified(t).unwrap();
        }
        let found = scan(dir.path());
        let names: Vec<_> = found
            .splats
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "island Plateau Splatmap 769.png",
                "island Plateau Splatmap B 769.png"
            ]
        );
    }

    #[test]
    fn default_channels_pick_cliff_rock_gravel_grass() {
        let set: Vec<(u8, String)> = [
            (14, "x/cliff_limestone_01.xml"),
            (10, "x/layeredstone_01_dummy.xml"),
            (8, "x/gravel_01_dummy.xml"),
            (20, "x/underwatergrass_01_dummy.xml"),
            (13, "x/grass_01_b_dummy.xml"),
            (12, "x/grass_01_dummy.xml"),
        ]
        .into_iter()
        .map(|(i, f)| (i, f.to_owned()))
        .collect();
        assert_eq!(
            default_channels(&set),
            [Some(14), Some(10), Some(8), Some(12)]
        );
    }

    #[test]
    fn default_channels_pick_1800_regional_materials() {
        for files in [
            ["m_mountain_01", "m_stone_03", "c_gravle_01", "m_gras_02"],
            [
                "tropical_mountain_02",
                "s_rock_01",
                "s_beach_06",
                "d_grass_07",
            ],
            ["p_mountain_01", "p_stone_01", "p_beach_01", "p_snow_01"],
            [
                "colony02_mountain_01",
                "colony02_rock_01",
                "colony02_beach_06",
                "colony02_grass_07",
            ],
        ] {
            let materials: Vec<_> = files
                .iter()
                .enumerate()
                .map(|(id, file)| (id as u8, format!("terrain/{file}.xml")))
                .collect();
            assert_eq!(
                default_channels(&materials),
                [Some(0), Some(1), Some(2), Some(3)],
                "{files:?}"
            );
        }
    }

    // Needs the World Machine default export in ~/Documents/WorldMachine.
    #[test]
    fn reads_the_world_machine_default_export() {
        let Some(home) = std::env::home_dir() else {
            return;
        };
        let folder = home.join("Documents/WorldMachine");
        let exports = scan(&folder);
        if exports.height.is_none() {
            eprintln!("skip: no World Machine export");
            return;
        }
        let mut link = Link::new(folder);
        link.channels = vec![[Some(14), Some(10), Some(8), Some(12)]];
        let grid = FieldGrid {
            width: 1025,
            depth: 1025,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let t = std::time::Instant::now();
        let out = load(
            &link,
            &exports,
            Target {
                field: grid,
                alphas: [4096, 4096],
                world: [512, 512],
            },
        )
        .unwrap();
        eprintln!("import {:?}", t.elapsed());
        let h = out.heights.unwrap();
        let (lo, hi) = h
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
        // Whatever the export holds, it lands inside the link's mapping
        // and is not flat.
        let (floor, top) = (-link.sea * link.range, (1. - link.sea) * link.range);
        assert!(
            lo >= floor - 0.01 && hi <= top + 0.01 && hi - lo > 1.,
            "{lo}..{hi}"
        );
        // Whatever the folder holds: splatmaps become material maps.
        assert_eq!(out.materials.is_some(), !exports.splats.is_empty());
        if let Some((ids, alphas)) = out.materials {
            assert_eq!((ids.width, alphas.width), (2048, 4096));
        }
        assert_eq!(out.tint.is_some(), exports.color.is_some());
    }
}
