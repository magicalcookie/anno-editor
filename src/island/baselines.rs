// Baselines of what trunk does today, for the re-architecture tracks to keep
// equal: the bytes a fixed edit sequence saves, undoes and reopens to, the
// bytes its undo history holds on the continental island, and the frames
// island_shots draws. Deterministic records only (bytes, digests, counts):
// builds running alongside cannot move them, wall-clock numbers would.
//
// Each golden is a `const` string below, one `key = value` line per fact.
// A test fails listing every key that differs. To regenerate after an
// intended change, or after a game update moves the shipped bytes:
//
//   ANNO117_DIR=… ANNO1800_DIR=… ANNO_EDITOR_BLESS=1 cargo test -p anno-editor
//     island::baselines -- --nocapture --test-threads=1
//
// and paste the printed constants over the old ones. Without an install a
// test prints why it skipped and passes. Frames are per GPU adapter and
// driver (`FRAMES`); on another the frame test skips with a note.
// ANNO_EDITOR_BASELINE_DUMP=dir writes each frame there as a PNG, to see
// where a digest that differs differs.
use super::test_support::{MODERATE_M_01, PIRATE, SMALL, install, install_1800, reopen, staged};
use super::{Drag, IslandEditor, Tool};
use anno_formats::{rda, vfs::Install};
use gpui_editor_kit::heightfield::Stroke;
use std::collections::BTreeMap;
use std::sync::Arc;

fn bless() -> bool {
    std::env::var("ANNO_EDITOR_BLESS").is_ok_and(|v| v == "1")
}

/// FNV-1a, 64 bit: the same on every Rust and crate version, and easy to
/// recompute outside the tests.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn digest(bytes: &[u8]) -> String {
    format!("len {} fnv {:016x}", bytes.len(), fnv(bytes))
}

/// Facts in the order taken, compared with a golden by key.
#[derive(Default)]
struct Record(Vec<(String, String)>);

impl Record {
    fn put(&mut self, key: impl Into<String>, value: impl std::fmt::Display) {
        let key = key.into();
        debug_assert!(!self.0.iter().any(|(k, _)| *k == key), "{key} twice");
        self.0.push((key, value.to_string()));
    }

    /// Fails naming each key whose value differs from `golden`'s or that
    /// only one side has. Under ANNO_EDITOR_BLESS=1 prints `name` as a
    /// fresh constant instead.
    fn check(&self, name: &str, golden: &str) {
        if bless() {
            let mut text = format!("const {name}: &str = r#\"\n");
            for (k, v) in &self.0 {
                text += &format!("{k} = {v}\n");
            }
            eprintln!("{text}\"#;");
            return;
        }
        let want: BTreeMap<&str, &str> = (golden.lines())
            .filter_map(|l| l.split_once(" = "))
            .collect();
        let have: BTreeMap<&str, &str> = (self.0.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut diffs = Vec::new();
        for (k, v) in &want {
            match have.get(k) {
                Some(h) if h == v => {}
                h => diffs.push(format!(
                    "{k}\n      recorded {v}\n      now      {}",
                    h.unwrap_or(&"(missing)")
                )),
            }
        }
        for (k, h) in &have {
            if !want.contains_key(k) {
                diffs.push(format!("{k}\n      recorded (missing)\n      now      {h}"));
            }
        }
        assert!(
            diffs.is_empty(),
            "{name}: {} of {} records differ from the baseline (ANNO_EDITOR_BLESS=1 prints \
             fresh ones):\n  {}",
            diffs.len(),
            want.len().max(have.len()),
            diffs.join("\n  ")
        );
    }

    /// `check` for byte counts that may only fall: fails naming each key
    /// now over its record, or held now with none; one under its record
    /// passes with a note to bless the lower count.
    fn check_at_most(&self, name: &str, golden: &str) {
        if bless() {
            return self.check(name, golden);
        }
        let want: BTreeMap<&str, usize> = (golden.lines())
            .filter_map(|l| l.split_once(" = "))
            .map(|(k, v)| (k, v.parse().unwrap()))
            .collect();
        let (mut over, mut under) = (Vec::new(), Vec::new());
        for (k, v) in &self.0 {
            let now: usize = v.parse().unwrap();
            match want.get(k.as_str()) {
                None => over.push(format!("{k}: {now} bytes, none recorded")),
                Some(&w) if now > w => {
                    over.push(format!("{k}: {now} bytes, recorded {w} (+{})", now - w))
                }
                Some(&w) if now < w => under.push(format!("{k}: {now} bytes, recorded {w}")),
                Some(_) => {}
            }
        }
        for (k, w) in &want {
            if !self.0.iter().any(|(h, _)| h == k) {
                under.push(format!("{k}: none held, recorded {w}"));
            }
        }
        if !under.is_empty() {
            eprintln!(
                "{name}: under the record (ANNO_EDITOR_BLESS=1 to lower it):\n  {}",
                under.join("\n  ")
            );
        }
        assert!(
            over.is_empty(),
            "{name}: {} counts over the baseline:\n  {}",
            over.len(),
            over.join("\n  ")
        );
    }
}

// --- The fixed edit sequence ------------------------------------------------

/// One undo step each, through the tool's own path as the app runs it.
const STEPS: [&str; 3] = ["sculpt", "paint", "place"];

/// The middle of the island's ground, where the sequence edits.
fn middle(editor: &IslandEditor) -> [f32; 2] {
    let g = editor.island.doc.field.grid;
    let [w, d] = g.size();
    [
        (g.origin[0] + w / 2.).round(),
        (g.origin[1] + d / 2.).round(),
    ]
}

/// A drag of `tool` 24 m across `at`, along x or z, released: the drag's
/// dabs, then `finish_drag` checkpoints the gesture as one step.
fn stroke(editor: &mut IslandEditor, tool: Tool, at: [f32; 2], along_x: bool) {
    editor.tool = tool;
    editor.brush.radius = 6.;
    editor.brush.strength = 1.;
    let mut drag = Drag::Brush {
        stroke: Stroke::default(),
        before: Box::new(editor.island.doc.clone()),
        level: editor.island.doc.field.height(at),
        changed: false,
        hold: None,
    };
    for i in 0..=12 {
        let d = 2. * i as f32 - 12.;
        let p = if along_x {
            [at[0] + d, at[1]]
        } else {
            [at[0], at[1] + d]
        };
        let y = editor.island.doc.field.height(p);
        editor.continue_brush(&mut drag, [p[0], y, p[1]], false);
    }
    editor.views[0].drag = Some(drag);
    editor.finish_drag(0);
}

/// Runs step `k` of `STEPS` at `at`.
fn edit(editor: &mut IslandEditor, k: usize, at: [f32; 2]) {
    match STEPS[k] {
        "sculpt" => stroke(editor, Tool::Raise, at, true),
        "paint" => {
            // The set's last material, as undo_paint_bench paints.
            editor.paint = (editor.palette.as_ref())
                .and_then(|p| p.materials.keys().last().copied())
                .unwrap_or(31);
            stroke(editor, Tool::Paint, at, false);
        }
        _ => {
            assert!(
                !editor.island.doc.meshes.is_empty(),
                "no prop model to place"
            );
            editor.tool = Tool::Place;
            editor.place = Some(0);
            let y = editor.island.doc.field.height(at);
            editor.place_at([at[0], y, at[1]]);
        }
    }
}

/// What the document shows at the edit, as decoded values: heights (with
/// their bits, saves quantize them), the dominant ground material, and the
/// props and objects.
fn probe(editor: &IslandEditor, at: [f32; 2]) -> Vec<(String, String)> {
    let doc = &editor.island.doc;
    let mut out = Vec::new();
    for dz in [-6., 0., 6.] {
        for dx in [-6., 0., 6.] {
            let p = [at[0] + dx, at[1] + dz];
            let h = doc.field.height(p);
            out.push((
                format!("height {},{}", p[0], p[1]),
                format!("{h} ({:08x})", h.to_bits()),
            ));
            if let Some(m) = &doc.materials {
                out.push((
                    format!("material {},{}", p[0], p[1]),
                    m.dominant(p).to_string(),
                ));
            }
        }
    }
    out.push(("props".into(), doc.props.len().to_string()));
    if let Some(p) = doc.props.last() {
        let mesh = doc.meshes.get(p.source.file as usize);
        out.push((
            "last prop".into(),
            format!("{mesh:?} at {:?}", p.source.position),
        ));
    }
    out.push(("objects".into(), doc.objects.len().to_string()));
    out
}

/// The members of island archive `bytes`, by stored path.
fn members(path: &str, bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let archive = rda::Archive::parse(bytes.to_vec(), path).unwrap();
    (archive.entries().iter())
        .map(|e| (e.key().into_owned(), archive.read(e).unwrap()))
        .collect()
}

/// `bytes` against what the install ships there.
fn versus(shipped: Option<&[u8]>, bytes: &[u8]) -> &'static str {
    match shipped {
        None => "new",
        Some(s) if s == bytes => "as shipped",
        Some(_) => "changed",
    }
}

