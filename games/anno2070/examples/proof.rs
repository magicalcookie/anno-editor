// Spike part 2 input: one shipped island edited the way the editor will edit
// it, written outside the install for the in-game proof. The sculpt goes through the
// codec's own edits (`isd::terrain`) and the two paint strokes through the
// editor's brush (core's `Layers`), saved as the editor saves them: gameplay
// heights are recomputed from the terrain by the shipped rule, `Flags` bit 0
// flips only when its predicate does, paint follows the plan's brush rule
// (one stroke inserts a layer), and everything else is written back as
// loaded. Probes
// test what rests on the engine alone: the buildable band's edges, a new
// build-blocker polygon, and a path-blocker strip.
//
//   ANNO2070_DIR=... cargo run --release -p anno2070 --example proof -- <out dir>
//
// <out>/a replaces the shipped island in place. <out>/b writes the same
// island under a new name and points scenario 06's world at it, so it also
// proves a codec-written world and a new island file.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use std::fmt::Write as _;
use std::ops::RangeInclusive;

use anno_core::island::Layers;
use anno_formats::isd::grids::{Sculpt, gameplay_height};
use anno_formats::isd::terrain::{Corners, GroundTextures, Terrain};
use anno_formats::isd::{Doc, NodeId, Value};
use anno_formats::vfs::{FileId, Install};

const ISLAND: &str = "data/levels/islands/normal/n_m02.isd";
const IN_PLACE: &str = "data/levels/islands/normal/N_M02.isd";
const RENAMED: &str = "N_M02_proof.isd";
// Scenario 06 places N_M02 and is in the mission menu
// (main_menu_mission_overview.dlg lists its GUID 3263500). Scenario 07 also
// places it but is not in the menu.
const WORLD: &str = "data/levels/scenarios/singleplayer/06.www";
const REGISTRY: &str = "data/config/engine/groundtextures.xml";

// The bump: 5 m high, 12 m radius, cosine falloff. Its top leaves the
// buildable band, so the game should refuse buildings on it.
const BUMP_HEIGHT: f64 = 5.0;
const BUMP_RADIUS: f64 = 12.0;
// Paint strokes: full inside 5 m, gone at 8 m.
const PAINT_INNER: f64 = 5.0;
const PAINT_OUTER: f64 = 8.0;
// earth_01 (SplatID 812) ranks above gras_01-03 (800-810) but sorts before
// them by `TextureIndex` (52, 71, 72), so it shows the rank, not file order.
const EARTH: u32 = 18;
// The land band the game derives buildable ground from at load
// (0x140B9D780: 4h - 2703 <= 369 unsigned, 0.66-0.75 m).
const LAND: RangeInclusive<i16> = 676..=768;
// Probe pads: gameplay heights only, terrain untouched, either side of the
// band's edges. 8 m fits a small building whole.
const PADS: [i16; 4] = [675, 676, 768, 769];
const PAD: f64 = 8.0;
const PAD_GAP: f64 = 4.0;
// A new build-blocker square, with room to build beside it.
const SQUARE: f64 = 8.0;
const SQUARE_ROOM: f64 = 6.0;
// A path-blocker strip, 2 m by 16 m, with room for a road across it.
const STRIP: (f64, f64) = (2.0, 16.0);
const STRIP_ROOM: f64 = 6.0;
// Space between the edits, so each probe reads on its own.
const GAP: f64 = 6.0;

fn text(doc: &Doc, id: NodeId, name: &str) -> i64 {
    match doc.value(doc.child(id, name).unwrap()) {
        Value::Text(t) => std::str::from_utf8(t).unwrap().parse().unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

fn bytes(doc: &Doc, id: NodeId) -> &[u8] {
    match doc.value(id) {
        Value::Blob(b) | Value::Text(b) => b,
        _ => &[],
    }
}

fn blob<'a>(doc: &'a Doc, id: NodeId, name: &str) -> &'a [u8] {
    match doc.value(doc.child(id, name).unwrap()) {
        Value::Blob(b) => b,
        other => panic!("{name}: {other:?}"),
    }
}

// Chunk `k`'s `Flags` and stored heights (samples a side, then the
// samples), from the bytes.
fn chunk(doc: &Doc, k: usize) -> (i64, Option<(usize, &[u8])>) {
    let map = doc.child(doc.root("Terrain").unwrap(), "ChunkMap").unwrap();
    let element = (doc.children(map))
        .filter(|&e| doc.name(e) == b"Element")
        .nth(k)
        .unwrap();
    let heights = (doc.child(element, "HeightMap"))
        .map(|m| (text(doc, m, "Width") as usize, blob(doc, m, "Data")));
    (text(doc, element, "Flags"), heights)
}

