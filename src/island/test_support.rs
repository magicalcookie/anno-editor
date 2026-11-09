// Shared setup for the island tests: the installed games, shipped and new
// islands in the editor, and a save read back. Game tests return early
// (pass silently) when ANNO117_DIR or ANNO1800_DIR is unset.
use super::{IslandEditor, Skeleton, Spec, Start, doc::Doc, harbour::Library, skeletons};
use crate::project::Project;
use anno_formats::{
    assets::Assets,
    grids::{Kind, Layer},
    island::Island,
    objects::{Object, Objects},
    rda,
    vfs::Install,
};
use std::sync::Arc;

/// A 117 pirate island: over a thousand objects, props and material maps.
pub const PIRATE: &str = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m";
/// A small shipped 117 pool island.
pub const SMALL: &str =
    "data/base/provinces/roman/islands/pool/roman_island_small_02/roman_island_small_02.a7m";
/// A medium shipped 1800 Old World island.
pub const MODERATE_M_01: &str =
    "data/sessions/islands/pool/moderate/moderate_m_01/moderate_m_01.a7m";

/// The installed Anno 117, or None without ANNO117_DIR.
pub fn install() -> Option<Install> {
    Some(Install::open(std::env::var_os("ANNO117_DIR")?).unwrap())
}

/// The installed Anno 1800, or None without ANNO1800_DIR.
pub fn install_1800() -> Option<Install> {
    Some(Install::open(std::env::var_os("ANNO1800_DIR")?).unwrap())
}

/// Shipped Anno 117 island `path` open in the editor, with its install.
pub fn open(path: &str) -> Option<(Install, IslandEditor)> {
    let install = install()?;
    let editor = IslandEditor::load(&install, path).unwrap();
    Some((install, editor))
}

/// A new island called `name` on `skeleton`, its terrain started as `start`.
pub fn create(install: &Install, skeleton: Skeleton, start: Start, name: &str) -> IslandEditor {
    let spec = Spec {
        name: name.into(),
        skeleton,
        start,
        listing: Default::default(),
    };
    IslandEditor::create(install, &spec).unwrap()
}

/// A new `side` m island of `region` called `name`, started as `start`,
/// on the shipped template of that size.
pub fn new_island(
    install: &Install,
    region: &str,
    side: u32,
    start: Start,
    name: &str,
) -> IslandEditor {
    let skeleton = (skeletons(install, None).into_iter())
        .find(|s| s.region == region && s.world[0] == side)
        .unwrap_or_else(|| panic!("no {side} m {region} template"));
    create(install, skeleton, start, name)
}

/// The game's asset registry.
pub fn assets(install: &Install) -> Arc<Assets> {
    Arc::new(Assets::load(install).unwrap())
}

/// The harbour, slot and river pieces of the editor's region, as the
/// Harbour and River tools load them.
pub fn harbour_library(install: &Install, assets: &Assets, editor: &IslandEditor) -> Arc<Library> {
    Arc::new(Library::load(install, assets, &editor.region()).unwrap())
}

/// Grid layer `kind` of `doc`.
pub fn layer(doc: &Doc, kind: Kind) -> &Layer {
    doc.grids.as_ref().unwrap().layer(kind).unwrap()
}

/// `editor` staged into a project of its own (kept while the TempDir lives).
pub fn staged(install: &Install, editor: &mut IslandEditor) -> (tempfile::TempDir, Project) {
    let config = tempfile::tempdir().unwrap();
    let mut project = Project::load(config.path(), install.game());
    (editor.stage(install, &mut project))
        .unwrap_or_else(|e| panic!("staging {}: {e:#}", editor.path));
    (config, project)
}

/// Writes `project`, mounts it over the install and opens `path` again.
pub fn reopen(install: &Install, project: &mut Project, path: &str) -> IslandEditor {
    project.write().unwrap();
    install.mount_mod(&project.folder()).unwrap();
    IslandEditor::load(install, path).unwrap()
}

/// Island `path` as staged in `project`.
pub fn saved(project: &Project, path: &str) -> Island {
    let archive = rda::Archive::from_bytes(project.files[path].clone(), path).unwrap();
    Island::parse(
        &archive.read_path("gamedata.data").unwrap(),
        &archive.read_path("rd3d.data").unwrap(),
    )
    .unwrap()
}

/// The objects `island` stores.
pub fn objects(island: &Island) -> Vec<Object> {
    Objects::read(island.gamedata()).unwrap().objects
}
