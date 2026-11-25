// Every shipped island's River records by RenderFlags: which set
// IWF_REVERSE (8) or IWF_LAVA (4).
// cargo run --release --example river_flags -- <install>
use anno_core::game::Module;
use anno_formats::{a7m::objects::Areas, adf::Adf, rda::Archive, vfs::Install};
use std::{collections::BTreeMap, path::Path};

fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).expect("install dir");
    let module = [&anno117::MODULE as &dyn Module, &anno1800::MODULE]
        .into_iter()
        .find(|m| anno_core::discovery::is_install(Path::new(&dir), m.profile().exe))
        .expect("a 117 or 1800 install");
    let install = Install::open(&dir, module.profile().archive_families)?;
    let mut paths: Vec<String> = (install.search(".a7m", 100_000).into_iter())
        .map(|id| install.file_path(id).to_owned())
        .filter(|p| p.ends_with(".a7m"))
        .collect();
    paths.sort();
    paths.dedup();
    let (mut islands, mut rivers) = (0, 0);
    let mut by_flags: BTreeMap<Option<u32>, usize> = BTreeMap::new();
    for path in &paths {
        let Ok(archive) = Archive::parse(install.read(path)?, path) else {
            continue;
        };
        let Ok(gd) = archive.read_path("gamedata.data") else {
            continue;
        };
        let Ok(gd) = Adf::parse(&gd) else { continue };
        let Ok((areas, objects)) = Areas::read(&gd) else {
            continue;
        };
        islands += 1;
        for o in &objects {
            let fields = areas.component_floats(&o.origin, "River");
            if fields.is_empty() || o.path.is_none() {
                continue;
            }
            rivers += 1;
            let flags = (fields.iter())
                .find(|(n, _)| n == "RenderFlags")
                .and_then(|(_, v)| v.first())
                .map(|f| f.to_bits());
            *by_flags.entry(flags).or_default() += 1;
            // Lava, reversed or 0x20 records: where each runs and what it holds.
            if flags.is_some_and(|f| f & (4 | 8 | 0x20) != 0) {
                let pts = o.path.as_ref().unwrap();
                let (a, b) = (pts[0].p, pts[pts.len() - 1].p);
                let ys: Vec<String> = (pts.iter().step_by((pts.len() / 6).max(1)))
                    .map(|q| format!("{:.0}", q.p[1]))
                    .collect();
                let file = path.rsplit('/').next().unwrap_or(path);
                println!(
                    "flags={:?} guid={} id={:#x} n={} first=({:.0},{:.0},{:.0}) last=({:.0},{:.0},{:.0}) y[..]={} {file}",
                    flags,
                    o.guid,
                    o.id,
                    pts.len(),
                    a[0],
                    a[1],
                    a[2],
                    b[0],
                    b[1],
                    b[2],
                    ys.join(",")
                );
                let names: Vec<String> = (fields.iter())
                    .map(|(n, v)| match n.as_str() {
                        "RenderFlags" => format!("{n}"),
                        _ => format!("{n}={:?}", &v[..v.len().min(4)]),
                    })
                    .collect();
                println!("    {}", names.join(" "));
            }
        }
    }
    println!("{islands} islands, {rivers} river records");
    for (f, n) in &by_flags {
        println!("{n:6}  RenderFlags {f:?}");
    }
    Ok(())
}