/// Each file of save `step` with its digest and how it compares with the
/// shipped one, and inside island archives the same per member: a member
/// recorded "as shipped" is one the save must leave byte for byte.
fn put_files(
    out: &mut Record,
    step: &str,
    files: &BTreeMap<String, Vec<u8>>,
    shipped: &BTreeMap<String, Option<Vec<u8>>>,
) {
    for (path, bytes) in files {
        let original = shipped[path].as_deref();
        out.put(
            format!("{step} {path}"),
            format!("{} {}", digest(bytes), versus(original, bytes)),
        );
        if !path.ends_with(".a7m") {
            continue;
        }
        let ours = members(path, bytes);
        let theirs = original.map(|o| members(path, o)).unwrap_or_default();
        for (m, b) in &ours {
            let v = versus(theirs.get(m).map(Vec::as_slice), b);
            out.put(
                format!("{step} {path} :: {m}"),
                format!("{} {v}", digest(b)),
            );
        }
        for m in theirs.keys().filter(|m| !ours.contains_key(*m)) {
            out.put(format!("{step} {path} :: {m}"), "dropped");
        }
    }
}

/// The shipped bytes at every path either save wrote, read past the
/// mounted project, so a game update shows as its own lines.
fn shipped(
    install: &Install,
    saves: &[&BTreeMap<String, Vec<u8>>],
) -> BTreeMap<String, Option<Vec<u8>>> {
    (saves.iter().flat_map(|s| s.keys()))
        .map(|p| {
            let bytes = install.file(p).map(|id| install.read_file(id).unwrap());
            (p.clone(), bytes)
        })
        .collect()
}

// A sculpt, paint and place sequence on one 117 and one 1800 island: save,
// undo all, save, reopen. Every file and island member of both saves is
// recorded with its digest and whether it is the shipped one; the edited
// values decoded after the first save's reopen are recorded; and the
// second save's reopen must decode as the island loaded.
#[test]
fn a_fixed_edit_sequence_saves_undoes_and_reopens_to_recorded_bytes() {
    for (game, install, path, golden) in [
        ("117", install(), PIRATE, SEQUENCE_117),
        ("1800", install_1800(), MODERATE_M_01, SEQUENCE_1800),
    ] {
        let Some(install) = install else {
            eprintln!("{game}: skipped, no install (ANNO{game}_DIR)");
            continue;
        };
        let mut out = Record::default();
        let mut editor = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let at = middle(&editor);
        out.put("edited at", format!("{},{}", at[0], at[1]));
        let loaded = probe(&editor, at);
        for (k, v) in &loaded {
            out.put(format!("loaded {k}"), v);
        }
        for k in 0..STEPS.len() {
            edit(&mut editor, k, at);
        }
        out.put("paint id", editor.paint);
        let (_config, mut project) = staged(&install, &mut editor);
        let first = project.files.clone();
        let reopened = reopen(&install, &mut project, path);
        for (k, v) in probe(&reopened, at) {
            out.put(format!("edited {k}"), v);
        }
        drop(reopened);
        let mut steps = 0;
        while editor.undo(false) {
            steps += 1;
        }
        out.put("undo steps", steps);
        editor.stage(&install, &mut project).unwrap();
        let second = project.files.clone();
        let back = reopen(&install, &mut project, path);
        let restored = probe(&back, at);
        for ((k, was), (_, now)) in loaded.iter().zip(&restored) {
            assert_eq!(was, now, "{game}: {k} after undo all, save and reopen");
        }
        assert_eq!(loaded.len(), restored.len(), "{game}: probes after undo");
        let shipped = shipped(&install, &[&first, &second]);
        for (p, bytes) in &shipped {
            let v = bytes.as_deref().map_or("none".into(), digest);
            out.put(format!("shipped {p}"), v);
            if let Some(bytes) = bytes.as_deref().filter(|_| p.ends_with(".a7m")) {
                for (m, b) in members(p, bytes) {
                    out.put(format!("shipped {p} :: {m}"), digest(&b));
                }
            }
        }
        put_files(&mut out, "save", &first, &shipped);
        put_files(&mut out, "undone save", &second, &shipped);
        out.check(&format!("SEQUENCE_{game}"), golden);
    }
}

