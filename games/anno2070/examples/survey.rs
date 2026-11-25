// Shipped-data surveys behind 2070's object tools, over every island the
// install ships:
// - `turns`: how buildings, walls and props are turned (`m_Direction` and
//   the drawn rotation), per list and asset template
// - `templates`: the assets each list places, with counts, to find the
//   harbour pieces
// - `index`: how long reading the shipped-record index takes
// - `harbour <guid>...`: how those pieces are laid: spacing to the next
//   piece, turn, and which side the sea is on
//
//   ANNO2070_DIR=... cargo run --release -p anno2070 --example survey -- turns

use std::collections::BTreeMap;

use anno_formats::isd::Doc;
use anno_formats::isd::objects::{self, Section};
use anno_formats::isd::terrain::Terrain;
use anno_formats::vfs::{FileId, Install};
use anno2070::objects::Shipped;

const QUARTER: i64 = 368_640;

fn islands(install: &Install) -> impl Iterator<Item = (String, Vec<u8>)> + '_ {
    (0..install.file_count()).filter_map(|i| {
        let id = FileId(i as u32);
        let path = install.file_path(id);
        path.ends_with(".isd")
            .then(|| (path.to_owned(), install.read_file(id).unwrap()))
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::env::var_os("ANNO2070_DIR").expect("ANNO2070_DIR");
    use anno_core::game::Module;
    let families = anno2070::MODULE.profile().archive_families;
    let install = Install::open(dir, families).unwrap();
    let assets = anno2070::assets::load(&install).unwrap();
    let template = |guid: u32| assets.get(guid).map_or("?", |a| a.template.as_str());
    match args.first().map(String::as_str) {
        Some("turns") => turns(&install, &template),
        Some("templates") => templates(&install, &assets),
        Some("index") => {
            let t = std::time::Instant::now();
            let index = Shipped::read(&install).unwrap();
            let quarter = (assets.iter())
                .filter(|a| index.quarter_turns(a.guid))
                .count();
            println!(
                "{} assets shipped, {quarter} only on quarter turns, read in {:?}",
                index.len(),
                t.elapsed()
            );
        }
        Some("asset") => {
            for g in &args[1..] {
                let a = assets.get(g.parse().unwrap()).unwrap();
                println!("{} {} {}", a.guid, a.name, a.template);
                for (k, c) in a.cfgs.iter().enumerate() {
                    println!("  {k}\t{c}");
                }
            }
        }
        Some("perguid") => perguid(&install, &assets),
        Some("offturn") => offturn(&install, &assets, &args[1..]),
        Some("harbour") => {
            let guids: Vec<u32> = args[1..].iter().map(|g| g.parse().unwrap()).collect();
            harbour(&install, &guids)
        }
        _ => eprintln!("survey turns | templates | harbour <guid>..."),
    }
}

// Per list and template: records, how many stand on a quarter turn
// (m_Direction a multiple of 368,640), how many on an eighth, and how many
// draw tilted (a rotation that isn't the yaw of their direction).
fn turns<'a>(install: &Install, template: &dyn Fn(u32) -> &'a str) {
    #[derive(Default)]
    struct Count {
        all: usize,
        quarter: usize,
        eighth: usize,
        tilted: usize,
        yaw_off: usize,
        // m_Direction mod a full turn, in quarter turns, bucketed by 1/16.
        buckets: BTreeMap<i64, usize>,
    }
    let mut counts: BTreeMap<(String, String), Count> = BTreeMap::new();
    let mut files = 0;
    for (path, bytes) in islands(install) {
        files += 1;
        let doc = Doc::parse(&bytes).unwrap();
        for o in objects::read(&doc, &path).unwrap() {
            let key = (format!("{:?}", o.section), template(o.guid).to_owned());
            let c = counts.entry(key).or_default();
            c.all += 1;
            let d = o.direction.rem_euclid(4 * QUARTER);
            c.quarter += usize::from(d % QUARTER == 0);
            c.eighth += usize::from(d % (QUARTER / 2) == 0);
            *c.buckets.entry(d * 16 / QUARTER).or_default() += 1;
            let [x, _, z, _] = o.rotation;
            c.tilted += usize::from(x.abs() > 1e-3 || z.abs() > 1e-3);
            // The drawn yaw against the direction's, as the yaw-only
            // quaternion (0, -sin(a/2), 0, cos(a/2)) holds it.
            let drawn = -2. * o.rotation[1].atan2(o.rotation[3]);
            let want = d as f32 / QUARTER as f32 * std::f32::consts::FRAC_PI_2;
            let off = (drawn - want).rem_euclid(std::f32::consts::TAU);
            let off = off.min(std::f32::consts::TAU - off);
            c.yaw_off += usize::from(off > 0.01);
        }
    }
    println!("{files} islands");
    println!(
        "list\ttemplate\trecords\tquarter\teighth\ttilted\tyaw!=dir\tother directions (1/16 turns: n)"
    );
    for ((section, template), c) in &counts {
        let other: Vec<String> = (c.buckets.iter())
            .filter(|(b, _)| *b % 16 != 0)
            .take(8)
            .map(|(b, n)| format!("{b}:{n}"))
            .collect();
        println!(
            "{section}\t{template}\t{}\t{}\t{}\t{}\t{}\t{}",
            c.all,
            c.quarter,
            c.eighth,
            c.tilted,
            c.yaw_off,
            other.join(" ")
        );
    }
}

