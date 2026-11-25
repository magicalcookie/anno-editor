// Counts the `<Clothes>` entries of every shipped cfg of 117 and 1800: their
// fields and values, the files they name, and where they sit. With
// CLOTH_DUMP=<dir>, also copies each such cfg and the files its cloth names
// there, for reading by hand. The numbers in docs/cloth-format.md come from
// here.
//
//   ANNO117_DIR=… ANNO1800_DIR=… cargo run --release -p anno-formats --example cloth_survey

use std::collections::BTreeMap;

use anno_formats::model::mesh_path;
use anno_formats::rdm::Rdm;
use anno_formats::vfs::{FileId, Install};
use anno_formats::xml::{Doc, Id};

fn main() {
    let dump = std::env::var_os("CLOTH_DUMP").map(std::path::PathBuf::from);
    for var in ["ANNO117_DIR", "ANNO1800_DIR"] {
        let Some(dir) = std::env::var_os(var) else {
            continue;
        };
        let install = Install::open(dir, &["data"]).unwrap();
        let mut fields: BTreeMap<String, (usize, BTreeMap<String, usize>)> = BTreeMap::new();
        let (mut cfgs, mut entries) = (0, 0);
        let mut examples = Vec::new();
        let mut meshes: BTreeMap<String, usize> = BTreeMap::new();
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            if !install.file_name(id).ends_with(".cfg") {
                continue;
            }
            let path = install.file_path(id);
            if !path.starts_with("data/") {
                continue;
            }
            let bytes = install.read_file(id).unwrap();
            if !bytes.windows(9).any(|w| w == b"<Clothes>") {
                continue;
            }
            let doc = Doc::parse(&bytes);
            let mut found = 0;
            for clothes in all(&doc, Id::ROOT, "Clothes") {
                for c in doc.children(clothes) {
                    found += 1;
                    walk(&doc, c, "", &mut fields);
                    let file = doc.child(c, "FileName").map_or("", |f| doc.text(f));
                    let mesh = mesh_path(file);
                    let key = match install.read(&mesh) {
                        Err(_) => "mesh: missing".to_string(),
                        Ok(bytes) => {
                            if let Some(dump) = &dump {
                                let out = dump.join(var).join(&mesh);
                                std::fs::create_dir_all(out.parent().unwrap()).unwrap();
                                std::fs::write(&out, &bytes).unwrap();
                            }
                            match Rdm::parse(&bytes) {
                                Ok(r) => format!(
                                    "mesh: {} verts<=256 {} skinned {}",
                                    r.format,
                                    r.positions.len() <= 256,
                                    r.skinned
                                ),
                                Err(e) => format!("mesh: unreadable {e}"),
                            }
                        }
                    };
                    *meshes.entry(key).or_default() += 1;
                }
            }
            if found == 0 {
                continue;
            }
            cfgs += 1;
            entries += found;
            if examples.len() < 40 {
                examples.push(format!("{found} {path}"));
            }
            if let Some(dump) = &dump {
                let out = dump.join(var).join(&path);
                std::fs::create_dir_all(out.parent().unwrap()).unwrap();
                std::fs::write(&out, &bytes).unwrap();
            }
        }
        println!("== {var}: {cfgs} cfgs, {entries} cloth entries");
        for e in &examples {
            println!("  {e}");
        }
        for (k, n) in &meshes {
            println!("{n:6} {k}");
        }
        for (k, (n, values)) in &fields {
            let mut top: Vec<_> = values.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1));
            let shown: Vec<_> = (top.iter().take(6))
                .map(|(v, n)| format!("{v:?}×{n}"))
                .collect();
            println!("{n:6} {k} [{} values] {}", values.len(), shown.join(" "));
        }
    }
}

fn all(doc: &Doc, id: Id, name: &str) -> Vec<Id> {
    let mut out = Vec::new();
    for c in doc.children(id) {
        if doc.name(c) == name {
            out.push(c);
        } else {
            out.extend(all(doc, c, name));
        }
    }
    out
}

// Leaf paths below one cloth entry, with value counts; list items share
// their parent's path.
fn walk(
    doc: &Doc,
    id: Id,
    prefix: &str,
    fields: &mut BTreeMap<String, (usize, BTreeMap<String, usize>)>,
) {
    let name = doc.name(id);
    let path = match prefix {
        "" => name.to_string(),
        p => format!("{p}/{name}"),
    };
    if doc.is_leaf(id) {
        let e = fields.entry(path).or_default();
        e.0 += 1;
        let v = doc.text(id);
        let v = if v.len() > 60 { &v[..60] } else { v };
        *e.1.entry(v.to_string()).or_default() += 1;
        return;
    }
    for c in doc.children(id) {
        walk(doc, c, &path, fields);
    }
}