const SEQUENCE_117: &str = r#"
edited at = 160,160
loaded height 154,154 = 5.9140625 (40bd4000)
loaded material 154,154 = 7
loaded height 160,154 = 1.765625 (3fe20000)
loaded material 160,154 = 7
loaded height 166,154 = 6.2265625 (40c74000)
loaded material 166,154 = 7
loaded height 154,160 = 3.5859375 (40658000)
loaded material 154,160 = 7
loaded height 160,160 = 8.796875 (410cc000)
loaded material 160,160 = 7
loaded height 166,160 = 9.675781 (411ad000)
loaded material 166,160 = 7
loaded height 154,166 = 4.421875 (408d8000)
loaded material 154,166 = 7
loaded height 160,166 = 9.800781 (411cd000)
loaded material 160,166 = 7
loaded height 166,166 = 6.9140625 (40dd4000)
loaded material 166,166 = 7
loaded props = 1290
loaded last prop = Some("data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_b.cfg") at [149.87488, -0.5647702, 248.27905]
loaded objects = 3731
paint id = 25
edited height 154,154 = 5.9140625 (40bd4000)
edited material 154,154 = 7
edited height 160,154 = 1.765625 (3fe20000)
edited material 160,154 = 25
edited height 166,154 = 6.2265625 (40c74000)
edited material 166,154 = 7
edited height 154,160 = 7.5859375 (40f2c000)
edited material 154,160 = 7
edited height 160,160 = 12.796875 (414cc000)
edited material 160,160 = 25
edited height 166,160 = 13.675781 (415ad000)
edited material 166,160 = 7
edited height 154,166 = 4.421875 (408d8000)
edited material 154,166 = 7
edited height 160,166 = 9.800781 (411cd000)
edited material 160,166 = 25
edited height 166,166 = 6.9140625 (40dd4000)
edited material 166,166 = 7
edited props = 1291
edited last prop = Some("data/base/graphics/library/environment/props/vegetation/trees/tree_dead_02_b/tree_dead_02_b.cfg") at [160.0, 12.796875, 160.0]
edited objects = 3731
undo steps = 3
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/activemapimage.png = len 310292 fnv 1c2fe38bd8a640d0
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/activemapimage_0.dds = len 306052 fnv 689456fd9d8bd48e
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/gamemapimage.png = len 205106 fnv d803460e58494695
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/gamemapimage_0.dds = len 349700 fnv f2e5f188e0934ae2
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/mapimage.png = len 167991 fnv ec501262911ff305
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/mapimage_0.dds = len 349700 fnv 61661ea0023b34af
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/0/matalpha.dds = len 26214548 fnv 8ba5632ba1ae8790
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/0/matid.dds = len 6553748 fnv dca81667f8b60359
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/1/matalpha.dds = len 6553748 fnv 791645bf23ff62e0
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/1/matid.dds = len 1638548 fnv 6ed042bb35ce19c4
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/2/matalpha.dds = len 1638548 fnv 249ed4d07541c97e
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/2/matid.dds = len 409748 fnv 65b0f39b6feac782
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/3/matalpha.dds = len 409748 fnv 4154ebeb5bf5b7cf
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/3/matid.dds = len 102548 fnv 1c79f72a9072a090
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/4x4.tmc = len 65736 fnv bf78728a91cf65ad
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/4x5.tmc = len 65736 fnv 4d1b73bb6be2df75
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/5x4.tmc = len 65736 fnv f46ab53d6508764d
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/5x5.tmc = len 65736 fnv c1792168c509dc25
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/1/2x2.tmc = len 65736 fnv cd4d34b5b0b20144
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/2/1x1.tmc = len 1226008 fnv 4cb6c9f250e4d89b
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/3/0x0.tmc = len 1368368 fnv a4d0be31cf360faa
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/4/0x0.tmc = len 1030392 fnv 47cfcb4d8e0354dc
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/5/0x0.tmc = len 824248 fnv e38668fa6f9afe93
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/6/0x0.tmc = len 545368 fnv 729c9915c290a800
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/7/0x0.tmc = len 326968 fnv 4012a849c48669bc
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/8/0x0.tmc = len 252392 fnv 5416b75d51ff0b96
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/9/0x0.tmc = len 221152 fnv 54a61be416fe14ff
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/10x10.ctt = len 17437 fnv f0fa05fb464805b6
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/10x9.ctt = none
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/11x10.ctt = none
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/11x9.ctt = none
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/8x10.ctt = len 12443 fnv 19673a643926c331
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/8x9.ctt = none
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/9x10.ctt = len 27639 fnv 0e26169bd8af6abf
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/9x9.ctt = none
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/4x4.ctt = len 54475 fnv 21ade7e3d4a8066f
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/4x5.ctt = len 32448 fnv 5f5f883bbec057db
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/5x4.ctt = len 43440 fnv bfbe1276ed1a6ab0
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/5x5.ctt = len 27623 fnv e3adfecc06a49345
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/2/2x2.ctt = len 84257 fnv 87ec03af2bec34bd
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/3/1x1.ctt = len 151806 fnv 6c270271da42bcc4
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/4/0x0.ctt = len 196029 fnv 994379ad9a288738
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/5/0x0.ctt = len 108226 fnv c1857b93a902bf55
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/provincemapimage.png = len 108849 fnv 3c366059940d8f30
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/provincemapimage_0.dds = len 349700 fnv 123fd527416d06c3
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m = len 2788637 fnv c79deeef2bc5e8ff
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m :: gamedata.data = len 3957160 fnv 1ab6fcd126b6dd28
shipped data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m :: rd3d.data = len 5388824 fnv 132d6b3f2df80bcb
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/activemapimage.png = len 611132 fnv 5fbf972c106088c7 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/activemapimage_0.dds = len 1398248 fnv 85ce23c3fe9ba35f changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/gamemapimage.png = len 611132 fnv 5fbf972c106088c7 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/gamemapimage_0.dds = len 1398248 fnv 85ce23c3fe9ba35f changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/mapimage.png = len 611132 fnv 5fbf972c106088c7 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/mapimage_0.dds = len 1398248 fnv 85ce23c3fe9ba35f changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/0/matalpha.dds = len 26214548 fnv cf1abd624c5a6ca4 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/0/matid.dds = len 6553748 fnv 914a5f4c61c21b8f changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/1/matalpha.dds = len 6553748 fnv bec9e9a87b22c912 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/1/matid.dds = len 1638548 fnv a563d4fe7b39a7f2 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/2/matalpha.dds = len 1638548 fnv c5f0081556d4d944 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/2/matid.dds = len 409748 fnv 457b8b6e3c909973 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/3/matalpha.dds = len 409748 fnv d00a0ce85d8e7d60 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/3/matid.dds = len 102548 fnv 9369d53eb97665de changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/4x4.tmc = len 65736 fnv 2b86a0a19afa3158 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/4x5.tmc = len 65736 fnv 3961a953281fc1ea changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/5x4.tmc = len 65736 fnv 9f0cf94695f0c9aa changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/5x5.tmc = len 65736 fnv b5356038bd59cb58 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/1/2x2.tmc = len 65736 fnv 9abb3457a5dad240 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/2/1x1.tmc = len 1245840 fnv 528b3d51a7d3c2f4 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/3/0x0.tmc = len 1396072 fnv 93f6cadcdb256c2f changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/4/0x0.tmc = len 1065584 fnv 8cc491f5af19a8be changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/5/0x0.tmc = len 844240 fnv 4835d14794ea0ad3 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/6/0x0.tmc = len 567336 fnv 79842372af734279 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/7/0x0.tmc = len 393104 fnv 81b00dfff3ec8d2d changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/8/0x0.tmc = len 308968 fnv 6a5c7f40e4561508 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/9/0x0.tmc = len 212840 fnv 50345c3cdce290ad changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/10x10.ctt = len 31711 fnv 1b8019d2b090c316 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/10x9.ctt = len 86902 fnv 9ddef68a3720525f new
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/11x10.ctt = len 65902 fnv 92698ad9bc5f70a7 new
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/11x9.ctt = len 88648 fnv e1e05ffa70f4f4ed new
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/8x10.ctt = len 15940 fnv 9c17ba39e2e72132 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/8x9.ctt = len 88716 fnv 815e90bfc8897e43 new
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/9x10.ctt = len 43346 fnv a8cd24ea7a8f16a4 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/9x9.ctt = len 101395 fnv 73c55b6708634718 new
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/4x4.ctt = len 70380 fnv 621fc3d5f09f3d54 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/4x5.ctt = len 45468 fnv 23cd6878a5e937fd changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/5x4.ctt = len 57949 fnv 575e7d9b23124dc5 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/5x5.ctt = len 39382 fnv 554b8024e25f555a changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/2/2x2.ctt = len 108818 fnv 5417aa82e6579052 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/3/1x1.ctt = len 188911 fnv a6b99003f54cbd3a changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/4/0x0.ctt = len 243702 fnv 232033e16c578352 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/5/0x0.ctt = len 135600 fnv 2f347186b2b46371 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/provincemapimage.png = len 611132 fnv 5fbf972c106088c7 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/provincemapimage_0.dds = len 1398248 fnv 85ce23c3fe9ba35f changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m = len 2467733 fnv c33e7e380caa36f6 changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m :: gamedata.data = len 3957160 fnv 6b44ddfb20b9769b changed
save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m :: rd3d.data = len 5388952 fnv c88ed291a7e6c649 changed
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/activemapimage.png = len 310292 fnv 1c2fe38bd8a640d0 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/activemapimage_0.dds = len 306052 fnv 689456fd9d8bd48e as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/gamemapimage.png = len 205106 fnv d803460e58494695 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/gamemapimage_0.dds = len 349700 fnv f2e5f188e0934ae2 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/mapimage.png = len 167991 fnv ec501262911ff305 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/mapimage_0.dds = len 349700 fnv 61661ea0023b34af as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/0/matalpha.dds = len 26214548 fnv 8ba5632ba1ae8790 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/0/matid.dds = len 6553748 fnv dca81667f8b60359 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/1/matalpha.dds = len 6553748 fnv 791645bf23ff62e0 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/1/matid.dds = len 1638548 fnv 6ed042bb35ce19c4 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/2/matalpha.dds = len 1638548 fnv 249ed4d07541c97e as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/2/matid.dds = len 409748 fnv 65b0f39b6feac782 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/3/matalpha.dds = len 409748 fnv 4154ebeb5bf5b7cf as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/materialmaps/3/matid.dds = len 102548 fnv 1c79f72a9072a090 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/4x4.tmc = len 65736 fnv bf78728a91cf65ad as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/4x5.tmc = len 65736 fnv 4d1b73bb6be2df75 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/5x4.tmc = len 65736 fnv f46ab53d6508764d as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/0/5x5.tmc = len 65736 fnv c1792168c509dc25 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/1/2x2.tmc = len 65736 fnv cd4d34b5b0b20144 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/2/1x1.tmc = len 1226008 fnv 4cb6c9f250e4d89b as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/3/0x0.tmc = len 1368368 fnv a4d0be31cf360faa as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/4/0x0.tmc = len 1030392 fnv 47cfcb4d8e0354dc as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/5/0x0.tmc = len 824248 fnv e38668fa6f9afe93 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/6/0x0.tmc = len 545368 fnv 729c9915c290a800 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/7/0x0.tmc = len 326968 fnv 4012a849c48669bc as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/8/0x0.tmc = len 252392 fnv 5416b75d51ff0b96 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/meshes/9/0x0.tmc = len 221152 fnv 54a61be416fe14ff as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/10x10.ctt = len 17437 fnv f0fa05fb464805b6 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/8x10.ctt = len 12443 fnv 19673a643926c331 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/0/9x10.ctt = len 27639 fnv 0e26169bd8af6abf as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/4x4.ctt = len 54475 fnv 21ade7e3d4a8066f as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/4x5.ctt = len 32448 fnv 5f5f883bbec057db as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/5x4.ctt = len 43440 fnv bfbe1276ed1a6ab0 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/1/5x5.ctt = len 27623 fnv e3adfecc06a49345 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/2/2x2.ctt = len 84257 fnv 87ec03af2bec34bd as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/3/1x1.ctt = len 151806 fnv 6c270271da42bcc4 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/4/0x0.ctt = len 196029 fnv 994379ad9a288738 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/normalmaps/5/0x0.ctt = len 108226 fnv c1857b93a902bf55 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/provincemapimage.png = len 108849 fnv 3c366059940d8f30 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/_gamedata/roman_island_3rdparty_pirate_01/provincemapimage_0.dds = len 349700 fnv 123fd527416d06c3 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m = len 2480650 fnv d884b327f3000099 changed
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m :: gamedata.data = len 3957160 fnv 1ab6fcd126b6dd28 as shipped
undone save data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m :: rd3d.data = len 5388824 fnv 30f6e48990cf720f changed
"#;

