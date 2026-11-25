// Times what opening a game costs: the archive index, assets.xml, and
// mounting mod folders over it.
// `cargo run --release --example open_timing -- <game dir> [mod dir...]`.
use anno_core::game::Module;
use anno_formats::{assets::Assets, vfs::Install};
use std::{path::Path, time::Instant};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("game folder");
    let mods: Vec<String> = args.collect();
    let module = [&anno117::MODULE as &dyn Module, &anno1800::MODULE]
        .into_iter()
        .find(|m| anno_core::discovery::is_install(Path::new(&dir), m.profile().exe))
        .expect("a 117 or 1800 install");
    let t = Instant::now();
    let install = Install::open(&dir, module.profile().archive_families)?;
    eprintln!("index: {:?}, {} files", t.elapsed(), install.file_count());
    let t = Instant::now();
    let schema = module.database().expect("an asset database");
    let _assets = Assets::load(&install, schema)?;
    eprintln!("assets.xml: {:?}", t.elapsed());
    let dirs: Vec<&Path> = mods.iter().map(Path::new).collect();
    let t = Instant::now();
    let n = install.mount_mods(&dirs, &["data"])?;
    eprintln!(
        "mount {} mod folders: {:?}, {n} files",
        dirs.len(),
        t.elapsed()
    );
    Ok(())
}
