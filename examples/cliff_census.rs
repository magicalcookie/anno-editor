// Counts the cliff and rock props (island::cliffs::is_rock's rule) shipped
// islands of a province place, by cfg: what a region's CliffSet takes.
// cargo run --release --example cliff_census -- <install> <path marker, e.g. /provinces/egypt/>
use anno_core::game::Module;
use anno_formats::{a7m::island::Island, rda::Archive, vfs::Install};
use std::collections::BTreeMap;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let module = &anno117::MODULE as &dyn Module;
    let install = Install::open(&a[1], module.profile().archive_families)?;
    let mut paths: Vec<String> = (install.search(".a7m", 100_000).into_iter())
        .map(|id| install.file_path(id).to_owned())
        .filter(|p| {
            p.ends_with(".a7m")
                && p.contains(&a[2])
                && p.contains("/pool/")
                && !p.contains("_layer")
        })
        .collect();
    paths.sort();
    paths.dedup();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut islands = 0;
    for path in &paths {
        let Ok(archive) = Archive::parse(install.read(path)?, path) else {
            continue;
        };
        let (Ok(gd), Ok(rd)) = (
            archive.read_path("gamedata.data"),
            archive.read_path("rd3d.data"),
        ) else {
            continue;
        };
        let Ok(island) = Island::parse(&gd, &rd) else {
            continue;
        };
        islands += 1;
        let files = island.prop_files();
        for p in island.props() {
            let Some(file) = files.get(p.file as usize) else {
                continue;
            };
            let cfg = file.to_ascii_lowercase().replace('\\', "/");
            if (cfg.contains("cliff") || cfg.contains("/rocks/")) && !cfg.contains("decal") {
                *counts.entry(cfg).or_default() += 1;
            }
        }
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    println!("{islands} islands");
    for (cfg, n) in v.iter().take(40) {
        println!("{n:6} {cfg}");
    }
    Ok(())
}