const SEQUENCE_1800: &str = r#"
edited at = 160,160
loaded height 154,154 = 9.832031 (411d5000)
loaded material 154,154 = 1
loaded height 160,154 = 5.1640625 (40a54000)
loaded material 160,154 = 1
loaded height 166,154 = 4.2695313 (4088a000)
loaded material 166,154 = 1
loaded height 154,160 = 7.6914063 (40f62000)
loaded material 154,160 = 1
loaded height 160,160 = 4.6054688 (40936000)
loaded material 160,160 = 1
loaded height 166,160 = 4.2382813 (4087a000)
loaded material 166,160 = 1
loaded height 154,166 = 8.972656 (410f9000)
loaded material 154,166 = 1
loaded height 160,166 = 4.7304688 (40976000)
loaded material 160,166 = 0
loaded height 166,166 = 4.6875 (40960000)
loaded material 166,166 = 1
loaded props = 11692
loaded last prop = Some("data/graphics/props/terrain_props/rocks/rocks_moderate/rock_moderate_19.prp") at [150.67284, -0.8347206, 275.78064]
loaded objects = 113
paint id = 31
edited height 154,154 = 9.832031 (411d5000)
edited material 154,154 = 1
edited height 160,154 = 5.1640625 (40a54000)
edited material 160,154 = 31
edited height 166,154 = 4.2695313 (4088a000)
edited material 166,154 = 1
edited height 154,160 = 11.691406 (413b1000)
edited material 154,160 = 1
edited height 160,160 = 8.605469 (4109b000)
edited material 160,160 = 31
edited height 166,160 = 8.238281 (4103d000)
edited material 166,160 = 1
edited height 154,166 = 8.972656 (410f9000)
edited material 154,166 = 1
edited height 160,166 = 4.7304688 (40976000)
edited material 160,166 = 31
edited height 166,166 = 4.6875 (40960000)
edited material 166,166 = 1
edited props = 11693
edited last prop = Some("data/graphics/buildings/production/mining_01/mining_01_right_decal_detail01.prp") at [160.0, 8.605469, 160.0]
edited objects = 113
undo steps = 3
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/activemapimage.png = len 1538558 fnv 0a18ed3fc9b889b2
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/activemapimage_0.dds = len 927732 fnv 593da1e07afc9b60
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/gamemapimage.png = len 434442 fnv c32761ac4ad103ef
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/gamemapimage_0.dds = len 808020 fnv a05efa68bd6ceda0
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/mapimage.png = len 474231 fnv 57c55880dcb9c2e3
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/mapimage_0.dds = len 349700 fnv 22642a211837255c
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/4x4.tmc = len 21622 fnv a7654198af16a558
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/4x5.tmc = len 22405 fnv 3158326e51a16e36
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/5x4.tmc = len 21622 fnv ceb6544ec46d81d8
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/5x5.tmc = len 22769 fnv 476fda2d611c1322
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/1/2x2.tmc = len 22465 fnv 910048465360a931
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/2/1x1.tmc = len 1324839 fnv c53adc6c9b506b47
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/3/0x0.tmc = len 1464722 fnv 0ea171046a9ff1d6
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/4/0x0.tmc = len 1068415 fnv 48340188ee146db3
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/5/0x0.tmc = len 802564 fnv 62ef89996f8d8e53
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/6/0x0.tmc = len 492616 fnv be3f16f1d1265243
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/7/0x0.tmc = len 253373 fnv 5bc36eafefb40c9b
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/8/0x0.tmc = len 173678 fnv 53555c6a1efa643c
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/9/0x0.tmc = len 21622 fnv 21609c87e961bcd1
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/10x10.ctt = len 73712 fnv 8134d6fd36df8e05
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/10x9.ctt = len 11088 fnv c3f33b945b647f3a
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/11x10.ctt = len 8754 fnv e46a51b437ca93a1
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/11x9.ctt = none
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/8x10.ctt = len 153370 fnv ea8a94519cb7deb7
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/8x9.ctt = len 155937 fnv 367b26e7bdcf699e
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/9x10.ctt = len 150036 fnv 705c37acd4b21dac
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/9x9.ctt = len 99143 fnv 60884daeb47aea77
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/4x4.ctt = len 167084 fnv 21ecd7d725b22957
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/4x5.ctt = len 215479 fnv d5fb758e142c7e20
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/5x4.ctt = len 11519 fnv 11e123e138873633
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/5x5.ctt = len 98720 fnv 8db01f4a5abe0eeb
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/2/2x2.ctt = len 160549 fnv 53c354bd687af704
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/3/1x1.ctt = len 222116 fnv f8fdc6f9aee02f59
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/4/0x0.ctt = len 133829 fnv 58a14623c03c39d2
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/5/0x0.ctt = len 52635 fnv b3a5ebba044f80d8
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/provincemapimage.png = none
shipped data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/provincemapimage_0.dds = none
shipped data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m = len 2093678 fnv 6af3c43cfa82b81d
shipped data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m :: gamedata.data = len 1302279 fnv db12bef552954192
shipped data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m :: rd3d.data = len 7652031 fnv 1f70ee891991b3f6
shipped data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7minfo = len 2478 fnv 7248c01790aeacb1
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/activemapimage.png = len 609206 fnv 5e6ef527138b349f changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/activemapimage_0.dds = len 1398248 fnv 13116d9bf2ef992a changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/gamemapimage.png = len 609206 fnv 5e6ef527138b349f changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/gamemapimage_0.dds = len 1398248 fnv 13116d9bf2ef992a changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/mapimage.png = len 609206 fnv 5e6ef527138b349f changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/mapimage_0.dds = len 1398248 fnv 13116d9bf2ef992a changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/4x4.tmc = len 21622 fnv 0a456b61867692f9 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/4x5.tmc = len 22405 fnv a7743b33ba4d659e changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/5x4.tmc = len 21622 fnv e915f63ee20f7237 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/5x5.tmc = len 22769 fnv aa79f47f767736ff changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/1/2x2.tmc = len 22465 fnv 8eb9fd3400384119 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/2/1x1.tmc = len 1345674 fnv 3147b5be9055e870 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/3/0x0.tmc = len 1492360 fnv d351a3cb86273b18 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/4/0x0.tmc = len 1116165 fnv 0f69e52650153097 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/5/0x0.tmc = len 838045 fnv 373c5d4c68a9daa6 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/6/0x0.tmc = len 541594 fnv 9197557385975220 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/7/0x0.tmc = len 367733 fnv aee09b3853c9039b changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/8/0x0.tmc = len 254300 fnv abcb74d2f97ad2a6 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/9/0x0.tmc = len 154288 fnv 37102f143342fb59 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/10x10.ctt = len 107302 fnv 50e1333ac1133d75 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/10x9.ctt = len 23406 fnv 0f6d368f15641ffb changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/11x10.ctt = len 12902 fnv 957db64ff4256a23 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/11x9.ctt = len 15425 fnv 59046f19edb5dfb4 new
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/8x10.ctt = len 209295 fnv 219630fb875b7f1b changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/8x9.ctt = len 213139 fnv 0cb5f67107730644 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/9x10.ctt = len 207448 fnv 6ff242d91e68c427 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/9x9.ctt = len 135101 fnv 4e3cc41efea39f59 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/4x4.ctt = len 220466 fnv 6f64cdc4015d398f changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/4x5.ctt = len 289645 fnv 98d2d42d3d841fa5 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/5x4.ctt = len 20574 fnv 5bb6fb4080d2e820 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/5x5.ctt = len 134879 fnv 1e00f15c3a1d8550 changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/2/2x2.ctt = len 209981 fnv 9216221750562cdb changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/3/1x1.ctt = len 277368 fnv 92d00185c965021b changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/4/0x0.ctt = len 162847 fnv 580ad2a77408745d changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/5/0x0.ctt = len 65226 fnv 67aa90abae74f55a changed
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/provincemapimage.png = len 609206 fnv 5e6ef527138b349f new
save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/provincemapimage_0.dds = len 1398248 fnv 13116d9bf2ef992a new
save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m = len 1996788 fnv f58a4b8bfda0c31d changed
save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m :: gamedata.data = len 1302279 fnv 9cfefb0655c58800 changed
save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m :: rd3d.data = len 7652114 fnv c58022dc172fe5da changed
save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7minfo = len 2478 fnv e3dae1696dc2e2ed changed
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/activemapimage.png = len 1538558 fnv 0a18ed3fc9b889b2 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/activemapimage_0.dds = len 927732 fnv 593da1e07afc9b60 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/gamemapimage.png = len 434442 fnv c32761ac4ad103ef as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/gamemapimage_0.dds = len 808020 fnv a05efa68bd6ceda0 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/mapimage.png = len 474231 fnv 57c55880dcb9c2e3 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/mapimage_0.dds = len 349700 fnv 22642a211837255c as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/4x4.tmc = len 21622 fnv a7654198af16a558 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/4x5.tmc = len 22405 fnv 3158326e51a16e36 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/5x4.tmc = len 21622 fnv ceb6544ec46d81d8 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/0/5x5.tmc = len 22769 fnv 476fda2d611c1322 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/1/2x2.tmc = len 22465 fnv 910048465360a931 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/2/1x1.tmc = len 1324839 fnv c53adc6c9b506b47 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/3/0x0.tmc = len 1464722 fnv 0ea171046a9ff1d6 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/4/0x0.tmc = len 1068415 fnv 48340188ee146db3 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/5/0x0.tmc = len 802564 fnv 62ef89996f8d8e53 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/6/0x0.tmc = len 492616 fnv be3f16f1d1265243 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/7/0x0.tmc = len 253373 fnv 5bc36eafefb40c9b as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/8/0x0.tmc = len 173678 fnv 53555c6a1efa643c as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/meshes/9/0x0.tmc = len 21622 fnv 21609c87e961bcd1 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/10x10.ctt = len 73712 fnv 8134d6fd36df8e05 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/10x9.ctt = len 11088 fnv c3f33b945b647f3a as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/11x10.ctt = len 8754 fnv e46a51b437ca93a1 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/8x10.ctt = len 153370 fnv ea8a94519cb7deb7 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/8x9.ctt = len 155937 fnv 367b26e7bdcf699e as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/9x10.ctt = len 150036 fnv 705c37acd4b21dac as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/0/9x9.ctt = len 99143 fnv 60884daeb47aea77 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/4x4.ctt = len 167084 fnv 21ecd7d725b22957 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/4x5.ctt = len 215479 fnv d5fb758e142c7e20 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/5x4.ctt = len 11519 fnv 11e123e138873633 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/1/5x5.ctt = len 98720 fnv 8db01f4a5abe0eeb as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/2/2x2.ctt = len 160549 fnv 53c354bd687af704 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/3/1x1.ctt = len 222116 fnv f8fdc6f9aee02f59 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/4/0x0.ctt = len 133829 fnv 58a14623c03c39d2 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/_gamedata/moderate_m_01/normalmaps/5/0x0.ctt = len 52635 fnv b3a5ebba044f80d8 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m = len 1994625 fnv 0bf8d33d152b0f7f changed
undone save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m :: gamedata.data = len 1302279 fnv db12bef552954192 as shipped
undone save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m :: rd3d.data = len 7652031 fnv 5b8bc90bc7ed7323 changed
undone save data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7minfo = len 2478 fnv e3dae1696dc2e2ed changed
"#;

