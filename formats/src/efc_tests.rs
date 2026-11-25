use super::*;
use crate::rdp::Frame;
use crate::vfs::{FileId, Install};

// One particle emitter as the game writes them, cut down: a 2 s life, a
// 1 s looped emission every 50 ms, rising 2 m over the life.
const SMOKE: &str = "<Effect0><Texture0>data/graphics/effects/particles/maps/smoke.png</Texture0>\r
<Texture1></Texture1>\r
<Name>Smoke</Name>\r
<EffectType>1</EffectType>\r
<Technique>0</Technique>\r
<Delay>0</Delay>\r
<LifeTime>2000</LifeTime>\r
<Duration>1000</Duration>\r
<Loop>1</Loop>\r
<Bound>1</Bound>\r
<OffsetX>0.000000</OffsetX>\r
<OffsetY>0.000000</OffsetY>\r
<OffsetZ>0.000000</OffsetZ>\r
<PEPValueMin>0.000000</PEPValueMin>\r
<PEPValueMax>100.000000</PEPValueMax>\r
<PEPValueDef>50.000000</PEPValueDef>\r
<PEPVarianz>0.000000</PEPVarianz>\r
<PEPSize>0</PEPSize>\r
<AtlasTilingU>1.000000</AtlasTilingU>\r
<AtlasTilingV>0.000000</AtlasTilingV>\r
<AtlasOffsetU>0.000000</AtlasOffsetU>\r
<AtlasOffsetV>1.000000</AtlasOffsetV>\r
<Alignment>0</Alignment>\r
<VelocityYValueMin>-2.000000</VelocityYValueMin>\r
<VelocityYValueMax>2.000000</VelocityYValueMax>\r
<VelocityYValueDef>0.000000</VelocityYValueDef>\r
<VelocityYVarianz>0.000000</VelocityYVarianz>\r
<VelocityYSize>2</VelocityYSize>\r
<VelocityYKnotPos_0>0.000000</VelocityYKnotPos_0>\r
<VelocityYKnotX_0>0.500000</VelocityYKnotX_0>\r
<VelocityYKnotY_0>0.500000</VelocityYKnotY_0>\r
<VelocityYKnotZ_0>0.500000</VelocityYKnotZ_0>\r
<VelocityYKnotW_0>0.500000</VelocityYKnotW_0>\r
<VelocityYKnotPos_1>1.000000</VelocityYKnotPos_1>\r
<VelocityYKnotX_1>1.000000</VelocityYKnotX_1>\r
<VelocityYKnotY_1>1.000000</VelocityYKnotY_1>\r
<VelocityYKnotZ_1>1.000000</VelocityYKnotZ_1>\r
<VelocityYKnotW_1>1.000000</VelocityYKnotW_1>\r
<ScaleValueMin>0.000000</ScaleValueMin>\r
<ScaleValueMax>1.000000</ScaleValueMax>\r
<ScaleValueDef>0.500000</ScaleValueDef>\r
<ScaleVarianz>0.000000</ScaleVarianz>\r
<ScaleSize>0</ScaleSize>\r
</Effect0>\r
";

fn smoke() -> Effect {
    Efc::parse(SMOKE.as_bytes()).unwrap().effects().remove(0)
}

// Effects baked from `e` at `seconds`: how many particles are alive.
fn alive(rdp: &Rdp, seconds: f32) -> usize {
    let mut frame = Frame::default();
    rdp.frame(seconds, &mut frame);
    frame.len()
}

// Every shipped `.efc`, shadowed copies included, parses and writes back
// byte for byte; the winning copies hold what an independent regex count
// over the same files found (formats/examples survey, 2026-11-22): 304
// files, 711 effects by kind, 11,829 curves, 9,927 knots by `Size` (21
// numbered in hex past 9), LifeTimes summing to 1,302,010 ms and knot X
// values to 4008.339. Every drawn effect bakes within the `.rdp` bounds.
#[test]
fn shipped_efc_read_write_and_bake() {
    let Some(dir) = crate::vfs::install_dir("ANNO2070_DIR") else {
        eprintln!("skip: ANNO2070_DIR unset");
        return;
    };
    let install = Install::open(dir, &["data", "patch", "addon", "exedata"]).unwrap();
    let (mut files, mut copies, mut kinds) = (0, 0, std::collections::BTreeMap::new());
    let (mut curves, mut knots, mut hex, mut life, mut xsum) = (0, 0, 0, 0u64, 0f64);
    let (mut busiest, mut slowest) = ((0, String::new()), std::time::Duration::ZERO);
    for i in 0..install.file_count() {
        let id = FileId(i as u32);
        if !install.file_name(id).ends_with(".efc") {
            continue;
        }
        let path = install.file_path(id);
        for layer in install.layers(id).iter().filter(|l| !l.is_tombstone()) {
            copies += 1;
            let bytes = install.read_layer(id, layer).unwrap();
            let efc = Efc::parse(&bytes).unwrap_or_else(|e| panic!("{path}: {e:#}"));
            assert!(efc.write() == bytes, "{path}: written back differently");
        }
        files += 1;
        let efc = Efc::parse(&install.read_file(id).unwrap()).unwrap();
        for (k, e) in efc.effects().iter().enumerate() {
            *kinds.entry(e.kind).or_insert(0) += 1;
            life += u64::from(e.life_time);
            for (_, c) in &e.curves {
                curves += 1;
                knots += c.knots.len();
                hex += c.knots.len().saturating_sub(10);
                xsum += c.knots.iter().map(|n| f64::from(n.value[0])).sum::<f64>();
            }
            let took = std::time::Instant::now();
            let Some(rdp) = e.bake(&Launch::default(), k as u32) else {
                continue;
            };
            slowest = slowest.max(took.elapsed());
            assert!((2..=256).contains(&rdp.frame_count), "{path}");
            assert!(!rdp.starts.is_empty(), "{path} effect {k}: no particles");
            if rdp.starts.len() > busiest.0 {
                busiest = (rdp.starts.len(), format!("{path} {k}"));
            }
        }
    }
    eprintln!("{files} files, {copies} copies; busiest {busiest:?}; slowest bake {slowest:?}");
    assert_eq!(files, 304);
    let want = [(1, 472), (2, 114), (3, 83), (4, 15), (6, 1), (7, 26)];
    assert_eq!(kinds.into_iter().collect::<Vec<_>>(), want);
    assert_eq!((curves, knots, hex, life), (11_829, 9_927, 21, 1_302_010));
    assert!((xsum - 4008.339).abs() < 0.01, "knot X sum {xsum}");
}

