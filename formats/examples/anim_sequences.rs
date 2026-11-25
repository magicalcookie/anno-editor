// Compares each cfg sequence's animation spans (type-0 track elements,
// `EndTime - StartTime`) with the animation they play: its rdm duration,
// the `<Animations>` entry's `Scale` and `LoopCount`. The game's ANIMATION
// config derives its play length as `duration * Scale * max(LoopCount, 1)`
// (docs/model-animation.md); this counts how many shipped spans that
// predicts, and lists the rest.
//
//   cargo run --release -p anno-formats --example anim_sequences
//
// Reads ANNO117_DIR (else ANNO1800_DIR).

use std::collections::BTreeMap;

use anno_formats::rdm::Animation;
use anno_formats::vfs::{FileId, Install, install_dir};
use anno_formats::xml::{Doc, Id};
use anyhow::{Context, Result};

struct Clip {
    file: String,
    loops: u32,
    scale: f32,
}

fn main() -> Result<()> {
    let dir = install_dir("ANNO117_DIR")
        .or_else(|| install_dir("ANNO1800_DIR"))
        .context("set ANNO117_DIR or ANNO1800_DIR")?;
    let install = Install::open(dir, &["data"])?;
    let mut durations: BTreeMap<String, Option<u32>> = BTreeMap::new();
    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    let mut scales: BTreeMap<String, usize> = BTreeMap::new();
    let mut odd = Vec::new();
    for i in 0..install.file_count() {
        let id = FileId(i as u32);
        let path = install.file_path(id);
        if !path.ends_with(".cfg") || !path.starts_with("data/") {
            continue;
        }
        let Ok(bytes) = install.read_file(id) else {
            continue;
        };
        let doc = Doc::parse(&bytes);
        let Some(models) = find(&doc, Id::ROOT, "Models") else {
            continue;
        };
        let models: Vec<Vec<Clip>> = configs(&doc, models)
            .map(|m| {
                let Some(list) = doc.child(m, "Animations") else {
                    return Vec::new();
                };
                configs(&doc, list)
                    .map(|a| Clip {
                        file: text(&doc, a, "FileName").replace(".fbx", ".rdm"),
                        loops: text(&doc, a, "LoopCount").parse().unwrap_or(0),
                        scale: text(&doc, a, "Scale").parse().unwrap_or(1.),
                    })
                    .collect()
            })
            .collect();
        let Some(sequences) = find(&doc, Id::ROOT, "Sequences") else {
            continue;
        };
        for seq in configs(&doc, sequences) {
            let looped = text(&doc, seq, "Looped");
            let speed = text(&doc, seq, "Speed");
            for track in doc.children(seq).filter(|&c| doc.name(c) == "Track") {
                for e in doc
                    .children(track)
                    .filter(|&c| doc.name(c) == "TrackElement")
                {
                    if text(&doc, e, "Type") != "0" {
                        continue;
                    }
                    let num = |k: &str| text(&doc, e, k).parse::<i64>().unwrap_or(-1);
                    let span = num("EndTime") - num("StartTime");
                    let clip = (models.get(num("ModelID") as usize))
                        .and_then(|m| m.get(num("AnimationID") as usize));
                    let Some(clip) = clip else {
                        *tally.entry("index out of range").or_default() += 1;
                        continue;
                    };
                    *scales.entry(format!("{}", clip.scale)).or_default() += 1;
                    let ms = *durations.entry(clip.file.clone()).or_insert_with(|| {
                        let a = install.read(&clip.file).ok()?;
                        let a = Animation::parse(&a).ok()?;
                        Some((a.duration * 1000.).round() as u32)
                    });
                    let Some(ms) = ms else {
                        *tally.entry("animation does not ship").or_default() += 1;
                        continue;
                    };
                    let loops = clip.loops.max(1) as f32;
                    let predicted = (ms as f32 * clip.scale * loops).round() as i64;
                    let key = match span {
                        s if (s - predicted).abs() <= 2 && clip.scale != 1. => {
                            "= duration * Scale * loops, Scale != 1"
                        }
                        s if (s - predicted).abs() <= 2 && clip.loops > 1 => {
                            "= duration * loops, LoopCount > 1"
                        }
                        s if (s - predicted).abs() <= 2 => "= duration",
                        s if s < predicted => "shorter",
                        _ => "longer",
                    };
                    *tally.entry(key).or_default() += 1;
                    if key == "longer" || (key == "shorter" && odd.len() < 400) {
                        odd.push(format!(
                            "{key:8} span {span:6} of {ms:6} ms x {} x {} looped {looped} speed {speed}: {path} -> {}",
                            clip.scale, clip.loops, clip.file
                        ));
                    }
                }
            }
        }
    }
    for (k, n) in &tally {
        println!("{n:7}  {k}");
    }
    let mut scales: Vec<_> = scales.into_iter().collect();
    scales.sort_by_key(|s| std::cmp::Reverse(s.1));
    println!(
        "Scale of played clips: {:?}",
        &scales[..scales.len().min(12)]
    );
    let longer: Vec<_> = odd.iter().filter(|o| o.starts_with("longer")).collect();
    println!("{} longer, first 40:", longer.len());
    for o in longer.iter().take(40) {
        println!("  {o}");
    }
    println!("shorter, first 15:");
    for o in odd.iter().filter(|o| o.starts_with("shorter")).take(15) {
        println!("  {o}");
    }
    Ok(())
}

/// The first element named `name` under `at`, depth first.
fn find(doc: &Doc, at: Id, name: &str) -> Option<Id> {
    for c in doc.children(at) {
        if doc.name(c) == name {
            return Some(c);
        }
        if let Some(f) = find(doc, c, name) {
            return Some(f);
        }
    }
    None
}

fn configs<'a>(doc: &'a Doc, list: Id) -> impl Iterator<Item = Id> + 'a {
    doc.children(list).filter(move |&c| doc.name(c) == "Config")
}

fn text<'a>(doc: &'a Doc, at: Id, name: &str) -> &'a str {
    doc.child(at, name)
        .map(|c| doc.text(c).trim())
        .unwrap_or("")
}
