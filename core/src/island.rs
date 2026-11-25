// Owns the island being edited: the document (heights, level-0 material
// maps in copy-on-write chunks, props, and the conversion to and from the
// game's island files), and around it the object store, harbour records and
// material set that tools and conversion read and write.
pub mod landscape;
pub mod objects;

use crate::game::Harbours;
use crate::harbour::{CONSTRUCTION, Library, Piece, Resource};
use anno_formats::a7m::{
    grids::{Grids, Layer as GridLayer},
    session::SessionSettings,
    tint::TintMap,
};
use anno_formats::isd::terrain as isd;
use anno_formats::{a7m::island, vfs::Install};
use anyhow::{Context as _, Result, ensure};
use gpui_editor_kit::{
    heightfield::{Brush, Grid, Heightfield, Rect},
    math::{self, Mat4},
};
use objects::{Object, Objects, PathPoint};
use std::sync::Arc;

/// A selectable thing on the island: an rd3d prop or a gamedata object, by
/// their stable ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Item {
    Prop(u64),
    Object(u64),
}

/// The sample rectangle (half-open) where two height arrays of `grid`
/// differ, `None` when they are equal.
pub fn changed_samples(grid: Grid, a: &[f32], b: &[f32]) -> Option<Rect> {
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

#[derive(Clone, Debug)]
pub struct Prop {
    pub id: u64,
    pub matrix: Mat4,
    /// The stored record: mesh file index, tint color and flags ride along.
    pub source: island::Prop,
    /// Placed by Auto cliffs, which replaces its own pieces on a rerun.
    /// The game file has no room for it; project islands keep the list in
    /// their listing (`Listing::auto_cliffs`).
    pub auto_cliff: bool,
}

/// The island's props, sorted by id, in copy-on-write runs. Every undo
/// step holds the list, and one placed prop copied the continental
/// island's whole 45k-prop list into the history (6.46 MB). A placement now
/// copies the last run and the run table (2 KB there), an edit or removal
/// the runs it reaches; steps that leave the props alone share all of it.
#[derive(Clone, Default)]
pub struct Props(Arc<Runs>);

#[derive(Clone, Default)]
struct Runs {
    runs: Vec<Arc<Vec<Prop>>>,
    /// Props before each run, then the total: one more entry than runs,
    /// or none while there are none.
    starts: Vec<u32>,
}

/// Props a run holds at most: 36 KB at 144 bytes a prop, 180 runs on the
/// continental island.
const RUN: usize = 256;

pub type PropIter<'a> = std::iter::FlatMap<
    std::slice::Iter<'a, Arc<Vec<Prop>>>,
    std::slice::Iter<'a, Prop>,
    fn(&'a Arc<Vec<Prop>>) -> std::slice::Iter<'a, Prop>,
>;

impl Props {
    pub fn len(&self) -> usize {
        self.0.starts.last().map_or(0, |&n| n as usize)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter<'a>(&'a self) -> PropIter<'a> {
        let run: fn(&'a Arc<Vec<Prop>>) -> std::slice::Iter<'a, Prop> = |r| r.iter();
        self.0.runs.iter().flat_map(run)
    }

    pub fn get(&self, i: usize) -> Option<&Prop> {
        let (run, at) = self.locate(i)?;
        Some(&self.0.runs[run][at])
    }

    /// Copies the run holding prop `i` unless this list alone holds it.
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Prop> {
        let (run, at) = self.locate(i)?;
        Some(&mut Arc::make_mut(&mut Arc::make_mut(&mut self.0).runs[run])[at])
    }

    pub fn first(&self) -> Option<&Prop> {
        self.0.runs.first()?.first()
    }

    pub fn last(&self) -> Option<&Prop> {
        self.0.runs.last()?.last()
    }

    pub fn push(&mut self, prop: Prop) {
        let t = Arc::make_mut(&mut self.0);
        match t.runs.last_mut() {
            Some(run) if run.len() < RUN => Arc::make_mut(run).push(prop),
            _ => {
                let mut run = Vec::with_capacity(RUN);
                run.push(prop);
                t.runs.push(Arc::new(run));
                let total = t.starts.last().copied().unwrap_or(0);
                if t.starts.is_empty() {
                    t.starts.push(0);
                }
                t.starts.push(total);
            }
        }
        *t.starts.last_mut().expect("a run") += 1;
    }

    pub fn clear(&mut self) {
        self.0 = Arc::default();
    }

    /// Keeps the props `keep` picks, asking once a prop. Runs that lose
    /// none stay shared with the undo history, and so does the table when
    /// none lose any.
    pub fn retain(&mut self, mut keep: impl FnMut(&Prop) -> bool) {
        // A bit per prop of each run, runs one after another.
        let mut kept = vec![0u64; self.0.runs.len() * (RUN / 64)];
        let mut cut = false;
        for (r, run) in self.0.runs.iter().enumerate() {
            for (i, p) in run.iter().enumerate() {
                if keep(p) {
                    kept[r * (RUN / 64) + i / 64] |= 1 << (i % 64);
                } else {
                    cut = true;
                }
            }
        }
        if !cut {
            return;
        }
        let t = Arc::make_mut(&mut self.0);
        for (r, run) in t.runs.iter_mut().enumerate() {
            let bits = &kept[r * (RUN / 64)..(r + 1) * (RUN / 64)];
            if (0..run.len()).all(|i| bits[i / 64] >> (i % 64) & 1 != 0) {
                continue;
            }
            let mut i = 0;
            Arc::make_mut(run).retain(|_| {
                i += 1;
                bits[(i - 1) / 64] >> ((i - 1) % 64) & 1 != 0
            });
        }
        t.runs.retain(|r| !r.is_empty());
        t.restart();
    }

    /// Calls `edit` on each prop `pick` picks, copying only the runs that
    /// hold one.
    pub fn edit_where(
        &mut self,
        mut pick: impl FnMut(&Prop) -> bool,
        mut edit: impl FnMut(&mut Prop),
    ) {
        for r in 0..self.0.runs.len() {
            let Some(first) = self.0.runs[r].iter().position(&mut pick) else {
                continue;
            };
            let run = Arc::make_mut(&mut Arc::make_mut(&mut self.0).runs[r]);
            edit(&mut run[first]);
            for p in &mut run[first + 1..] {
                if pick(p) {
                    edit(p);
                }
            }
        }
    }

    /// Every prop, mutably, copying every run still shared: for edits to
    /// the whole list. `get_mut` and `edit_where` copy only what they reach.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Prop> + '_ {
        let t = Arc::make_mut(&mut self.0);
        t.runs.iter_mut().flat_map(|r| Arc::make_mut(r).iter_mut())
    }

    /// The index of the prop with `id`. Props stay sorted by id (ids are
    /// handed out in increasing order and appended), so this is a binary
    /// search; a miss scans, in case some path ever breaks the order.
    pub fn position(&self, id: u64) -> Option<usize> {
        let t = &self.0;
        let run = (t.runs).partition_point(|r| r.last().is_some_and(|p| p.id < id));
        if let Some(r) = t.runs.get(run)
            && let Ok(at) = r.binary_search_by_key(&id, |p| p.id)
        {
            return Some(t.starts[run] as usize + at);
        }
        self.iter().position(|p| p.id == id)
    }

    /// Whether both hold the very same runs: true across undo steps that
    /// left the props alone.
    pub fn same(&self, other: &Props) -> bool {
        let (a, b) = (&self.0, &other.0);
        Arc::ptr_eq(a, b)
            || (a.runs.len() == b.runs.len()
                && (a.runs.iter().zip(&b.runs)).all(|(x, y)| Arc::ptr_eq(x, y)))
    }

    /// Each run as (allocation, bytes), then the run table: for memory
    /// diagnostics that count what undo steps share.
    pub fn allocations(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let t = &self.0;
        let table = t.runs.len() * size_of::<Arc<Vec<Prop>>>() + t.starts.len() * 4;
        (t.runs.iter())
            .map(|r| (Arc::as_ptr(r) as usize, r.len() * size_of::<Prop>()))
            .chain([(Arc::as_ptr(t) as usize, table)])
    }

    /// The run holding prop `i`, and its place there.
    fn locate(&self, i: usize) -> Option<(usize, usize)> {
        if i >= self.len() {
            return None;
        }
        let starts = &self.0.starts;
        let run = starts.partition_point(|&s| s as usize <= i) - 1;
        Some((run, i - starts[run] as usize))
    }
}

impl Runs {
    fn restart(&mut self) {
        self.starts.clear();
        if self.runs.is_empty() {
            return;
        }
        self.starts.push(0);
        let mut n = 0;
        for r in &self.runs {
            n += r.len() as u32;
            self.starts.push(n);
        }
    }
}

impl From<Vec<Prop>> for Props {
    fn from(props: Vec<Prop>) -> Self {
        let mut runs = Runs {
            runs: props.chunks(RUN).map(|c| Arc::new(c.to_vec())).collect(),
            starts: Vec::new(),
        };
        runs.restart();
        Props(Arc::new(runs))
    }
}

impl FromIterator<Prop> for Props {
    fn from_iter<I: IntoIterator<Item = Prop>>(iter: I) -> Self {
        let mut out = Props::default();
        out.extend(iter);
        out
    }
}

impl Extend<Prop> for Props {
    fn extend<I: IntoIterator<Item = Prop>>(&mut self, iter: I) {
        for p in iter {
            self.push(p);
        }
    }
}

impl std::ops::Index<usize> for Props {
    type Output = Prop;
    fn index(&self, i: usize) -> &Prop {
        let n = self.len();
        self.get(i).unwrap_or_else(|| panic!("prop {i} of {n}"))
    }
}

impl std::ops::IndexMut<usize> for Props {
    fn index_mut(&mut self, i: usize) -> &mut Prop {
        let n = self.len();
        self.get_mut(i).unwrap_or_else(|| panic!("prop {i} of {n}"))
    }
}

impl<'a> IntoIterator for &'a Props {
    type Item = &'a Prop;
    type IntoIter = PropIter<'a>;
    fn into_iter(self) -> PropIter<'a> {
        self.iter()
    }
}

/// Material map level 0 in copy-on-write chunks. Islands carry 2560² alpha
/// words at 320 m (26 MB); whole-map undo snapshots would cost a gigabyte
/// over the history depth, so a stroke copies only the chunks it touches.
#[derive(Clone, PartialEq)]
pub struct Layer {
    width: usize,
    height: usize,
    chunks: Vec<Arc<Vec<u32>>>,
}

/// Chunk side in words.
const CHUNK: usize = 256;