// The scalars and curves read as the file holds them: knots at 0..1 of
// the range, `def` without knots, Size knots in order.
#[test]
fn an_effect_reads_its_fields_and_curves() {
    let e = smoke();
    assert_eq!(
        (e.name.as_str(), e.kind, e.life_time, e.duration),
        ("Smoke", PARTICLE, 2000, 1000)
    );
    assert!(e.looped && e.bound);
    assert_eq!(e.atlas, [1., 0., 0., 1.]);
    let names: Vec<&str> = e.curves.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["PEP", "VelocityY", "Scale"]);
    assert_eq!(e.curve("PEP").unwrap().at(0.3), 50.);
    let rise = e.curve("VelocityY").unwrap();
    assert_eq!(
        (rise.at(0.), rise.at(0.5), rise.at(1.), rise.at(2.)),
        (0., 1., 2., 2.)
    );
}

// The loader's quirks: knot numbers past 9 are hex, a knot field the file
// leaves out reads `(min + def) / range`, and Size counts, not the knots
// written.
#[test]
fn curves_read_as_the_game_loader_reads_them() {
    let mut xml = String::from("<Effect0><EffectType>1</EffectType>\r\n");
    xml += "<ScaleValueMin>0.000000</ScaleValueMin>\r\n<ScaleValueMax>2.000000</ScaleValueMax>\r\n";
    xml += "<ScaleValueDef>0.500000</ScaleValueDef>\r\n<ScaleVarianz>0.000000</ScaleVarianz>\r\n";
    xml += "<ScaleSize>12</ScaleSize>\r\n";
    for i in 0..11 {
        xml += &format!(
            "<ScaleKnotPos_{i:x}>{:.6}</ScaleKnotPos_{i:x}>\r\n",
            i as f32 / 10.
        );
        xml += &format!(
            "<ScaleKnotX_{i:x}>{:.6}</ScaleKnotX_{i:x}>\r\n",
            i as f32 / 20.
        );
    }
    xml += "</Effect0>\r\n";
    let e = Efc::parse(xml.as_bytes()).unwrap().effects().remove(0);
    let c = e.curve("Scale").unwrap();
    assert_eq!(c.knots.len(), 12);
    assert_eq!(c.knots[10].pos, 1., "_a is knot 10");
    // Y, Z, W absent: (0 + 0.5) / 2.
    assert_eq!(c.knots[3].value[1], 0.25);
    // The twelfth knot is all defaults, at position 0.
    assert_eq!(c.knots[11].pos, 0.);
    assert!(Efc::parse(b"<Config></Config>").is_err());
}

// An edit writes only what changed, in the game's spelling, and reads back;
// a curve's knots are rewritten after its Size with hex numbers.
#[test]
fn an_edited_effect_writes_back_what_changed() {
    let mut efc = Efc::parse(SMOKE.as_bytes()).unwrap();
    let mut e = efc.effects().remove(0);
    efc.set(0, &e).unwrap();
    assert_eq!(efc.write(), SMOKE.as_bytes(), "no change, no bytes");
    e.life_time = 1500;
    e.curve_mut("VelocityY", Curve::default)
        .set_edge(true, 0, 3.);
    e.curve_mut("Alpha", Curve::default).set_edge(true, 0, 0.);
    efc.set(0, &e).unwrap();
    let text = String::from_utf8(efc.write()).unwrap();
    assert!(text.contains("<LifeTime>1500</LifeTime>"));
    assert!(text.contains("<VelocityYValueMax>3.000000</VelocityYValueMax>"));
    assert!(
        text.contains("<AlphaSize>2</AlphaSize>"),
        "a new curve is added"
    );
    let again = Efc::parse(text.as_bytes()).unwrap().effects().remove(0);
    assert_eq!(again, e);
    let rise = again.curve("VelocityY").unwrap();
    assert!((rise.at(0.5) - 1.5).abs() < 1e-5, "{}", rise.at(0.5));
    // Untouched lines stay as they were.
    assert!(text.starts_with("<Effect0><Texture0>data/graphics/effects/particles/maps/smoke.png"));
    assert!(text.contains("<Texture1></Texture1>\r\n<Name>Smoke</Name>"));
}