// The shipped rule's gameplay height of the 1 m cell at (x, z).
fn gameplay(corners: &Corners, x: usize, z: usize) -> Option<i16> {
    Some(gameplay_height([
        corners.get(x, z)?,
        corners.get(x + 1, z)?,
        corners.get(x, z + 1)?,
        corners.get(x + 1, z + 1)?,
    ]))
}

fn bump(x: f64, z: f64, cx: f64, cz: f64) -> f64 {
    let d = ((x - cx).powi(2) + (z - cz).powi(2)).sqrt();
    if d >= BUMP_RADIUS {
        return 0.0;
    }
    BUMP_HEIGHT * 0.5 * (1.0 + (std::f64::consts::PI * d / BUMP_RADIUS).cos())
}

fn stroke(x: f64, z: f64, cx: f64, cz: f64) -> f64 {
    let d = ((x - cx).powi(2) + (z - cz).powi(2)).sqrt();
    ((PAINT_OUTER - d) / (PAINT_OUTER - PAINT_INNER)).clamp(0.0, 1.0)
}

struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }

    fn get(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
    }

    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x0: f64,
    z0: f64,
    x1: f64,
    z1: f64,
}

impl Rect {
    fn around(cx: f64, cz: f64, half_x: f64, half_z: f64) -> Rect {
        Rect {
            x0: cx - half_x,
            z0: cz - half_z,
            x1: cx + half_x,
            z1: cz + half_z,
        }
    }

    fn gap(&self, o: &Rect) -> f64 {
        let dx = (o.x0 - self.x1).max(self.x0 - o.x1).max(0.0);
        let dz = (o.z0 - self.z1).max(self.z0 - o.z1).max(0.0);
        dx.max(dz)
    }

    fn cells(&self) -> impl Iterator<Item = (usize, usize)> {
        let (x0, x1) = (self.x0.round() as usize, self.x1.round() as usize);
        let (z0, z1) = (self.z0.round() as usize, self.z1.round() as usize);
        (z0..z1).flat_map(move |z| (x0..x1).map(move |x| (x, z)))
    }
}

// What writing `layers` would change in `stored`: how many planes get new
// bytes, and the chunks that gain a layer.
fn pending(layers: &Layers, stored: &Terrain) -> (usize, Vec<usize>) {
    let (w, h) = stored.size();
    let (mut planes, mut gained) = (0, Vec::new());
    for k in 0..w * h {
        for (layer, &id) in layers.ids(k).iter().enumerate() {
            match stored.ids(k).iter().position(|&s| s == u32::from(id)) {
                Some(s) => planes += (stored.plane(k, s) != layers.plane(k, layer)) as usize,
                None => gained.push(k),
            }
        }
    }
    (planes, gained)
}

// (group, m_Position node, x, z, height) for every placed object.
fn objects(doc: &Doc) -> Vec<(String, NodeId, f64, f64, f64)> {
    let gop = doc.root("m_GOPManager").unwrap();
    let mut out = Vec::new();
    let Some(objs) = doc.child(gop, "Objects") else {
        return out;
    };
    for group in doc.children(objs) {
        let kind = String::from_utf8_lossy(doc.name(group)).into_owned();
        let list: Vec<NodeId> = match doc.child(group, "Objects") {
            Some(o) => doc.children(o).collect(),
            None => doc.children(group).collect(),
        };
        for o in list {
            let Some(p) = doc.child(o, "m_Position") else {
                continue;
            };
            let Value::Blob(b) = doc.value(p) else {
                continue;
            };
            // (x, z, height), fixed point at 4096 per metre.
            let c = |i: usize| {
                i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap()) as f64 / 4096.0
            };
            out.push((kind.clone(), p, c(0), c(1), c(2)));
        }
    }
    out
}

// Build-blocker polygons in metres; points are (x, z) at 4096 per metre.
fn polygons(doc: &Doc) -> Vec<Vec<(f64, f64)>> {
    let Some(shapes) = doc.root("BuildBlockerShapes") else {
        return Vec::new();
    };
    doc.children(shapes)
        .filter_map(|s| doc.child(s, "Polygon"))
        .map(|p| {
            doc.children(p)
                .map(|pt| {
                    let b = bytes(doc, pt);
                    let c = |i: usize| {
                        i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap()) as f64 / 4096.0
                    };
                    (c(0), c(1))
                })
                .collect()
        })
        .collect()
}

fn inside(poly: &[(f64, f64)], x: f64, z: f64) -> bool {
    let mut odd = false;
    for (i, &(ax, az)) in poly.iter().enumerate() {
        let (bx, bz) = poly[(i + 1) % poly.len()];
        if (az > z) != (bz > z) && x < ax + (z - az) / (bz - az) * (bx - ax) {
            odd = !odd;
        }
    }
    odd
}