impl Layer {
    /// A level 0 material map file as chunks, read a band of chunk rows at a
    /// time on every core, and its frame: the header and whatever follows
    /// level 0, which a save writes back around the words. Read whole,
    /// decoded into a grid and copied into chunks, the continental island's
    /// two maps had held 570 MB at once.
    pub fn read(install: &Install, path: &str) -> Result<(Self, Vec<u8>)> {
        let header = install.read_range(path, 0, island::MaterialMap::HEADER as u64)?;
        let [width, height] = island::MaterialMap::size(&header)?;
        let row = width * 4;
        let words = (height * row) as u64;
        // The archives store the maps as they are, and a range reads only
        // itself; a compressed entry would inflate whole for each, so one of
        // those reads once.
        let ranged = install.mod_layer(path).is_some()
            || install.file(path).is_some_and(|id| {
                let f = install.winner(id).flags;
                !(f.compressed() || f.memres() || f.encrypted())
            });
        let whole = if ranged {
            None
        } else {
            Some(install.read(path)?)
        };
        let tail = match &whole {
            Some(bytes) => bytes
                .get(island::MaterialMap::HEADER + words as usize..)
                .unwrap_or(&[])
                .to_vec(),
            None => {
                install.read_range(path, island::MaterialMap::HEADER as u64 + words, u64::MAX)?
            }
        };
        let (cw, ch) = (width.div_ceil(CHUNK), height.div_ceil(CHUNK));
        // A band: chunk row `r`'s texel rows, as stored.
        let band = |r: usize| -> Result<Vec<Arc<Vec<u32>>>> {
            let (y0, y1) = (r * CHUNK, ((r + 1) * CHUNK).min(height));
            let read;
            let bytes = match &whole {
                Some(bytes) => {
                    let at = island::MaterialMap::HEADER + y0 * row;
                    let part = bytes.get(at..at + (y1 - y0) * row);
                    part.with_context(|| format!("{path}: material map is truncated"))?
                }
                None => {
                    let at = island::MaterialMap::HEADER as u64 + (y0 * row) as u64;
                    read = install.read_range(path, at, ((y1 - y0) * row) as u64)?;
                    anyhow::ensure!(
                        read.len() == (y1 - y0) * row,
                        "{path}: material map is truncated"
                    );
                    &read
                }
            };
            Ok((0..cw)
                .map(|c| {
                    let (x0, x1) = (c * CHUNK, ((c + 1) * CHUNK).min(width));
                    let mut chunk = vec![0u32; CHUNK * CHUNK];
                    for y in 0..y1 - y0 {
                        let src = &bytes[y * row + x0 * 4..y * row + x1 * 4];
                        let dst = &mut chunk[y * CHUNK..y * CHUNK + (x1 - x0)];
                        for (d, s) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                            *d = u32::from_le_bytes(*s);
                        }
                    }
                    Arc::new(chunk)
                })
                .collect())
        };
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let per = ch.div_ceil(threads).max(1);
        let chunks = std::thread::scope(|scope| {
            let parts: Vec<_> = (0..ch)
                .step_by(per)
                .map(|start| {
                    let band = &band;
                    scope.spawn(move || {
                        (start..(start + per).min(ch))
                            .map(band)
                            .collect::<Result<Vec<_>>>()
                    })
                })
                .collect();
            let mut chunks = Vec::with_capacity(cw * ch);
            for part in parts {
                for band in part.join().expect("material chunks")? {
                    chunks.extend(band);
                }
            }
            Ok::<_, anyhow::Error>(chunks)
        })?;
        let mut frame = header;
        frame.extend_from_slice(&tail);
        Ok((
            Self {
                width,
                height,
                chunks,
            },
            frame,
        ))
    }

    pub fn new(grid: &island::Grid<u32>) -> Self {
        let (cw, ch) = (grid.width.div_ceil(CHUNK), grid.height.div_ceil(CHUNK));
        let chunk = |i: usize| {
            let (x0, y0) = ((i % cw) * CHUNK, (i / cw) * CHUNK);
            let mut chunk = vec![0; CHUNK * CHUNK];
            for y in y0..(y0 + CHUNK).min(grid.height) {
                let row =
                    &grid.data[y * grid.width + x0..y * grid.width + (x0 + CHUNK).min(grid.width)];
                chunk[(y - y0) * CHUNK..][..row.len()].copy_from_slice(row);
            }
            Arc::new(chunk)
        };
        // Rows of chunks on all cores: one copied the continental island's
        // 144 MB alpha map in 70 ms.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let per = (cw * ch).div_ceil(threads).max(1);
        let chunks = std::thread::scope(|scope| {
            let parts: Vec<_> = (0..cw * ch)
                .step_by(per)
                .map(|start| {
                    let chunk = &chunk;
                    scope.spawn(move || {
                        (start..(start + per).min(cw * ch))
                            .map(chunk)
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            parts
                .into_iter()
                .flat_map(|p| p.join().expect("material chunks"))
                .collect()
        });
        Self {
            width: grid.width,
            height: grid.height,
            chunks,
        }
    }

    fn slot(&self, x: usize, y: usize) -> (usize, usize) {
        let cw = self.width.div_ceil(CHUNK);
        (
            (y / CHUNK) * cw + x / CHUNK,
            (y % CHUNK) * CHUNK + x % CHUNK,
        )
    }

    pub fn size(&self) -> [usize; 2] {
        [self.width, self.height]
    }

    pub fn get(&self, x: usize, y: usize) -> u32 {
        let (c, i) = self.slot(x, y);
        self.chunks[c][i]
    }

    pub fn set(&mut self, x: usize, y: usize, v: u32) {
        let (c, i) = self.slot(x, y);
        if self.chunks[c][i] != v {
            Arc::make_mut(&mut self.chunks[c])[i] = v;
        }
    }

    pub fn row_into(&self, y: usize, xs: std::ops::Range<usize>, out: &mut Vec<u32>) {
        let mut x = xs.start;
        while x < xs.end {
            let (c, i) = self.slot(x, y);
            let run = (CHUNK - x % CHUNK).min(xs.end - x);
            out.extend_from_slice(&self.chunks[c][i..i + run]);
            x += run;
        }
    }

    /// Copies a clipped rectangle out for the format's paint routine, a
    /// chunk-row run at a time.
    pub fn region(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> island::Grid<u32> {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0));
        for y in y0..y1 {
            let mut x = x0;
            while x < x1 {
                let (c, i) = self.slot(x, y);
                let run = (CHUNK - x % CHUNK).min(x1 - x);
                data.extend_from_slice(&self.chunks[c][i..i + run]);
                x += run;
            }
        }
        island::Grid {
            width: x1 - x0,
            height: y1 - y0,
            data,
        }
    }

    /// Writes `grid` at (x0, y0), copying only chunks whose words change.
    /// Returns whether any did.
    pub fn write(&mut self, x0: usize, y0: usize, grid: &island::Grid<u32>) -> bool {
        let mut changed = false;
        for y in 0..grid.height {
            let row = &grid.data[y * grid.width..(y + 1) * grid.width];
            let mut x = 0;
            while x < grid.width {
                let (c, i) = self.slot(x0 + x, y0 + y);
                let run = (CHUNK - (x0 + x) % CHUNK).min(grid.width - x);
                let new = &row[x..x + run];
                if self.chunks[c][i..i + run] != *new {
                    Arc::make_mut(&mut self.chunks[c])[i..i + run].copy_from_slice(new);
                    changed = true;
                }
                x += run;
            }
        }
        changed
    }

    pub fn grid(&self) -> island::Grid<u32> {
        self.region(0, 0, self.width, self.height)
    }

    /// The words (half-open rect) where two layers of one size differ,
    /// `None` when equal. Chunks both still share are skipped unread, so
    /// after one stroke this reads only the chunks the stroke copied.
    fn changed(&self, other: &Layer) -> Option<Rect> {
        debug_assert_eq!(self.size(), other.size());
        let cw = self.width.div_ceil(CHUNK);
        let mut rect: Option<Rect> = None;
        for (c, (a, b)) in self.chunks.iter().zip(&other.chunks).enumerate() {
            if Arc::ptr_eq(a, b) {
                continue;
            }
            let (x0, y0) = ((c % cw) * CHUNK, (c / cw) * CHUNK);
            for (y, (ra, rb)) in a.chunks(CHUNK).zip(b.chunks(CHUNK)).enumerate() {
                let Some(first) = ra.iter().zip(rb).position(|(p, q)| p != q) else {
                    continue;
                };
                let last = CHUNK - ra.iter().zip(rb).rev().position(|(p, q)| p != q).unwrap();
                let row = Rect {
                    x0: x0 + first,
                    z0: y0 + y,
                    x1: x0 + last,
                    z1: y0 + y + 1,
                };
                rect = Some(rect.map_or(row, |r| r.union(row)));
            }
        }
        rect
    }
}

/// Everything an undo step restores. Heights are metres. Materials are the
/// game's level-0 maps when the island ships them: one id block per 2x2
/// alpha texels, `per_metre` alpha texels per metre.
#[derive(Clone)]
pub struct Doc {
    /// Copy-on-write (`Arc::make_mut` to edit): undo steps that leave the
    /// heights alone share them, and a stroke copies them once.
    pub field: Arc<Heightfield>,
    /// Heights last used to synchronize gameplay grids. Undo keeps this
    /// snapshot with the grids, so manual painting survives later saves.
    /// After a sync it is `field` itself, shared.
    pub land_heights: Arc<Heightfield>,
    pub materials: Option<Materials>,
    pub meshes: Arc<Vec<String>>,
    /// Copy-on-write in runs (`Props`); objects whole (`Arc::make_mut` to
    /// edit). Every undo step copied the continental island's 45k props and
    /// 4k objects, 9 MB a step, 290 MB over the history's depth.
    pub props: Props,
    /// Placed gamedata objects (buildings, nature, editor markers). The
    /// decoded area trees stay with the editor; undo snapshots hold only this.
    pub objects: Arc<Vec<Object>>,
    /// Gameplay grids. Copy-on-write: undo steps share untouched layers.
    pub grids: Option<Arc<Grids>>,
    /// Working tint level (see `tint::WORK_LEVEL`), copy-on-write.
    pub tint: Option<Arc<TintMap>>,
    /// Ambient and vegetation prop set (gamedata `SessionSettings`).
    pub session: SessionSettings,
    /// In an undo snapshot: its heights as a patch against the step undo
    /// restores it from, `field` then holding none (`patch_heights`).
    pub heights_patch: Option<Arc<HeightsPatch>>,
}

/// 2070's two authored grids beside its terrain: the 0.5 m path-blocker
/// layers, and the build-blocker polygons the game fills its build grid
/// from at load. The game derives neither from heights, so they are edited,
/// not recomputed; a sculpt only blocks the cells it lifts off buildable
/// land, which the editor's height tools do as they go, so a save writes
/// them as they stand.
///
/// They ride in the 2070 paint form ([`Layers::blockers`]), the one place
/// in `Doc` with room to spare: a field of their own grew every game's undo
/// step. Path cells sit in copy-on-write chunks, so a stroke copies the
/// chunks it changes (512 bytes each) and the chunk list (under 1 KB on the
/// largest island); the polygons are a few dozen points, copied whole.
#[derive(Clone)]
pub struct Blockers {
    cols: usize,
    rows: usize,
    /// Both layers' chunks, [`PathLayer::Blocked`] first, each row by row.
    paths: Vec<Arc<PathChunk>>,
    /// `None` on the 23 islands without `BuildBlockerShapes`.
    pub shapes: Option<Arc<anno_formats::isd::grids::BuildBlockers>>,
    /// Whether the island lies under water (2070's Deep Ocean), so a
    /// height tool blocks the cells it lifts past the plateau's buildable
    /// band (`anno2070::island::UNDERWATER_BUILDABLE`) as well as the land's.
    pub under_water: bool,
}

pub use anno_formats::isd::grids::PathLayer;

/// A path chunk's side in 0.5 m cells: 32 m, two words a row.
const PATH_CHUNK: usize = 64;
const PATH_WORDS: usize = PATH_CHUNK / 32;
type PathChunk = [u32; PATH_CHUNK * PATH_WORDS];

impl Blockers {
    /// The grids as the codec read them.
    pub fn new(
        paths: &anno_formats::isd::grids::PathBlockers,
        shapes: Option<anno_formats::isd::grids::BuildBlockers>,
    ) -> Self {
        let (cols, rows) = paths.size();
        let (cw, ch) = (cols.div_ceil(PATH_CHUNK), rows.div_ceil(PATH_CHUNK));
        let mut out = Self {
            cols,
            rows,
            paths: Vec::with_capacity(2 * cw * ch),
            shapes: shapes.map(Arc::new),
            under_water: false,
        };
        for layer in [PathLayer::Blocked, PathLayer::HighGround] {
            for c in 0..cw * ch {
                let (x0, z0) = (c % cw * PATH_CHUNK, c / cw * PATH_CHUNK);
                let mut chunk = [0; PATH_CHUNK * PATH_WORDS];
                for z in z0..(z0 + PATH_CHUNK).min(rows) {
                    for x in x0..(x0 + PATH_CHUNK).min(cols) {
                        if paths.get(layer, x, z) {
                            let (i, bit) = Self::in_chunk(x, z);
                            chunk[i] |= bit;
                        }
                    }
                }
                out.paths.push(Arc::new(chunk));
            }
        }
        out
    }

    /// Cells per row, and rows: two a metre.
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    // The word and bit of cell (x, z) inside its chunk.
    fn in_chunk(x: usize, z: usize) -> (usize, u32) {
        let (lx, lz) = (x % PATH_CHUNK, z % PATH_CHUNK);
        (lz * PATH_WORDS + lx / 32, 1 << (lx % 32))
    }

    fn chunk(&self, layer: PathLayer, x: usize, z: usize) -> usize {
        assert!(x < self.cols && z < self.rows, "cell {x},{z}");
        let (cw, ch) = (
            self.cols.div_ceil(PATH_CHUNK),
            self.rows.div_ceil(PATH_CHUNK),
        );
        layer as usize * cw * ch + z / PATH_CHUNK * cw + x / PATH_CHUNK
    }

    /// Whether 0.5 m cell (x, z) of `layer` is set.
    pub fn get(&self, layer: PathLayer, x: usize, z: usize) -> bool {
        let (i, bit) = Self::in_chunk(x, z);
        self.paths[self.chunk(layer, x, z)][i] & bit != 0
    }

    /// Sets (`on`) or clears every cell of `layer` whose centre lies within
    /// `radius` metres of (x, z), in island metres, copying only the chunks
    /// it changes. Gives how many cells changed.
    pub fn brush(&mut self, layer: PathLayer, x: f32, z: f32, radius: f32, on: bool) -> usize {
        // Cell (i, j) spans [i/2, (i+1)/2) m; its centre is (i + 0.5) / 2.
        let span = |c: f32, n: usize| {
            let lo = ((c - radius) * 2. - 0.5).ceil().max(0.) as usize;
            let hi = (((c + radius) * 2. - 0.5).floor() + 1.).clamp(0., n as f32) as usize;
            lo..hi
        };
        let mut changed = 0;
        for j in span(z, self.rows) {
            for i in span(x, self.cols) {
                let (dx, dz) = ((i as f32 + 0.5) / 2. - x, (j as f32 + 0.5) / 2. - z);
                if dx * dx + dz * dz > radius * radius || self.get(layer, i, j) == on {
                    continue;
                }
                let (w, bit) = Self::in_chunk(i, j);
                let c = self.chunk(layer, i, j);
                Arc::make_mut(&mut self.paths[c])[w] ^= bit;
                changed += 1;
            }
        }
        changed
    }

    /// The cells (half-open, both layers together) of every chunk that
    /// differs from `other`'s, the same island's grids at another undo
    /// step; chunks both share are skipped unread. And whether the polygons
    /// differ.
    pub fn changed(&self, other: &Blockers) -> (Option<Rect>, bool) {
        let (cw, ch) = (
            self.cols.div_ceil(PATH_CHUNK),
            self.rows.div_ceil(PATH_CHUNK),
        );
        let mut rect: Option<Rect> = None;
        for (c, (a, b)) in self.paths.iter().zip(&other.paths).enumerate() {
            if Arc::ptr_eq(a, b) || a == b {
                continue;
            }
            let c = c % (cw * ch);
            let (x0, z0) = (c % cw * PATH_CHUNK, c / cw * PATH_CHUNK);
            let chunk = Rect {
                x0,
                z0,
                x1: (x0 + PATH_CHUNK).min(self.cols),
                z1: (z0 + PATH_CHUNK).min(self.rows),
            };
            rect = Some(rect.map_or(chunk, |r| r.union(chunk)));
        }
        let shapes = match (&self.shapes, &other.shapes) {
            (Some(a), Some(b)) => {
                !Arc::ptr_eq(a, b)
                    && (a.len() != b.len() || (0..a.len()).any(|s| a.points(s).ne(b.points(s))))
            }
            (None, None) => false,
            _ => true,
        };
        (rect, shapes)
    }

    /// The polygons, copied for this step if another step shares them.
    pub fn shapes_mut(&mut self) -> Option<&mut anno_formats::isd::grids::BuildBlockers> {
        self.shapes.as_mut().map(Arc::make_mut)
    }

    // Each allocation and its bytes, for memory diagnostics.
    fn allocations(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let shapes = self.shapes.iter().map(|s| {
            let points: usize = (0..s.len()).map(|n| s.points(n).count()).sum();
            (Arc::as_ptr(s) as usize, points * 16 + s.len() * 8)
        });
        (self.paths.iter())
            .map(|c| (Arc::as_ptr(c) as usize, size_of::<PathChunk>()))
            .chain(shapes)
    }
}

/// An undo step's heights as the samples that differ from its neighbour
/// in the history. Whole heights made every sculpt step 9 MB on the
/// continental island, 290 MB over the history's depth; a stroke's patch
/// is its footprint.
pub struct HeightsPatch {
    rect: Rect,
    /// The step's heights in `rect`, row by row.
    pub values: Vec<f32>,
    /// The land sync's heights were these heights.
    land: bool,
}

/// Ground paint in the island's own form. Tools that never see the form
/// (brush, river bed, eyedropper, overview, new island) go through these
/// methods and `Materials`'; readers that do match on it. Coordinates are
/// texels of the form's finest map: a7m alpha texels, or 2070's metre
/// vertices.
#[derive(Clone, PartialEq)]
pub enum Paint {
    /// 117 and 1800.
    A7m(A7m),
    /// 2070.
    Layers(Layers),
    /// A game's own form, which its codec reads and its ground draws.
    Own(Own),
}

/// A game's own paint form, for islands that are neither a7m nor 2070's:
/// the game's crate implements it on its own type, its codec reads and
/// writes it (`Own::get`), and the shell's tools reach it through
/// `Paint`'s and `Materials`' methods. Coordinates are texels of its finest
/// map.
pub trait Form: Send + Sync + 'static {
    /// Texels of the finest map, x and z.
    fn size(&self) -> [usize; 2];
    /// Whether texels are corners a world position rounds to (2070's metre
    /// vertices), rather than cells it falls in.
    fn corners(&self) -> bool {
        false
    }
    /// What texel (x, y) is painted with, as stored.
    fn cover(&self, x: usize, y: usize) -> Cover;
    /// `cover` as the ground shows it: (id, share) laid over each other,
    /// the entries at the front. By default by `stack`, else each id shows
    /// its coverage.
    fn shares(&self, cover: &mut [(u8, f32)]) -> usize {
        self.stack().map_or(cover.len(), |s| s.shares(cover))
    }
    /// How the ground lays the ids over each other, for conversion to
    /// encode into it and read it back: ranks and sharpness. `None` for a
    /// form whose ids don't stack.
    fn stack(&self) -> Option<&Stack> {
        None
    }
    /// Paints `id` over `lo`..`hi` (metres, `per_metre` texels a metre) at
    /// `weight` of each texel's centre; see `Materials::paint_with`.
    fn paint_with(
        &mut self,
        per_metre: f32,
        lo: [f32; 2],
        hi: [f32; 2],
        id: u8,
        weight: &(dyn Fn([f32; 2]) -> f32 + Sync),
    ) -> bool;
    /// Whether `other` has this form and its size, so the two compare
    /// texel by texel.
    fn same_shape(&self, other: &dyn Form) -> bool;
    /// The texels (half-open) where `other`, of the same shape, differs.
    fn changed(&self, other: &dyn Form) -> Option<Rect>;
    /// Rewrites every texel to show `shares` (id, share shown) at its
    /// position in metres, `base` lying under all, for conversion into
    /// this form; gives how many texels lost ground the form had no room
    /// for. By default the form takes no conversion: its paint stays, and
    /// every texel counts as lost.
    fn fill(
        &mut self,
        _per_metre: f32,
        _base: u8,
        _shares: &dyn Fn([f32; 2]) -> Vec<(u8, f32)>,
    ) -> usize {
        let [w, h] = self.size();
        w * h
    }
    /// A copy to edit, for an undo step that shares this one.
    fn duplicate(&self) -> Box<dyn Form>;
    /// Bytes held, for memory diagnostics.
    fn bytes(&self) -> usize;
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A game's paint form ([`Form`]), shared between undo steps until one
/// edits it.
#[derive(Clone)]
pub struct Own(Arc<dyn Form>);

impl Own {
    pub fn new(form: impl Form) -> Self {
        Self(Arc::new(form))
    }

    /// The game's form, for its codec and ground.
    pub fn get<T: Form>(&self) -> Option<&T> {
        self.0.as_any().downcast_ref()
    }

    pub fn form(&self) -> &dyn Form {
        &*self.0
    }

    /// The form to edit, copied first while another step shares it.
    pub fn form_mut(&mut self) -> &mut dyn Form {
        if Arc::get_mut(&mut self.0).is_none() {
            self.0 = Arc::from(self.0.duplicate());
        }
        Arc::get_mut(&mut self.0).expect("just copied")
    }
}

impl PartialEq for Own {
    fn eq(&self, other: &Own) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.same_shape(&*other.0) && self.0.changed(&*other.0).is_none())
    }
}