// Setting a curve's start or end keeps the knots between, widening the
// range so they keep their values; the spread factor runs 1 - 2v..1.
#[test]
fn curve_edges_edit_and_spread_bounds() {
    let mut c = Curve::constant(0., 1., 0.);
    c.knots = vec![
        Knot {
            pos: 0.,
            value: [0.; 4],
        },
        Knot {
            pos: 0.5,
            value: [1.; 4],
        },
        Knot {
            pos: 1.,
            value: [0.5; 4],
        },
    ];
    c.set_edge(true, 0, 4.);
    assert_eq!((c.min, c.max), (0., 4.));
    assert!(
        (c.at(0.5) - 1.).abs() < 1e-6,
        "the middle keeps 1: {}",
        c.at(0.5)
    );
    assert_eq!(c.at(1.), 4.);
    c.set_edge(false, 0, -1.);
    assert_eq!((c.at(0.), c.min), (-1., -1.));
    c.varianz = 0.25;
    assert_eq!((c.spread(0.), c.spread(1.)), (0.5, 1.));
    let colour = Curve::ramp(0., 1., [1., 0.5, 0., 1.], [0., 0., 1., 1.]);
    assert_eq!(colour.values_at(0.5), [0.5, 0.25, 0.5, 1.]);
}

// A looped emitter every 50 ms with a 2 s life keeps about 40 alive; the
// rise curve puts a particle halfway up at half its life; the same seed
// bakes the same run, and the run writes as an `.rdp`.
#[test]
fn a_looped_emitter_bakes_its_rate_and_path() {
    let e = smoke();
    let rdp = e.bake(&Launch::default(), 1).unwrap();
    assert!(rdp.looped);
    // 1 s cycles, two of them for a 2 s life.
    assert!((rdp.duration() - 2.).abs() < 1e-3, "{}", rdp.duration());
    assert_eq!(rdp.starts.len(), 40);
    for t in [0.1, 0.77, 1.5] {
        let n = alive(&rdp, t);
        assert!((38..=41).contains(&n), "{n} alive at {t} s");
    }
    // Particle 0 is born at 0 on the ring's centre (no EmitArea): its tick at
    // half its life is 1 m up.
    let mid = rdp.positions[rdp.life(0) as usize / 2];
    assert!((mid[1] - 1.).abs() < 0.05, "{mid:?}");
    assert_eq!(rdp.scales[0], [1.; 3], "Scale 0.5 spans 1 m");
    let again = e.bake(&Launch::default(), 1).unwrap();
    assert_eq!(again.positions, rdp.positions);
    let read = Rdp::parse(&rdp.encode().unwrap()).unwrap();
    assert_eq!(read.starts.len(), 40);
}

// Once, the emitter's first particle comes one interval in and its last is
// the one whose interval began within the duration: 1000 / 50 + 1. They
// are gone a life after the last.
#[test]
fn a_one_shot_emitter_stops_after_its_duration() {
    let mut e = smoke();
    e.looped = false;
    let rdp = e.bake(&Launch::default(), 1).unwrap();
    assert!(!rdp.looped);
    assert_eq!(rdp.starts.len(), 21);
    assert!(alive(&rdp, 0.5) > 0);
    assert_eq!(alive(&rdp, rdp.duration() + 0.1), 0);
}

// A billboard is one sprite a cycle; a model effect draws nothing here; a
// launch throws particles up and gravity brings them back.
#[test]
fn billboards_meshes_and_launches_bake_as_they_should() {
    let mut e = smoke();
    e.kind = BILLBOARD;
    let rdp = e.bake(&Launch::default(), 1).unwrap();
    assert_eq!((rdp.starts.len(), alive(&rdp, 0.5)), (1, 1));
    e.kind = MODEL;
    assert!(e.bake(&Launch::default(), 1).is_none());
    let mut plume = Effect::plume();
    plume
        .curve_mut("VelocityY", Curve::default)
        .set_edge(true, 0, 0.);
    let thrown = Launch {
        speed: 4.,
        spread: 0.,
        gravity: -9.81,
    };
    let rdp = plume.bake(&thrown, 1).unwrap();
    let ticks = rdp.life(0) as usize;
    let top = rdp.positions[..ticks]
        .iter()
        .map(|p| p[1])
        .fold(f32::MIN, f32::max);
    // v² / 2g = 0.82 m at 0.41 s, within a 1 s life.
    assert!((top - 0.815).abs() < 0.03, "top {top}");
    assert!(rdp.positions[ticks - 1][1] < top - 0.3, "falls back");
}
