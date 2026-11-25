// Every shipped island's grids, terrain heights, object positions and
// blocker polygons as flat files, plus each world's island list. The spike
// worked out the path-blocker layers from these (spike-2070-fields.md).
//
//   ANNO2070_DIR=... cargo run --release -p anno2070 --example grids -- <out dir>

use std::io::Write;

use anno_formats::isd::terrain::Terrain;
use anno_formats::isd::{Doc, NodeId, Value};
use anno_formats::vfs::{FileId, Install};

fn text(doc: &Doc, id: NodeId, name: &str) -> Option<i64> {
    match doc.value(doc.child(id, name)?) {
        Value::Text(t) => std::str::from_utf8(t).ok()?.parse().ok(),
        _ => None,
    }
}

fn blob<'a>(doc: &'a Doc, id: NodeId, name: &str) -> Option<&'a [u8]> {
    match doc.value(doc.child(id, name)?) {
        Value::Blob(b) => Some(b),
        _ => None,
    }
}

fn bitgrid(doc: &Doc, grid: NodeId, name: &str) -> Option<(i64, i64, i64, Vec<u8>)> {
    let layer = doc.child(grid, name)?;
    let g = doc.child(layer, "BitGrid")?;
    Some((
        text(doc, g, "m_XSize")?,
        text(doc, g, "m_YSize")?,
        text(doc, g, "m_IntXSize")?,
        blob(doc, g, "m_BitGrid")?.to_vec(),
    ))
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
    let mut index = std::fs::File::create(out.join("index.tsv")).unwrap();
    let mut n = 0;
    let mut worlds =
        std::io::BufWriter::new(std::fs::File::create(out.join("worlds.tsv")).unwrap());
    for i in 0..install.file_count() {
        let id = FileId(i as u32);
        let path = install.file_path(id).to_owned();
        if path.ends_with(".www") {
            // World, island key, file, x, z, direction, type.
            let bytes = install.read_file(id).unwrap();
            let doc = Doc::parse(&bytes).unwrap();
            let Some(config) = doc.root("WorldConfig") else {
                continue;
            };
            let (ww, wh) = (text(&doc, config, "Width"), text(&doc, config, "Height"));
            let Some(list) = doc.child(config, "Islands") else {
                continue;
            };
            for entry in doc.children(list) {
                let island = doc.child(entry, "IslandConfig").unwrap_or(entry);
                let file = match doc.child(island, "Filename").map(|f| doc.value(f)) {
                    Some(Value::Text(t)) => String::from_utf8_lossy(t).into_owned(),
                    _ => "-".into(),
                };
                let num = |name: &str| text(&doc, island, name).unwrap_or(-1);
                writeln!(
                    worlds,
                    "{path}\t{ww:?}x{wh:?}\t{}\t{file}\t{}\t{}\t{}\t{}",
                    String::from_utf8_lossy(doc.name(entry)),
                    num("PositionX"),
                    num("PositionZ"),
                    num("Direction"),
                    num("Type"),
                )
                .unwrap();
            }
            continue;
        }
        if !path.ends_with(".isd") {
            continue;
        }
        let bytes = install.read_file(id).unwrap();
        let doc = Doc::parse(&bytes).unwrap();
        let d = out.join(format!("{n:03}"));
        std::fs::create_dir_all(&d).unwrap();
        let top = |name: &str| {
            doc.roots()
                .find(|&r| doc.name(r) == name.as_bytes())
                .and_then(|r| match doc.value(r) {
                    Value::Text(t) => std::str::from_utf8(t).ok()?.parse::<i64>().ok(),
                    _ => None,
                })
        };
        let w = top("Width").unwrap();
        let sea = top("SeaLevel").unwrap_or(0);
        let clime = top("Clime").unwrap_or(-1);

        let gop = doc.root("m_GOPManager").unwrap();
        let grid = doc.child(gop, "m_GRIDManager").unwrap();
        let (gh_kind, gh) = match blob(&doc, grid, "m_HeightMap_v2") {
            Some(b) => ("v2", b),
            None => ("v1", blob(&doc, grid, "m_HeightMap").unwrap()),
        };
        std::fs::write(d.join("gh.i16"), gh).unwrap();
        let mut meta = format!("{path}\t{w}\t{sea}\t{clime}\t{gh_kind}\t{}", gh.len());
        for (name, file) in [
            ("m_PathBlockerLayer", "path.u32"),
            ("m_HeightPathBlockerLayer", "hpath.u32"),
            ("m_SubmarineBlockerLayer", "sub.u32"),
        ] {
            match bitgrid(&doc, grid, name) {
                Some((x, y, ints, bits)) => {
                    std::fs::write(d.join(file), &bits).unwrap();
                    meta += &format!("\t{x}x{y}/{ints}");
                }
                None => meta += "\t-",
            }
        }
        if let Some(s) = blob(&doc, gop, "m_StreetGrid") {
            std::fs::write(d.join("street.u8"), s).unwrap();
        }

        // Terrain heights at 1 m, (w+1)², NaN where a chunk holds none.
        let view = Terrain::read(&doc, &path).unwrap();
        let corners = view.corners();
        let (side, rows) = corners.size();
        let terrain: Vec<f32> = (0..side * rows)
            .map(|i| corners.get(i % side, i / side).unwrap_or(f32::NAN))
            .collect();
        // Chunks stored finer than 1 m: `VertexResolution` 5 (0.5 m) or 6.
        let map = doc.child(doc.root("Terrain").unwrap(), "ChunkMap").unwrap();
        let fine = (doc.children(map))
            .filter(|&e| doc.name(e) == b"Element")
            .filter(|&e| text(&doc, e, "VertexResolution").is_some_and(|r| r > 4))
            .count();
        let tb: Vec<u8> = terrain.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(d.join("terrain.f32"), tb).unwrap();
        meta += &format!("\t{side}\t{fine}");

        // Objects: kind, GUID, x, y, z in metres.
        let mut objects =
            std::io::BufWriter::new(std::fs::File::create(d.join("objects.tsv")).unwrap());
        if let Some(objs) = doc.child(gop, "Objects") {
            for group in doc.children(objs) {
                let kind = String::from_utf8_lossy(doc.name(group)).into_owned();
                let list = |g: NodeId| -> Vec<NodeId> {
                    match doc.child(g, "Objects") {
                        Some(o) => doc.children(o).collect(),
                        None => doc.children(g).collect(),
                    }
                };
                for o in list(group) {
                    let Some(pos) = blob(&doc, o, "m_Position") else {
                        continue;
                    };
                    let c = |i: usize| {
                        i64::from_le_bytes(pos[i * 8..i * 8 + 8].try_into().unwrap()) as f64
                            / 4096.0
                    };
                    let guid = text(&doc, o, "m_GUID").unwrap_or(-1);
                    writeln!(objects, "{kind}\t{guid}\t{}\t{}\t{}", c(0), c(1), c(2)).unwrap();
                }
            }
        }
        // Blocker polygons: kind, polygon number, x, z in metres.
        let mut polys =
            std::io::BufWriter::new(std::fs::File::create(d.join("polys.tsv")).unwrap());
        for kind in ["BuildBlockerShapes", "CamBlockerShapes"] {
            let Some(shapes) = doc.root(kind) else {
                continue;
            };
            for (k, shape) in doc.children(shapes).enumerate() {
                let Some(poly) = doc.child(shape, "Polygon") else {
                    continue;
                };
                for pt in doc.children(poly) {
                    let Value::Blob(b) = doc.value(pt) else {
                        continue;
                    };
                    let c = |i: usize| {
                        i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap()) as f64 / 4096.0
                    };
                    writeln!(polys, "{kind}\t{k}\t{}\t{}", c(0), c(1)).unwrap();
                }
            }
        }
        writeln!(index, "{n:03}\t{meta}").unwrap();
        n += 1;
    }
    eprintln!("{n} islands");
}
