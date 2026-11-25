// The grids an island's `m_GOPManager/m_GRIDManager` holds beside its
// terrain, and what a sculpt changes in them: gameplay heights follow the
// terrain by the shipped rule, the submarine blocker is rebuilt around
// what the sculpt moved, and the path blockers, which are authored, only
// gain the cells a sculpt lifts past the height the caller names (2070's
// buildable band, which the game reads from the gameplay heights at load,
// so that rule is the game's: `Sculpt`). Also owns the two
// authored inputs the editor draws: the path blockers and the
// build-blocker polygons.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::terrain::{Bits, Corners, blob, child, number};
use super::{Doc, FIXED, NodeId};

// The gameplay heights' versions, by the element that holds them. 44
// shipped islands store v1 (`m_HeightMap`), which the game smooths into v2
// at load (0x140B9E2D0) and which follows no rule we know (44% of cells
// match); the rest store v2, which follows `gameplay_height` on 99.89% of
// cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Version {
    V1,
    V2,
}

impl Version {
    // In the order a reader looks for them.
    const ALL: [Version; 2] = [Version::V2, Version::V1];
    // What a sculpt writes: the newest the game reads.
    const NEWEST: Version = Version::V2;

    pub(super) fn layout(self) -> Layout {
        match self {
            Version::V1 => Layout::V1,
            Version::V2 => Layout::V2,
        }
    }
}

// What differs between the versions. Both store a little-endian i16 per
// 1 m cell, row by row, in 1/`Layout::UNITS` m.
#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub(super) tag: &'static str,
    // Whether stored cells follow the rule, so a sculpt need only redo the
    // cells around what it moved; otherwise it redoes every cell.
    ruled: bool,
}

impl Layout {
    const V1: Layout = Layout {
        tag: "m_HeightMap",
        ruled: false,
    };
    const V2: Layout = Layout {
        tag: "m_HeightMap_v2",
        ruled: true,
    };

    // Both versions: gameplay heights per metre.
    pub(super) const UNITS: f64 = 1024.;
    // Both versions: bytes in one gameplay height cell.
    const CELL: usize = size_of::<i16>();
    // Both versions: gameplay heights bottom out here, -31 m. Cells with a
    // corner in empty ocean hold it on shipped v2 islands (783,564 cells).
    pub(super) const FLOOR: i16 = -31 * Layout::UNITS as i16;
}

// The movement grids' cells a metre: they are 0.5 m.
const PATH_CELLS: usize = 2;

/// What a sculpt fills in beyond the heights it moved. The values are the
/// game's (games/anno2070 holds them), so there is no default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sculpt {
    /// Block, in `m_PathBlockerLayer`, every 1 m cell the sculpt lifts
    /// from this gameplay height (1/1024 m) or below to above it; `None`
    /// blocks none. The high-ground layer (`m_HeightPathBlockerLayer`)
    /// stays as loaded either way.
    pub block_lifted_above: Option<i16>,
    /// Where the corners of an empty ocean chunk go, in metres, when a
    /// sculpt or a dab first gives it heights and no neighbour holds them.
    pub seabed: f32,
}

impl Corners {
    // What the shipped rule gives the 1 m cell between corners (x, z) and
    // (x + 1, z + 1) as its gameplay height ([`gameplay_height`]); `None`
    // when a corner holds no height.
    pub(super) fn gameplay_height(&self, x: usize, z: usize) -> Option<i16> {
        Some(gameplay_height([
            self.get(x, z)?,
            self.get(x + 1, z)?,
            self.get(x, z + 1)?,
            self.get(x + 1, z + 1)?,
        ]))
    }

    fn mean(&self, x: usize, z: usize) -> Option<f64> {
        let corners = [
            self.get(x, z)?,
            self.get(x + 1, z)?,
            self.get(x, z + 1)?,
            self.get(x + 1, z + 1)?,
        ];
        Some(corners.iter().map(|&c| c as f64).sum::<f64>() / 4.)
    }
}

/// The shipped rule's gameplay height of a 1 m cell with these four corner
/// heights, in 1/1024 m: their mean, truncated, floored at the deep-sea
/// clamp. It holds on 99.89% of shipped v2 cells.
pub fn gameplay_height(corners: [f32; 4]) -> i16 {
    let mean = corners.iter().map(|&c| c as f64).sum::<f64>() / 4.;
    (mean * Layout::UNITS)
        .trunc()
        .clamp(Layout::FLOOR as f64, i16::MAX as f64) as i16
}