/// Six (id, 5-bit weight) slots per texel: the ids in one `matid` word per
/// 2x2 block of `matalpha` words, which hold the weights. Ids are five bits
/// (`& 31`); that mask stays inside this form.
#[derive(Clone, PartialEq)]
pub struct A7m {
    pub ids: Layer,
    pub alphas: Layer,
}

impl A7m {
    /// The six (id, 5-bit weight) slots of alpha texel (x, y).
    pub fn slots(&self, x: usize, y: usize) -> ([u8; 6], [u8; 6]) {
        (
            island::unpack_slots(self.ids.get(x / 2, y / 2)),
            island::unpack_slots(self.alphas.get(x, y)),
        )
    }

    /// The alpha texels (half-open) where two maps of one shape differ.
    fn changed(&self, other: &A7m) -> Option<Rect> {
        // An id block covers 2x2 alpha texels.
        let ids = self.ids.changed(&other.ids).map(|r| Rect {
            x0: r.x0 * 2,
            z0: r.z0 * 2,
            x1: r.x1 * 2,
            z1: r.z1 * 2,
        });
        match (ids, self.alphas.changed(&other.alphas)) {
            (Some(a), Some(b)) => Some(a.union(b)),
            (r, None) | (None, r) => r,
        }
    }

    /// See [`Materials::paint_with`]. The format's routine runs on a
    /// block-aligned copy of the footprint so chunk copies stay limited to
    /// what is painted.
    fn paint_with(
        &mut self,
        pm: f32,
        lo: [f32; 2],
        hi: [f32; 2],
        id: u8,
        weight: impl Fn([f32; 2]) -> f32 + Sync,
    ) -> bool {
        let even = |v: f32| ((v.max(0.) as usize) / 2) * 2;
        let (x0, y0) = (even(lo[0] * pm), even(lo[1] * pm));
        let (x1, y1) = (
            ((hi[0] * pm).ceil().max(0.) as usize + 2) & !1,
            ((hi[1] * pm).ceil().max(0.) as usize + 2) & !1,
        );
        let (x1, y1) = (x1.min(self.alphas.width), y1.min(self.alphas.height));
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        // Rows of id blocks are independent (each block owns its 2x2
        // texels), so bands of them paint in parallel. A 21 m brush on an
        // 8-texel/m map is 113k texels; one core took 4 ms.
        let paint_band = |by0: usize, by1: usize| {
            let (ty0, ty1) = (by0 * 2, (by1 * 2).min(y1));
            let mut ids = self.ids.region(x0 / 2, by0, x1.div_ceil(2), by1);
            let mut alphas = self.alphas.region(x0, ty0, x1, ty1);
            for y in 0..alphas.height {
                for x in 0..alphas.width {
                    let wx = ((x0 + x) as f32 + 0.5) / pm;
                    let wy = ((ty0 + y) as f32 + 0.5) / pm;
                    let w = weight([wx, wy]).min(1.);
                    if w > 0. {
                        island::paint_material(&mut ids, &mut alphas, x, y, id, w);
                    }
                }
            }
            (by0, ty0, ids, alphas)
        };
        let (by0, by1) = (y0 / 2, y1.div_ceil(2));
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let texels = (x1 - x0) * (y1 - y0);
        let bands = if texels < 16_384 {
            1
        } else {
            threads.min(by1 - by0)
        };
        let step = (by1 - by0).div_ceil(bands.max(1));
        // One band paints here: a scoped thread per dab cost more than the
        // dab, and lake shores lay hundreds of small ones.
        let painted: Vec<_> = if bands <= 1 {
            vec![paint_band(by0, by1)]
        } else {
            std::thread::scope(|scope| {
                let workers: Vec<_> = (by0..by1)
                    .step_by(step.max(1))
                    .map(|b| {
                        let end = (b + step).min(by1);
                        scope.spawn(move || paint_band(b, end))
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|w| w.join().expect("paint band"))
                    .collect()
            })
        };
        let mut changed = false;
        for (by, ty, ids, alphas) in painted {
            changed |= self.ids.write(x0 / 2, by, &ids);
            changed |= self.alphas.write(x0, ty, &alphas);
        }
        changed
    }
}

/// 2070's paint: per 16 m chunk its layer ids, ascending, and a 17×17
/// coverage plane for each, a byte per metre vertex. Layer 0 is the base;
/// its plane is kept as stored, though the shader draws it opaque.
/// Neighbouring chunks hold the same coverage along the edge they share, a
/// missing layer counting as 0, and the brush keeps it so. Chunks are
/// copy-on-write: a stroke copies only those it changes, at most 6.9 KB
/// each, and undo steps share the rest.
#[derive(Clone)]
pub struct Layers {
    /// Chunks per row, and rows.
    chunks: [usize; 2],
    paint: Vec<Arc<Chunk>>,
    registry: Arc<Registry>,
    /// Vertices the brush skipped since `Materials::take_skipped`. It sits
    /// here, not on `Materials`, where it fits beside the a7m form and so
    /// costs undo steps nothing.
    skipped: usize,
    /// The island's blocker grids, for the same reason: see [`Blockers`].
    /// Not paint, so equality and `changed` leave them out.
    blockers: Option<Arc<Blockers>>,
}

// `Layers` holds what `Doc` would otherwise grow by only while it fits in
// the a7m form's room: past that, every 117 and 1800 undo step pays.
const _: () = assert!(size_of::<Layers>() <= size_of::<A7m>());

/// One chunk's layers: ids ascending and their planes back to back in the
/// same order. Empty ocean chunks hold none.
#[derive(Clone, Default, PartialEq)]
struct Chunk {
    ids: Vec<u8>,
    planes: Vec<u8>,
}

impl Chunk {
    fn plane(&self, layer: usize) -> &[u8; isd::Terrain::PLANE] {
        self.planes[layer * isd::Terrain::PLANE..][..isd::Terrain::PLANE]
            .try_into()
            .expect("a whole plane")
    }

    // A zeroed plane for `id`, in its place among the ids.
    fn insert(&mut self, id: u8) {
        let at = self.ids.partition_point(|&held| held < id);
        self.ids.insert(at, id);
        let at = at * isd::Terrain::PLANE;
        self.planes.splice(at..at, [0; isd::Terrain::PLANE]);
    }

    fn remove(&mut self, layer: usize) {
        self.ids.remove(layer);
        self.planes
            .drain(layer * isd::Terrain::PLANE..(layer + 1) * isd::Terrain::PLANE);
    }
}

impl PartialEq for Layers {
    fn eq(&self, other: &Layers) -> bool {
        self.chunks == other.chunks
            && (self.paint.iter().zip(&other.paint)).all(|(a, b)| Arc::ptr_eq(a, b) || a == b)
    }
}

/// Why a paint dab leaves a vertex as it was: the two ways 2070's ground
/// refuses a layer there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// `chunk` lacks the layer and already holds
    /// [`isd::Terrain::MAX_LAYERS`]. No chunk at the vertex took it.
    Full { chunk: usize },
    /// The registry has no such texture, so nothing says where it draws.
    Unregistered,
}

/// The ranked splat's sharpness on 2070's ground (terrain.fx:54-60).
pub const LAYERS_SHARPNESS: f32 = 5.;

/// 2070's ground texture registry (`groundtextures.xml`) as its paint reads
/// it: each id's draw rank, its `SplatID`, and whether it scatters grass or
/// bushes instead of drawing colour; and the colour layers as a [`Stack`]
/// at the shader's sharpness, so [`Stack::shares`] stacks them as the
/// ground draws them.
pub struct Registry {
    textures: isd::GroundTextures,
    stack: Stack,
    /// Metres one repeat of a layer's textures spans (`TextureScale`).
    pub scale: f32,
}

impl Registry {
    /// `textures` with layer textures repeating every `scale` metres.
    /// Fails on an id over a byte: shipped ids reach 108, and the paint
    /// keeps them in bytes.
    pub fn new(textures: isd::GroundTextures, scale: f32) -> Result<Self> {
        let mut colour = Vec::new();
        for t in textures.textures() {
            let id = u8::try_from(t.id).map_err(|_| {
                anyhow::anyhow!("ground texture {} over the 255 the paint holds", t.id)
            })?;
            if t.diffuse.is_some() {
                colour.push((t.rank, id));
            }
        }
        // Stable: equal ranks keep the file's order, as the layer ground's
        // `LayerPaint` lays them out.
        colour.sort_by_key(|&(rank, _)| rank);
        let mut stack = Stack::default();
        for (_, id) in colour {
            stack.push(id, LAYERS_SHARPNESS);
        }
        Ok(Self {
            textures,
            stack,
            scale,
        })
    }

    /// Draw rank; `None` for an unregistered id.
    pub fn rank(&self, id: u8) -> Option<u16> {
        self.textures.rank(u32::from(id))
    }

    /// Whether `id` places grass or bush objects instead of drawing colour.
    pub fn scatter(&self, id: u8) -> bool {
        self.textures.scatter(u32::from(id))
    }

    /// The colour layers, lowest rank first, at [`LAYERS_SHARPNESS`].
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    pub fn textures(&self) -> &isd::GroundTextures {
        &self.textures
    }
}

impl Layers {
    /// `chunks` (per row, rows) with no paint: all empty ocean.
    pub fn new(chunks: [usize; 2], registry: Arc<Registry>) -> Self {
        Self {
            chunks,
            paint: vec![Arc::default(); chunks[0] * chunks[1]],
            registry,
            skipped: 0,
            blockers: None,
        }
    }

    /// These layers holding `blockers`, the island's blocker grids.
    pub fn with_blockers(mut self, blockers: Blockers) -> Self {
        self.blockers = Some(Arc::new(blockers));
        self
    }

    /// The paint of `terrain`, an island read through C's view.
    pub fn read(terrain: &isd::Terrain, registry: Arc<Registry>) -> Result<Self> {
        let (w, h) = terrain.size();
        let mut layers = Self::new([w, h], registry);
        for k in 0..w * h {
            let ids = (terrain.ids(k).iter())
                .map(|&id| u8::try_from(id).map_err(|_| anyhow::anyhow!("layer {id} over 255")))
                .collect::<Result<Vec<u8>>>()?;
            let planes = (0..ids.len()).flat_map(|l| terrain.plane(k, l)).copied();
            layers.set(k, &ids, &planes.collect::<Vec<_>>())?;
        }
        Ok(layers)
    }

    /// Gives chunk `k` these layers: `ids` ascending from the base 0, at
    /// most [`isd::Terrain::MAX_LAYERS`], and a plane per id back to back. No ids
    /// leaves it unpainted. Edges are the caller's to keep agreeing.
    pub fn set(&mut self, k: usize, ids: &[u8], planes: &[u8]) -> Result<()> {
        ensure!(
            ids.len() <= isd::Terrain::MAX_LAYERS,
            "{} layers in chunk {k}",
            ids.len()
        );
        ensure!(
            ids.first().is_none_or(|&id| id == 0) && ids.is_sorted_by(|a, b| a < b),
            "chunk {k}: layers {ids:?} don't ascend from the base 0"
        );
        ensure!(
            planes.len() == ids.len() * isd::Terrain::PLANE,
            "chunk {k}: {} bytes for {} planes",
            planes.len(),
            ids.len()
        );
        self.paint[k] = Arc::new(Chunk {
            ids: ids.to_vec(),
            planes: planes.to_vec(),
        });
        Ok(())
    }

    /// Chunks per row, and rows.
    pub fn chunks(&self) -> [usize; 2] {
        self.chunks
    }

    /// Chunk `k`'s layer ids, ascending; empty for empty ocean.
    pub fn ids(&self, k: usize) -> &[u8] {
        &self.paint[k].ids
    }