// --- Undo history bytes --------------------------------------------------------

/// Allocations the undo history reaches, each counted once, by document
/// member. Buffers count their length, not capacity.
#[derive(Default)]
struct Held {
    seen: std::collections::HashSet<usize>,
    bytes: BTreeMap<&'static str, usize>,
}

impl Held {
    /// Counts `bytes` under `member` unless allocation `at` was seen.
    fn add(&mut self, count: bool, member: &'static str, at: usize, bytes: usize) {
        if self.seen.insert(at) && count {
            *self.bytes.entry(member).or_default() += bytes;
        }
    }

    /// Walks `doc`; with `count` false only marks what it holds as seen.
    fn walk(&mut self, doc: &super::doc::Doc, count: bool) {
        use anno_formats::a7m::grids::Values;
        use std::mem::size_of;
        let heights = |h: &gpui_editor_kit::heightfield::Heightfield| h.heights.len() * 4;
        let ptr = |p: *const u8| p as usize;
        self.add(
            count,
            "heights",
            ptr(Arc::as_ptr(&doc.field).cast()),
            heights(&doc.field),
        );
        let land = &doc.land_heights;
        self.add(
            count,
            "land heights",
            ptr(Arc::as_ptr(land).cast()),
            heights(land),
        );
        if let Some(p) = &doc.heights_patch {
            self.add(
                count,
                "height patches",
                ptr(Arc::as_ptr(p).cast()),
                p.values.len() * 4,
            );
        }
        if let Some(m) = &doc.materials {
            for (at, bytes) in m.chunks() {
                self.add(count, "materials", at, bytes);
            }
        }
        for (at, bytes) in doc.props.allocations() {
            self.add(count, "props", at, bytes);
        }
        let objects = (doc.objects.iter())
            .map(|o| {
                size_of::<anno_core::island::objects::Object>()
                    + o.config.as_ref().map_or(0, String::len)
                    + o.path.as_ref().map_or(0, |p| {
                        p.len() * size_of::<anno_core::island::objects::PathPoint>()
                    })
            })
            .sum();
        self.add(
            count,
            "objects",
            ptr(Arc::as_ptr(&doc.objects).cast()),
            objects,
        );
        let meshes = (doc.meshes.iter())
            .map(|m| size_of::<String>() + m.len())
            .sum();
        self.add(
            count,
            "meshes",
            ptr(Arc::as_ptr(&doc.meshes).cast()),
            meshes,
        );
        if let Some(grids) = &doc.grids {
            let layers = grids.layers.len() * size_of::<anno_formats::a7m::grids::Layer>();
            self.add(count, "grids", ptr(Arc::as_ptr(grids).cast()), layers);
            for l in &grids.layers {
                let bytes = match &*l.values {
                    Values::U8(v) => v.len(),
                    Values::U16(v) => v.len() * 2,
                    Values::U32(v) => v.len() * 4,
                };
                self.add(count, "grids", ptr(Arc::as_ptr(&l.values).cast()), bytes);
            }
        }
        if let Some(t) = &doc.tint {
            self.add(
                count,
                "tint",
                ptr(Arc::as_ptr(t).cast()),
                t.texels.len() * 3,
            );
        }
        // Inline in each step, never shared: the step itself and its
        // session names.
        if count {
            let s = &doc.session;
            let names = s.global_ambient.len()
                + s.vegetation_prop_set.len()
                + s.north_ambient.as_ref().map_or(0, String::len)
                + s.south_ambient.as_ref().map_or(0, String::len);
            *self.bytes.entry("steps").or_default() += size_of::<super::doc::Doc>() + names;
        }
    }
}