// A `BitGrid` as the grid manager stores it: `m_YSize` rows of
// `m_IntXSize` little-endian u32 words, bit x % 32 of word x / 32 for
// column x.
const WORD_BITS: usize = u32::BITS as usize;
struct BitGrid {
    data: NodeId,
    cols: usize,
    rows: usize,
    ints: usize,
    words: Vec<u32>,
}

impl BitGrid {
    fn read(doc: &Doc, layer: NodeId) -> Result<BitGrid> {
        let grid = child(doc, layer, "BitGrid")?;
        let cols: usize = number(doc, child(doc, grid, "m_XSize")?)?;
        let rows: usize = number(doc, child(doc, grid, "m_YSize")?)?;
        let ints: usize = number(doc, child(doc, grid, "m_IntXSize")?)?;
        ensure!(
            ints * WORD_BITS >= cols,
            "{ints} words a row for {cols} columns"
        );
        let (data, bytes) = blob(doc, child(doc, grid, "m_BitGrid")?)?;
        ensure!(
            bytes.len() == ints * rows * size_of::<u32>(),
            "{} bytes for {rows} rows of {ints} words",
            bytes.len()
        );
        let words = bytes
            .as_chunks::<{ size_of::<u32>() }>()
            .0
            .iter()
            .map(|&w| u32::from_le_bytes(w));
        Ok(BitGrid {
            data,
            cols,
            rows,
            ints,
            words: words.collect(),
        })
    }

    fn set(&mut self, x: usize, z: usize, on: bool) {
        let (bit, at) = (x % WORD_BITS, z * self.ints + x / WORD_BITS);
        let word = &mut self.words[at];
        *word = (*word & !(1 << bit)) | (on as u32) << bit;
    }