    /// Coverage of chunk `k`'s `layer`th id.
    pub fn plane(&self, k: usize, layer: usize) -> &[u8; isd::Terrain::PLANE] {
        self.paint[k].plane(layer)
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Vertices along x and z.
    fn size(&self) -> [usize; 2] {
        self.chunks.map(|c| c * isd::Terrain::CHUNK + 1)
    }

    // Where vertex (x, z) sits in chunk `k`'s planes.
    fn at(&self, k: usize, x: usize, z: usize) -> usize {
        let w = self.chunks[0];
        (z - k / w * isd::Terrain::CHUNK) * isd::Terrain::PLANE_SIDE + x
            - k % w * isd::Terrain::CHUNK
    }

    // Chunks holding vertex (x, z): one inside a chunk, two on an edge,
    // four where chunks meet.
    fn around(&self, x: usize, z: usize) -> impl Iterator<Item = usize> + Clone + use<> {
        isd::chunks_at((self.chunks[0], self.chunks[1]), x, z)
    }

    /// What vertex (x, z) is painted with, as stored: the first painted
    /// chunk holding it speaks for all, since they agree.
    fn cover(&self, x: usize, z: usize) -> Cover {
        let mut out = Cover::default();
        if let Some(k) = self.around(x, z).find(|&k| !self.paint[k].ids.is_empty()) {
            let (chunk, i) = (&self.paint[k], self.at(k, x, z));
            for (layer, &id) in chunk.ids.iter().enumerate() {
                let v = chunk.plane(layer)[i];
                if v > 0 {
                    out.add(id, f32::from(v) / 255.);
                }
            }
        }
        out
    }

    /// Paints layer `id` at vertex (x, z), in island metres, by `amount`
    /// from 0 to 1, the a7m brush's rule (`island::paint_material`): `id`
    /// rises toward full coverage, and only colour layers ranked above it
    /// fall toward none. The base layer, scatter layers and layers ranked
    /// below keep theirs; painting a scatter layer changes that layer
    /// alone. A colour layer lowered to zero across its chunk leaves it.
    ///
    /// Every painted chunk at the vertex takes the edit, so they go on
    /// agreeing: a chunk lacking `id` gets it, and when one of them is full
    /// ([`isd::Terrain::MAX_LAYERS`]) none is touched. With no painted chunk at the
    /// vertex, each chunk there is painted first: an opaque base and its
    /// painted neighbours' coverage along its edges, as the `.isd` codec
    /// paints a chunk a first sculpt creates (`isd::terrain::Corners::write`).
    /// Returns whether anything changed.
    pub fn paint(&mut self, x: usize, z: usize, id: u8, amount: f32) -> Result<bool, Skipped> {
        let top = self.registry.rank(id).ok_or(Skipped::Unregistered)?;
        let [w, h] = self.size();
        assert!(x < w && z < h, "vertex {x},{z}");
        // Also turns away NaN, which would round every value it met to 0.
        if !(amount > 0.) {
            return Ok(false);
        }
        let amount = amount.min(1.);
        let toward = |v: u8, target: f32| (v as f32 + (target - v as f32) * amount).round() as u8;
        // Where a chunk lacks `id` the others cover nothing with it, so a
        // new plane starts from 0; one that would stay 0 isn't worth a
        // layer.
        let insert = toward(0, 255.) > 0;
        let painted = |layers: &Layers| {
            let mut owners = [0; 4];
            let mut n = 0;
            for k in layers.around(x, z) {
                if !layers.paint[k].ids.is_empty() {
                    owners[n] = k;
                    n += 1;
                }
            }
            (owners, n)
        };
        let (mut owners, mut n) = painted(self);
        let mut created = false;
        if n == 0 {
            if !insert {
                return Ok(false);
            }
            // Empty ocean all round: paint every chunk at the vertex, or none.
            for k in self.around(x, z) {
                if let Err(full) = self.create(k) {
                    for k in self.around(x, z) {
                        self.paint[k] = Arc::default();
                    }
                    return Err(full);
                }
            }
            created = true;
            (owners, n) = painted(self);
        }
        let owners = &owners[..n];
        let lacks = |k: usize| self.paint[k].ids.binary_search(&id).is_err();
        if insert
            && let Some(&chunk) = (owners.iter())
                .find(|&&k| lacks(k) && self.paint[k].ids.len() == isd::Terrain::MAX_LAYERS)
        {
            // C keeps chunks it created when they come out full; none here,
            // so the vertex is left exactly as it was.
            if created {
                for k in self.around(x, z) {
                    self.paint[k] = Arc::default();
                }
            }
            return Err(Skipped::Full { chunk });
        }
        let registry = &self.registry;
        let colour = !registry.scatter(id);
        // The target of each of a chunk's layers, or `None` to leave it.
        let target = |other: u8| {
            if other == id {
                Some(255.)
            } else if colour
                && other != 0
                && !registry.scatter(other)
                && registry.rank(other).is_some_and(|rank| rank > top)
            {
                Some(0.)
            } else {
                None
            }
        };
        let mut changed = created;
        for &k in owners {
            let i = self.at(k, x, z);
            let chunk = &self.paint[k];
            // Copied only when it changes, so undo steps keep sharing it.
            let missing = insert && chunk.ids.binary_search(&id).is_err();
            let moves = (chunk.ids.iter().enumerate()).any(|(layer, &other)| {
                let v = chunk.plane(layer)[i];
                target(other).is_some_and(|t| toward(v, t) != v)
            });
            if !(missing || moves) {
                continue;
            }
            changed = true;
            let chunk = Arc::make_mut(&mut self.paint[k]);
            if missing {
                chunk.insert(id);
            }
            let mut layer = 0;
            while layer < chunk.ids.len() {
                let Some(t) = target(chunk.ids[layer]) else {
                    layer += 1;
                    continue;
                };
                let v = &mut chunk.planes[layer * isd::Terrain::PLANE + i];
                *v = toward(*v, t);
                // A layer lowered to nothing everywhere goes, and frees its
                // place among the chunk's 24. Missing reads as 0, so its
                // neighbours still agree with it.
                if *v == 0 && t == 0. && chunk.plane(layer).iter().all(|&a| a == 0) {
                    chunk.remove(layer);
                    continue;
                }
                layer += 1;
            }
        }
        Ok(changed)
    }

    // Paints the unpainted chunk so it agrees with every painted chunk it
    // shares a vertex with: along its edges each of their layers covers
    // what it covers there, inside it only the base, opaque.
    fn create(&mut self, chunk: usize) -> Result<(), Skipped> {
        let (w, c) = (self.chunks[0], isd::Terrain::CHUNK);
        let (cx, cz) = (chunk % w, chunk / w);
        let mut ids = vec![0u8];
        let mut planes = vec![[255u8; isd::Terrain::PLANE]];
        for n in 0..isd::Terrain::PLANE {
            let (lx, lz) = (n % isd::Terrain::PLANE_SIDE, n / isd::Terrain::PLANE_SIDE);
            if !(lx == 0 || lx == c || lz == 0 || lz == c) {
                continue;
            }
            let (x, z) = (cx * c + lx, cz * c + lz);
            let holder = (self.around(x, z)).find(|&k| k != chunk && !self.paint[k].ids.is_empty());
            let Some(k) = holder else {
                continue;
            };
            let (held, at) = (&self.paint[k], self.at(k, x, z));
            for (layer, &id) in held.ids.iter().enumerate() {
                let v = held.plane(layer)[at];
                let j = match ids.binary_search(&id) {
                    Ok(j) => j,
                    Err(_) if v == 0 => continue,
                    Err(j) => {
                        ids.insert(j, id);
                        planes.insert(j, [0; isd::Terrain::PLANE]);
                        j
                    }
                };
                planes[j][n] = v;
            }
        }
        if ids.len() > isd::Terrain::MAX_LAYERS {
            return Err(Skipped::Full { chunk });
        }
        self.paint[chunk] = Arc::new(Chunk {
            ids,
            planes: planes.concat(),
        });
        Ok(())
    }

    /// The vertices (half-open) of every chunk that differs from `other`'s.
    /// Chunks both still share are skipped unread.
    fn changed(&self, other: &Layers) -> Option<Rect> {
        let (w, c) = (self.chunks[0], isd::Terrain::CHUNK);
        let mut rect: Option<Rect> = None;
        for (k, (a, b)) in self.paint.iter().zip(&other.paint).enumerate() {
            if Arc::ptr_eq(a, b) || a == b {
                continue;
            }
            let (x0, z0) = (k % w * c, k / w * c);
            let chunk = Rect {
                x0,
                z0,
                x1: x0 + c + 1,
                z1: z0 + c + 1,
            };
            rect = Some(rect.map_or(chunk, |r| r.union(chunk)));
        }
        rect
    }

    // See `Materials::paint_with`. Vertices sit on whole metres; a full
    // chunk's vertices are skipped and counted.
    fn paint_with(
        &mut self,
        lo: [f32; 2],
        hi: [f32; 2],
        id: u8,
        weight: impl Fn([f32; 2]) -> f32,
    ) -> bool {
        let [w, h] = self.size();
        let span = |lo: f32, hi: f32, n: usize| {
            (lo.ceil().max(0.) as usize)..((hi.floor() + 1.).clamp(0., n as f32) as usize)
        };
        let mut changed = false;
        for z in span(lo[1], hi[1], h) {
            for x in span(lo[0], hi[0], w) {
                let amount = weight([x as f32, z as f32]).min(1.);
                if amount <= 0. {
                    continue;
                }
                match self.paint(x, z, id, amount) {
                    Ok(c) => changed |= c,
                    Err(Skipped::Full { .. }) => self.skipped += 1,
                    // Not in the registry: nothing says where it draws.
                    Err(Skipped::Unregistered) => return changed,
                }
            }
        }
        changed
    }

    fn allocations(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let blockers = self.blockers.iter().flat_map(|b| {
            let list = b.paths.len() * size_of::<usize>();
            [(Arc::as_ptr(b) as usize, list)]
                .into_iter()
                .chain(b.allocations())
        });
        (self.paint.iter())
            .map(|c| (Arc::as_ptr(c) as usize, c.ids.len() + c.planes.len()))
            .chain(blockers)
    }
}

impl Paint {
    /// Texels of the finest map, x and z.
    pub fn size(&self) -> [usize; 2] {
        match self {
            Paint::A7m(a) => a.alphas.size(),
            Paint::Layers(l) => l.size(),
            Paint::Own(o) => o.0.size(),
        }
    }

    /// The a7m form, which the a7m splat, its bake and the ground's sinking
    /// read in place; `None` for a form that draws through its own ground.
    pub fn a7m(&self) -> Option<&A7m> {
        match self {
            Paint::A7m(a) => Some(a),
            Paint::Layers(_) | Paint::Own(_) => None,
        }
    }

    /// Whether texels are corners a world position rounds to (2070's metre
    /// vertices), rather than cells it falls in (a7m's alpha texels).
    pub fn corners(&self) -> bool {
        match self {
            Paint::A7m(_) => false,
            Paint::Layers(_) => true,
            Paint::Own(o) => o.0.corners(),
        }
    }

    /// The id with the most weight at texel (x, y): an a7m texel's heaviest
    /// slot, or the layer that shows most once stacked as the form's ground
    /// stacks them.
    pub fn dominant(&self, x: usize, y: usize) -> u8 {
        match self {
            Paint::A7m(a) => {
                let (ids, weights) = a.slots(x, y);
                (0..6).max_by_key(|&k| weights[k]).map_or(0, |k| ids[k])
            }
            Paint::Layers(_) | Paint::Own(_) => {
                let mut cover = self.cover(x, y);
                let n = self.shares(&mut cover).unwrap_or(cover.len());
                (cover[..n].iter())
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .map_or(0, |c| c.0)
            }
        }
    }

    /// How the form's own ground stacks its ids; `None` for a7m paint,
    /// which its material set's palette stacks.
    pub fn stack(&self) -> Option<&Stack> {
        match self {
            Paint::A7m(_) => None,
            Paint::Layers(l) => Some(&l.registry.stack),
            Paint::Own(o) => o.0.stack(),
        }
    }

    /// `cover` stacked as the form's own ground shows it, the entries at
    /// the front (see [`Stack::shares`]); `None` for a7m paint, which
    /// stacks by its material set's palette, outside the paint.
    pub fn shares(&self, cover: &mut [(u8, f32)]) -> Option<usize> {
        match self {
            Paint::A7m(_) => None,
            Paint::Layers(l) => Some(l.registry.stack.shares(cover)),
            Paint::Own(o) => Some(o.0.shares(cover)),
        }
    }

    /// How texel (x, y) shows: its cover laid over each other as the
    /// ground draws it, with `palette` the stack of an a7m island's
    /// material set.
    pub fn shown(&self, x: usize, y: usize, palette: &Stack) -> Cover {
        let mut cover = self.cover(x, y);
        let n = match self.shares(&mut cover) {
            Some(n) => n,
            None => palette.shares(&mut cover),
        };
        cover.n = n;
        cover
    }

    /// What texel (x, y) is painted with; see [`Cover`].
    pub fn cover(&self, x: usize, y: usize) -> Cover {
        match self {
            Paint::A7m(a) => {
                let mut out = Cover::default();
                let (ids, weights) = a.slots(x, y);
                for (id, w) in ids.into_iter().zip(weights).filter(|e| e.1 > 0) {
                    out.add(id, f32::from(w) / 31.);
                }
                out
            }
            Paint::Layers(l) => l.cover(x, y),
            Paint::Own(o) => o.0.cover(x, y),
        }
    }

    // Each chunk's allocation and bytes, for memory diagnostics.
    fn allocations(&self) -> Box<dyn Iterator<Item = (usize, usize)> + '_> {
        match self {
            Paint::A7m(a) => Box::new(
                [&a.ids, &a.alphas]
                    .into_iter()
                    .flat_map(|l| &l.chunks)
                    .map(|c| (Arc::as_ptr(c) as usize, c.len() * 4)),
            ),
            Paint::Layers(l) => Box::new(l.allocations()),
            Paint::Own(o) => Box::new(std::iter::once((
                Arc::as_ptr(&o.0) as *const () as usize,
                o.0.bytes(),
            ))),
        }
    }

    /// Whether `other` has this form and this form's map sizes, so the two
    /// compare texel by texel.
    fn same_shape(&self, other: &Paint) -> bool {
        match (self, other) {
            (Paint::A7m(a), Paint::A7m(b)) => {
                a.ids.size() == b.ids.size() && a.alphas.size() == b.alphas.size()
            }
            (Paint::Layers(a), Paint::Layers(b)) => a.chunks == b.chunks,
            (Paint::Own(a), Paint::Own(b)) => a.0.same_shape(&*b.0),
            _ => false,
        }
    }

    /// The texels (half-open) where `other`, of the same shape, differs.
    /// Forms that differ have no texels in common: `same_shape` tells them
    /// apart first.
    fn changed(&self, other: &Paint) -> Option<Rect> {
        match (self, other) {
            (Paint::A7m(a), Paint::A7m(b)) => a.changed(b),
            (Paint::Layers(a), Paint::Layers(b)) => a.changed(b),
            (Paint::Own(a), Paint::Own(b)) => a.0.changed(&*b.0),
            _ => None,
        }
    }
}

/// The most entries a [`Cover`] holds: a 2070 chunk's 24 layers, above an
/// a7m texel's six slots.
const COVER_MAX: usize = isd::Terrain::MAX_LAYERS;

/// A texel's paint as (id, coverage 0..1) in the form's own order, before
/// any stacking: a repeated id is listed once, where it first appears, with
/// its coverages summed (so it can pass 1), and ids with no coverage are
/// left out. Readers stack it the game's way (`Stack::shares` in the
/// palette) or by their own rule (scatter).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cover {
    n: usize,
    entries: [(u8, f32); COVER_MAX],
}

impl Default for Cover {
    fn default() -> Self {
        Self {
            n: 0,
            entries: [(0, 0.); COVER_MAX],
        }
    }
}

impl Cover {
    /// Adds `coverage` of `id`, summed with any it holds.
    pub fn add(&mut self, id: u8, coverage: f32) {
        match self.entries[..self.n].iter_mut().find(|e| e.0 == id) {
            Some(e) => e.1 += coverage,
            None => {
                self.entries[self.n] = (id, coverage);
                self.n += 1;
            }
        }
    }
}

impl std::ops::Deref for Cover {
    type Target = [(u8, f32)];
    fn deref(&self) -> &[(u8, f32)] {
        &self.entries[..self.n]
    }
}

impl std::ops::DerefMut for Cover {
    fn deref_mut(&mut self) -> &mut [(u8, f32)] {
        &mut self.entries[..self.n]
    }
}

/// A material set's draw order and each id's sharpness: what laying paint
/// over itself needs, without the textures. Derefs to the ids, bottom to
/// top. The game bakes ground one pass per material in set file order,
/// each alpha-blended over the ones before (`terrain_baking.fx`,
/// `BlendSplat`), so a texel's weights are coverages, not shares: the
/// latest full-weight material hides the rest. 2070's ground stacks the
/// same way, by `SplatID` ([`Registry`]).
#[derive(Clone)]
pub struct Stack {
    ids: Vec<u8>,
    /// Per id: rank in `ids` and sharpness. Any byte, since 2070 ids reach
    /// 108; the a7m `& 31` mask stays with the a7m slots ([`Stack::visible`]).
    layers: Box<[Option<(u8, f32)>; 256]>,
}

impl Default for Stack {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            layers: Box::new([None; 256]),
        }
    }
}

impl std::ops::Deref for Stack {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.ids
    }
}

impl Stack {
    /// Lays `id` over every id already in the stack. `sharpness` is the
    /// material's `AlphaSharpness`: how far its diffuse alpha (a height)
    /// cuts into its coverage.
    pub fn push(&mut self, id: u8, sharpness: f32) {
        self.layers[usize::from(id)] = Some((self.ids.len() as u8, sharpness.max(0.)));
        self.ids.push(id);
    }

    /// Gives `id` a new sharpness in its place in the stack.
    pub fn set_sharpness(&mut self, id: u8, sharpness: f32) {
        if let Some(l) = &mut self.layers[usize::from(id)] {
            l.1 = sharpness.max(0.);
        }
    }

    /// Takes `id` out; the ones above it move down a rank.
    pub fn remove(&mut self, id: u8) {
        self.ids.retain(|&s| s != id);
        let old = std::mem::replace(&mut self.layers, Box::new([None; 256]));
        for (rank, &s) in self.ids.iter().enumerate() {
            let sharpness = old[usize::from(s)].map_or(0., |l| l.1);
            self.layers[usize::from(s)] = Some((rank as u8, sharpness));
        }
    }

    /// Rank of `id`, `None` when the set has no such material.
    pub fn rank(&self, id: u8) -> Option<u8> {
        self.layers[usize::from(id)].map(|l| l.0)
    }

    /// How much of an a7m texel each of its six slots shows once the game
    /// has laid them over each other.
    pub fn visible(&self, ids: [u8; 6], weights: [u8; 6]) -> [f32; 6] {
        let mut cover = [(0u8, 0f32); 6];
        for k in 0..6 {
            cover[k] = (ids[k] & 31, f32::from(weights[k]) / 31.);
        }
        let n = self.shares(&mut cover);
        let mut out = [0f32; 6];
        for &(id, v) in &cover[..n] {
            // A repeated id shows once, in its first slot.
            if let Some(k) = (0..6).find(|&k| ids[k] & 31 == id && weights[k] > 0) {
                out[k] = v;
            }
        }
        out
    }

