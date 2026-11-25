// Every element path in a set of 2070 documents (`.isd`, `.www`): how many
// files and nodes have it, what it holds, and the sizes of its binary
// values. The raw material of the field map.
//
//   cargo run --release -p anno-formats --example isd_fields -- <dir or files>...

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anno_formats::isd::{Doc, NodeId, Value};

#[derive(Default)]
struct Field {
    files: u32,
    nodes: u64,
    text: u64,
    blobs: u64,
    empty: u64,
    parents: u64,
    blob_sizes: BTreeMap<u32, u64>,
    texts: BTreeMap<String, u64>,
    last_file: usize,
}

fn walk(
    doc: &Doc,
    id: NodeId,
    path: &mut String,
    file: usize,
    fields: &mut BTreeMap<String, Field>,
) {
    let len = path.len();
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(&String::from_utf8_lossy(doc.name(id)));
    let f = fields.entry(path.clone()).or_default();
    if f.last_file != file + 1 {
        f.files += 1;
        f.last_file = file + 1;
    }
    f.nodes += 1;
    match doc.value(id) {
        Value::Empty => f.empty += 1,
        Value::Text(t) => {
            f.text += 1;
            if f.texts.len() < 12 || f.texts.contains_key(&*String::from_utf8_lossy(t)) {
                *f.texts
                    .entry(String::from_utf8_lossy(t).into_owned())
                    .or_default() += 1;
            }
        }
        Value::Blob(b) => {
            f.blobs += 1;
            if f.blob_sizes.len() < 12 || f.blob_sizes.contains_key(&(b.len() as u32)) {
                *f.blob_sizes.entry(b.len() as u32).or_default() += 1;
            }
        }
        Value::Children => {
            f.parents += 1;
            for c in doc.children(id) {
                walk(doc, c, path, file, fields);
            }
        }
    }
    path.truncate(len);
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut items: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        items.sort();
        for p in items {
            collect(&p, out);
        }
    } else if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("isd") || e.eq_ignore_ascii_case("www"))
    {
        out.push(path.to_path_buf());
    }
}

fn main() {
    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        collect(Path::new(&arg), &mut files);
    }
    let mut fields = BTreeMap::new();
    for (n, file) in files.iter().enumerate() {
        let bytes = std::fs::read(file).unwrap();
        let doc = Doc::parse(&bytes).unwrap_or_else(|e| panic!("{}: {e:#}", file.display()));
        let mut path = String::new();
        for root in doc.roots() {
            walk(&doc, root, &mut path, n, &mut fields);
        }
    }
    println!("{} files", files.len());
    for (path, f) in &fields {
        let mut what = Vec::new();
        if f.parents > 0 {
            what.push(format!("{} lists", f.parents));
        }
        if f.text > 0 {
            let some: Vec<String> = f
                .texts
                .iter()
                .take(6)
                .map(|(t, n)| format!("{t:?}x{n}"))
                .collect();
            what.push(format!("{} text [{}]", f.text, some.join(" ")));
        }
        if f.blobs > 0 {
            let some: Vec<String> = f
                .blob_sizes
                .iter()
                .take(6)
                .map(|(s, n)| format!("{s}x{n}"))
                .collect();
            what.push(format!("{} blobs [{}]", f.blobs, some.join(" ")));
        }
        if f.empty > 0 {
            what.push(format!("{} empty", f.empty));
        }
        println!(
            "{path}\t{} files\t{} nodes\t{}",
            f.files,
            f.nodes,
            what.join("; ")
        );
    }
}