/// Bytes the undo and redo stacks hold past the live document, by member.
fn undo_bytes(editor: &IslandEditor) -> BTreeMap<&'static str, usize> {
    let mut held = Held::default();
    held.walk(&editor.island.doc, false);
    for step in editor.history.snapshots() {
        held.walk(step, true);
    }
    held.bytes
}

// The fixed sequence on the continental island, the largest shipped:
// after each step, and after undoing all, the bytes undo holds past the
// live document, per member, each allocation once. A member over its
// record is the regression the plan's undo-memory risk names, such as a
// migration that stops sharing chunks between steps; under it passes with
// a note to bless the lower number.
#[test]
fn undo_history_after_the_fixed_sequence_stays_within_recorded_bytes() {
    let Some(install) = install() else {
        eprintln!("skipped, no install (ANNO117_DIR)");
        return;
    };
    let path = super::gpu_bench::CONTINENTAL_01;
    let mut editor = IslandEditor::load(
        crate::games::module(crate::games::of(&install)),
        &install,
        path,
    )
    .unwrap();
    let at = middle(&editor);
    let loaded = probe(&editor, at);
    let mut out = Record::default();
    let put = |out: &mut Record, step: &str, editor: &IslandEditor| {
        let bytes = undo_bytes(editor);
        out.put(format!("{step} total"), bytes.values().sum::<usize>());
        for (member, n) in bytes {
            out.put(format!("{step} {member}"), n);
        }
    };
    for k in 0..STEPS.len() {
        edit(&mut editor, k, at);
        put(&mut out, &format!("after {}", STEPS[k]), &editor);
    }
    // The byte record is a ceiling, which holding nothing passes: undo must
    // still have taken a step per edit and give the island back.
    let edited = probe(&editor, at);
    assert_ne!(edited, loaded, "the sequence changed nothing at {at:?}");
    let mut steps = 0;
    while editor.undo(false) {
        steps += 1;
    }
    assert_eq!(steps, STEPS.len(), "undo steps");
    assert_eq!(probe(&editor, at), loaded, "undo all gives the island back");
    put(&mut out, "after undo all", &editor);
    out.check_at_most("UNDO_BYTES", UNDO_BYTES);
    while editor.undo(true) {}
    assert_eq!(probe(&editor, at), edited, "redo all gives the edits back");
}