    /// Turns (id, coverage 0..1) into (id, share shown) in place, bottom to
    /// top, and returns how many entries remain at the front. Each material
    /// is laid over the ones below with alpha
    /// `saturate(s·(a − 1) + (s + 1)·coverage)`, the diffuse alpha `a` taken
    /// as a mid 0.5; the lowest is opaque. Repeats merge; ids outside the
    /// set drop.
    pub fn shares(&self, cover: &mut [(u8, f32)]) -> usize {
        let key = |c: &(u8, f32)| match self.rank(c.0) {
            Some(rank) if c.1 > 0. => u16::from(rank),
            _ => u16::MAX,
        };
        cover.sort_unstable_by_key(key);
        let mut n = 0;
        for i in 0..cover.len() {
            if key(&cover[i]) == u16::MAX {
                break;
            }
            if n > 0 && cover[n - 1].0 == cover[i].0 {
                cover[n - 1].1 += cover[i].1;
            } else {
                cover[n] = cover[i];
                n += 1;
            }
        }
        for i in 0..n {
            let (_, s) = self.layers[usize::from(cover[i].0)].expect("ranked");
            let alpha = if i == 0 {
                1.
            } else {
                (-0.5 * s + (s + 1.) * cover[i].1).clamp(0., 1.)
            };
            for c in &mut cover[..i] {
                c.1 *= 1. - alpha;
            }
            cover[i].1 = alpha;
        }
        n
    }

    /// The coverage, 0 to 1, that lays material `id` over what is below
    /// at `alpha`, inverting [`Stack::shares`]. Below `alpha` 1 a sharp
    /// material's coverage sits in a narrow band: at 2070's 5, between 5/12
    /// and 7/12.
    pub fn coverage(&self, id: u8, alpha: f32) -> f32 {
        let s = self.layers[usize::from(id)].map_or(0., |l| l.1);
        if alpha >= 0.999 {
            return 1.;
        }
        if alpha <= 0. {
            return 0.;
        }
        ((alpha + 0.5 * s) / (s + 1.)).clamp(0., 1.)
    }

    /// [`Stack::coverage`] as an a7m slot's 5-bit weight.
    pub fn coverage_for(&self, id: u8, alpha: f32) -> u8 {
        (self.coverage(id, alpha) * 31.).round() as u8
    }
}

#[derive(Clone)]
pub struct Materials {
    pub paint: Paint,
    /// Finest-map texels per metre.
    pub per_metre: f32,
    /// Shipped DDS files per level, (matid, matalpha); saves keep headers.
    /// Level 0 keeps only its header and what follows its words
    /// (`templates` puts `original`'s words back): they were 180 MB on the
    /// continental island.
    templates: Arc<Vec<(Template, Template)>>,
    /// The paint as loaded: a save that finds it unchanged hands back the
    /// shipped mip chain.
    original: Paint,
}

/// A shipped material level file: its bytes, or, when the game's archives
/// serve it, its path to read again from them on save. Levels 1 to 3 held
/// 59 MB on the continental island for saves that change nothing.
#[derive(Clone)]
enum Template {
    Bytes(Vec<u8>),
    /// Read with `Install::read_file`, which skips the mounted mod: a save
    /// mounts its own files over these.
    Archived(String),
}

/// Where two material maps differ (`Materials::changed`).
pub enum MaterialChange {
    Nothing,
    /// Island metres x0, z0, x1, z1 covering every changed alpha texel and
    /// id block.
    Area([f32; 4]),
    /// Of another size: nothing to compare word by word.
    Whole,
}

impl Materials {
    /// Each chunk of the maps and their originals as (allocation, bytes),
    /// then the shipped levels': for memory diagnostics that count what
    /// undo steps share, such as the shell's undo-bytes baseline.
    pub fn chunks(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let levels = (self.templates.iter())
            .flat_map(|(a, b)| [a, b])
            .map(|t| match t {
                Template::Bytes(b) => b.len(),
                Template::Archived(p) => p.len(),
            })
            .sum();
        (self.paint.allocations())
            .chain(self.original.allocations())
            .chain([(Arc::as_ptr(&self.templates) as usize, levels)])
    }

    /// Where `other`, the same island's maps at another undo step, differs
    /// from these.
    pub fn changed(&self, other: &Materials) -> MaterialChange {
        if !self.paint.same_shape(&other.paint) || self.per_metre != other.per_metre {
            return MaterialChange::Whole;
        }
        let Some(rect) = self.paint.changed(&other.paint) else {
            return MaterialChange::Nothing;
        };
        let pm = self.per_metre;
        MaterialChange::Area([
            rect.x0 as f32 / pm,
            rect.z0 as f32 / pm,
            rect.x1 as f32 / pm,
            rect.z1 as f32 / pm,
        ])
    }

    /// Paint with no shipped material files behind it: saves write it into
    /// the island's own maps.
    pub fn from_paint(paint: Paint, per_metre: f32) -> Self {
        Self {
            original: paint.clone(),
            paint,
            per_metre,
            templates: Arc::new(Vec::new()),
        }
    }

    fn embedded(island: &island::Island) -> Self {
        let paint = Paint::A7m(A7m {
            ids: Layer::new(island.material_ids()),
            alphas: Layer::new(island.material_alphas()),
        });
        let per_metre = island.material_alphas().width as f32 / island.world_size()[0] as f32;
        Self::from_paint(paint, per_metre)
    }

    /// The `matid.dds` / `matalpha.dds` levels next to the island archive:
    /// 0..3 on most islands, fewer on the 64 m deco ones. Saves write back
    /// as many levels as shipped.
    /// With them, level 0 as the plain grids it decoded (ids, alphas) when
    /// they come from files: the island attaches them and the ground bakes
    /// from them, where each copy back out of the chunks cost 50 ms.
    pub fn load_level0(
        material_files: bool,
        install: &Install,
        path: &str,
        island: &island::Island,
    ) -> Result<Self> {
        if !material_files {
            return Ok(Self::embedded(island));
        }
        let base = island_data_dir(path)?;
        let file = |level: usize, name: &str| format!("{base}/materialmaps/{level}/{name}.dds");
        // Both level 0 maps at once, each straight into its chunks
        // (`Layer::read`); the splat, the bake and the sinking read them in
        // place (`terrain::MaterialWords`).
        let (ids, alphas) = std::thread::scope(|s| {
            let ids = s.spawn(|| Layer::read(install, &file(0, "matid")));
            let alphas = Layer::read(install, &file(0, "matalpha"));
            (ids.join().expect("material ids"), alphas)
        });
        let ((ids, id_frame), (alphas, alpha_frame)) = (ids?, alphas?);
        // A level read from the archives is only noted; one a mod serves
        // (a project's own island) is kept, since the next save overwrites it.
        let template = |path: String| -> Option<Template> {
            if install.mod_layer(&path).is_none() && install.file(&path).is_some() {
                return Some(Template::Archived(path));
            }
            install.read(&path).ok().map(Template::Bytes)
        };
        let mut finer = Vec::new();
        for level in 1..4 {
            match (
                template(file(level, "matid")),
                template(file(level, "matalpha")),
            ) {
                (Some(ids), Some(alphas)) => finer.push((ids, alphas)),
                _ => break,
            }
        }
        let mut templates = vec![(Template::Bytes(id_frame), Template::Bytes(alpha_frame))];
        templates.extend(finer);
        let paint = Paint::A7m(A7m { ids, alphas });
        Ok(Self {
            original: paint.clone(),
            paint,
            per_metre: 0.,
            templates: Arc::new(templates),
        })
    }

    /// The shipped files of every level, level 0 rebuilt from its frame.
    fn templates(&self, install: Option<&Install>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let bytes = |t: &Template| match t {
            Template::Bytes(b) => Ok(b.clone()),
            Template::Archived(path) => {
                let install = install.context("no install to read shipped material levels")?;
                let id = (install.file(path))
                    .with_context(|| format!("{path}: no longer in the game's archives"))?;
                install.read_file(id)
            }
        };
        let mut out = (self.templates.iter())
            .map(|(ids, alphas)| Ok((bytes(ids)?, bytes(alphas)?)))
            .collect::<Result<Vec<_>>>()?;
        let unframe = |frame: &mut Vec<u8>, layer: &Layer| {
            let words = layer.grid().data;
            let bytes = words.iter().flat_map(|w| w.to_le_bytes());
            frame.splice(
                island::MaterialMap::HEADER..island::MaterialMap::HEADER,
                bytes,
            );
        };
        // Only a7m paint ships level files.
        if let (Paint::A7m(original), Some((ids, alphas))) = (&self.original, out.first_mut()) {
            unframe(ids, &original.ids);
            unframe(alphas, &original.alphas);
        }
        Ok(out)
    }

    // The a7m texel a world position lies in, or the 2070 vertex nearest it.
    fn texel(&self, p: [f32; 2]) -> (usize, usize) {
        let [w, h] = self.paint.size();
        let round = self.paint.corners();
        let clamp = |v: f32, n: usize| {
            let v = v * self.per_metre;
            ((if round { v.round() } else { v }).max(0.) as usize).min(n - 1)
        };
        (clamp(p[0], w), clamp(p[1], h))
    }

    /// The id with the most weight at a world position.
    pub fn dominant(&self, p: [f32; 2]) -> u8 {
        let (x, y) = self.texel(p);
        self.paint.dominant(x, y)
    }

    /// What the texel at a world position is painted with.
    pub fn cover(&self, p: [f32; 2]) -> Cover {
        let (x, y) = self.texel(p);
        self.paint.cover(x, y)
    }

    /// Paints one dab.
    pub fn paint(&mut self, center: [f32; 2], brush: &Brush, id: u8) -> bool {
        let r = brush.radius;
        let lo = [center[0] - r, center[1] - r];
        let hi = [center[0] + r, center[1] + r];
        let strength = brush.strength.min(1.);
        self.paint_with(lo, hi, id, |p| {
            strength * brush.weight((p[0] - center[0]).hypot(p[1] - center[1]))
        })
    }

    /// Paints `id` over the rectangle `lo`..`hi` (metres) at `weight` of
    /// each texel's centre, 0 leaving it alone. Returns whether anything
    /// changed.
    pub fn paint_with(
        &mut self,
        lo: [f32; 2],
        hi: [f32; 2],
        id: u8,
        weight: impl Fn([f32; 2]) -> f32 + Sync,
    ) -> bool {
        match &mut self.paint {
            Paint::A7m(a) => a.paint_with(self.per_metre, lo, hi, id, weight),
            Paint::Layers(l) => l.paint_with(lo, hi, id, weight),
            Paint::Own(o) => o.form_mut().paint_with(self.per_metre, lo, hi, id, &weight),
        }
    }

    /// How many 2070 vertices painting skipped since the last call, each
    /// where a chunk already held 24 layers and lacked the id: the content
    /// limit, which a stroke reports once. Always 0 for a7m paint.
    pub fn take_skipped(&mut self) -> usize {
        match &mut self.paint {
            Paint::A7m(_) | Paint::Own(_) => 0,
            Paint::Layers(l) => std::mem::take(&mut l.skipped),
        }
    }
}

/// `data/.../<name>/<name>.a7m` -> `data/.../<name>/_gamedata/<name>`.
pub fn island_data_dir(path: &str) -> Result<String> {
    let (dir, file) = path.rsplit_once('/').context("island path has a folder")?;
    let name = file.rsplit_once('.').map_or(file, |(n, _)| n);
    Ok(format!("{dir}/_gamedata/{name}"))
}

impl Doc {
    /// 2070's blocker grids; `None` for islands of games without them.
    pub fn blockers(&self) -> Option<&Blockers> {
        match &self.materials.as_ref()?.paint {
            Paint::Layers(l) => l.blockers.as_deref(),
            Paint::A7m(_) | Paint::Own(_) => None,
        }
    }

    /// The blocker grids to edit, copied for this step if another undo
    /// step shares them (only the list; see [`Blockers`]).
    pub fn blockers_mut(&mut self) -> Option<&mut Blockers> {
        match &mut self.materials.as_mut()?.paint {
            Paint::Layers(l) => l.blockers.as_mut().map(Arc::make_mut),
            Paint::A7m(_) | Paint::Own(_) => None,
        }
    }

    /// The island's prop file index of `path`, adding it to the file list
    /// when the island has never used that model.
    pub fn mesh_index(&mut self, path: &str) -> u32 {
        let index = match self
            .meshes
            .iter()
            .position(|m| m.eq_ignore_ascii_case(path))
        {
            Some(i) => i,
            None => {
                Arc::make_mut(&mut self.meshes).push(path.to_owned());
                self.meshes.len() - 1
            }
        };
        index as u32
    }

    pub fn from_island(island: &island::Island, materials: Option<Materials>) -> Result<Self> {
        let heights = island.heights();
        let [size_x, _] = island.world_size();
        ensure!(
            heights.width > 1 && heights.height > 1,
            "island has no height grid"
        );
        let grid = Grid {
            width: heights.width,
            depth: heights.height,
            spacing: size_x as f32 / (heights.width - 1) as f32,
            origin: [0., 0.],
        };
        let mut field = Heightfield::new(grid, heights.data.clone());
        // rd3d stores i16 in 1/64 m.
        field.limits = [
            f32::from(i16::MIN) / island::Island::HEIGHT_UNITS_PER_METRE,
            f32::from(i16::MAX) / island::Island::HEIGHT_UNITS_PER_METRE,
        ];
        let materials = materials.map(|mut m| {
            m.per_metre = m.paint.size()[0] as f32 / size_x as f32;
            m
        });
        let props: Vec<Prop> = island
            .props()
            .iter()
            .enumerate()
            .map(|(i, p)| Prop {
                id: i as u64 + 1,
                matrix: trs(p.position, p.rotation, p.scale),
                source: p.clone(),
                auto_cliff: false,
            })
            .collect();
        let field = Arc::new(field);
        Ok(Self {
            land_heights: field.clone(),
            field,
            materials,
            meshes: Arc::new(island.prop_files().to_vec()),
            props: props.into(),
            objects: Arc::default(),
            grids: None,
            tint: None,
            session: SessionSettings::read(island.gamedata())?,
            heights_patch: None,
        })
    }

    /// Stores this snapshot's heights as a patch against `next`, the
    /// heights of the step undo or redo restores it from.
    pub fn patch_heights(&mut self, next: &Arc<Heightfield>) {
        if self.heights_patch.is_some() || self.field.grid != next.grid {
            return;
        }
        let g = self.field.grid;
        let rect = changed_samples(g, &self.field.heights, &next.heights).unwrap_or_default();
        let mut values = Vec::with_capacity((rect.x1 - rect.x0) * (rect.z1 - rect.z0));
        for z in rect.z0..rect.z1 {
            values.extend_from_slice(
                &self.field.heights[z * g.width + rect.x0..z * g.width + rect.x1],
            );
        }
        let land = Arc::ptr_eq(&self.land_heights, &self.field);
        let empty = Arc::new(Heightfield {
            grid: g,
            heights: Vec::new(),
            limits: self.field.limits,
        });
        if land {
            self.land_heights = empty.clone();
        }
        self.field = empty;
        self.heights_patch = Some(Arc::new(HeightsPatch { rect, values, land }));
    }

    /// A patched snapshot's heights back from `next`'s.
    pub fn unpatch_heights(&mut self, next: &Heightfield) {
        let Some(patch) = self.heights_patch.take() else {
            return;
        };
        let mut field = next.clone();
        let (r, w) = (patch.rect, field.grid.width);
        if r.x1 > r.x0 {
            for (z, row) in (r.z0..r.z1).zip(patch.values.chunks(r.x1 - r.x0)) {
                field.heights[z * w + r.x0..z * w + r.x1].copy_from_slice(row);
            }
        }
        let field = Arc::new(field);
        if patch.land {
            self.land_heights = field.clone();
        }
        self.field = field;
    }