fn bitgrid(doc: &Doc, grid: NodeId, name: &str) -> (NodeId, usize, Vec<u32>) {
    let g = doc
        .child(doc.child(grid, name).unwrap(), "BitGrid")
        .unwrap();
    let words = blob(doc, g, "m_BitGrid")
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&w| u32::from_le_bytes(w))
        .collect();
    (
        doc.child(g, "m_BitGrid").unwrap(),
        text(doc, g, "m_IntXSize") as usize,
        words,
    )
}

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("out dir"));
    let dir = std::env::var_os("ANNO2070_DIR").expect("ANNO2070_DIR");
    let families = [
        "data",
        "cze",
        "eng",
        "esp",
        "fra",
        "ger",
        "ita",
        "pol",
        "rus",
        "patch",
        "addon",
        "addon_cze",
        "addon_eng",
        "addon_esp",
        "addon_fra",
        "addon_ger",
        "addon_ita",
        "addon_pol",
        "addon_rus",
        "exedata",
    ];
    let install = Install::open(&dir, &families).unwrap();
    let find = |path: &str| {
        (0..install.file_count())
            .map(|i| FileId(i as u32))
            .find(|&i| install.file_path(i).eq_ignore_ascii_case(path))
            .unwrap_or_else(|| panic!("{path} not in the install"))
    };
    let ground = GroundTextures::parse(&String::from_utf8_lossy(
        &install.read_file(find(REGISTRY)).unwrap(),
    ))
    .unwrap();
    let texture = |t: u32| {
        (ground.textures().iter())
            .find(|g| g.id == t)
            .unwrap_or_else(|| panic!("ground texture {t} not in the registry"))
    };
    // Draw rank (`SplatID`), and whether a layer is colour: a real diffuse
    // image, not grass or bush objects or a dummy test texture.
    let rank = |t: u32| texture(t).rank;
    let colour = |t: u32| (texture(t).diffuse.as_ref()).is_some_and(|d| !d.contains("dummy"));
    let grass = |t: u32| texture(t).name.starts_with("gras");
    let source = install.read_file(find(ISLAND)).unwrap();
    let before = Doc::parse(&source).unwrap();
    let loaded = Terrain::read(&before, ISLAND).unwrap();
    let mut doc = Doc::parse(&source).unwrap();

    let w = doc
        .roots()
        .find(|&r| doc.name(r) == b"Width")
        .map(|r| {
            std::str::from_utf8(bytes(&doc, r))
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .unwrap();
    let grid = doc
        .child(doc.root("m_GOPManager").unwrap(), "m_GRIDManager")
        .unwrap();
    let gh_node = doc
        .child(grid, "m_HeightMap_v2")
        .expect("v2 gameplay heights");
    let loaded_gh: Vec<i16> = bytes(&doc, gh_node)
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&v| i16::from_le_bytes(v))
        .collect();
    assert_eq!(loaded_gh.len(), w * w);
    let (path_node, pints, loaded_path) = bitgrid(&doc, grid, "m_PathBlockerLayer");
    let (_, hints, hpath) = bitgrid(&doc, grid, "m_HeightPathBlockerLayer");
    let mut path = loaded_path.clone();
    let half = |words: &[u32], ints: usize, i: usize, j: usize| {
        words[i * ints + j / 32] >> (j % 32) & 1 == 1
    };
    let objs = objects(&doc);
    let polys = polygons(&doc);
    let (cw, ch) = loaded.size();
    let side = cw * 16 + 1;
    assert_eq!((side, ch), (w + 1, cw));
    let loaded_corners = loaded.corners();

    // Per 1 m cell: open ground (in the land band, no path blocker under
    // it, outside every build-blocker polygon), and where buildings stand.
    let (mut open, mut buildings) = (Bits::new(w * w), Bits::new(w * w));
    for z in 0..w {
        for x in 0..w {
            let blocked = (0..4).any(|q| {
                let (i, j) = (2 * z + q / 2, 2 * x + q % 2);
                half(&path, pints, i, j) || half(&hpath, hints, i, j)
            });
            let (cx, cz) = (x as f64 + 0.5, z as f64 + 0.5);
            if LAND.contains(&loaded_gh[z * w + x])
                && !blocked
                && !polys.iter().any(|p| inside(p, cx, cz))
            {
                open.set(z * w + x);
            }
        }
    }
    for (kind, _, x, z, _) in &objs {
        if *x < 0.0 || *z < 0.0 || *x >= w as f64 || *z >= w as f64 {
            continue;
        }
        let i = *z as usize * w + *x as usize;
        if kind != "Nature" && kind != "Grass" {
            buildings.set(i);
        }
    }

    // Sites, nearest the plateau's middle first. Each needs open ground 2 m
    // past its edges, no buildings, and GAP from the others. Trees may stand
    // anywhere: the plateau is forest, its largest tree-free square is 6 m,
    // and a building placed over trees clears them.
    let land: Vec<(usize, usize)> = (0..w * w)
        .filter(|&i| open.get(i))
        .map(|i| (i % w, i / w))
        .collect();
    let (mx, mz) = land
        .iter()
        .fold((0.0, 0.0), |(a, b), &(x, z)| (a + x as f64, b + z as f64));
    let (mx, mz) = (mx / land.len() as f64, mz / land.len() as f64);
    let mut centres: Vec<(f64, f64)> = land.iter().map(|&(x, z)| (x as f64, z as f64)).collect();
    centres.sort_by(|a, b| {
        let d = |p: &(f64, f64)| (p.0 - mx).powi(2) + (p.1 - mz).powi(2);
        d(a).total_cmp(&d(b))
    });
    let fits = |r: &Rect, taken: &[Rect]| -> bool {
        let m = 2.0;
        let (x0, z0) = ((r.x0 - m).floor() as i64, (r.z0 - m).floor() as i64);
        let (x1, z1) = ((r.x1 + m).ceil() as i64, (r.z1 + m).ceil() as i64);
        if x0 < 0
            || z0 < 0
            || x1 > w as i64
            || z1 > w as i64
            || taken.iter().any(|t| t.gap(r) < GAP)
        {
            return false;
        }
        (z0..z1).all(|z| {
            (x0..x1).all(|x| {
                let i = z as usize * w + x as usize;
                open.get(i) && !buildings.get(i)
            })
        })
    };
    let place = |half_x: f64, half_z: f64, taken: &[Rect], also: &dyn Fn(f64, f64) -> bool| {
        centres
            .iter()
            .map(|&(x, z)| Rect::around(x, z, half_x, half_z))
            .find(|r| {
                let (x, z) = ((r.x0 + r.x1) / 2.0, (r.z0 + r.z1) / 2.0);
                fits(r, taken) && also(x, z)
            })
    };
    // Chunks whose vertices a stroke reaches.
    let stroked = |cx: f64, cz: f64| -> Vec<usize> {
        (0..cw * ch)
            .filter(|&k| {
                let (x0, z0) = ((k % cw * 16) as f64, (k / cw * 16) as f64);
                (0..17).any(|i| (0..17).any(|j| stroke(x0 + j as f64, z0 + i as f64, cx, cz) > 0.0))
            })
            .collect()
    };
    let has = |k: usize, t: u32| loaded.ids(k).contains(&t);
    let coverage = |k: usize, t: u32| -> u64 {
        let layer = loaded.ids(k).iter().position(|&l| l == t);
        layer.map_or(0, |l| loaded.plane(k, l).iter().map(|&a| a as u64).sum())
    };

    let mut taken = Vec::new();
    let r = BUMP_RADIUS;
    let bump_site = place(r, r, &taken, &|_, _| true).expect("no site for the bump");
    taken.push(bump_site);
    let (bx, bz) = (
        (bump_site.x0 + bump_site.x1) / 2.0,
        (bump_site.z0 + bump_site.z1) / 2.0,
    );

    // Probes, before the strokes: they need the larger bare areas.
    let pads_half = PAD + PAD_GAP / 2.0;
    let pads = place(pads_half, pads_half, &taken, &|_, _| true).expect("no site for the pads");
    taken.push(pads);
    let square_half = SQUARE / 2.0 + SQUARE_ROOM;
    let square_site =
        place(square_half, square_half, &taken, &|_, _| true).expect("no site for the square");
    taken.push(square_site);
    let strip_site = place(
        STRIP.0 / 2.0 + STRIP_ROOM,
        STRIP.1 / 2.0 + 1.0,
        &taken,
        &|_, _| true,
    )
    .expect("no site for the strip");
    taken.push(strip_site);

    // Stroke 1 changes values only: a colour texture every chunk it reaches
    // already holds, not a grass one, the least present so the stroke shows.
    let r = PAINT_OUTER;
    let pick = |cx: f64, cz: f64| -> Option<u32> {
        let ks = stroked(cx, cz);
        let first = ks.first()?;
        loaded
            .ids(*first)
            .iter()
            .copied()
            .filter(|&t| t != 0 && colour(t) && !grass(t) && ks.iter().all(|&k| has(k, t)))
            .min_by_key(|&t| ks.iter().map(|&k| coverage(k, t)).sum::<u64>())
    };
    let stroke1 = place(r, r, &taken, &|x, z| pick(x, z).is_some()).expect("no site for stroke 1");
    taken.push(stroke1);
    let (s1x, s1z) = (
        (stroke1.x0 + stroke1.x1) / 2.0,
        (stroke1.z0 + stroke1.z1) / 2.0,
    );
    let tex1 = pick(s1x, s1z).unwrap();

    // Stroke 2 paints earth_01 across a chunk border into chunks that lack
    // it, so the brush inserts layers. Those chunks hold grass ranked below
    // earth, which must stay as it was and still be covered.
    let below =
        |k: usize| (loaded.ids(k).iter()).any(|&t| t != 0 && colour(t) && rank(t) < rank(EARTH));
    let stroke2 = place(r, r, &taken, &|x, z| {
        let ks = stroked(x, z);
        ks.len() >= 2
            && ks.iter().any(|&k| !has(k, EARTH))
            && ks
                .iter()
                .all(|&k| has(k, EARTH) || (loaded.ids(k).len() < Terrain::MAX_LAYERS && below(k)))
    })
    .expect("no site for stroke 2");
    taken.push(stroke2);
    let (s2x, s2z) = (
        (stroke2.x0 + stroke2.x1) / 2.0,
        (stroke2.z0 + stroke2.z1) / 2.0,
    );

    let mut notes = String::new();
    let mut sites = String::from("kind\tx0\tz0\tx1\tz1\tnote\n");
    let mut site = |kind: &str, r: Rect, note: String| {
        writeln!(
            sites,
            "{kind}\t{}\t{}\t{}\t{}\t{note}",
            r.x0, r.z0, r.x1, r.z1
        )
        .unwrap();
    };
    writeln!(
        notes,
        "island {IN_PLACE} ({w} m), island-local metres, x east and z south as stored; \
         open-ground middle {mx:.1}, {mz:.1}"
    )
    .unwrap();

    // Sculpt: every corner moves by the bump at its position. The write
    // gives each chunk its samples from the one grid, so vertices two
    // chunks share get the same bits; flips `Flags` bit 0 only where "all
    // heights above 0.25 m" changed (1,290 shipped chunks disagree with that
    // predicate, so it is not the whole rule); and recomputes the gameplay
    // heights of the cells whose corners moved, carrying the rest, which
    // keeps the shipped exceptions to the rule.
    let mut new_corners = loaded.corners();
    for z in 0..side {
        for x in 0..side {
            let d = bump(x as f64, z as f64, bx, bz);
            if d > 0.0
                && let Some(h) = new_corners.get(x, z)
            {
                new_corners.set(x, z, (h as f64 + d) as f32);
            }
        }
    }
    // The bump stays open to movement: the proof checks that open high
    // ground does no harm, and its strip probe tests the path layer.
    let open = Sculpt {
        block_lifted_above: None,
        ..anno2070::island::SCULPT
    };
    new_corners.write(&mut doc, ISLAND, open).unwrap();
    let (mut vertices, mut height_chunks, mut flag_changes) = (0, 0, 0);
    for k in 0..cw * ch {
        let ((old_flags, old), (new_flags, new)) = (chunk(&before, k), chunk(&doc, k));
        let (Some((side, old)), Some((_, new))) = (old, new) else {
            continue;
        };
        let (x0, z0) = ((k % cw * 16) as f64, (k / cw * 16) as f64);
        let step = 16.0 / (side - 1) as f64;
        vertices += (0..side * side)
            .filter(|n| {
                let (j, i) = (n % side, n / side);
                bump(x0 + j as f64 * step, z0 + i as f64 * step, bx, bz) > 0.0
            })
            .count();
        height_chunks += (old != new) as usize;
        flag_changes += (old_flags != new_flags) as usize;
    }
    let mut gh: Vec<i16> = bytes(&doc, gh_node)
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&v| i16::from_le_bytes(v))
        .collect();
    let mut sculpted = Bits::new(w * w);
    let (mut cells, mut off_rule) = (0, 0);
    for z in 0..w {
        for x in 0..w {
            let moved = [(0, 0), (1, 0), (0, 1), (1, 1)].iter().any(|&(dx, dz)| {
                let bits = |t: &Corners| t.get(x + dx, z + dz).map(f32::to_bits);
                bits(&loaded_corners) != bits(&new_corners)
            });
            if !moved {
                continue;
            }
            off_rule += (gameplay(&loaded_corners, x, z) != Some(loaded_gh[z * w + x])) as usize;
            sculpted.set(z * w + x);
            cells += 1;
        }
    }
    let shipped_off = (0..w * w)
        .filter(|&i| gameplay(&loaded_corners, i % w, i / w).is_some_and(|v| v != loaded_gh[i]))
        .count();
    // Trees and grass on the bump keep their footing: their height (the
    // third fixed-point value) rises by the bump at their position.
    let mut lifted = 0;
    for (_, node, x, z, _) in &objs {
        let d = bump(*x, *z, bx, bz);
        if d <= 0.0 {
            continue;
        }
        let mut p = bytes(&doc, *node).to_vec();
        let y = i64::from_le_bytes(p[16..24].try_into().unwrap()) + (d * 4096.0).round() as i64;
        p[16..24].copy_from_slice(&y.to_le_bytes());
        doc.set(*node, &p, true).unwrap();
        lifted += 1;
    }
    let unbuildable = (0..w * w)
        .filter(|&i| sculpted.get(i) && !LAND.contains(&gh[i]))
        .count();
    site(
        "bump",
        bump_site,
        format!("+{BUMP_HEIGHT} m, radius {BUMP_RADIUS} m, centre {bx} {bz}"),
    );
    writeln!(
        notes,
        "bump: centre x {bx} z {bz}, +{BUMP_HEIGHT} m, radius {BUMP_RADIUS} m. \
         {vertices} terrain vertices in {height_chunks} chunks, {flag_changes} Flags changed; \
         {cells} gameplay cells recomputed ({off_rule} of them were off the rule as loaded; \
         {shipped_off} on the whole island), {unbuildable} now outside the band; \
         {lifted} trees and grass lifted"
    )
    .unwrap();

    // Paint by the plan's brush rule, a dab per vertex the stroke reaches:
    // the painted id rises toward the stroke, only colour planes ranked
    // above it fall, layer 0 and scatter planes stay. A vertex on a chunk
    // edge is painted in every chunk around it, and chunks that lack the id
    // get a new plane. Stroke 2 is written with the other insertions below,
    // because inserting moves node ids.
    let registry = anno2070::island::registry(&install).unwrap();
    let mut layers = Layers::read(&loaded, registry).unwrap();
    let stamp = |layers: &mut Layers, tex: u32, cx: f64, cz: f64| {
        for z in 0..side {
            for x in 0..side {
                let f = stroke(x as f64, z as f64, cx, cz);
                if f > 0.0 {
                    (layers.paint(x, z, tex as u8, f as f32))
                        .unwrap_or_else(|e| panic!("vertex {x},{z}: {e:?}"));
                }
            }
        }
    };
    stamp(&mut layers, tex1, s1x, s1z);
    let (p1, a1) = pending(&layers, &loaded);
    assert!(a1.is_empty(), "stroke 1 must not insert");
    anno2070::island::write_paint(&mut doc, ISLAND, &layers).unwrap();
    stamp(&mut layers, EARTH, s2x, s2z);
    let (p2, inserts) = {
        let once = Doc::parse(&doc.encode()).unwrap();
        pending(&layers, &Terrain::read(&once, ISLAND).unwrap())
    };
    let a2 = inserts.len();
    site(
        "stroke1",
        stroke1,
        format!("TextureIndex {tex1}, full to {PAINT_INNER} m, gone at {PAINT_OUTER} m"),
    );
    site(
        "stroke2",
        stroke2,
        format!("earth_01 (18), inserted into {a2} chunks"),
    );
    writeln!(
        notes,
        "stroke 1: centre x {s1x} z {s1z}, TextureIndex {tex1}, {p1} planes changed, none inserted\n\
         stroke 2: centre x {s2x} z {s2z}, earth_01 (18), {p2} planes changed, {a2} inserted \
         (chunks {:?})",
        inserts.iter().map(|k| (k % cw, k / cw)).collect::<Vec<_>>()
    )
    .unwrap();

    // Pads: gameplay heights only, in a 2x2 block, terrain left as it is.
    let mut pad_cells = Vec::new();
    for (n, &v) in PADS.iter().enumerate() {
        let x0 = pads.x0 + (n % 2) as f64 * (PAD + PAD_GAP);
        let z0 = pads.z0 + (n / 2) as f64 * (PAD + PAD_GAP);
        let r = Rect {
            x0,
            z0,
            x1: x0 + PAD,
            z1: z0 + PAD,
        };
        for (x, z) in r.cells() {
            gh[z * w + x] = v;
            pad_cells.push((z * w + x, v));
        }
        site(
            "pad",
            r,
            format!("gameplay height {v} ({:.4} m)", v as f64 / 1024.0),
        );
        writeln!(notes, "pad {v}: x {x0}..{} z {z0}..{}", x0 + PAD, z0 + PAD).unwrap();
    }
    let gh_bytes: Vec<u8> = gh.iter().flat_map(|v| v.to_le_bytes()).collect();
    doc.set(gh_node, &gh_bytes, true).unwrap();

    // The path strip: every 0.5 m cell under a 2 m x 16 m rectangle.
    let strip = Rect::around(
        (strip_site.x0 + strip_site.x1) / 2.0,
        (strip_site.z0 + strip_site.z1) / 2.0,
        STRIP.0 / 2.0,
        STRIP.1 / 2.0,
    );
    let mut strip_bits = 0;
    for (x, z) in strip.cells() {
        for q in 0..4 {
            let (i, j) = (2 * z + q / 2, 2 * x + q % 2);
            path[i * pints + j / 32] |= 1 << (j % 32);
            strip_bits += 1;
        }
    }
    let path_bytes: Vec<u8> = path.iter().flat_map(|v| v.to_le_bytes()).collect();
    doc.set(path_node, &path_bytes, true).unwrap();
    site("strip", strip, "m_PathBlockerLayer bits set".into());
    writeln!(
        notes,
        "path strip: x {}..{} z {}..{}, {strip_bits} half-metre cells blocked",
        strip.x0, strip.x1, strip.z0, strip.z1
    )
    .unwrap();
    // Stroke 2's planes are still to come.
    let changed = changed_values(&before, &doc) + p2;

    // Insertions: stroke 2 with its new paint planes, then the
    // build-blocker square as a copy of the first shape cut to four points.
    anno2070::island::write_paint(&mut doc, ISLAND, &layers).unwrap();
    let square = Rect::around(
        (square_site.x0 + square_site.x1) / 2.0,
        (square_site.z0 + square_site.z1) / 2.0,
        SQUARE / 2.0,
        SQUARE / 2.0,
    );
    let shapes = doc.root("BuildBlockerShapes").unwrap();
    let first = doc.children(shapes).next().unwrap();
    // Same winding as the shipped polygons.
    let area: f64 = {
        let p = &polys[0];
        (0..p.len())
            .map(|i| {
                let (a, b) = (p[i], p[(i + 1) % p.len()]);
                a.0 * b.1 - b.0 * a.1
            })
            .sum()
    };
    let mut corners4 = vec![
        (square.x0, square.z0),
        (square.x1, square.z0),
        (square.x1, square.z1),
        (square.x0, square.z1),
    ];
    if area < 0.0 {
        corners4.reverse();
    }
    let new = doc.append_copies(shapes, &[first]).unwrap()[0];
    let poly = doc.child(new, "Polygon").unwrap();
    let points: Vec<NodeId> = doc.children(poly).collect();
    assert!(points.len() >= 4);
    for &p in points[4..].iter().rev() {
        doc.remove(p);
    }
    let points: Vec<NodeId> = doc.children(poly).collect();
    for (&p, &(x, z)) in points.iter().zip(&corners4) {
        let mut b = Vec::with_capacity(16);
        b.extend(((x * 4096.0).round() as i64).to_le_bytes());
        b.extend(((z * 4096.0).round() as i64).to_le_bytes());
        doc.set(p, &b, true).unwrap();
    }
    site("square", square, "new BuildBlockerShapes polygon".into());
    writeln!(
        notes,
        "build-blocker square: x {}..{} z {}..{}",
        square.x0, square.x1, square.z0, square.z1
    )
    .unwrap();

    // Check what was written, from the bytes: it re-reads and re-writes the
    // same bytes, only the planned values and nodes changed, and the paint,
    // border and gameplay-height rules hold.
    let written = doc.encode();
    let back = Doc::parse(&written).unwrap();
    assert_eq!(back.encode(), written, "edited file does not re-encode");
    // A layer is TexIndexData, TextureIndex, AlphaMap, Width and Data; the
    // square is a shape, its Polygon and four points.
    assert_eq!(
        node_count(&back),
        node_count(&before) + 5 * inserts.len() + 6,
        "node count"
    );
    // Reading checks the layers are sorted and at most 24, and that
    // neighbours agree on heights and coverage along every shared edge.
    let after = Terrain::read(&back, IN_PLACE).unwrap();
    let low = rank(tex1).min(rank(EARTH));
    for k in 0..cw * ch {
        let ids = after.ids(k);
        for (layer, &t) in ids.iter().enumerate() {
            let was = (loaded.ids(k).iter())
                .position(|&l| l == t)
                .map(|l| loaded.plane(k, l));
            if was == Some(after.plane(k, layer)) {
                continue;
            }
            assert!(
                t == tex1 || t == EARTH || (t != 0 && colour(t) && rank(t) > low),
                "chunk {k}: plane {t} changed against the brush rule"
            );
            if was.is_none() {
                assert_eq!(t, EARTH, "chunk {k}: only earth is inserted");
            }
        }
        assert!(
            loaded.ids(k).iter().all(|l| ids.contains(l)),
            "chunk {k} lost a layer"
        );
    }
    let back_corners = after.corners();
    let back_grid = back
        .child(back.root("m_GOPManager").unwrap(), "m_GRIDManager")
        .unwrap();
    let back_gh: Vec<i16> = bytes(&back, back.child(back_grid, "m_HeightMap_v2").unwrap())
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&v| i16::from_le_bytes(v))
        .collect();
    let mut pad_at = vec![None; w * w];
    for &(i, v) in &pad_cells {
        pad_at[i] = Some(v);
    }
    for i in 0..w * w {
        let want = match pad_at[i] {
            Some(v) => v,
            None if sculpted.get(i) => gameplay(&back_corners, i % w, i / w).unwrap(),
            None => loaded_gh[i],
        };
        assert_eq!(back_gh[i], want, "gameplay height at {} {}", i % w, i / w);
    }
    let (_, _, back_path) = bitgrid(&back, back_grid, "m_PathBlockerLayer");
    for (n, (&a, &b)) in loaded_path.iter().zip(&back_path).enumerate() {
        for bit in 0..32 {
            if (a ^ b) >> bit & 1 == 1 {
                let (i, j) = (n / pints, n % pints * 32 + bit);
                let (x, z) = (j as f64 / 2.0, i as f64 / 2.0);
                assert!(
                    x >= strip.x0 && x < strip.x1 && z >= strip.z0 && z < strip.z1,
                    "path bit outside the strip at {x} {z}"
                );
            }
        }
    }
    let shapes_back = polygons(&back);
    assert_eq!(shapes_back.len(), polys.len() + 1);
    assert_eq!(shapes_back.last().unwrap().len(), 4);
    writeln!(
        notes,
        "values changed before insertions: {changed}; nodes {} -> {}; re-encodes; \
         borders, sorted layers, brush rule, gameplay heights and path bits checked from the bytes",
        node_count(&before),
        node_count(&back)
    )
    .unwrap();

    // Variant b: the world. Scenario 06's entry for N_M02 names the new
    // file; nothing else changes.
    let world_source = install.read_file(find(WORLD)).unwrap();
    let mut world = Doc::parse(&world_source).unwrap();
    let config = world.root("WorldConfig").unwrap();
    let list_node = world.child(config, "Islands").unwrap();
    let mut placed = Vec::new();
    for entry in world.children(list_node).collect::<Vec<_>>() {
        let island = world.child(entry, "IslandConfig").unwrap_or(entry);
        let Some(f) = world.child(island, "Filename") else {
            continue;
        };
        let name = String::from_utf8_lossy(bytes(&world, f)).into_owned();
        let file = name.rsplit(['/', '\\']).next().unwrap();
        if !file.eq_ignore_ascii_case("N_M02.isd") {
            continue;
        }
        let renamed = format!("{}{RENAMED}", &name[..name.len() - file.len()]);
        world.set(f, renamed.as_bytes(), false).unwrap();
        let num = |n: &str| text(&world, island, n);
        placed.push(format!(
            "{renamed} at PositionX {} PositionZ {} Direction {}",
            num("PositionX"),
            num("PositionZ"),
            num("Direction")
        ));
    }
    assert_eq!(placed.len(), 1, "{WORLD} should place N_M02 once");
    let world_bytes = world.encode();
    let world_back = Doc::parse(&world_bytes).unwrap();
    assert_eq!(world_back.encode(), world_bytes, "world does not re-encode");
    assert_eq!(
        changed_values(&Doc::parse(&world_source).unwrap(), &world_back),
        1
    );
    writeln!(notes, "variant b: {WORLD} places {}", placed[0]).unwrap();

    let renamed_path = IN_PLACE.replace("N_M02.isd", RENAMED);
    for (file, data) in [
        (out.join("a").join(IN_PLACE), &written),
        (out.join("b").join(&renamed_path), &written),
        (out.join("b").join(WORLD), &world_bytes),
    ] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, data).unwrap();
        writeln!(notes, "wrote {}", file.display()).unwrap();
    }
    std::fs::write(out.join("edit.txt"), &notes).unwrap();
    std::fs::write(out.join("sites.tsv"), &sites).unwrap();
    print!("{notes}");
}

// Values that differ between two documents of the same shape, walked in
// document order side by side.
fn changed_values(a: &Doc, b: &Doc) -> usize {
    let mut stack: Vec<(NodeId, NodeId)> = a.roots().zip(b.roots()).collect();
    let mut n = 0;
    while let Some((x, y)) = stack.pop() {
        assert_eq!(a.name(x), b.name(y), "shape changed");
        if bytes(a, x) != bytes(b, y) {
            n += 1;
        }
        stack.extend(a.children(x).zip(b.children(y)));
    }
    n
}

/// Elements in the whole document, at every depth.
fn node_count(doc: &Doc) -> usize {
    fn under(doc: &Doc, id: NodeId) -> usize {
        1 + doc.children(id).map(|c| under(doc, c)).sum::<usize>()
    }
    doc.roots().map(|r| under(doc, r)).sum()
}