const UNDO_BYTES: &str = r#"
after sculpt total = 6921
after sculpt height patches = 6532
after sculpt heights = 0
after sculpt steps = 389
after paint total = 7310
after paint height patches = 6532
after paint heights = 0
after paint steps = 778
after place total = 21047
after place height patches = 6532
after place heights = 0
after place props = 13348
after place steps = 1167
after undo all total = 5661655
after undo all grids = 3543312
after undo all height patches = 6532
after undo all heights = 0
after undo all materials = 2097152
after undo all props = 13492
after undo all steps = 1167
"#;

// --- Island frames -------------------------------------------------------------

/// The islands drawn: island_shots' frame of a 117 pool island, a 117
/// island with a thousand objects and material maps, and an 1800 one.
const SHOT_ISLANDS: [(&str, &str); 3] = [("117", SMALL), ("117", PIRATE), ("1800", MODERATE_M_01)];

/// Frames are island_shots' size.
const SHOT_SIZE: (u32, u32) = (1280, 720);

/// Per adapter and driver (`adapter()`), the recorded `key = value` lines.
const FRAMES: &[(&str, &str)] = &[(
    "NVIDIA GeForce RTX 4080 SUPER (Vulkan), NVIDIA 620.70.06",
    r#"
117 roman_island_small_02 frame = len 3686400 fnv f01f82dd070c21b6
117 roman_island_3rdparty_pirate_01 frame = len 3686400 fnv 2d381f3eaa6ede2a
1800 moderate_m_01 frame = len 3686400 fnv f8143e7277f2e87a
"#,
)];