    /// Every prop and object with its world position.
    pub fn items(&self) -> impl Iterator<Item = (Item, [f32; 3])> + '_ {
        let props = self
            .props
            .iter()
            .map(|p| (Item::Prop(p.id), math::translation(&p.matrix)));
        let objects = self
            .objects
            .iter()
            .map(|o| (Item::Object(o.id), o.position));
        props.chain(objects)
    }

    /// The prop with `id` (`Props::position`).
    pub fn prop(&self, id: u64) -> Option<&Prop> {
        self.props.get(self.props.position(id)?)
    }

    /// Copies only the run of props holding it (`Props`).
    pub fn prop_mut(&mut self, id: u64) -> Option<&mut Prop> {
        let i = self.props.position(id)?;
        self.props.get_mut(i)
    }

    pub fn matrix(&self, item: Item) -> Option<Mat4> {
        match item {
            Item::Prop(id) => self.prop(id).map(|p| p.matrix),
            Item::Object(id) => self
                .objects
                .iter()
                .find(|o| o.id == id)
                .map(|o| trs(o.position, o.rotation, o.scale)),
        }
    }

    pub fn set_matrix(&mut self, item: Item, m: Mat4) {
        match item {
            Item::Prop(id) => {
                if let Some(p) = self.prop_mut(id) {
                    p.matrix = m;
                }
            }
            Item::Object(id) => {
                if let Some(o) = Arc::make_mut(&mut self.objects)
                    .iter_mut()
                    .find(|o| o.id == id)
                {
                    let (t, q, s) = decompose(&m);
                    // Curves move with their object; their first point is
                    // the position, and heights stay on the waterline.
                    let delta = [t[0] - o.position[0], t[2] - o.position[2]];
                    for p in o.path.iter_mut().flatten() {
                        p.p[0] += delta[0];
                        p.p[2] += delta[1];
                    }
                    o.position = t;
                    o.rotation = q;
                    o.scale = s;
                    // Shipped yaw-only objects store q = (0, -sin(d/2), 0,
                    // cos(d/2)), which turns local X to (cos d, 0, sin d).
                    o.direction = m[2].atan2(m[0]);
                }
            }
        }
    }

    /// Writes heights, materials and props into the island model. Returns
    /// the material DDS files to stage, by level.
    /// `install` reads shipped material levels again (`Template`); islands
    /// without material files need none.
    pub fn write_into(
        &self,
        island: &mut island::Island,
        install: Option<&Install>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        island.heights_mut().data.clone_from(&self.field.heights);
        // Props index `meshes`; models added from the library are new to the
        // island's file list, which appends them.
        let props: Vec<island::Prop> = self
            .props
            .iter()
            .map(|p| {
                let (position, rotation, scale) = decompose(&p.matrix);
                let file = self
                    .meshes
                    .get(p.source.file as usize)
                    .map_or(p.source.file, |path| island.prop_file_index(path));
                island::Prop {
                    position,
                    rotation,
                    scale,
                    file,
                    ..p.source.clone()
                }
            })
            .collect();
        *island.props_mut() = props;
        self.session.write(island.gamedata_mut())?;
        let Some(m) = &self.materials else {
            return Ok(Vec::new());
        };
        let (Paint::A7m(paint), Paint::A7m(original)) = (&m.paint, &m.original) else {
            anyhow::bail!("2070 paint saves through its game's codec, not into an a7m island");
        };
        if m.templates.is_empty() {
            *island.material_ids_mut() = paint.ids.grid();
            *island.material_alphas_mut() = paint.alphas.grid();
            return Ok(Vec::new());
        }
        // Undoing a paint edit restores the original mip chain exactly,
        // rather than replacing it with our approximate downsampling.
        if m.paint == m.original {
            return m.templates(install);
        }
        // Attached here rather than on load, where it held 360 MB with its
        // copy: the shipped layer, which saves compare against, then the
        // edits over it.
        if island.full_material_ids().is_none() {
            island.attach_full_material_map(original.ids.grid(), original.alphas.grid())?;
        }
        *island.full_material_ids_mut().expect("attached above") = paint.ids.grid();
        *island.full_material_alphas_mut().expect("attached above") = paint.alphas.grid();
        let levels = island
            .sync_material_levels()
            .context("material level 0 is attached")?;
        levels
            .iter()
            .zip(m.templates(install)?)
            .map(|((ids, alphas), (tid, talpha))| {
                Ok((
                    island::MaterialMap::encode(&tid, ids)?,
                    island::MaterialMap::encode(&talpha, alphas)?,
                ))
            })
            .collect()
    }
}

/// The island being edited: its document, and what tools and conversion
/// read and write beyond what an undo step restores. The editor holds one;
/// conversion reads one island and builds another.
// The object store maps 117's and 1800's gamedata records. 2070's objects
// carry their `.isd` record (`objects::Record`) and anno2070 writes them
// from `native`, with no store.
pub struct Island {
    pub doc: Doc,
    /// Decoded object areas, kept for writing `doc.objects` back on save.
    /// New objects take their ids from it (`with_store`).
    pub object_store: Option<Objects>,
    /// Shipped records the harbour and slot tools copy; loaded on first use.
    pub harbour: Option<Arc<Library>>,
    /// The harbour tool places the island's start coast instead.
    pub start_coast: bool,
    /// What the slot tool places.
    pub resource: Resource,
    /// The slot tool places always-active (fixed) slots, not random ones.
    pub fixed_slots: bool,
    /// The id the next new prop gets.
    pub next_id: u64,
    /// The material-set file the island names on save. Removal and import
    /// change it here; saving writes it into the save copy, so the codec
    /// source keeps the name it loaded with.
    pub material_set: String,
    /// The island's own material set once edited: saved under the path
    /// `material_set` names.
    pub own_set: Option<String>,
    /// The island as its codec loaded it, which a save writes the
    /// document's edits into, so all the document doesn't hold is carried
    /// byte for byte. Outside undo; `None` for an island built in memory.
    pub native: Option<Native>,
    /// The models the island's file held when read (`set_native`): every
    /// game ships islands naming models it lacks and plays them, so the
    /// level-5 checks count only models the editor added.
    pub loaded_models: Arc<Vec<String>>,
    /// Records other islands of the install ship, one per asset, each a
    /// one-record document (as `Record::Foreign` holds): what a new tree,
    /// rock or grass of a kind the `native` file holds no record to copy
    /// for copies when it saves (2070 islands without trees or rocks).
    /// Conversion and Place add them; outside undo.
    pub foreign: Vec<Arc<[u8]>>,
}

/// An island file as its game's codec loaded it (`Island::native`).
pub enum Native {
    /// 117's and 1800's a7m tree, parked after load (`Island::park` in
    /// formats): a save unparks a copy and writes into that.
    A7m(anno_formats::a7m::island::Island),
    /// The bytes of a file the game's own codec parses again at save
    /// (2070's `.isd`, `Islands::write`).
    File(Arc<[u8]>),
}

impl Native {
    pub fn a7m(&self) -> Option<&anno_formats::a7m::island::Island> {
        match self {
            Native::A7m(tree) => Some(tree),
            Native::File(_) => None,
        }
    }

    pub fn file(&self) -> Option<&Arc<[u8]>> {
        match self {
            Native::File(bytes) => Some(bytes),
            Native::A7m(_) => None,
        }
    }
}

impl Island {
    /// A loaded island: `doc` with the object store it was decoded from and
    /// the material set its file names. No harbour records yet; the slot
    /// tool starts on random mine slots.
    pub fn new(doc: Doc, object_store: Option<Objects>, material_set: String) -> Self {
        Self {
            next_id: doc.props.len() as u64 + 1,
            doc,
            object_store,
            harbour: None,
            start_coast: false,
            resource: Resource::Mine,
            fixed_slots: false,
            material_set,
            own_set: None,
            native: None,
            loaded_models: Arc::default(),
            foreign: Vec::new(),
        }
    }

    /// The file the island was read from, and with it the models it
    /// placed: the document's as they stand now.
    pub fn set_native(&mut self, native: Native) {
        self.native = Some(native);
        self.loaded_models = self.doc.meshes.clone();
    }

    /// The a7m tree the island was read from; `None` for one another codec
    /// reads (2070's) or built in memory.
    pub fn a7m(&self) -> Option<&anno_formats::a7m::island::Island> {
        self.native.as_ref()?.a7m()
    }

    pub fn a7m_mut(&mut self) -> Option<&mut anno_formats::a7m::island::Island> {
        match self.native.as_mut()? {
            Native::A7m(tree) => Some(tree),
            Native::File(_) => None,
        }
    }

    /// Whether a game's own codec saves this island from its file (2070),
    /// copying records for new objects, rather than the a7m writers.
    pub fn saves_through_codec(&self) -> bool {
        matches!(self.native, Some(Native::File(_)))
    }

    /// Runs `f` with the object store holding the document's objects and
    /// the area grid; the store is restored after, so ids `f` hands out
    /// stay unique against the document. `None` without object areas.
    pub fn with_store<T>(
        &mut self,
        f: impl FnOnce(&mut Objects, Option<&GridLayer>) -> T,
    ) -> Option<T> {
        let mut store = self.object_store.take()?;
        store.objects.clone_from(&self.doc.objects);
        let grids = self.doc.grids.clone();
        let out = f(
            &mut store,
            grids
                .as_ref()
                .and_then(|g| g.layer(anno_formats::a7m::grids::Kind::AreaIds)),
        );
        store.objects.clone_from(&self.doc.objects);
        self.object_store = Some(store);
        Some(out)
    }

    /// Adds a harbour along `curve`, or the start coast with `start_coast`
    /// (replacing the island's one), and returns its id. The pad's areas
    /// are the caller's to derive: the shell's harbour geometry draws them.
    pub fn add_harbour(&mut self, curve: Vec<PathPoint>) -> Result<u64, Unplaced> {
        let lib = self.harbour.clone().ok_or(Unplaced::NoRecords)?;
        let piece = if self.start_coast {
            Piece::StartCoast
        } else {
            Piece::Coast
        };
        let guid = lib.guid(piece).ok_or(Unplaced::NoRecord)?;
        let placed = self.with_store(|store, areas| lib.place(store, piece, areas, curve[0].p));
        let mut o = placed
            .ok_or(Unplaced::NoObjectAreas)?
            .ok_or(Unplaced::NoRecord)?;
        o.area = CONSTRUCTION as u16;
        o.path = Some(curve);
        let objects = Arc::make_mut(&mut self.doc.objects);
        // One start coast per island.
        if piece == Piece::StartCoast {
            objects.retain(|x| x.guid != guid);
        }
        let id = o.id;
        objects.push(o);
        Ok(id)
    }

    /// Adds a slot of `resource` (fixed with `fixed_slots`, where the kind
    /// has a fixed variant) at `at`, turned to `yaw`, with its work areas
    /// laid by `harbours`' roads, and returns the slot's id.
    pub fn add_slot(
        &mut self,
        at: [f32; 3],
        yaw: Option<f32>,
        harbours: &dyn Harbours,
    ) -> Result<u64, Unplaced> {
        let lib = self.harbour.clone().ok_or(Unplaced::NoRecords)?;
        let (slot, work) = (self.resource).pieces(self.fixed_slots && self.resource.has_fixed());
        let field = self.doc.field.clone();
        let placed = self.with_store(|store, areas| {
            let mut s = lib.place(store, slot, areas, at)?;
            if let Some(yaw) = yaw {
                s.set_direction(yaw);
            }
            // The work areas' ids must not collide with the slot's.
            store.objects.push(s.clone());
            let ws = lib.place_work_areas(store, areas, &field, harbours, work, &s);
            Some((s, ws))
        });
        let (s, ws) = placed
            .ok_or(Unplaced::NoObjectAreas)?
            .ok_or(Unplaced::NoRecord)?;
        let id = s.id;
        let objects = Arc::make_mut(&mut self.doc.objects);
        objects.push(s);
        objects.extend(ws);
        Ok(id)
    }
}

/// How much a loss matters. What each level asks of the user is
/// [`POLICY`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Level {
    /// A look-alike swapped in, tint, scale, dropped paint.
    Look = 1,
    /// Unmatched props, effects, decals, animated parts.
    Decoration,
    /// Land cropped or resampled, river paths cropped.
    Shape,
    /// Slots, resources, harbours, island type or size class.
    Gameplay,
    /// A dangling reference or something the target game lacks.
    Broken,
}

/// What a loss asks before its island leaves the editor (exported, put on
/// a test map, tried in game).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Listed in the report.
    Report,
    /// Confirmed once.
    Confirm,
    /// Confirmed once per category: a newly lost kind asks again.
    ConfirmEach,
    /// Stops the island leaving; a save in its own game only warns, since
    /// saving is the only way to keep work.
    Block,
}

/// The loss policy, by [`Level`] in order: one table, so changing what a
/// level asks is a one-line edit (the levels and policies were decided
/// with the user).
pub const POLICY: [Policy; 5] = [
    Policy::Report,      // Look
    Policy::Report,      // Decoration
    Policy::Confirm,     // Shape
    Policy::ConfirmEach, // Gameplay
    Policy::Block,       // Broken
];

impl Level {
    pub fn policy(self) -> Policy {
        POLICY[self as usize - 1]
    }
}

/// One kind of loss on one island: its level, what it is in words, how
/// many items it takes, and which. Conversion lists levels 1 to 4 for what
/// it leaves out; `Islands::validate` lists level 5, what stops the island
/// leaving the editor.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Problem {
    pub level: Level,
    /// The category acceptances are keyed by, as the report says it:
    /// "Oil slots left out".
    pub what: String,
    pub count: usize,
    /// The ids of the objects or props it takes, sorted, where it has
    /// any: a source object's id for a conversion loss, so converting the
    /// same island again names the same items. Empty for a loss of the
    /// whole island (its paint, its kind).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<u64>,
    /// What `items` are. Islands' losses and checks leave it `Objects`,
    /// which their saved listings don't write.
    #[serde(default, skip_serializing_if = "Names::is_objects")]
    pub names: Names,
}

/// What a [`Problem`]'s items name: an island's objects, or a map check's
/// slots, variants or the whole map.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Names {
    /// Object or prop ids, or none for a loss of the whole island.
    #[default]
    Objects,
    /// A map's slots, each `(variant << 16) | slot` (`map::slot_item`).
    Slots,
    /// A map's variants, by index.
    Variants,
    /// The whole map: the setup options (`MapRules::setups` indices) none
    /// of its variants answers.
    Setups,
}

impl Names {
    fn is_objects(&self) -> bool {
        *self == Names::Objects
    }
}

impl Problem {
    /// "Start island without a harbour coast", or "3 × Objects of assets
    /// the install lacks": the count leads where there is more than one.
    pub fn said(&self) -> String {
        match self.count {
            0 | 1 => self.what.clone(),
            n => format!("{n} × {}", self.what),
        }
    }
}

/// A loss the user accepted for an island going to `game`: its category
/// and the items it took then. The same category asks again once it takes
/// an item the acceptance doesn't list (a newly lost slot).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Acceptance {
    pub game: crate::game::Game,
    pub what: String,
    /// Sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<u64>,
}

/// Whether `accepted` covers `problem` for `game`: same category, and
/// every item it takes listed.
fn covered(accepted: &[Acceptance], game: crate::game::Game, problem: &Problem) -> bool {
    accepted.iter().any(|a| {
        a.game == game
            && a.what == problem.what
            && problem
                .items
                .iter()
                .all(|i| a.items.binary_search(i).is_ok())
    })
}

/// What `problems`, a conversion into `game`, still ask of the user past
/// `accepted`, one question per entry as [`POLICY`] says: a `Confirm` level
/// asks once for all its problems, a `ConfirmEach` level once per
/// category. Reported losses ask nothing, and a blocking one isn't the
/// user's to accept (`Islands::validate` stops it instead).
pub fn asks<'a>(
    accepted: &[Acceptance],
    game: crate::game::Game,
    problems: &'a [Problem],
) -> Vec<Vec<&'a Problem>> {
    let open = |p: &&Problem| !covered(accepted, game, p);
    let mut out: Vec<Vec<&Problem>> = Vec::new();
    for level in [
        Level::Look,
        Level::Decoration,
        Level::Shape,
        Level::Gameplay,
        Level::Broken,
    ] {
        let of = problems.iter().filter(|p| p.level == level).filter(open);
        match level.policy() {
            Policy::Report | Policy::Block => {}
            Policy::Confirm => {
                let all: Vec<&Problem> = of.collect();
                if !all.is_empty() {
                    out.push(all);
                }
            }
            Policy::ConfirmEach => out.extend(of.map(|p| vec![p])),
        }
    }
    out
}