// Per asset: records, and how many stand more than 1/100 off a quarter turn.
fn perguid(install: &Install, assets: &anno_formats::assets::Assets) {
    let mut counts: BTreeMap<u32, (usize, usize, String)> = BTreeMap::new();
    for (path, bytes) in islands(install) {
        let doc = Doc::parse(&bytes).unwrap();
        for o in objects::read(&doc, &path).unwrap() {
            let d = o.direction.rem_euclid(QUARTER);
            let off = d.min(QUARTER - d) > QUARTER / 100;
            let c = counts
                .entry(o.guid)
                .or_insert((0, 0, format!("{:?}", o.section)));
            c.0 += 1;
            c.1 += usize::from(off);
        }
    }
    println!("guid\tname\ttemplate\tlist\trecords\toff");
    for (g, (n, off, list)) in counts {
        let a = assets.get(g);
        println!(
            "{g}\t{}\t{}\t{list}\t{n}\t{off}",
            a.map_or("?", |a| a.name.as_str()),
            a.map_or("?", |a| a.template.as_str())
        );
    }
}

// Records of `templates` off the quarter turns: island, GUID, name, turn.
fn offturn(install: &Install, assets: &anno_formats::assets::Assets, templates: &[String]) {
    println!("island\tguid\tname\ttemplate\tdirection/q\tplayer");
    for (path, bytes) in islands(install) {
        let doc = Doc::parse(&bytes).unwrap();
        for o in objects::read(&doc, &path).unwrap() {
            let Some(a) = assets.get(o.guid) else {
                continue;
            };
            if !templates.contains(&a.template) || o.direction.rem_euclid(QUARTER) == 0 {
                continue;
            }
            println!(
                "{path}\t{}\t{}\t{}\t{:.4}\t{:?}",
                o.guid,
                a.name,
                a.template,
                o.direction as f64 / QUARTER as f64,
                o.player
            );
        }
    }
}

fn templates(install: &Install, assets: &anno_formats::assets::Assets) {
    let mut counts: BTreeMap<(String, String, u32, String), (usize, usize)> = BTreeMap::new();
    for (path, bytes) in islands(install) {
        let doc = Doc::parse(&bytes).unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for o in objects::read(&doc, &path).unwrap() {
            if matches!(o.section, Section::Nature | Section::Grass) {
                continue;
            }
            let a = assets.get(o.guid);
            let key = (
                format!("{:?}", o.section),
                a.map_or("?".into(), |a| a.template.clone()),
                o.guid,
                a.map_or("?".into(), |a| a.name.clone()),
            );
            let c = counts.entry(key.clone()).or_default();
            c.0 += 1;
            if seen.insert(key) {
                c.1 += 1;
            }
        }
    }
    println!("list\ttemplate\tguid\tname\trecords\tislands");
    for ((s, t, g, n), (r, i)) in counts {
        println!("{s}\t{t}\t{g}\t{n}\t{r}\t{i}");
    }
}

// For each record of `guids`: its turn, the distance to the nearest other
// record of `guids` and that one's GUID, and which way the ground falls:
// the height 6 m out along each of the four quarter-turn axes of its frame
// (+x, +z, -x, -z after its turn), so "lowest" names the side the sea is on.
fn harbour(install: &Install, guids: &[u32]) {
    println!(
        "island\tid\tguid\tvariation\tx\tz\ty\tdirection/q\tnearest m\tnearest guid\tdx\tdz\tlowest side\th(+x,+z,-x,-z)"
    );
    for (path, bytes) in islands(install) {
        let doc = Doc::parse(&bytes).unwrap();
        let all = objects::read(&doc, &path).unwrap();
        let pieces: Vec<_> = all.iter().filter(|o| guids.contains(&o.guid)).collect();
        if pieces.is_empty() {
            continue;
        }
        let corners = Terrain::read(&doc, &path).unwrap().corners();
        let height = |x: f32, z: f32| {
            let (xi, zi) = (x.round().max(0.) as usize, z.round().max(0.) as usize);
            corners.get(xi, zi).unwrap_or(f32::NAN)
        };
        let name = path.rsplit('/').next().unwrap();
        for o in &pieces {
            let near = (pieces.iter())
                .filter(|p| p.id != o.id)
                .map(|p| {
                    let (dx, dz) = (p.position[0] - o.position[0], p.position[2] - o.position[2]);
                    (dx.hypot(dz), p.guid, dx, dz)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0));
            let a = o.direction as f32 / QUARTER as f32 * std::f32::consts::FRAC_PI_2;
            let (s, c) = a.sin_cos();
            // Local +x and +z turned by the yaw quaternion (0,-sin,0,cos).
            let axes = [[c, s], [-s, c], [-c, -s], [s, -c]];
            let h: Vec<f32> = (axes.iter())
                .map(|v| height(o.position[0] + 6. * v[0], o.position[2] + 6. * v[1]))
                .collect();
            let lowest = (0..4).min_by(|&i, &j| h[i].total_cmp(&h[j])).unwrap();
            let (nd, ng, dx, dz) = near.unwrap_or((f32::NAN, 0, f32::NAN, f32::NAN));
            println!(
                "{name}\t{}\t{}\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.3}\t{nd:.2}\t{ng}\t{dx:.2}\t{dz:.2}\t{}\t{:.1},{:.1},{:.1},{:.1}",
                o.id,
                o.guid,
                o.variation,
                o.position[0],
                o.position[2],
                o.position[1],
                o.direction as f64 / QUARTER as f64,
                ["+x", "+z", "-x", "-z"][lowest],
                h[0],
                h[1],
                h[2],
                h[3]
            );
        }
    }
}