    fn bytes(&self) -> Vec<u8> {
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

// The grid values a sculpt sets, `moved` the corners it moved from
// `loaded` to `edited`, pushed to `values` as (node, bytes, binary). Gives
// the nodes to rename.
//
// Gameplay heights: the cells around a moved corner follow the rule, and
// the rest carry, shipped cells off the rule included. A version whose
// cells follow no rule (v1) gets every cell by the rule on its first
// sculpt, the floor where a corner is in empty ocean, written as the
// newest version in its place: the layout v2 islands have.
pub(super) fn sculpted(
    doc: &Doc,
    loaded: &Corners,
    edited: &Corners,
    moved: &Bits,
    sculpt: Sculpt,
    values: &mut Vec<(NodeId, Vec<u8>, bool)>,
) -> Result<Vec<(NodeId, &'static str)>> {
    let grid = grid_manager(doc)?;
    let (width, height) = edited.size();
    let (cols, rows) = (width - 1, height - 1);
    // Cells with a moved corner.
    let reached = |x: usize, z: usize| {
        let i = z * width + x;
        [i, i + 1, i + width, i + width + 1]
            .iter()
            .any(|&c| moved.get(c))
    };
    let mut renames = Vec::new();
    let Some((node, version)) =
        (Version::ALL.into_iter()).find_map(|v| Some((doc.child(grid, v.layout().tag)?, v)))
    else {
        bail!("no gameplay heights");
    };
    if version != Version::NEWEST {
        renames.push((node, Version::NEWEST.layout().tag));
    }
    // Every cell is redone where the stored ones follow no rule.
    let all = !version.layout().ruled;
    let (_, stored) = blob(doc, node)?;
    ensure!(
        stored.len() == cols * rows * Layout::CELL,
        "gameplay heights hold {} bytes, {cols}×{rows} cells need {}",
        stored.len(),
        cols * rows * Layout::CELL
    );
    let cell = |cells: &[u8], n: usize| {
        i16::from_le_bytes(
            cells[n * Layout::CELL..][..Layout::CELL]
                .try_into()
                .expect("a whole cell"),
        )
    };
    let mut cells = stored.to_vec();
    // Cells the sculpt takes from the band or below to above it.
    let mut lifted = Vec::new();
    for z in 0..rows {
        for x in 0..cols {
            let n = z * cols + x;
            let near = reached(x, z);
            let v = match edited.gameplay_height(x, z) {
                Some(v) if all || near => v,
                None if all => Layout::FLOOR,
                // A cell reaching into empty ocean has no rule and carries.
                _ => continue,
            };
            // Values off every rule: what the loaded terrain gives is the
            // closer guess at the height the game had.
            let was = match all {
                true => loaded.gameplay_height(x, z).unwrap_or(Layout::FLOOR),
                false => cell(stored, n),
            };
            if let Some(top) = sculpt.block_lifted_above
                && near
                && was <= top
                && v > top
            {
                lifted.push((x, z));
            }
            cells[n * Layout::CELL..][..Layout::CELL].copy_from_slice(&v.to_le_bytes());
        }
    }
    values.push((node, cells, true));

    if !lifted.is_empty()
        && let Some(layer) = doc.child(grid, "m_PathBlockerLayer")
    {
        let named = |e: anyhow::Error| anyhow!("m_PathBlockerLayer: {e:#}");
        let mut path = BitGrid::read(doc, layer).map_err(named)?;
        ensure!(
            (path.cols, path.rows) == (cols * PATH_CELLS, rows * PATH_CELLS),
            "m_PathBlockerLayer is {}×{} for {cols}×{rows} cells of 1 m",
            path.cols,
            path.rows
        );
        // Every path cell inside the 1 m cell.
        for &(x, z) in &lifted {
            for q in 0..PATH_CELLS * PATH_CELLS {
                let (dx, dz) = (q % PATH_CELLS, q / PATH_CELLS);
                path.set(x * PATH_CELLS + dx, z * PATH_CELLS + dz, true);
            }
        }
        values.push((path.data, path.bytes(), true));
    }

    if let Some(layer) = doc.child(grid, "m_SubmarineBlockerLayer") {
        let named = |e: anyhow::Error| anyhow!("m_SubmarineBlockerLayer: {e:#}");
        let blocker = submarine(doc, layer, loaded, edited, &reached).map_err(named)?;
        values.extend(blocker.map(|(node, bytes)| (node, bytes, true)));
    }
    Ok(renames)
}

// The submarine blocker after a sculpt, or `None` if it keeps its bits.
// The game rebuilds it only when its stored height and radius differ from
// its own (0x140B9D780), and on all 380 shipped files they match, so the
// stored grid is what plays. The rule: a cell is blocked when a cell whose
// corners average at or above `SubmarineLevelHeight` (-24 m) lies within
// `SubmarineIslandBlockerRadius` (4 m) of it, as a square. It is exact on
// 224 of 380 islands and off on 2.8% of blocked cells elsewhere; "at or
// above" misses 3,042 fewer shipped bits than "above". So only the cells
// within the radius of one whose side of the level changed are rebuilt,
// and the rest carry.
fn submarine(
    doc: &Doc,
    layer: NodeId,
    loaded: &Corners,
    edited: &Corners,
    reached: &dyn Fn(usize, usize) -> bool,
) -> Result<Option<(NodeId, Vec<u8>)>> {
    let level: i64 = number(doc, child(doc, layer, "SubmarineLevelHeight")?)?;
    let radius: i64 = number(doc, child(doc, layer, "SubmarineIslandBlockerRadius")?)?;
    ensure!(
        radius >= 0 && radius % FIXED == 0,
        "radius {radius} is not whole metres"
    );
    let (level, r) = (level as f64 / FIXED as f64, (radius / FIXED) as usize);
    let mut grid = BitGrid::read(doc, layer)?;
    let (width, height) = edited.size();
    let (cols, rows) = (width - 1, height - 1);
    ensure!(
        (grid.cols, grid.rows) == (cols, rows),
        "{}×{} for {cols}×{rows} cells",
        grid.cols,
        grid.rows
    );
    let high = |c: &Corners, x: usize, z: usize| c.mean(x, z).is_some_and(|m| m >= level);
    let mut redo = Bits::new(cols * rows);
    let mut any = false;
    for z in 0..rows {
        for x in 0..cols {
            if reached(x, z) && high(loaded, x, z) != high(edited, x, z) {
                any = true;
                for rz in z.saturating_sub(r)..(z + r + 1).min(rows) {
                    for rx in x.saturating_sub(r)..(x + r + 1).min(cols) {
                        redo.set(rz * cols + rx);
                    }
                }
            }
        }
    }
    if !any {
        return Ok(None);
    }
    for z in 0..rows {
        for x in 0..cols {
            if !redo.get(z * cols + x) {
                continue;
            }
            let blocked = (z.saturating_sub(r)..(z + r + 1).min(rows)).any(|hz| {
                (x.saturating_sub(r)..(x + r + 1).min(cols)).any(|hx| high(edited, hx, hz))
            });
            grid.set(x, z, blocked);
        }
    }
    Ok(Some((grid.data, grid.bytes())))
}

/// One of the two movement grids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathLayer {
    /// `m_PathBlockerLayer`: ground units cannot walk.
    Blocked,
    /// `m_HeightPathBlockerLayer`: the high ground inside it.
    HighGround,
}

const PATH_LAYERS: [&str; 2] = ["m_PathBlockerLayer", "m_HeightPathBlockerLayer"];

/// An island's two movement grids, owned and editable, at their own 0.5 m.
/// They are authored: the game only loads them (0x140B9D780 via
/// 0x140BA5460), and no rule over heights or polygons reproduces them, so
/// the editor paints them cell by cell.
#[derive(Clone)]
pub struct PathBlockers {
    cols: usize,
    rows: usize,
    ints: usize,
    /// Both layers' rows of words, [`PathLayer::Blocked`] first.
    words: Vec<u32>,
    /// The same, as read.
    loaded: Vec<u32>,
}

impl PathBlockers {
    /// Reads both layers of `doc`; they must be the same size.
    pub fn read(doc: &Doc, file: &str) -> Result<PathBlockers> {
        let named = |e: anyhow::Error| anyhow!("{file}: {e:#}");
        let grid = grid_manager(doc).map_err(named)?;
        let mut layers = PATH_LAYERS.iter().map(|&name| {
            let layer = child(doc, grid, name)?;
            BitGrid::read(doc, layer).map_err(|e| anyhow!("{name}: {e:#}"))
        });
        let path = layers.next().unwrap().map_err(named)?;
        let high = layers.next().unwrap().map_err(named)?;
        ensure!(
            (path.cols, path.rows) == (high.cols, high.rows) && path.ints == high.ints,
            "{file}: movement grids of {}×{} and {}×{}",
            path.cols,
            path.rows,
            high.cols,
            high.rows
        );
        let mut words = path.words;
        words.extend(high.words);
        Ok(PathBlockers {
            cols: path.cols,
            rows: path.rows,
            ints: path.ints,
            loaded: words.clone(),
            words,
        })
    }