/// Records that the user accepted `problems` for `game`, adding their items
/// to the category's acceptance.
pub fn accept(accepted: &mut Vec<Acceptance>, game: crate::game::Game, problems: &[&Problem]) {
    for p in problems {
        let k = match accepted
            .iter()
            .position(|a| a.game == game && a.what == p.what)
        {
            Some(k) => k,
            None => {
                accepted.push(Acceptance {
                    game,
                    what: p.what.clone(),
                    items: Vec::new(),
                });
                accepted.len() - 1
            }
        };
        let items = &mut accepted[k].items;
        items.extend_from_slice(&p.items);
        items.sort_unstable();
        items.dedup();
    }
}

/// Why `Island::add_harbour` or `add_slot` placed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unplaced {
    /// No harbour records: still loading, or they failed to.
    NoRecords,
    /// The province ships no record of that piece.
    NoRecord,
    /// The island's objects did not decode, so nothing can take an id.
    NoObjectAreas,
}

/// Column-major TRS matrix from a stored (x, y, z, w) quaternion.
pub fn trs(t: [f32; 3], q: [f32; 4], s: [f32; 3]) -> Mat4 {
    let [x, y, z, w] = q;
    let r = [
        [
            1. - 2. * (y * y + z * z),
            2. * (x * y + z * w),
            2. * (x * z - y * w),
        ],
        [
            2. * (x * y - z * w),
            1. - 2. * (x * x + z * z),
            2. * (y * z + x * w),
        ],
        [
            2. * (x * z + y * w),
            2. * (y * z - x * w),
            1. - 2. * (x * x + y * y),
        ],
    ];
    [
        r[0][0] * s[0],
        r[0][1] * s[0],
        r[0][2] * s[0],
        0.,
        r[1][0] * s[1],
        r[1][1] * s[1],
        r[1][2] * s[1],
        0.,
        r[2][0] * s[2],
        r[2][1] * s[2],
        r[2][2] * s[2],
        0.,
        t[0],
        t[1],
        t[2],
        1.,
    ]
}

/// Inverse of [`trs`] for matrices without shear.
pub fn decompose(m: &Mat4) -> ([f32; 3], [f32; 4], [f32; 3]) {
    let col = |i: usize| [m[i * 4], m[i * 4 + 1], m[i * 4 + 2]];
    let s = [0, 1, 2].map(|i| math::length(col(i)).max(1e-8));
    let r = [0, 1, 2].map(|i| math::scale(col(i), 1. / s[i]));
    // Standard rotation-matrix-to-quaternion; r[c][row].
    let (m00, m11, m22) = (r[0][0], r[1][1], r[2][2]);
    let trace = m00 + m11 + m22;
    let q = if trace > 0. {
        let k = 0.5 / (trace + 1.).sqrt();
        [
            (r[1][2] - r[2][1]) * k,
            (r[2][0] - r[0][2]) * k,
            (r[0][1] - r[1][0]) * k,
            0.25 / k,
        ]
    } else if m00 > m11 && m00 > m22 {
        let k = 2. * (1. + m00 - m11 - m22).sqrt();
        [
            0.25 * k,
            (r[1][0] + r[0][1]) / k,
            (r[2][0] + r[0][2]) / k,
            (r[1][2] - r[2][1]) / k,
        ]
    } else if m11 > m22 {
        let k = 2. * (1. + m11 - m00 - m22).sqrt();
        [
            (r[1][0] + r[0][1]) / k,
            0.25 * k,
            (r[2][1] + r[1][2]) / k,
            (r[2][0] - r[0][2]) / k,
        ]
    } else {
        let k = 2. * (1. + m22 - m00 - m11).sqrt();
        [
            (r[2][0] + r[0][2]) / k,
            (r[2][1] + r[1][2]) / k,
            0.25 * k,
            (r[0][1] - r[1][0]) / k,
        ]
    };
    ([m[12], m[13], m[14]], q, s)
}

#[cfg(test)]
mod tests {
    use super::{
        A7m, Doc, Layer, MaterialChange, Materials, Paint, Stack, decompose, island,
        island_data_dir, trs,
    };
    use anno_formats::{a7m::island::Island, rda};
    use gpui_editor_kit::heightfield::{Brush, Sculpt};
    use std::sync::Arc;

    // Shape losses share one question, gameplay ones ask per category,
    // reported and blocking ones ask nothing; accepting covers exactly the
    // items accepted.
    #[test]
    fn losses_ask_as_the_policy_groups_them() {
        use super::{Level, Problem, accept, asks};
        use crate::game::Game;
        let p = |level, what: &str, items: &[u64]| Problem {
            level,
            what: what.into(),
            count: items.len().max(1),
            items: items.to_vec(),
            names: Default::default(),
        };
        let problems = [
            p(Level::Look, "Paint", &[]),
            p(Level::Decoration, "Props dropped", &[1]),
            p(Level::Shape, "Rivers cropped", &[2]),
            p(Level::Shape, "Lakes cropped", &[3]),
            p(Level::Gameplay, "Oil slots left out", &[4, 5]),
            p(Level::Gameplay, "Clay slots left out", &[6]),
            p(Level::Broken, "No buildable ground", &[]),
        ];
        let what = |q: Vec<Vec<&Problem>>| -> Vec<Vec<String>> {
            q.into_iter()
                .map(|q| q.into_iter().map(|p| p.what.clone()).collect())
                .collect()
        };
        assert_eq!(
            what(asks(&[], Game::Anno117, &problems)),
            [
                vec!["Rivers cropped".to_owned(), "Lakes cropped".to_owned()],
                vec!["Oil slots left out".to_owned()],
                vec!["Clay slots left out".to_owned()],
            ]
        );
        let mut accepted = Vec::new();
        accept(&mut accepted, Game::Anno117, &[&problems[4]]);
        let more = [p(Level::Gameplay, "Oil slots left out", &[4, 7])];
        assert_eq!(
            what(asks(&accepted, Game::Anno117, &problems[4..5])).len(),
            0
        );
        assert_eq!(what(asks(&accepted, Game::Anno117, &more)).len(), 1);
    }

    // The decided loss table: look and decoration are reported, shape is
    // confirmed, gameplay confirmed per category, and broken blocks.
    #[test]
    fn each_loss_level_asks_what_the_loss_table_says() {
        use super::{Level, Policy};
        let asks = [
            Level::Look,
            Level::Decoration,
            Level::Shape,
            Level::Gameplay,
            Level::Broken,
        ]
        .map(Level::policy);
        assert_eq!(
            asks,
            [
                Policy::Report,
                Policy::Report,
                Policy::Confirm,
                Policy::ConfirmEach,
                Policy::Block
            ]
        );
    }

    #[test]
    fn prop_transforms_survive_matrix_roundtrip() {
        let h = std::f32::consts::FRAC_1_SQRT_2;
        for q in [[0., 0., 0., 1.], [0., h, 0., h], [0.1, 0.7, -0.2, 0.67]] {
            let n = q.iter().map(|v: &f32| v * v).sum::<f32>().sqrt();
            let q = q.map(|v| v / n);
            let (t, r, s) = decompose(&trs([1., 2., 3.], q, [2., 0.5, 1.]));
            assert_eq!(t, [1., 2., 3.]);
            assert!(
                s.iter()
                    .zip([2., 0.5, 1.])
                    .all(|(a, b)| (a - b).abs() < 1e-5),
                "{s:?}"
            );
            // q and -q are the same rotation.
            let dot: f32 = r.iter().zip(q).map(|(a, b)| a * b).sum();
            assert!((dot.abs() - 1.).abs() < 1e-5, "{r:?} vs {q:?}");
        }
        assert_eq!(
            island_data_dir("data/a/isl/isl.a7m").unwrap(),
            "data/a/isl/_gamedata/isl"
        );
    }