/// The offscreen device's adapter, name and driver, as `FRAMES` keys it.
fn adapter(device: &wgpu::Device) -> String {
    let info = device.adapter_info();
    format!(
        "{} ({:?}), {} {}",
        info.name, info.backend, info.driver, info.driver_info
    )
}

/// The island_shots frame of `path` (island_shots.rs `shoot`: noon, clock
/// and waves held, the sea at 4 s, default graphics). Where `shoot` draws
/// a fixed count of frames, this draws until the renderer is settled and
/// every ground page the view wants is baked, so the bytes do not depend
/// on how fast uploads ran beside other builds.
fn shot(
    install: &Arc<Install>,
    assets: &Arc<anno_formats::assets::Assets>,
    target: &mut gpui_editor_kit::gpu::Offscreen,
    path: &str,
) -> Vec<u8> {
    use gpui_editor_kit::gpu::HELD_CLOCK;
    use std::sync::atomic::Ordering;
    let hold = |t: f32| HELD_CLOCK.store(t.to_bits(), Ordering::Relaxed);
    let game = crate::games::of(install);
    let assets = Some((install.clone(), assets.clone()));
    let mut editor =
        IslandEditor::load_with(crate::games::module(game), install, path, assets).unwrap();
    // Motion off: the recorded frames stand still, as the editor drew
    // islands before it animated them.
    editor.set_graphics(crate::graphics::Graphics {
        animation: false,
        ..Default::default()
    });
    editor.layers.harbours = true;
    hold(4.);
    editor.time_of_day = 12.;
    editor.sky = None;
    editor.scene_stale = true;
    super::gpu_bench::settle(&mut editor, install);
    let renderer = editor.renderer.clone();
    target
        .render(&renderer, editor.views[0].camera, editor.extent())
        .unwrap();
    for _ in 0..8 {
        target.frame(&renderer).unwrap();
    }
    editor.layers.waves = false;
    editor.scene_stale = true;
    editor.refresh_scene();
    super::water_shots::show_water_at(&editor, 4.);
    // Residency lets texture levels go on the wall clock, two to four
    // seconds after the view stops asking (`Renderer::settled`).
    let started = std::time::Instant::now();
    loop {
        target.frame(&renderer).unwrap();
        let p = renderer.drawn().pages;
        if renderer.settled() && p.baked >= p.wanted && p.new == 0 {
            break;
        }
        assert!(
            started.elapsed().as_secs() < 120,
            "{path}: not settled after 120 s of frames ({:?}, {} of {} pages)",
            renderer.waiting_on(),
            p.baked,
            p.wanted
        );
    }
    // Exposure jumps to its target after a pause over 0.1 s (exposure.rs
    // JUMP_AFTER); four jumps meet the frame they measure.
    let mut pixels = Vec::new();
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        pixels = target.frame(&renderer).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    let again = target.frame(&renderer).unwrap();
    hold(f32::NAN);
    assert!(pixels == again, "{path}: the next frame differs");
    pixels
}

// island_shots' frames of SHOT_ISLANDS, each from a fresh open: the frame's
// digest. Its frames are trunk's but for the impostor fix (7ce9148): the quads
// of rows that bend to the terrain now keep their filtered crown edges, 2
// pixels at 1 level on this island; for ground page sheets, whose fixed size
// moved ground texels by at most 5 levels (means 0.001-0.002) on every island;
// for trunk's PBR cutout at the game's 0.3 (6837a4a), which moved 32 pixels of
// the pirate island's harbour and building edges by more than 3 levels (max
// 14), and about 2,000 more across the frame by 1-3 (likely exposure; not
// traced); and for decals blending in row order. The cull listed them in
// whatever order its threads ran, so overlapping decals on 1800
// moderate_m_01's summit drew 39 pixels up to 3 levels apart in about one
// redraw in twenty, and a frame kept whichever it drew last. In row order
// every redraw draws what the odd one out drew before. Frames recorded before
// the classic path went (2026-11-23) are the visibility buffer's.
#[test]
fn island_shots_render_the_recorded_frames() {
    let installs: Vec<(&str, Arc<Install>)> = [("117", install()), ("1800", install_1800())]
        .into_iter()
        .filter_map(|(game, i)| match i {
            Some(i) => Some((game, Arc::new(i))),
            None => {
                eprintln!("{game}: skipped, no install (ANNO{game}_DIR)");
                None
            }
        })
        .collect();
    if installs.is_empty() {
        return;
    }
    let mut target = gpui_editor_kit::gpu::Offscreen::new(SHOT_SIZE.0, SHOT_SIZE.1).unwrap();
    let device = adapter(target.device());
    let golden = FRAMES.iter().find(|(a, _)| *a == device);
    if golden.is_none() && !bless() {
        let recorded: Vec<&str> = FRAMES.iter().map(|(a, _)| *a).collect();
        eprintln!(
            "skipped: no recorded frames for adapter {device:?} (recorded: {recorded:?}); \
             ANNO_EDITOR_BLESS=1 prints them"
        );
        return;
    }
    let mut out = Record::default();
    for (game, install) in &installs {
        let assets = super::test_support::assets(install);
        for (_, path) in SHOT_ISLANDS.iter().filter(|(g, _)| g == game) {
            let name = path.rsplit('/').next().unwrap().trim_end_matches(".a7m");
            let pixels = shot(install, &assets, &mut target, path);
            out.put(format!("{game} {name} frame"), digest(&pixels));
            if let Some(dir) = std::env::var_os("ANNO_EDITOR_BASELINE_DUMP") {
                let (w, h) = SHOT_SIZE;
                let file = std::path::Path::new(&dir).join(format!("{game}_{name}.png"));
                let image = image::RgbaImage::from_raw(w, h, pixels).unwrap();
                image.save(&file).unwrap();
            }
        }
    }
    if bless() {
        let mut text = format!("    (\n        {device:?},\n        r#\"\n");
        for (k, v) in &out.0 {
            text += &format!("{k} = {v}\n");
        }
        eprintln!("{text}\"#,\n    ),");
        return;
    }
    // Only the games installed here: a golden holds both.
    let golden: String = (golden.unwrap().1.lines())
        .filter(|l| {
            installs
                .iter()
                .any(|(g, _)| l.starts_with(&format!("{g} ")))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    out.check(&format!("FRAMES[{device}]"), &golden);
}