    /// Cells per row, and rows: two a metre.
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    fn bit(&self, layer: PathLayer, x: usize, z: usize) -> (usize, u32) {
        assert!(x < self.cols && z < self.rows, "cell {x},{z}");
        let at = layer as usize * self.ints * self.rows + z * self.ints + x / WORD_BITS;
        (at, 1 << (x % WORD_BITS))
    }

    /// Whether 0.5 m cell (x, z) of `layer` is set.
    pub fn get(&self, layer: PathLayer, x: usize, z: usize) -> bool {
        let (at, bit) = self.bit(layer, x, z);
        self.words[at] & bit != 0
    }

    /// Sets (`on`) or clears 0.5 m cell (x, z) of `layer`.
    pub fn set(&mut self, layer: PathLayer, x: usize, z: usize, on: bool) {
        let (at, bit) = self.bit(layer, x, z);
        self.words[at] = (self.words[at] & !bit) | if on { bit } else { 0 };
    }

    /// Writes the cells changed since [`read`](Self::read) into `doc`, a
    /// copy of the island they were read from. Every other cell stays as
    /// `doc` has it, so the cells a sculpt blocks stay blocked whichever
    /// is written first.
    pub fn write(&self, doc: &mut Doc, file: &str) -> Result<()> {
        let named = |e: anyhow::Error| anyhow!("{file}: {e:#}");
        let grid = grid_manager(doc).map_err(named)?;
        let per = self.ints * self.rows;
        let mut values = Vec::new();
        for (n, name) in PATH_LAYERS.iter().enumerate() {
            let range = n * per..(n + 1) * per;
            let mut stored = BitGrid::read(doc, child(doc, grid, name).map_err(named)?)
                .map_err(|e| anyhow!("{file}: {name}: {e:#}"))?;
            ensure!(
                (stored.cols, stored.rows, stored.ints) == (self.cols, self.rows, self.ints),
                "{file}: {name} is {}×{}, the edit {}×{}",
                stored.cols,
                stored.rows,
                self.cols,
                self.rows
            );
            let (now, was) = (&self.words[range.clone()], &self.loaded[range]);
            if now == was {
                continue;
            }
            for ((word, &now), &was) in stored.words.iter_mut().zip(now).zip(was) {
                let edited = now ^ was;
                *word = (*word & !edited) | (now & edited);
            }
            values.push((stored.data, stored.bytes()));
        }
        for (node, bytes) in values {
            doc.set(node, &bytes, true).map_err(named)?;
        }
        Ok(())
    }
}

fn grid_manager(doc: &Doc) -> Result<NodeId> {
    doc.root("m_GOPManager")
        .and_then(|gop| doc.child(gop, "m_GRIDManager"))
        .context("no <m_GRIDManager>")
}

/// An island's build-blocker polygons (`BuildBlockerShapes`), owned and
/// editable, in island metres. The game fills its 1 m build-block grid
/// from them at load (cell centres inside a polygon, end of 0x140B9D780),
/// so nothing else stores the result. Every shipped polygon winds the same
/// way, counter-clockwise in (x, z) with positive area (1,525 of 1,525):
/// reading fails on one that does not, and new ones are put that way.
#[derive(Clone)]
pub struct BuildBlockers {
    /// Each shape's first point in `points`, then the end of the last.
    starts: Vec<u32>,
    /// Points in the file's fixed point, 4096 per metre.
    points: Vec<[i64; 2]>,
    /// Beside each shape, the loaded shape it is, or `NEW`.
    origin: Vec<u32>,
    loaded: usize,
}

const NEW: u32 = u32::MAX;
// A point: x then z, each a little-endian i64 in fixed-point metres.
const POINT: usize = 2 * size_of::<i64>();

impl BuildBlockers {
    pub fn read(doc: &Doc, file: &str) -> Result<BuildBlockers> {
        let named = |e: anyhow::Error| anyhow!("{file}: {e:#}");
        let root = doc
            .root("BuildBlockerShapes")
            .context("no <BuildBlockerShapes>")
            .map_err(named)?;
        let mut blockers = BuildBlockers {
            starts: vec![0],
            points: Vec::new(),
            origin: Vec::new(),
            loaded: 0,
        };
        for (n, shape) in doc.children(root).enumerate() {
            let polygon =
                child(doc, shape, "Polygon").map_err(|e| anyhow!("{file}: shape {n}: {e:#}"))?;
            for point in doc.children(polygon) {
                let (_, bytes) =
                    blob(doc, point).map_err(|e| anyhow!("{file}: shape {n}: {e:#}"))?;
                ensure!(
                    bytes.len() == POINT,
                    "{file}: shape {n}: a point of {} bytes",
                    bytes.len()
                );
                let (x, z) = bytes.split_at(POINT / 2);
                let at = |b: &[u8]| i64::from_le_bytes(b.try_into().expect("a whole i64"));
                blockers.points.push([at(x), at(z)]);
            }
            blockers.starts.push(blockers.points.len() as u32);
            blockers.origin.push(n as u32);
            blockers
                .check(n)
                .map_err(|e| anyhow!("{file}: shape {n}: {e:#}"))?;
        }
        blockers.loaded = blockers.origin.len();
        Ok(blockers)
    }