    // Objects placed through the island's store take ids no object of the
    // document has, and the store comes back holding the document's objects,
    // not the ones handed out.
    #[test]
    fn ids_from_the_islands_store_stay_unique_against_its_document() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        // 117's mount order, as anno117 gives it: core's tests can't reach
        // the game crates.
        let install = anno_formats::vfs::Install::open(dir, &["data"]).unwrap();
        let path = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m";
        let archive = rda::Archive::parse(install.read(path).unwrap(), path).unwrap();
        let member = |n: &str| archive.read_path(n).unwrap();
        let source = Island::parse(&member("gamedata.data"), &member("rd3d.data")).unwrap();
        let store = super::Objects::read(source.gamedata()).unwrap();
        let mut doc = Doc::from_island(&source, None).unwrap();
        doc.objects = Arc::new(store.objects.clone());
        let mut island = super::Island::new(doc, Some(store), "set.xml".into());
        assert_eq!(island.next_id, island.doc.props.len() as u64 + 1);
        let template = island.object_store.as_ref().unwrap().template(0);
        let placed = island
            .with_store(|store, _| {
                let a = store.add_template(&template, 1, [1., 0., 1.]);
                store.objects.push(a.clone());
                let b = store.add_template(&template, 1, [2., 0., 2.]);
                [a, b]
            })
            .unwrap();
        assert_ne!(placed[0].id, placed[1].id);
        assert!(
            placed
                .iter()
                .all(|p| island.doc.objects.iter().all(|o| o.id != p.id))
        );
        let store = island.object_store.as_ref().unwrap();
        assert_eq!(store.objects, *island.doc.objects);
        island.object_store = None;
        assert!(island.with_store(|_, _| ()).is_none());
    }

    // Load an extracted island, sculpt and move a prop, write it, read it back.
    #[test]
    fn edits_survive_a_save_roundtrip() {
        // The island comes from the install, so the test runs wherever the other
        // install tests do and skips loudly elsewhere.
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        // 117's mount order, as anno117 gives it: core's tests can't reach
        // the game crates.
        let install = anno_formats::vfs::Install::open(dir, &["data"]).unwrap();
        let path = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m";
        let bytes = install.read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let archive = rda::Archive::parse(bytes, path).unwrap();
        let member = |n: &str| archive.read_path(n).unwrap();
        let mut source = Island::parse(&member("gamedata.data"), &member("rd3d.data")).unwrap();
        let mut doc = Doc::from_island(&source, None).unwrap();
        let before = doc.field.height([160., 160.]);
        let brush = Brush {
            radius: 10.,
            strength: 3.,
            ..Brush::default()
        };
        Arc::make_mut(&mut doc.field).sculpt([160., 160.], &brush, Sculpt::Raise);
        doc.props[0].matrix[12] += 5.;
        let moved = doc.props[0].matrix[12];
        assert!(
            doc.write_into(&mut source, None).unwrap().is_empty(),
            "no material maps attached"
        );
        let (gamedata, rd3d) = source.encode().unwrap();
        let reread = Doc::from_island(&Island::parse(&gamedata, &rd3d).unwrap(), None).unwrap();
        let after = reread.field.height([160., 160.]);
        assert!((after - before - 3.).abs() < 0.05, "{before} -> {after}");
        assert!((reread.props[0].matrix[12] - moved).abs() < 1e-3);
        assert_eq!(reread.props.len(), doc.props.len());
    }

    #[test]
    fn chunk_crossing_row_matches_the_flat_grid() {
        let grid = island::Grid {
            width: 537,
            height: 259,
            data: (0..537 * 259).collect(),
        };
        let layer = Layer::new(&grid);
        let mut words = Vec::new();
        for y in [255, 256, 258] {
            words.clear();
            layer.row_into(y, 253..537, &mut words);
            assert_eq!(words, grid.data[y * grid.width + 253..(y + 1) * grid.width]);
        }
    }

    #[test]
    fn undoing_a_sparse_sculpt_restores_the_heights_and_land_rectangle() {
        use gpui_editor_kit::heightfield::{Grid, Heightfield, Rect};

        let grid = Grid {
            width: 16,
            depth: 12,
            spacing: 1.,
            origin: [0.; 2],
        };
        let field = Arc::new(Heightfield::new(grid, vec![0.; grid.width * grid.depth]));
        let mut undo = Doc {
            field: field.clone(),
            land_heights: field.clone(),
            materials: None,
            meshes: Arc::default(),
            props: Default::default(),
            objects: Arc::default(),
            grids: None,
            tint: None,
            session: Default::default(),
            heights_patch: None,
        };
        let mut edited = field.clone();
        let brush = Brush {
            radius: 0.5,
            strength: 1.,
            ..Brush::default()
        };
        for p in [[8., 2.], [3., 7.]] {
            Arc::make_mut(&mut edited).sculpt(p, &brush, Sculpt::Raise);
        }
        undo.patch_heights(&edited);
        let rect = Rect {
            x0: 3,
            z0: 2,
            x1: 9,
            z1: 8,
        };
        assert_eq!(undo.heights_patch.as_ref().unwrap().rect, rect);
        undo.unpatch_heights(&edited);
        assert_eq!(undo.field.heights, field.heights);
        assert!(Arc::ptr_eq(&undo.land_heights, &undo.field));
        assert_eq!(
            super::changed_samples(grid, &edited.heights, &undo.land_heights.heights),
            Some(rect)
        );
    }

    /// Materials over a `w`×`h` alpha map at 2 texels per metre, every
    /// texel `base` at full weight.
    fn a7m_materials(w: usize, h: usize, base: u8) -> Materials {
        let grid = |w: usize, h: usize, word: u32| island::Grid {
            width: w,
            height: h,
            data: vec![word; w * h],
        };
        let paint = Paint::A7m(A7m {
            ids: Layer::new(&grid(
                w / 2,
                h / 2,
                island::pack_slots([base, 1, 2, 3, 4, 5]),
            )),
            alphas: Layer::new(&grid(w, h, island::pack_slots([31, 0, 0, 0, 0, 0]))),
        });
        Materials::from_paint(paint, 2.)
    }

    #[test]
    fn a_dab_shows_its_id_and_undo_finds_only_its_texels() {
        let before = a7m_materials(600, 520, 7);
        let mut after = before.clone();
        let brush = Brush {
            radius: 3.,
            strength: 1.,
            ..Brush::default()
        };
        assert!(after.paint([140., 130.], &brush, 9));
        assert_eq!(after.dominant([140., 130.]), 9);
        assert_eq!(after.dominant([100., 100.]), 7, "outside the dab");
        assert_eq!(
            before.dominant([140., 130.]),
            7,
            "the snapshot keeps its paint"
        );
        let MaterialChange::Area([x0, z0, x1, z1]) = after.changed(&before) else {
            panic!("a dab changes an area");
        };
        // The dab's footprint, block-aligned, and nothing near the map's edges.
        assert!(x0 <= 137. && z0 <= 127. && x1 >= 143. && z1 >= 133.);
        assert!(x0 >= 130. && z0 >= 120. && x1 <= 150. && z1 <= 140.);
        assert!(matches!(
            before.changed(&before.clone()),
            MaterialChange::Nothing
        ));
        assert!(matches!(
            a7m_materials(600, 512, 7).changed(&before),
            MaterialChange::Whole
        ));
    }

    #[test]
    fn a_cover_lists_each_painted_id_once_in_slot_order() {
        let mut m = a7m_materials(8, 8, 7);
        let Paint::A7m(a) = &mut m.paint else {
            panic!("a7m paint")
        };
        a.ids.set(0, 0, island::pack_slots([4, 9, 4, 2, 3, 5]));
        a.alphas
            .set(1, 1, island::pack_slots([31, 10, 20, 0, 3, 0]));
        let cover = m.cover([0.75, 0.75]);
        let expect = [(4, 51. / 31.), (9, 10. / 31.), (3, 3. / 31.)];
        assert_eq!(cover.len(), expect.len());
        for (got, want) in cover.iter().zip(expect) {
            assert_eq!(got.0, want.0);
            assert!((got.1 - want.1).abs() < 1e-6, "{got:?} vs {want:?}");
        }
        assert_eq!(&*m.cover([3., 3.]), &[(7, 1.)]);
    }

    #[test]
    fn a_stack_lays_coverage_by_rank_and_sharpness_and_inverts_it() {
        let mut stack = Stack::default();
        for (id, sharpness) in [(4, 1.), (9, 3.), (2, -1.)] {
            stack.push(id, sharpness);
        }
        assert_eq!(&*stack, &[4, 9, 2]);
        assert_eq!(
            [4, 9, 2, 7].map(|id| stack.rank(id)),
            [Some(0), Some(1), Some(2), None]
        );
        // Listed top first: 2 (sharpness 0) at 0.25 over 9 at 0.6 over 4,
        // which is opaque whatever its coverage. 7 is not in the set.
        let mut cover = [(2, 0.25), (7, 1.), (9, 0.6), (4, 0.1)];
        let n = stack.shares(&mut cover);
        let nine = (-0.5 * 3. + 4. * 0.6f32).clamp(0., 1.);
        let want = [(4, (1. - nine) * 0.75), (9, nine * 0.75), (2, 0.25)];
        assert_eq!(n, 3);
        for (got, want) in cover[..n].iter().zip(want) {
            assert_eq!(got.0, want.0);
            assert!((got.1 - want.1).abs() < 1e-6, "{got:?} vs {want:?}");
        }
        // The weight `coverage_for` gives lays 9 over 4 at the asked alpha.
        for alpha in [0.1, 0.5, 0.9] {
            let w = stack.coverage_for(9, alpha);
            let mut back = [(4, 1.), (9, f32::from(w) / 31.)];
            stack.shares(&mut back);
            assert!((back[1].1 - alpha).abs() <= 4. / 31., "{alpha}: {back:?}");
        }
        // A repeated id shows once, in its first slot with weight.
        let shown = stack.visible([4, 9, 0, 9, 3, 2], [31, 0, 31, 15, 31, 0]);
        assert_eq!(shown[1], 0.);
        assert!(shown[3] > 0. && shown[0] > 0.);
        assert_eq!(
            [shown[2], shown[4], shown[5]],
            [0.; 3],
            "ids outside the set"
        );
        // Removing an id moves the ones above it down and keeps sharpness.
        stack.remove(4);
        assert_eq!(&*stack, &[9, 2]);
        assert_eq!(stack.rank(9), Some(0));
        let mut again = [(9, 1.), (2, 0.25)];
        stack.shares(&mut again);
        assert!((again[1].1 - 0.25).abs() < 1e-6);
    }

    /// A registry of `(id, SplatID, colour)` textures; scatter ones have no
    /// diffuse map.
    fn registry(textures: &[(u8, u16, bool)]) -> Arc<super::Registry> {
        let xml: String = (textures.iter())
            .map(|&(id, rank, colour)| {
                let diffuse = if colour {
                    format!("<DiffuseTexture>t{id}_diff.dds</DiffuseTexture>")
                } else {
                    String::new()
                };
                format!("<Texture><GUID>{id}</GUID><SplatID>{rank}</SplatID>{diffuse}</Texture>")
            })
            .collect();
        let ground = anno_formats::isd::terrain::GroundTextures::parse(&xml).unwrap();
        Arc::new(super::Registry::new(ground, 4.).unwrap())
    }

    /// `w`×`h` chunks, each holding `layers(k)` as (id, value on every
    /// vertex), ascending.
    fn layers(
        w: usize,
        h: usize,
        registry: Arc<super::Registry>,
        layers: impl Fn(usize) -> Vec<(u8, u8)>,
    ) -> super::Layers {
        let mut out = super::Layers::new([w, h], registry);
        for k in 0..w * h {
            let l = layers(k);
            let ids: Vec<u8> = l.iter().map(|e| e.0).collect();
            let planes: Vec<u8> = l.iter().flat_map(|e| [e.1; 289]).collect();
            out.set(k, &ids, &planes).unwrap();
        }
        out
    }

    // The brush on 2070 paint follows `paint_material`: the painted layer
    // rises, colour ranked above it falls, and the base, scatter and colour
    // ranked below keep their coverage. A layer drawn to nothing leaves.
    #[test]
    fn a_layers_dab_raises_its_id_and_lowers_only_colour_ranked_above() {
        // Ranks are SplatIDs, not ids: 30 draws under 10, 20 over it.
        let reg = registry(&[
            (0, 0, true),
            (10, 5, true),
            (20, 9, true),
            (30, 2, true),
            (40, 7, false),
        ]);
        let mut paint = layers(1, 1, reg.clone(), |_| {
            vec![(0, 200), (10, 40), (20, 90), (30, 60), (40, 70)]
        });
        assert_eq!(paint.paint(8, 8, 10, 1.), Ok(true));
        let i = 8 * 17 + 8;
        let at = |p: &super::Layers, id: u8| {
            (p.ids(0).iter().position(|&x| x == id)).map(|l| p.plane(0, l)[i])
        };
        assert_eq!(at(&paint, 10), Some(255));
        assert_eq!(at(&paint, 20), Some(0), "colour ranked above falls");
        assert_eq!(
            [0, 30, 40].map(|id| at(&paint, id)),
            [Some(200), Some(60), Some(70)],
            "base, colour below and scatter stay"
        );
        // Half strength moves halfway, rounded.
        assert_eq!(paint.paint(9, 8, 10, 0.5), Ok(true));
        assert_eq!(
            paint.plane(0, 1)[8 * 17 + 9],
            (40. + 215. * 0.5f32).round() as u8
        );
        // Painting scatter changes that layer alone.
        let before = paint.clone();
        assert_eq!(paint.paint(3, 3, 40, 1.), Ok(true));
        for (l, &id) in paint.ids(0).iter().enumerate() {
            let want = if id == 40 {
                255
            } else {
                before.plane(0, l)[3 * 17 + 3]
            };
            assert_eq!(paint.plane(0, l)[3 * 17 + 3], want, "layer {id}");
        }
        // A colour layer on one vertex only goes when painted over there.
        let mut lone = layers(1, 1, reg.clone(), |_| vec![(0, 255), (10, 0), (20, 0)]);
        let mut planes: Vec<u8> = (0..3).flat_map(|l| *lone.plane(0, l)).collect();
        planes[2 * 289 + 5] = 80;
        lone.set(0, &[0, 10, 20], &planes).unwrap();
        assert_eq!(lone.paint(5, 0, 10, 1.), Ok(true));
        assert_eq!(lone.ids(0), &[0, 10]);
        // Nothing to do changes nothing, and an unknown id is refused.
        let same = lone.clone();
        assert_eq!(lone.paint(5, 0, 10, 1.), Ok(false));
        assert!(Arc::ptr_eq(&lone.paint[0], &same.paint[0]), "not copied");
        assert_eq!(lone.paint(5, 0, 99, 1.), Err(super::Skipped::Unregistered));
    }

    // Where four chunks meet, a layer one of them lacks goes into all four
    // or, when that one already holds 24, into none: the brush counts the
    // vertex skipped and reports it once. Neighbours agree afterwards.
    #[test]
    fn a_layer_goes_into_every_chunk_at_a_vertex_or_none() {
        let reg = registry(
            &(0..=30)
                .map(|id| (id, u16::from(id), true))
                .collect::<Vec<_>>(),
        );
        // Chunk 0 holds ids 0..=23 (full); the others the base and 27.
        let full = |k: usize| match k {
            0 => (0..24)
                .map(|id| (id, if id == 0 { 255 } else { 10 }))
                .collect(),
            _ => vec![(0, 255), (27, 30)],
        };
        let paint = Paint::Layers(layers(2, 2, reg.clone(), full));
        let mut m = Materials::from_paint(paint, 1.);
        let before = m.clone();
        let brush = |p: [f32; 2]| if p == [16., 16.] { 1. } else { 0. };
        assert!(!m.paint_with([15., 15.], [17., 17.], 28, brush));
        assert!(!m.paint_with([15., 15.], [17., 17.], 28, brush));
        assert_eq!(m.take_skipped(), 2, "each skipped dab counted");
        assert_eq!(m.take_skipped(), 0, "and reported once");
        assert!(matches!(m.changed(&before), MaterialChange::Nothing));
        // An id chunk 0 holds goes into the three others.
        assert!(m.paint_with([15., 15.], [17., 17.], 5, brush));
        let Paint::Layers(l) = &m.paint else {
            panic!("2070 paint")
        };
        for (k, i) in [(0, 16 * 17 + 16), (1, 16 * 17), (2, 16), (3, 0)] {
            let layer = l.ids(k).iter().position(|&id| id == 5).unwrap();
            assert_eq!(l.plane(k, layer)[i], 255, "chunk {k}");
            // 27 ranks above 5 and falls at the shared vertex.
            if let Some(layer) = l.ids(k).iter().position(|&id| id == 27) {
                assert_eq!(l.plane(k, layer)[i], 0, "chunk {k}");
            }
        }
        // Undo finds the four chunks; a copy keeps the untouched ones.
        let MaterialChange::Area(area) = m.changed(&before) else {
            panic!("the dab changed paint")
        };
        assert_eq!(area, [0., 0., 33., 33.]);
        assert_eq!(m.dominant([16., 16.]), 5);
        assert_eq!(
            before.dominant([16., 16.]),
            0,
            "the snapshot keeps its paint"
        );
    }

    // A vertex lists every layer's stored coverage, base included, and the
    // stack lays them by SplatID at sharpness 5, the base opaque.
    #[test]
    fn layers_cover_lists_stored_coverage_and_stacks_by_splat_rank() {
        let reg = registry(&[(0, 0, true), (10, 9, true), (12, 3, true), (40, 7, false)]);
        let paint = Paint::Layers(layers(2, 1, reg.clone(), |k| match k {
            0 => vec![(0, 128), (10, 128), (12, 255), (40, 200)],
            _ => vec![(0, 255)],
        }));
        let m = Materials::from_paint(paint, 1.);
        let cover = m.cover([3.2, 4.4]);
        let got: Vec<(u8, u8)> = cover
            .iter()
            .map(|&(id, c)| (id, (c * 255.).round() as u8))
            .collect();
        assert_eq!(got, [(0, 128), (10, 128), (12, 255), (40, 200)]);
        let mut stacked = cover;
        let n = reg.stack().shares(&mut stacked);
        // Scatter (40) draws no colour; 12 (rank 3) covers the base, and
        // 10 (rank 9) lies over 12 at saturate(5·(0.5 − 1) + 6·c).
        let ten = (-2.5 + 6. * 128. / 255_f32).clamp(0., 1.);
        let want = [(0, 0.), (12, 1. - ten), (10, ten)];
        assert_eq!(n, 3);
        for (got, want) in stacked[..n].iter().zip(want) {
            assert_eq!(got.0, want.0);
            assert!((got.1 - want.1).abs() < 1e-6, "{got:?} vs {want:?}");
        }
        assert_eq!(m.dominant([3., 4.]), if ten > 0.5 { 10 } else { 12 });
        // On the shared edge either chunk answers; past it, the base.
        assert_eq!(m.cover([16.4, 2.]).len(), 4);
        assert_eq!(&*m.cover([20., 2.]), &[(0, 1.)]);
    }

    // A dab in empty ocean paints every chunk at the vertex first, each with
    // an opaque base and its painted neighbours' coverage along its edges.
    #[test]
    fn a_dab_in_empty_ocean_paints_the_chunks_there_to_agree_with_their_neighbours() {
        let reg = registry(&[(0, 0, true), (10, 9, true), (12, 3, true)]);
        let mut paint = layers(3, 1, reg, |k| match k {
            0 => vec![(0, 255), (12, 90)],
            _ => vec![],
        });
        // On the edge between the empty chunks 1 and 2.
        assert_eq!(paint.paint(32, 8, 10, 1.), Ok(true));
        assert_eq!(paint.ids(1), &[0, 10, 12], "chunk 1 takes chunk 0's edge");
        assert_eq!(paint.plane(1, 2)[8 * 17], 90);
        assert_eq!(paint.plane(1, 2)[8 * 17 + 1], 0, "inside, only the base");
        assert_eq!(paint.ids(2), &[0, 10]);
        assert_eq!(paint.plane(1, 1)[8 * 17 + 16], 255);
        assert_eq!(paint.plane(2, 1)[8 * 17], 255);
        assert!(paint.plane(2, 0).iter().all(|&v| v == 255));
    }

    #[test]
    fn layer_chunks_copy_on_write() {
        let grid = island::Grid {
            width: 300,
            height: 260,
            data: (0..300 * 260).collect(),
        };
        let mut a = Layer::new(&grid);
        let b = a.clone();
        a.set(299, 259, 7);
        assert_eq!(a.get(299, 259), 7);
        assert_eq!(b.get(299, 259), 299 + 259 * 300);
        assert!(
            Arc::ptr_eq(&a.chunks[0], &b.chunks[0]),
            "untouched chunk is shared"
        );
        assert_eq!(b.grid(), grid);
    }

    // A placement, an edit and a removal copy the runs they reach and
    // share the rest with the step before, and edits that change nothing
    // copy nothing; lookups by index and id agree
    // with a plain list's through it all.
    #[test]
    fn props_copy_only_the_runs_an_edit_reaches() {
        use super::{Prop, Props, RUN};
        let prop = |id: u64| Prop {
            id,
            matrix: gpui_editor_kit::math::IDENTITY,
            source: island::Prop {
                file: 0,
                position: [id as f32, 0., 0.],
                rotation: [0., 0., 0., 1.],
                scale: [1.; 3],
                color: [1.; 4],
                adapt_terrain_height: None,
            },
            auto_cliff: false,
        };
        let mut plain: Vec<Prop> = (1..=RUN as u64 * 3 + 10).map(prop).collect();
        let before = Props::from(plain.clone());
        let shared = |a: &Props, b: &Props| {
            (a.allocations()
                .zip(b.allocations())
                .filter(|(x, y)| x.0 == y.0))
            .count()
        };
        let runs = before.allocations().count() - 1;
        assert_eq!(runs, 4);
        // A step that leaves the props alone shares the run table too.
        let mut kept = before.clone();
        kept.retain(|_| true);
        kept.edit_where(|_| false, |p| p.auto_cliff = true);
        assert_eq!(shared(&before, &kept), runs + 1, "every run and the table");

        let mut placed = before.clone();
        placed.push(prop(10_000));
        plain.push(prop(10_000));
        assert_eq!(shared(&before, &placed), runs - 1, "only the last run");
        assert!(!placed.same(&before) && before.clone().same(&before));

        let mut moved = placed.clone();
        let i = moved.position(RUN as u64 + 5).unwrap();
        assert_eq!(i, RUN + 4);
        moved[i].matrix[12] = 3.;
        plain[i].matrix[12] = 3.;
        assert_eq!(shared(&placed, &moved), runs - 1, "only the second run");

        let mut cut = moved.clone();
        cut.retain(|p| p.id != 2);
        plain.retain(|p| p.id != 2);
        assert_eq!(shared(&moved, &cut), runs - 1, "only the first run");
        cut.edit_where(|p| p.id == 10_000, |p| p.auto_cliff = true);
        plain.last_mut().unwrap().auto_cliff = true;

        assert_eq!(cut.len(), plain.len());
        for (k, (a, b)) in cut.iter().zip(&plain).enumerate() {
            assert_eq!(
                (a.id, a.matrix, a.auto_cliff),
                (b.id, b.matrix, b.auto_cliff)
            );
            assert_eq!(cut[k].id, b.id);
            assert_eq!(cut.position(b.id), Some(k));
        }
        assert_eq!(cut.position(2), None);
        assert_eq!(cut.last().map(|p| p.id), Some(10_000));
        cut.retain(|_| false);
        assert!(cut.is_empty() && cut.first().is_none());
    }
}