    // Shoelace area, twice over, in fixed point squared.
    fn area(points: &[[i64; 2]]) -> i128 {
        (0..points.len())
            .map(|i| {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                a[0] as i128 * b[1] as i128 - b[0] as i128 * a[1] as i128
            })
            .sum()
    }

    fn check(&self, shape: usize) -> Result<()> {
        let points = self.fixed(shape);
        ensure!(points.len() >= 3, "{} points", points.len());
        ensure!(
            Self::area(points) > 0,
            "winds the other way from every shipped polygon"
        );
        Ok(())
    }

    fn fixed(&self, shape: usize) -> &[[i64; 2]] {
        &self.points[self.starts[shape] as usize..self.starts[shape + 1] as usize]
    }

    /// How many polygons there are.
    pub fn len(&self) -> usize {
        self.origin.len()
    }

    pub fn is_empty(&self) -> bool {
        self.origin.is_empty()
    }

    /// The polygon's points, (x, z) in island metres.
    pub fn points(&self, shape: usize) -> impl Iterator<Item = [f64; 2]> + '_ {
        self.fixed(shape)
            .iter()
            .map(|p| p.map(|c| c as f64 / FIXED as f64))
    }

    /// Adds a polygon of at least three points, in island metres, and
    /// gives its number. It is stored with the shipped winding, so a
    /// clockwise outline is reversed. Fails on fewer points or no area.
    pub fn add(&mut self, outline: &[[f64; 2]]) -> Result<usize> {
        let mut fixed: Vec<[i64; 2]> = outline
            .iter()
            .map(|p| p.map(|c| (c * FIXED as f64).round() as i64))
            .collect();
        ensure!(fixed.len() >= 3, "a polygon of {} points", fixed.len());
        match Self::area(&fixed) {
            0 => bail!("a polygon with no area"),
            a if a < 0 => fixed.reverse(),
            _ => {}
        }
        self.points.extend(fixed);
        self.starts.push(self.points.len() as u32);
        self.origin.push(NEW);
        Ok(self.origin.len() - 1)
    }

    /// Moves a polygon's point to `to`, in island metres. The write fails
    /// if the polygon then winds the other way.
    pub fn move_point(&mut self, shape: usize, point: usize, to: [f64; 2]) {
        let at = self.starts[shape] as usize + point;
        assert!(
            at < self.starts[shape + 1] as usize,
            "shape {shape} has no point {point}"
        );
        self.points[at] = to.map(|c| (c * FIXED as f64).round() as i64);
    }

    /// Removes a polygon; the ones after it move down a number.
    pub fn remove(&mut self, shape: usize) {
        let (start, end) = (self.starts[shape] as usize, self.starts[shape + 1] as usize);
        self.points.drain(start..end);
        self.starts.remove(shape + 1);
        for s in &mut self.starts[shape + 1..] {
            *s -= (end - start) as u32;
        }
        self.origin.remove(shape);
    }

    /// Writes the polygons into `doc`, a copy of the island they were read
    /// from: moved points in place, removed shapes out, new ones last, each
    /// an `i` holding a `Polygon` of `i` points as shipped shapes are.
    pub fn write(&self, doc: &mut Doc, file: &str) -> Result<()> {
        let named = |e: anyhow::Error| anyhow!("{file}: {e:#}");
        for shape in 0..self.len() {
            self.check(shape)
                .map_err(|e| anyhow!("{file}: shape {shape}: {e:#}"))?;
        }
        let root = doc
            .root("BuildBlockerShapes")
            .context("no <BuildBlockerShapes>")
            .map_err(named)?;
        let stored: Vec<NodeId> = doc.children(root).collect();
        ensure!(
            stored.len() == self.loaded,
            "{file}: {} shapes, the edit read {}",
            stored.len(),
            self.loaded
        );
        let bytes = |p: &[i64; 2]| [p[0].to_le_bytes(), p[1].to_le_bytes()].concat();
        // Points first: sets move no node.
        let mut kept = vec![false; self.loaded];
        let mut values = Vec::new();
        for shape in 0..self.len() {
            let Some(&node) = stored.get(self.origin[shape] as usize) else {
                continue;
            };
            kept[self.origin[shape] as usize] = true;
            let polygon = child(doc, node, "Polygon").map_err(named)?;
            let points: Vec<NodeId> = doc.children(polygon).collect();
            ensure!(
                points.len() == self.fixed(shape).len(),
                "{file}: shape {shape} holds {} points, the edit {}",
                points.len(),
                self.fixed(shape).len()
            );
            for (&node, point) in points.iter().zip(self.fixed(shape)) {
                let new = bytes(point);
                if blob(doc, node).map_err(named)?.1 != new {
                    values.push((node, new));
                }
            }
        }
        for (node, value) in values {
            doc.set(node, &value, true).map_err(named)?;
        }
        // Removals last shape first, so the earlier ids hold.
        for (n, &node) in stored.iter().enumerate().rev() {
            if !kept[n] {
                doc.remove(node);
            }
        }
        for shape in (0..self.len()).filter(|&s| self.origin[s] == NEW) {
            let root = doc.root("BuildBlockerShapes").expect("found above");
            let entry = doc.insert(root, None, "i").map_err(named)?;
            let polygon = doc.insert(entry, None, "Polygon").map_err(named)?;
            for point in self.fixed(shape) {
                let node = doc.insert(polygon, None, "i").map_err(named)?;
                doc.set(node, &bytes(point), true).map_err(named)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A grid manager holding the two movement grids, `cols`×`rows` cells,
    // with the set cells listed, written out in the shipped layout.
    fn movement(
        cols: usize,
        rows: usize,
        path: &[(usize, usize)],
        high: &[(usize, usize)],
    ) -> Vec<u8> {
        let mut b = b"<m_GOPManager><m_GRIDManager>".to_vec();
        for (tag, set) in [
            ("m_PathBlockerLayer", path),
            ("m_HeightPathBlockerLayer", high),
        ] {
            let ints = cols.div_ceil(WORD_BITS);
            let mut words = vec![0u32; ints * rows];
            for &(x, z) in set {
                words[z * ints + x / WORD_BITS] |= 1 << (x % WORD_BITS);
            }
            b.extend(
                format!(
                    "<{tag}><BitGrid><m_XSize>{cols}</m_XSize>\r\n<m_YSize>{rows}</m_YSize>\r\n\
                     <m_IntXSize>{ints}</m_IntXSize>\r\n<m_BitGrid>CDATA["
                )
                .bytes(),
            );
            b.extend(((words.len() * size_of::<u32>()) as u32).to_le_bytes());
            b.extend(words.iter().flat_map(|w| w.to_le_bytes()));
            b.extend(format!("]</m_BitGrid>\r\n</BitGrid>\r\n</{tag}>\r\n").bytes());
        }
        b.extend(b"</m_GRIDManager>\r\n</m_GOPManager>\r\n");
        b
    }

    // Cells set and cleared since the read are what a write changes. Cells
    // a sculpt set in the file after the read stay set.
    #[test]
    fn path_cells_write_only_what_changed_since_the_read() {
        let before = movement(40, 4, &[(0, 3)], &[(0, 0), (39, 3)]);
        let mut doc = Doc::parse(&before).unwrap();
        let mut paths = PathBlockers::read(&doc, "isle.isd").unwrap();
        assert_eq!(paths.size(), (40, 4));
        assert!(paths.get(PathLayer::HighGround, 39, 3) && !paths.get(PathLayer::Blocked, 39, 3));
        for (x, z) in [(3, 1), (4, 1), (3, 2), (4, 2)] {
            paths.set(PathLayer::Blocked, x, z, true);
        }
        paths.set(PathLayer::HighGround, 0, 0, false);
        paths.set(PathLayer::HighGround, 39, 0, true);
        // A sculpt blocks (35, 1) in the file meanwhile.
        let lifted = movement(40, 4, &[(0, 3), (35, 1)], &[(0, 0), (39, 3)]);
        let mut sculpted = Doc::parse(&lifted).unwrap();
        paths.write(&mut sculpted, "isle.isd").unwrap();
        let blocked = [(0, 3), (35, 1), (3, 1), (4, 1), (3, 2), (4, 2)];
        assert!(sculpted.encode() == movement(40, 4, &blocked, &[(39, 0), (39, 3)]));
        // Into the file as read, the same without the sculpt's cell.
        paths.write(&mut doc, "isle.isd").unwrap();
        assert!(
            doc.encode()
                == movement(
                    40,
                    4,
                    &blocked[..1]
                        .iter()
                        .chain(&blocked[2..])
                        .copied()
                        .collect::<Vec<_>>(),
                    &[(39, 0), (39, 3)]
                )
        );
        let wide = Doc::parse(&movement(64, 4, &[], &[])).unwrap();
        let e = PathBlockers::read(&wide, "isle.isd")
            .unwrap()
            .write(&mut doc, "isle.isd");
        assert_eq!(
            e.err().unwrap().to_string(),
            "isle.isd: m_PathBlockerLayer is 40×4, the edit 64×4"
        );
    }

    // Polygons in the shipped layout, points in metres.
    fn shapes(polygons: &[&[(f64, f64)]]) -> Vec<u8> {
        if polygons.is_empty() {
            return b"<BuildBlockerShapes></BuildBlockerShapes>\r\n".to_vec();
        }
        let mut b = b"<BuildBlockerShapes>".to_vec();
        for points in polygons {
            b.extend(b"<i><Polygon>");
            for &(x, z) in *points {
                b.extend(b"<i>CDATA[");
                b.extend(16u32.to_le_bytes());
                b.extend(((x * FIXED as f64) as i64).to_le_bytes());
                b.extend(((z * FIXED as f64) as i64).to_le_bytes());
                b.extend(b"]</i>\r\n");
            }
            b.extend(b"</Polygon>\r\n</i>\r\n");
        }
        b.extend(b"</BuildBlockerShapes>\r\n");
        b
    }

    const SQUARE: &[(f64, f64)] = &[(0., 0.), (4., 0.), (4., 4.), (0., 4.)];
    const TRIANGLE: &[(f64, f64)] = &[(10., 10.), (12., 10.), (10., 12.)];

    #[test]
    fn build_blockers_add_move_and_remove_keeping_the_shipped_winding() {
        let before = shapes(&[SQUARE, TRIANGLE]);
        let mut doc = Doc::parse(&before).unwrap();
        let mut blockers = BuildBlockers::read(&doc, "isle.isd").unwrap();
        assert_eq!(blockers.len(), 2);
        assert_eq!(
            blockers.points(1).collect::<Vec<_>>(),
            [[10., 10.], [12., 10.], [10., 12.]]
        );
        blockers.write(&mut doc, "isle.isd").unwrap();
        assert!(doc.encode() == before);

        blockers.move_point(0, 2, [5., 4.5]);
        blockers.remove(1);
        // Drawn clockwise; kept the shipped way round.
        let new = blockers
            .add(&[[20., 20.], [20., 22.25], [22., 22.25], [22., 20.]])
            .unwrap();
        assert_eq!(new, 1);
        assert_eq!(blockers.points(1).next(), Some([22., 20.]));
        blockers.write(&mut doc, "isle.isd").unwrap();
        let moved: &[(f64, f64)] = &[(0., 0.), (4., 0.), (5., 4.5), (0., 4.)];
        let added: &[(f64, f64)] = &[(22., 20.), (22., 22.25), (20., 22.25), (20., 20.)];
        assert!(doc.encode() == shapes(&[moved, added]));

        // Into an island without polygons, and back to none.
        let mut empty = Doc::parse(&shapes(&[])).unwrap();
        let mut blockers = BuildBlockers::read(&empty, "isle.isd").unwrap();
        assert!(blockers.is_empty());
        blockers
            .add(&TRIANGLE.iter().map(|p| [p.0, p.1]).collect::<Vec<_>>())
            .unwrap();
        blockers.write(&mut empty, "isle.isd").unwrap();
        assert!(empty.encode() == shapes(&[TRIANGLE]));
        let mut none = BuildBlockers::read(&empty, "isle.isd").unwrap();
        none.remove(0);
        none.write(&mut empty, "isle.isd").unwrap();
        assert!(empty.encode() == shapes(&[]));
    }

    #[test]
    fn build_blockers_winding_the_other_way_or_without_area_fail() {
        let clockwise: Vec<(f64, f64)> = SQUARE.iter().rev().copied().collect();
        let doc = Doc::parse(&shapes(&[TRIANGLE, &clockwise])).unwrap();
        let e = BuildBlockers::read(&doc, "isle.isd")
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            e,
            "isle.isd: shape 1: winds the other way from every shipped polygon"
        );

        let mut doc = Doc::parse(&shapes(&[SQUARE])).unwrap();
        let mut blockers = BuildBlockers::read(&doc, "isle.isd").unwrap();
        assert!(blockers.add(&[[0., 0.], [1., 1.], [2., 2.]]).is_err());
        assert!(blockers.add(&[[0., 0.], [1., 1.]]).is_err());
        // A move that turns the square inside out.
        blockers.move_point(0, 1, [-4., 8.]);
        let e = blockers
            .write(&mut doc, "isle.isd")
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            e,
            "isle.isd: shape 0: winds the other way from every shipped polygon"
        );
    }
}
