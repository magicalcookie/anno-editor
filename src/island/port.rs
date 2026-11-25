// Brings models into the mod from outside: props of another game (1800
// prps into a 117 mod, 117 cfgs into an 1800 mod, 2070 cfgs into either)
// and model files from disk (a cfg with its meshes and textures, or a bare
// mesh). Also lists the models a project carries and which files each one
// needs.
//
// Another game's prop goes through the neutral model (core::neutral): the
// source game's module reads it, the target game's writes it, so a game
// needs one reader and one writer instead of a converter per other game.
// What each conversion keeps and drops lives with those readers and
// writers in the game crates.
use anno_core::files;
use anno_core::game::Module;
use anno_core::neutral::{self, Ported, Source, texture_files};
use anno_formats::{
    model::{Transform, texture_path},
    rda::normalize_path,
    rdm::Rdm,
    vfs::Install,
    xml::tag,
};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where ported files live in the mod: `<root>/editor/anno<game>/` under
/// the target's first project root, then the source path without its root.
const EDITOR: &str = "editor/anno";

fn folder(from: &dyn Module, root: &str) -> String {
    format!("{root}/{EDITOR}{}/", from.profile().short_name)
}

/// The project root ported models go under in `to`; none for a game
/// without projects, which can't take any.
fn root(to: &dyn Module) -> Option<&'static str> {
    to.project_roots().first().copied()
}

/// Whether `path` is a model the island import ported from the other game:
/// under the ported folder of any project root.
pub fn is_ported(path: &str) -> bool {
    path.split_once('/')
        .is_some_and(|(_, rest)| rest.starts_with(EDITOR))
}

/// Whether `source`, a model of game `from`, can be brought into `to`:
/// props of a game `to` ports from, and effect cfgs placed on their own,
/// into a game with projects. Decals become the target's decals and effects
/// its own effects; which ones the source game's reader turns away (1800's
/// mesh decals) fail when ported.
pub fn portable(from: &dyn Module, to: &dyn Module, source: &str) -> bool {
    root(to).is_some()
        && to.ports().props_from.contains(&from.game())
        && (source.ends_with(from.ports().prop_ext)
            || (source.ends_with(".cfg") && source.contains("/effects/")))
}

/// Decals and effects. Nothing in the target game looks like either, so the
/// wizard brings them instead of matching by look.
pub fn always_brought(source: &str) -> bool {
    source.contains("/decals/") || source.contains("/effects/")
}

/// The model a ported `source` of `from` is placed as in `to`. `to` has
/// projects (`portable`).
pub fn target(from: &dyn Module, to: &dyn Module, source: &str) -> String {
    let stem = (source.strip_suffix(from.ports().prop_ext))
        .or_else(|| source.strip_suffix(".cfg"))
        .unwrap_or(source);
    let root = root(to).expect("a portable target has a project root");
    moved(from, root, &format!("{stem}{}", to.ports().prop_ext))
}

/// `path`, a file of `from`, under the ported folder in `root`. Its own
/// root comes off: one of `from`'s, else `data/`, where every game ships
/// (a game without project roots).
fn moved(from: &dyn Module, root: &str, path: &str) -> String {
    let rel = (from.project_roots().iter().copied().chain(["data"]))
        .find_map(|r| path.strip_prefix(r)?.strip_prefix('/'))
        .unwrap_or(path);
    format!("{}{}", folder(from, root), rel.to_ascii_lowercase())
}

/// Another game's install as a converter's source: files under the ported
/// folder named for that game, in the target's `root`.
struct Shipped<'a> {
    from: &'a dyn Module,
    root: &'a str,
    install: &'a Install,
}

impl Source for Shipped<'_> {
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.install.read(path)
    }

    fn place(&self, path: &str) -> String {
        moved(self.from, self.root, path)
    }
}

/// Ports prop `source` of module `from`, another game read from `from_install`,
/// for module `to`, written beside `to_install`'s template props and placed
/// under `to`'s first project root.
pub fn port(
    from: &dyn Module,
    to: &dyn Module,
    from_install: &Install,
    to_install: &Install,
    source: &str,
) -> Result<Ported> {
    ensure!(
        portable(from, to, source),
        "{source}: not a prop {} can bring in",
        to.profile().title
    );
    let model = (from.read_model(from_install, source))
        .with_context(|| format!("{} models are not read", from.profile().title))??;
    let shipped = Shipped {
        from,
        root: root(to).expect("checked by portable"),
        install: from_install,
    };
    let out = (to.write_model(&model, &shipped, to_install))
        .with_context(|| format!("{} models are not written", to.profile().title))??;
    if out.pieces.is_empty() && out.effects.is_empty() {
        if source.contains("/effects/") {
            bail!("{source}: {} has no effect like it", to.profile().title);
        }
        let shown: Vec<&str> = out.left_out.iter().take(3).map(String::as_str).collect();
        if shown.is_empty() {
            bail!("it holds no mesh (animated units and effects only)");
        }
        bail!(
            "nothing in it can be an {} prop: {}",
            to.profile().short_name,
            shown.join("; ")
        );
    }
    Ok(out)
}

/// Files to stage: virtual path and bytes.
type Files = Vec<(String, Vec<u8>)>;

/// Where island `name`'s own material set goes in a mod of `to`, under the
/// ported folder of `from`, the game its materials come from: a file per
/// island, so editing it never changes another island's set. None for a
/// game without projects.
pub fn material_set_path(from: &dyn Module, to: &dyn Module, name: &str) -> Option<String> {
    Some(format!(
        "{}material_sets/{}.xml",
        folder(from, root(to)?),
        name.to_ascii_lowercase()
    ))
}

/// The texture tags of a ground material xml (both games).
const MATERIAL_TEXTURES: [&str; 4] = [
    "DiffuseFileName",
    "HeightFileName",
    "SpecFileName",
    "NormalFileName",
];

/// A ground material of the other game (`file` is its xml) for `to`'s
/// sets: the xml and its textures under the ported folder of `to`'s first
/// project root, xml first. Both
/// games use the same four maps (diffuse with height in alpha, height, spec,
/// normal) and BC7 textures. We write the xml in the short-closed form 1800's
/// own materials use.
pub fn port_material(
    from_module: &dyn Module,
    to: &dyn Module,
    from: &Install,
    file: &str,
) -> Result<(String, Files)> {
    let root = root(to).with_context(|| format!("{} has no projects", to.profile().title))?;
    let xml = String::from_utf8_lossy(&from.read(file)?).into_owned();
    let moved = |path: &str| moved(from_module, root, path);
    let shipped = Shipped {
        from: from_module,
        root,
        install: from,
    };
    let mut files = Vec::new();
    let mut out = String::from("<Material>");
    for name in MATERIAL_TEXTURES {
        let Some(source) = tag(&xml, name).filter(|t| !t.is_empty()) else {
            continue;
        };
        let source = source.replace('\\', "/");
        let maps = texture_files(&shipped, &source);
        ensure!(
            !maps.is_empty() || name != "DiffuseFileName",
            "{file}: its diffuse texture does not ship"
        );
        files.extend(maps);
        out.push_str(&format!("<{name}>{}</>\n", moved(&source)));
    }
    out.push_str("</>\n");
    let path = moved(file);
    files.insert(0, (path.clone(), out.into_bytes()));
    Ok((path, files))
}

/// Where a model imported from a loose folder goes: its own folder under
/// the mod's namespace, so it can never replace a shipped or another mod's
/// file.
pub fn custom_folder(mod_id: &str, name: &str) -> String {
    format!(
        "data/{}/models/{}",
        mod_id.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    )
}

/// What a project model is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    /// Brought over from the other game by the island import.
    Ported,
    /// Added from disk.
    Custom,
}

/// A placeable model the project carries: its cfg (or 1800 prp) and every
/// staged file it needs.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectModel {
    pub cfg: String,
    pub kind: ModelKind,
    pub files: Vec<String>,
    /// Bytes over all its files.
    pub size: usize,
}

/// The models among the project's files (texts in `files`; `size` of any
/// file, kept or staged): the files whose kind in `module` names others
/// (`Kind::deps`). A model's files are the ones it names, with their
/// distance levels and mips, when staged.
pub fn project_models(
    module: &dyn Module,
    files: &BTreeMap<String, Vec<u8>>,
    size: impl Fn(&str) -> Option<usize>,
) -> Vec<ProjectModel> {
    (files.iter())
        .filter_map(|(p, bytes)| Some((p, bytes, files::kind_of(module, p)?.deps?)))
        .map(|(cfg, bytes, deps)| {
            let mut own = vec![cfg.clone()];
            own.extend(deps(bytes).into_iter().filter(|f| size(f).is_some()));
            own.sort();
            own.dedup();
            ProjectModel {
                kind: if is_ported(cfg) {
                    ModelKind::Ported
                } else {
                    ModelKind::Custom
                },
                size: own.iter().filter_map(|f| size(f)).sum(),
                files: own,
                cfg: cfg.clone(),
            }
        })
        .collect()
}

/// Files of `model` no other model of the project needs: what removing it
/// deletes.
pub fn removable(models: &[ProjectModel], model: &ProjectModel) -> Vec<String> {
    (model.files.iter())
        .filter(|f| {
            !models
                .iter()
                .any(|m| m.cfg != model.cfg && m.files.contains(f))
        })
        .cloned()
        .collect()
}

/// `rel` under `base`, matching each component without case: mods made on
/// Windows mix cases their cfgs do not.
fn find_ci(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut at = base.to_owned();
    for part in rel.split(['/', '\\']).filter(|p| !p.is_empty()) {
        let exact = at.join(part);
        if exact.exists() {
            at = exact;
            continue;
        }
        let entry = std::fs::read_dir(&at)
            .ok()?
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part))?;
        at = entry.path();
    }
    Some(at)
}

/// One model brought in from disk: files to stage, its cfg first.
#[derive(Debug)]
pub struct Imported {
    pub cfg: String,
    pub files: Vec<(String, Vec<u8>)>,
    /// References left pointing at the game's own files.
    pub shipped: usize,
}

/// Brings the model at `source` into the mod `mod_id`: a `.cfg` (or 1800
/// `.prp`) with the files it references, or (117) a bare `.rdm` wrapped
/// in a shipped prop's cfg with the textures beside it.
///
/// A cfg inside a mod's `data/` tree keeps its paths. A cfg in a loose
/// folder goes to [`custom_folder`] and its references are rewritten there.
/// Files the game ships are referenced, never copied.
pub fn import(
    module: &dyn Module,
    source: &Path,
    mod_id: &str,
    install: &Install,
) -> Result<Imported> {
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "cfg" | "prp" => import_cfg(source, &ext, mod_id, install),
        "rdm" => import_mesh(module, source, mod_id, install),
        _ => bail!("pick a .cfg, .prp or .rdm file"),
    }
}

fn stem_of(source: &Path) -> String {
    source
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "model".to_owned())
}

fn import_cfg(source: &Path, ext: &str, mod_id: &str, install: &Install) -> Result<Imported> {
    let text = String::from_utf8_lossy(
        &std::fs::read(source).with_context(|| format!("Reading {}", source.display()))?,
    )
    .into_owned();
    let dir = source.parent().context("the file has a folder")?;
    // Inside a mod: the folder holding the `data/` the cfg is in.
    let root = source
        .ancestors()
        .skip(1)
        .find(|a| source.starts_with(a.join("data")) || source.starts_with(a.join("Data")));
    let target = custom_folder(mod_id, &stem_of(source));
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut cfg = text.clone();
    let mut missing = Vec::new();
    let mut shipped = 0;
    let mut kept_paths = true;
    for r in files::references(&text) {
        let wanted = files::shipped_path(&r);
        let in_root = root.and_then(|root| find_ci(root, &wanted));
        let beside = || find_ci(dir, wanted.rsplit('/').next()?);
        let (found, keep) = match in_root {
            Some(path) => (path, true),
            None => match beside() {
                Some(path) => (path, false),
                None if install.file(&wanted).is_some() => {
                    shipped += 1;
                    continue;
                }
                None => {
                    missing.push(r);
                    continue;
                }
            },
        };
        kept_paths &= keep;
        // The file's own name stays; the folder is the model's when the
        // cfg came from a loose folder.
        let (dest, new_ref) = if keep {
            (wanted.clone(), r.clone())
        } else {
            let name = r.rsplit(['/', '\\']).next().unwrap_or(&r);
            let new_ref = format!("{target}/{name}");
            (files::shipped_path(&new_ref), new_ref)
        };
        if install.file(&dest).is_some() {
            shipped += 1;
            continue;
        }
        let dest_family = files::siblings(&dest);
        for (n, sibling) in files::siblings(&wanted).iter().enumerate() {
            let name = sibling.rsplit('/').next().unwrap_or(sibling);
            let path = if n == 0 {
                Some(found.clone())
            } else {
                found.parent().and_then(|p| find_ci(p, name))
            };
            if let Some(path) = path.filter(|p| p.is_file()) {
                files.push((dest_family[n].clone(), std::fs::read(&path)?));
            }
        }
        if new_ref != r {
            // Whole tag bodies only: `a.dds` must not rewrite `ba.dds`.
            cfg = cfg.replace(&format!(">{r}<"), &format!(">{new_ref}<"));
        }
    }
    if !missing.is_empty() {
        let shown: Vec<&str> = missing.iter().take(5).map(String::as_str).collect();
        bail!(
            "{} the model needs {} neither beside it nor in the game: {}",
            crate::project::plural(missing.len(), "file", "files"),
            if missing.len() == 1 { "is" } else { "are" },
            shown.join(", ")
        );
    }
    let cfg_path = match root {
        Some(root) if kept_paths => normalize_path(
            &source
                .strip_prefix(root)
                .unwrap_or(source)
                .to_string_lossy()
                .replace('\\', "/"),
        )
        .into_owned(),
        _ => format!("{target}/{}.{ext}", stem_of(source)),
    };
    ensure!(
        install.file(&cfg_path).is_none(),
        "{cfg_path} is a shipped model; a mod copy would replace it everywhere. Rename the file"
    );
    files.insert(0, (cfg_path.clone(), cfg.into_bytes()));
    Ok(Imported {
        cfg: cfg_path,
        files,
        shipped,
    })
}

/// A bare mesh: a cfg from the shipped templates, one material per mesh
/// material, all using the textures beside the mesh (`*diff*`, `*norm*`).
fn import_mesh(
    module: &dyn Module,
    source: &Path,
    mod_id: &str,
    install: &Install,
) -> Result<Imported> {
    ensure!(
        module.ports().mesh_import,
        "Anno 1800 props need a .prp; pick the model’s .prp or .cfg"
    );
    let bytes = std::fs::read(source).with_context(|| format!("Reading {}", source.display()))?;
    let rdm = Rdm::parse(&bytes).with_context(|| source.display().to_string())?;
    ensure!(
        !rdm.skinned,
        "skinned meshes need an animated cfg; pick the model’s .cfg"
    );
    let name = stem_of(source);
    let stem = name.trim_end_matches("_lod0");
    let target = custom_folder(mod_id, stem);
    let mesh = format!("{target}/{name}.rdm");
    let mut disk = Disk::default();
    disk.0.insert(mesh.clone(), source.to_owned());
    let dir = source.parent().context("the file has a folder")?;
    for k in 1..=4 {
        if let Some(p) = find_ci(dir, &format!("{stem}_lod{k}.rdm")) {
            disk.0.insert(format!("{target}/{stem}_lod{k}.rdm"), p);
        }
    }
    // The top mip of the first texture whose name says what it is, with
    // its lower mips.
    let mut texture = |kind: &[&str]| -> Result<Option<String>> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let n = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_ascii_lowercase());
                n.is_some_and(|n| n.ends_with(".dds") && kind.iter().any(|k| n.contains(k)))
            })
            .collect();
        found.sort();
        let Some(top) = found.first() else {
            return Ok(None);
        };
        let file = top
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        let base = file
            .trim_end_matches(".dds")
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches('_');
        let reference = format!("{target}/{base}.psd");
        for (n, sibling) in files::siblings(&texture_path(&reference))
            .iter()
            .enumerate()
        {
            let on_disk = found.iter().find(|p| {
                let f = p
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_ascii_lowercase();
                f == format!("{base}_{n}.dds") || (n == 0 && f == format!("{base}.dds"))
            });
            if let Some(p) = on_disk {
                disk.0.insert(sibling.clone(), p.clone());
            }
        }
        Ok(Some(reference))
    };
    let diffuse = texture(&["diff", "_d.", "albedo", "color"])?;
    let normal = texture(&["norm", "_n."])?;
    // One material: the writer repeats it for every material the mesh's
    // groups use.
    let model = neutral::Model {
        pieces: vec![neutral::Piece {
            name: format!("{target}/{stem}"),
            label: name.clone(),
            at: Transform::IDENTITY,
            shape: neutral::Shape::Mesh {
                rdm: mesh,
                transform: Transform::IDENTITY,
            },
            materials: vec![neutral::Material {
                diffuse,
                normal,
                metallic: None,
                color: [1.; 3],
                double_sided: false,
                alpha: anno_formats::model::Alpha::Opaque,
                ground_shaded: false,
                terrain_tint: 0.,
            }],
            tints: Vec::new(),
        }],
        ..neutral::Model::default()
    };
    let written = (module.write_model(&model, &disk, install))
        .with_context(|| format!("{} models are not written", module.profile().title))??;
    let cfg_path = format!("{target}/{stem}.cfg");
    let mut files = written.files;
    // The cfg first, as `Imported` promises.
    let at = files
        .iter()
        .position(|f| f.0 == cfg_path)
        .context("no cfg written")?;
    let cfg = files.remove(at);
    files.insert(0, cfg);
    Ok(Imported {
        cfg: cfg_path,
        files,
        shipped: 0,
    })
}

/// Files picked on disk as a converter's source, by the path each takes in
/// the mod; they keep that path.
#[derive(Default)]
struct Disk(BTreeMap<String, PathBuf>);

impl Source for Disk {
    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let file = self
            .0
            .get(path)
            .with_context(|| format!("no file {path}"))?;
        std::fs::read(file).with_context(|| format!("Reading {}", file.display()))
    }

    fn place(&self, path: &str) -> String {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::game::Game;
    use anno_formats::model::Model;

    /// The module of the game installed at `install`.
    fn module_of(install: &Install) -> &'static dyn Module {
        crate::games::module(crate::games::of(install))
    }

    /// A model cache drawing ported models as before looks: the tests
    /// check what loads, not how it glows.
    fn plain() -> super::super::models::Cache {
        super::super::models::Cache::new(&anno_core::files::ModelLook::PLAIN)
    }

    #[test]
    fn ported_paths_live_under_the_folder() {
        let (a1800, a117): (&dyn Module, &dyn Module) = (&::anno1800::MODULE, &::anno117::MODULE);
        assert_eq!(
            target(a1800, a117, "data/graphics/props/x/Tree_A.prp"),
            "data/editor/anno1800/graphics/props/x/tree_a.cfg"
        );
        assert_eq!(
            target(a117, a1800, "data/base/graphics/x/tree_a.cfg"),
            "data/editor/anno117/base/graphics/x/tree_a.prp"
        );
        assert!(is_ported(&target(a117, a1800, "data/base/x.cfg")));
        assert!(portable(a1800, a117, "a/b.prp"));
        assert!(!portable(a1800, a117, "a/b.cfg"));
        assert!(portable(a117, a1800, "a/props/b.cfg"));
        assert!(!portable(a117, a1800, "a/b.prp"));
        // Decals and effects come over both ways: 117's cfgs, 1800's decal
        // prps and the effect cfgs its islands place.
        assert!(portable(a117, a1800, "a/props/decals/beach/b.cfg"));
        assert!(portable(a117, a1800, "a/effects/fire/b.cfg"));
        assert!(portable(a1800, a117, "a/props/decals/b.prp"));
        assert!(portable(a1800, a117, "a/effects/fire/b.cfg"));
        assert!(!portable(a1800, a117, "a/props/b.cfg"));
        assert_eq!(
            target(a1800, a117, "data/graphics/effects/Fire.cfg"),
            "data/editor/anno1800/graphics/effects/fire.cfg"
        );
    }

    /// 117 under other project roots and nothing else: no islands, no
    /// packaging. A port that looked the target up by its `Game` would
    /// write under 117's `data/`.
    struct Rooted;
    impl Module for Rooted {
        fn game(&self) -> Game {
            Game::Anno117
        }
        fn profile(&self) -> &'static anno_core::game::Profile {
            ::anno117::MODULE.profile()
        }
        fn text(&self) -> &'static anno_core::text::GameText {
            ::anno117::MODULE.text()
        }
        fn card_art(&self) -> &'static [u8] {
            &[]
        }
        fn accent(&self) -> [u32; 2] {
            [0; 2]
        }
        // 117's port rules: what it takes and its prop extension.
        fn ports(&self) -> anno_core::files::Ports {
            ::anno117::MODULE.ports()
        }
        fn project_roots(&self) -> &'static [&'static str] {
            &["content", "extra"]
        }
        fn write_model(
            &self,
            model: &anno_core::neutral::Model,
            from: &dyn Source,
            to: &Install,
        ) -> Option<Result<Ported>> {
            ::anno117::MODULE.write_model(model, from, to)
        }
    }

    // Needs both games: an 1800 prop ported into a game whose first project
    // root is `content` lands under it, every file and piece, and is still
    // told apart as ported.
    #[test]
    fn a_port_lands_under_the_targets_project_root() {
        let (Some(a), Some(b)) = (
            anno_formats::vfs::install_dir("ANNO1800_DIR"),
            anno_formats::vfs::install_dir("ANNO117_DIR"),
        ) else {
            eprintln!("skipping: ANNO1800_DIR or ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let from = Install::open(a, families).unwrap();
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let to = Install::open(b, families).unwrap();
        let source = "data/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_23.prp";
        let module: &dyn Module = &::anno1800::MODULE;
        let placed = target(module, &Rooted, source);
        assert_eq!(
            placed,
            "content/editor/anno1800/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_23.cfg"
        );
        assert!(is_ported(&placed));
        let ported = port(module, &Rooted, &from, &to, source).unwrap();
        assert!(!ported.files.is_empty() && !ported.pieces.is_empty());
        for path in (ported.files.iter().map(|f| &f.0)).chain(ported.pieces.iter().map(|p| &p.0)) {
            assert!(path.starts_with("content/editor/anno1800/"), "{path}");
        }
        assert!(ported.pieces.iter().any(|p| p.0 == placed));
        // A game without projects takes no ports.
        assert!(!portable(module, &::anno2070::MODULE, source));
    }

    // Needs Anno 117: a 117 rock ports into the skeleton as its own text mesh
    // under its own root, every mesh and texture byte copied as shipped;
    // the skeleton reads that back into the model 117 read, and 117 writes
    // it again as a cfg that loads from a mod, its files under the folder
    // named for the skeleton.
    #[test]
    fn a_117_prop_ports_into_the_skeleton_and_back() {
        use crate::games::fixtures::{SKELETON, install};
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let a117 = Install::open(dir, families).unwrap();
        let m117: &dyn Module = &::anno117::MODULE;
        let rock =
            "data/base/graphics/roman/environment/props/rocks/roman_rock_06/roman_rock_06_a.cfg";
        let tmp = tempfile::tempdir().unwrap();
        let empty = install(&tmp.path().join("empty"), &[("skeleton/readme.txt", b"x")]);
        let placed = target(m117, &SKELETON, rock);
        assert_eq!(
            placed,
            "content/editor/anno117/base/graphics/roman/environment/props/rocks/roman_rock_06/roman_rock_06_a.skm"
        );
        let ported = port(m117, &SKELETON, &a117, &empty, rock).unwrap();
        assert_eq!(ported.pieces.len(), 1);
        assert_eq!(ported.pieces[0].0, placed);
        for (path, bytes) in &ported.files {
            assert!(path.starts_with("content/editor/anno117/"), "{path}");
            if !path.ends_with(".skm") {
                let shipped = path.replacen("content/editor/anno117/", "data/", 1);
                assert_eq!(bytes, &a117.read(&shipped).unwrap(), "{path}");
            }
        }
        assert!(ported.files.iter().any(|f| f.0.ends_with(".rdm")));
        assert!(ported.files.iter().any(|f| f.0.ends_with("_0.dds")));

        // Back: the skeleton reads its mesh from an install that holds it.
        let held: Vec<(&str, &[u8])> = (ported.files.iter())
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        let skel = install(&tmp.path().join("skeleton"), &held);
        let original = m117.read_model(&a117, rock).unwrap().unwrap();
        let read = SKELETON.read_model(&skel, &placed).unwrap().unwrap();
        assert_eq!(read.pieces.len(), original.pieces.len());
        for (a, b) in read.pieces.iter().zip(&original.pieces) {
            use neutral::Shape;
            let (
                Shape::Mesh {
                    rdm: ra,
                    transform: ta,
                },
                Shape::Mesh {
                    rdm: rb,
                    transform: tb,
                },
            ) = (&a.shape, &b.shape)
            else {
                panic!("{}: not a mesh", b.label);
            };
            assert_eq!(ta, tb);
            assert_eq!(skel.read(ra).unwrap(), a117.read(rb).unwrap());
            assert_eq!(a.materials.len(), b.materials.len());
            for (ma, mb) in a.materials.iter().zip(&b.materials) {
                assert_eq!(
                    (
                        ma.color,
                        ma.alpha,
                        ma.double_sided,
                        ma.ground_shaded,
                        ma.terrain_tint
                    ),
                    (
                        mb.color,
                        mb.alpha,
                        mb.double_sided,
                        mb.ground_shaded,
                        mb.terrain_tint
                    )
                );
                assert_eq!(ma.diffuse.is_some(), mb.diffuse.is_some());
            }
        }
        let back = Shipped {
            from: &SKELETON,
            root: "data",
            install: &skel,
        };
        let written = m117.write_model(&read, &back, &a117).unwrap().unwrap();
        // What 117 writes of its own rock, mesh for mesh and mip for mip.
        let direct = Shipped {
            from: m117,
            root: "data",
            install: &a117,
        };
        let direct = m117
            .write_model(&original, &direct, &a117)
            .unwrap()
            .unwrap();
        let bytes = |p: &Ported| -> Vec<Vec<u8>> {
            (p.files.iter())
                .filter(|(path, _)| !path.ends_with(".cfg"))
                .map(|(_, b)| b.clone())
                .collect()
        };
        assert_eq!(bytes(&written), bytes(&direct));
        let mods = tempfile::tempdir().unwrap();
        for (path, bytes) in &written.files {
            assert!(path.starts_with("data/editor/annoskel/"), "{path}");
            let file = mods.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, bytes).unwrap();
        }
        a117.mount_mods(&[mods.path()], &["data"]).unwrap();
        let cfg = &written.pieces[0].0;
        let model = Model::load(&a117, cfg).unwrap();
        assert!(model.problems.is_empty(), "{:?}", model.problems);
        let diffuse = model.parts[0].materials[0].diffuse.as_deref();
        assert!(diffuse.is_some_and(|t| a117.exists(t)), "{diffuse:?}");
        assert!(model.mesh_extent[1] > 0.1, "{:?}", model.mesh_extent);
    }

    // Needs both games: a ported jungle tree and rock load through the
    // editor's own model reader from a mod folder over the 117 install,
    // with their mesh, distance levels and textures.
    #[test]
    fn ported_props_load_from_the_mod() {
        let (Some(a), Some(b)) = (
            anno_formats::vfs::install_dir("ANNO1800_DIR"),
            anno_formats::vfs::install_dir("ANNO117_DIR"),
        ) else {
            eprintln!("skipping: ANNO1800_DIR or ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let from = Install::open(a, families).unwrap();
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let to = Install::open(b, families).unwrap();
        let dir = tempfile::tempdir().unwrap();
        for source in [
            "data/graphics/props/terrain_props/vegetation/trees/jungle/tree_jungle_very_large_01.prp",
            "data/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_23.prp",
        ] {
            let Ok(ported) = port(module_of(&from), module_of(&to), &from, &to, source) else {
                let found = from.search(source.rsplit('/').next().unwrap(), 5);
                panic!(
                    "{source}: {:?}",
                    found
                        .iter()
                        .map(|&id| from.file_path(id))
                        .collect::<Vec<_>>()
                );
            };
            assert!(
                ported.files.iter().any(|f| f.0.ends_with("_lod1.rdm")),
                "{source}: no levels"
            );
            assert!(
                ported.files.iter().any(|f| f.0.ends_with("_0.dds")),
                "{source}: no textures"
            );
            for (path, bytes) in &ported.files {
                let file = dir.path().join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, bytes).unwrap();
            }
            to.mount_mods(&[dir.path()], &["data"]).unwrap();
            let cfg = target(&::anno1800::MODULE, &::anno117::MODULE, source);
            let model = anno_formats::model::Model::load(&to, &cfg).unwrap();
            assert!(model.problems.is_empty(), "{:?}", model.problems);
            let part = model.parts.iter().find(|p| p.mesh.is_some()).unwrap();
            let diffuse = part.materials[0].diffuse.as_deref();
            assert!(diffuse.is_some_and(|t| to.exists(t)), "{diffuse:?}");
            assert!(model.mesh_extent[1] > 0.1, "{:?}", model.mesh_extent);
            let pieces = super::super::models::load(&to, &plain(), &cfg, 1.).unwrap();
            assert!(!pieces.is_empty());
        }
    }

    // Needs both games. 117 props become 1800 prps that load from a mod over
    // the 1800 install. The maritime pine is drawn at 5x in its cfg and has one
    // vertex color, so its prp mesh carries the scale and 1800's two-color
    // plant layout. The rock keeps its layout.
    #[test]
    fn ported_117_props_load_in_1800() {
        let (Some(a), Some(b)) = (
            anno_formats::vfs::install_dir("ANNO117_DIR"),
            anno_formats::vfs::install_dir("ANNO1800_DIR"),
        ) else {
            eprintln!("skipping: ANNO1800_DIR or ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let from = Install::open(a, families).unwrap();
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let to = Install::open(b, families).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pine = "data/base/graphics/roman/environment/props/vegetation/trees/tree_maritime_pine_01/tree_maritime_pine_01_k.cfg";
        let rock =
            "data/base/graphics/roman/environment/props/rocks/roman_rock_06/roman_rock_06_a.cfg";
        // 1800's plant layout, two colors, and its plain static one.
        let (plant, rock_layout) = (
            "P4h_N4b_G4b_B4b_T2h_C4b_C4b".to_owned(),
            "P4h_N4b_G4b_B4b_T2h".to_owned(),
        );
        for (source, format) in [(pine, &plant), (rock, &rock_layout)] {
            let original = Model::load(&from, source).unwrap();
            let ported = port(module_of(&from), module_of(&to), &from, &to, source).unwrap();
            assert!(
                ported.files.iter().any(|f| f.0.ends_with("_0.dds")),
                "{source}: no textures"
            );
            for (path, bytes) in &ported.files {
                let file = dir.path().join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, bytes).unwrap();
            }
            to.mount_mods(&[dir.path()], &["data"]).unwrap();
            let prp = target(&::anno117::MODULE, &::anno1800::MODULE, source);
            let model = Model::load(&to, &prp).unwrap();
            assert!(model.problems.is_empty(), "{:?}", model.problems);
            let part = &model.parts[0];
            let mesh = part.mesh.as_ref().unwrap();
            assert_eq!(&mesh.format.to_string(), format);
            assert_eq!(part.transform, Transform::IDENTITY);
            let diffuse = part.materials[0].diffuse.as_deref();
            assert!(diffuse.is_some_and(|t| to.exists(t)), "{diffuse:?}");
            // Same height as the 117 model draws.
            let height = |m: &Model| m.mesh_extent[1];
            let want = (original.parts[0].transform.scale[1])
                * (original.parts[0].mesh.as_ref().unwrap().bounds.1[1]
                    - original.parts[0].mesh.as_ref().unwrap().bounds.0[1])
                / 2.;
            assert!(
                (height(&model) - want).abs() < 0.02 * want,
                "{source}: {} against {want}",
                height(&model)
            );
            let pieces = super::super::models::load(&to, &plain(), &prp, 1.).unwrap();
            assert!(!pieces.is_empty());
        }
        let text = String::from_utf8(
            to.read(target(&::anno117::MODULE, &::anno1800::MODULE, pine))
                .unwrap(),
        )
        .unwrap();
        // The pine's own tints, not the template's green ones.
        assert!(text.contains("<Count>6</>"), "{text}");
        assert_eq!(text.matches("<Material>").count(), 1);
        assert_eq!(tag(&text, "DOUBLE_SIDED"), Some("1"));
    }

    // Needs both games. 117 models built from several meshes come over as
    // pieces. A Manx house is its own mesh plus torches and lamps (without
    // their fire); a wall end is nine stone blocks with no mesh of its own.
    // Placed at their offsets the pieces fill the same box as the 117 model,
    // and each loads from the mod over the 1800 install.
    #[test]
    fn grouped_117_models_port_as_pieces() {
        let (Some(a), Some(b)) = (
            anno_formats::vfs::install_dir("ANNO117_DIR"),
            anno_formats::vfs::install_dir("ANNO1800_DIR"),
        ) else {
            eprintln!("skipping: ANNO1800_DIR or ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let from = Install::open(a, families).unwrap();
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let to = Install::open(b, families).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let kit = "data/base/graphics/library/modular_level_kits";
        let house = format!("{kit}/manx_island/house_celtic_mid_long_wooden_01.cfg");
        let wall = format!("{kit}/generic/stone_block_wall/stone_block_wall_end_01.cfg");
        fn grow(b: &mut ([f32; 3], [f32; 3]), w: [f32; 3]) {
            b.0 = std::array::from_fn(|k| b.0[k].min(w[k]));
            b.1 = std::array::from_fn(|k| b.1[k].max(w[k]));
        }
        // World bounds of every mesh of a 117 model, sub-models included.
        fn bounds(i: &Install, cfg: &str, at: Transform, b: &mut ([f32; 3], [f32; 3])) {
            let m = Model::load(i, cfg).unwrap();
            for p in &m.parts {
                let Some(mesh) = &p.mesh else { continue };
                for v in &mesh.positions {
                    grow(b, at.then(&p.transform).apply(*v));
                }
            }
            for c in &m.props {
                if !c.cfg.contains("/effects/") && !c.cfg.contains("/decals/") {
                    bounds(i, &c.cfg, at.then(&c.transform), b);
                }
            }
        }
        let empty = || ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
        for (source, at_least) in [(&house, 2), (&wall, 9)] {
            let ported = port(module_of(&from), module_of(&to), &from, &to, source).unwrap();
            assert!(
                ported.pieces.len() >= at_least,
                "{source}: {:?} left out {:?}",
                ported.pieces,
                ported.left_out
            );
            for (path, bytes) in &ported.files {
                let file = dir.path().join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, bytes).unwrap();
            }
            to.mount_mods(&[dir.path()], &["data"]).unwrap();
            let mut got = empty();
            for (prp, offset) in &ported.pieces {
                assert!(prp.ends_with(".prp"), "{prp}");
                let model = Model::load(&to, prp).unwrap();
                assert!(model.problems.is_empty(), "{prp}: {:?}", model.problems);
                assert_eq!(model.parts[0].transform, Transform::IDENTITY);
                for v in &model.parts[0].mesh.as_ref().unwrap().positions {
                    grow(&mut got, offset.apply(*v));
                }
            }
            let mut want = empty();
            bounds(&from, source, Transform::IDENTITY, &mut want);
            for k in 0..3 {
                // Half floats: a few millimetres at house size.
                assert!(
                    (got.0[k] - want.0[k]).abs() < 0.05 && (got.1[k] - want.1[k]).abs() < 0.05,
                    "{source}: {got:?} against {want:?}"
                );
            }
        }
    }

    // Needs 117. A ground material's xml and textures move under the ported
    // folder, and the xml names the moved textures.
    #[test]
    fn a_ground_material_ports_with_its_textures() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let from = Install::open(dir, families).unwrap();
        let file = "data/base/graphics/library/environment/materials/beach_01/beach_01.xml";
        let (path, files) = port_material(module_of(&from), &anno117::MODULE, &from, file).unwrap();
        assert_eq!(
            path,
            "data/editor/anno117/base/graphics/library/environment/materials/beach_01/beach_01.xml"
        );
        let xml = String::from_utf8(files[0].1.clone()).unwrap();
        let diffuse = tag(&xml, "DiffuseFileName").unwrap();
        assert!(diffuse.starts_with("data/editor/anno117/"), "{xml}");
        let shipped = texture_path(diffuse);
        assert!(files.iter().any(|f| f.0 == shipped), "{shipped}");
        assert!(tag(&xml, "NormalFileName").is_some());
    }

    /// An install whose only archive ships `files`. It has no executable, so
    /// no game to detect: the tests import into it as 117.
    fn shipped(game_files: &[(&str, &[u8])]) -> (tempfile::TempDir, Install) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        let mut p = anno_formats::rda::Packer::new(Default::default());
        for (path, data) in game_files {
            p.add(path, data).unwrap();
        }
        std::fs::write(dir.path().join("maindata/data0.rda"), p.encode().unwrap()).unwrap();
        let install = Install::open(dir.path(), &[]).unwrap();
        (dir, install)
    }

    const CFG: &str = "<Config><Models><Config><FileName>data/art/tree/tree.fbx</FileName>\
        <Materials><Config><cModelDiffTex>data/art/tree/maps/tree_diff.psd</cModelDiffTex>\
        <cModelNormalTex>data/graphics/shared/flat_norm.psd</cModelNormalTex>\
        </Config></Materials></Config></Models></Config>";

    #[test]
    fn a_loose_model_folder_moves_under_the_mods_namespace() {
        let (_g, install) = shipped(&[("data/graphics/shared/flat_norm_0.dds", b"n")]);
        let dir = tempfile::tempdir().unwrap();
        let put = |name: &str, bytes: &[u8]| std::fs::write(dir.path().join(name), bytes).unwrap();
        put("Tree.cfg", CFG.as_bytes());
        put("tree.rdm", b"mesh");
        put("tree_lod1.rdm", b"far");
        put("TREE_DIFF_0.dds", b"d0");
        put("tree_diff_1.dds", b"d1");
        let model = import(
            crate::games::module(Game::Anno117),
            &dir.path().join("Tree.cfg"),
            "isles",
            &install,
        )
        .unwrap();
        let t = "data/isles/models/tree";
        assert_eq!(model.cfg, format!("{t}/tree.cfg"));
        let mut paths: Vec<&str> = model.files.iter().map(|f| f.0.as_str()).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                format!("{t}/tree.cfg"),
                format!("{t}/tree.rdm"),
                format!("{t}/tree_diff_0.dds"),
                format!("{t}/tree_diff_1.dds"),
                format!("{t}/tree_lod1.rdm"),
            ]
        );
        assert_eq!(model.shipped, 1, "the shipped normal map is referenced");
        let cfg = String::from_utf8(model.files[0].1.clone()).unwrap();
        assert!(cfg.contains(&format!(">{t}/tree.fbx<")));
        assert!(cfg.contains(&format!(">{t}/tree_diff.psd<")));
        assert!(cfg.contains(">data/graphics/shared/flat_norm.psd<"));

        // The project then lists it with all its files, and removing it
        // takes them all.
        let files: BTreeMap<String, Vec<u8>> = model.files.into_iter().collect();
        let models = project_models(&anno117::MODULE, &files, |p| files.get(p).map(Vec::len));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].kind, ModelKind::Custom);
        assert_eq!(removable(&models, &models[0]).len(), 5);
    }

    // A project's models are its files whose kind names others: 1800's
    // props too, which 117 doesn't read.
    #[test]
    fn project_models_are_the_files_whose_kind_has_deps() {
        let files: BTreeMap<String, Vec<u8>> = [
            ("data/x/a.cfg", "<M><FileName>data/x/a.rdm</FileName></M>"),
            ("data/x/a.rdm", "mesh"),
            (
                "data/x/b.prp",
                "<P><MeshFileName>data/x/b.rdm</MeshFileName></P>",
            ),
            ("data/x/notes.txt", "<n>data/x/a.cfg</n>"),
        ]
        .into_iter()
        .map(|(p, b)| (p.to_owned(), b.as_bytes().to_vec()))
        .collect();
        let size = |p: &str| files.get(p).map(Vec::len);
        let cfgs = |m: &dyn Module| -> Vec<(String, Vec<String>)> {
            (project_models(m, &files, size).into_iter())
                .map(|m| (m.cfg, m.files))
                .collect()
        };
        let a = (
            "data/x/a.cfg".to_owned(),
            vec!["data/x/a.cfg".to_owned(), "data/x/a.rdm".to_owned()],
        );
        assert_eq!(cfgs(&anno117::MODULE), std::slice::from_ref(&a));
        let b = ("data/x/b.prp".to_owned(), vec!["data/x/b.prp".to_owned()]);
        assert_eq!(cfgs(&anno1800::MODULE), [a, b]);
    }

    #[test]
    fn a_model_inside_a_mod_keeps_its_paths() {
        let (_g, install) = shipped(&[("data/graphics/shared/flat_norm_0.dds", b"n")]);
        let root = tempfile::tempdir().unwrap();
        let put = |rel: &str, bytes: &[u8]| {
            let p = root.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        };
        put("Data/Art/Tree/tree.cfg", CFG.as_bytes());
        put("Data/Art/Tree/tree.rdm", b"mesh");
        put("Data/Art/Tree/Maps/tree_diff_0.dds", b"d0");
        let model = import(
            crate::games::module(Game::Anno117),
            &root.path().join("Data/Art/Tree/tree.cfg"),
            "isles",
            &install,
        )
        .unwrap();
        assert_eq!(model.cfg, "data/art/tree/tree.cfg");
        assert!(
            model
                .files
                .iter()
                .any(|f| f.0 == "data/art/tree/maps/tree_diff_0.dds")
        );
        assert_eq!(model.files[0].1, CFG.as_bytes(), "references unchanged");
    }

    #[test]
    fn missing_files_and_shipped_names_are_refused() {
        let (_g, install) = shipped(&[("data/isles/models/tree/tree.cfg", b"shipped")]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tree.cfg"), CFG).unwrap();
        let e = import(
            crate::games::module(Game::Anno117),
            &dir.path().join("tree.cfg"),
            "isles",
            &install,
        )
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("neither beside it nor in the game"),
            "{e:#}"
        );
        assert!(
            import(
                crate::games::module(Game::Anno117),
                &dir.path().join("tree.txt"),
                "isles",
                &install
            )
            .is_err()
        );
    }

    // Needs 117: a shipped rock mesh and its textures, copied out and
    // brought back in as a bare mesh, load through the model reader.
    #[test]
    fn a_bare_mesh_becomes_a_loadable_prop() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        // A shipped rock: the static prop the 117 writer copies too.
        let rock =
            "data/base/graphics/roman/environment/props/rocks/roman_rock_07/roman_rock_07_e.cfg";
        let shipped = anno_formats::model::Model::load(&install, rock).unwrap();
        let part = shipped.parts.iter().find(|p| p.mesh.is_some()).unwrap();
        // The cfg's layout name is what the 117 writer writes from the rdm.
        assert_eq!(
            part.mesh.as_ref().unwrap().format.to_string(),
            part.materials[0].vertex_format
        );
        let disk = tempfile::tempdir().unwrap();
        let copy = |from: &str| {
            let name = from.rsplit('/').next().unwrap();
            std::fs::write(disk.path().join(name), install.read(from).unwrap()).unwrap();
            disk.path().join(name)
        };
        let mesh = copy(&part.rdm);
        for t in [&part.materials[0].diffuse, &part.materials[0].normal] {
            for f in files::siblings(t.as_deref().unwrap()) {
                if install.exists(&f) {
                    copy(&f);
                }
            }
        }
        let model = import(module_of(&install), &mesh, "isles", &install).unwrap();
        let folder = tempfile::tempdir().unwrap();
        for (path, bytes) in &model.files {
            let p = folder.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        install.mount_mods(&[folder.path()], &["data"]).unwrap();
        let loaded = anno_formats::model::Model::load(&install, &model.cfg).unwrap();
        assert!(loaded.parts.iter().any(|p| p.mesh.is_some()), "mesh loads");
    }

    /// FNV-1a, 64 bit: stable across toolchains, unlike std's hasher.
    fn fnv(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h = (*h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }

    /// One digest over what porting `source` produced: every staged file
    /// (path and bytes, in path order), the pieces and effects in order,
    /// what was left out (sorted: its order is display only), or the error.
    fn digest(result: Result<Ported>) -> u64 {
        let mut h = 0xcbf29ce484222325;
        match result {
            Err(e) => fnv(&mut h, format!("error {e:#}").as_bytes()),
            Ok(mut p) => {
                p.files.sort_by(|a, b| a.0.cmp(&b.0));
                for (path, bytes) in &p.files {
                    fnv(&mut h, path.as_bytes());
                    fnv(&mut h, bytes);
                }
                for (path, at) in p.pieces.iter().chain(&p.effects) {
                    fnv(&mut h, format!("{path} {at:?}").as_bytes());
                }
                p.left_out.sort();
                for l in &p.left_out {
                    fnv(&mut h, l.as_bytes());
                }
            }
        }
        h
    }

    // Needs both games. Golden output of porting a fixed sample of shipped
    // props each way, as digests of every file, piece, effect and note it
    // produces. Taken from the pairwise converters before they went through
    // the neutral model, which kept every byte but one thing: 117 cfgs
    // written for prps with no `cDiffuseColor` now say `1.000000`, not
    // `1`, like every other colour (5 of the 67 sampled; with that one
    // edit undone they matched the old digests). Since 117's reader reads
    // lights, 117 -> 1800 also notes each light left out; with that note
    // dropped the old 117 -> 1800 digest came back. 1800's ground-quad
    // decals and effect cfgs port too now, which shifts the 1800 -> 117
    // sample; leaving those paths out of it gave the old digest back. A
    // game update that
    // changes a sampled file changes its digest too. `PORT_DIGESTS=<file>`
    // writes each source's digest there, to find which one moved.
    #[test]
    fn ported_output_matches_its_golden_digests() {
        let (Some(a), Some(b)) = (
            anno_formats::vfs::install_dir("ANNO1800_DIR"),
            anno_formats::vfs::install_dir("ANNO117_DIR"),
        ) else {
            eprintln!("skipping: ANNO1800_DIR or ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let anno1800 = Install::open(a, families).unwrap();
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let anno117 = Install::open(b, families).unwrap();
        let mut lines = String::new();
        let mut run = |from: &Install, to: &Install, games: [&dyn Module; 2], every: usize| {
            let mut sources: Vec<String> = (0..from.file_count() as u32)
                .map(|f| from.file_path(anno_formats::vfs::FileId(f)))
                .filter(|p| p.contains("/props/") && portable(games[0], games[1], p))
                .collect();
            sources.sort_unstable();
            let mut total = 0xcbf29ce484222325;
            let mut n = 0;
            for source in sources.iter().step_by(every) {
                let d = digest(port(games[0], games[1], from, to, source));
                lines.push_str(&format!("{source} {d:016x}\n"));
                fnv(&mut total, &d.to_le_bytes());
                n += 1;
            }
            (n, total)
        };
        let to_117 = run(
            &anno1800,
            &anno117,
            [&::anno1800::MODULE, &::anno117::MODULE],
            29,
        );
        let to_1800 = run(
            &anno117,
            &anno1800,
            [&::anno117::MODULE, &::anno1800::MODULE],
            61,
        );
        if let Some(file) = std::env::var_os("PORT_DIGESTS") {
            std::fs::write(file, &lines).unwrap();
        }
        eprintln!("1800 -> 117: {to_117:x?}, 117 -> 1800: {to_1800:x?}");
        assert_eq!(to_117, GOLDEN_TO_117);
        assert_eq!(to_1800, GOLDEN_TO_1800);
    }

    // Needs all three games. Static 2070 models come into 117 and 1800 as
    // props: every one of a sample of shipped 2070 cfgs whose parts' meshes
    // are all static and readable ports, and each ported model loads from
    // the mod over the target install with its mesh and diffuse texture.
    // Meshes stay in 2070's rdm container, which the editor reads; whether
    // the games draw them is not checked. Rigged models (most 2070
    // buildings carry bone weights), effects and big-endian meshes do not
    // port.
    #[test]
    fn anno_2070_models_port_into_117_and_1800() {
        let dir = |game| {
            let profile = crate::games::profile(game);
            anno_formats::vfs::install_dir(profile.env_var)
                .map(|d| Install::open(d, profile.archive_families))
        };
        let (Some(Ok(anno2070)), Some(Ok(anno117)), Some(Ok(anno1800))) =
            (dir(Game::Anno2070), dir(Game::Anno117), dir(Game::Anno1800))
        else {
            eprintln!("skipping: ANNO2070_DIR, ANNO117_DIR or ANNO1800_DIR unset");
            return;
        };
        let static_model = |cfg: &str| {
            let parts =
                Model::parse(&anno2070.read(cfg).unwrap()).map_or_else(|_| Vec::new(), |m| m.parts);
            !parts.is_empty()
                && parts.iter().all(|p| {
                    (anno2070.read(&p.rdm).ok())
                        .filter(|b| b.get(3) == Some(&1))
                        .and_then(|b| Rdm::parse(&b).ok())
                        .is_some_and(|m| !m.skinned)
                })
        };
        let mut sources: Vec<String> = (0..anno2070.file_count() as u32)
            .map(|f| anno2070.file_path(anno_formats::vfs::FileId(f)))
            .filter(|p| p.ends_with(".cfg") && p.contains("/graphics/") && !p.contains("/effects/"))
            .collect();
        sources.sort_unstable();
        let sample: Vec<&str> = (sources.iter().step_by(sources.len() / 120))
            .map(String::as_str)
            .filter(|cfg| static_model(cfg))
            .take(24)
            .collect();
        assert!(sample.len() >= 12, "{} static models", sample.len());
        for (to, game) in [
            (&anno117, &::anno117::MODULE as &dyn Module),
            (&anno1800, &::anno1800::MODULE),
        ] {
            let mods = tempfile::tempdir().unwrap();
            let (mut ported, mut failed) = (Vec::new(), Vec::new());
            for source in &sample {
                match port(&::anno2070::MODULE, game, &anno2070, to, source) {
                    Ok(p) => {
                        for (path, bytes) in &p.files {
                            let file = mods.path().join(path);
                            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                            std::fs::write(file, bytes).unwrap();
                        }
                        ported.push(p);
                    }
                    Err(e) => failed.push(format!("{source}: {e:#}")),
                }
            }
            assert!(failed.is_empty(), "{:?}: {failed:#?}", game.game());
            to.mount_mods(&[mods.path()], &["data"]).unwrap();
            for p in &ported {
                for (model, _) in &p.pieces {
                    let loaded = Model::load(to, model).unwrap();
                    let part = loaded.parts.iter().find(|p| p.mesh.is_some());
                    let part = part.unwrap_or_else(|| panic!("{model}: no mesh loads"));
                    if let Some(diffuse) = part.materials[0].diffuse.as_deref() {
                        assert!(to.exists(diffuse), "{model}: {diffuse}");
                    }
                }
            }
        }
    }

    /// Every file of `files` into folder `dir`, as a mod lays them out.
    fn stage(dir: &Path, files: &[(String, Vec<u8>)]) {
        for (path, bytes) in files {
            let file = dir.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, bytes).unwrap();
        }
    }

    // Needs Anno 117. The asset preview draws a shipped model's ground
    // decals, and the same model read and written again into a mod as an
    // import writes it, alike: each decal's grid, UVs, tint, textures,
    // opacity and fade, and where it lies. The preview draws no particles
    // or lights; the 117 model tests check those entries.
    #[test]
    fn written_117_decals_preview_as_shipped() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let to = Install::open(dir, families).unwrap();
        let module = crate::games::module(Game::Anno117);
        let shipped = Shipped {
            from: module,
            root: "data",
            install: &to,
        };
        let mods = tempfile::tempdir().unwrap();
        let cases = [
            "data/base/graphics/library/environment/props/decals/cracks/crack_decal_01.cfg",
            "data/base/graphics/library/environment/props/decals/beach/beach_decal_02.cfg",
            "data/base/graphics/celtic/buildings/ornaments/hall_of_fame/fireplaces/fireplace_01.cfg",
            "data/dlc03/graphics/egypt/buildings/ornaments/hall_of_fame/statues/ancient_portal_01.cfg",
        ];
        let written: Vec<Ported> = (cases.iter())
            .map(|cfg| {
                let model = module.read_model(&to, cfg).unwrap().unwrap();
                let out = module.write_model(&model, &shipped, &to).unwrap().unwrap();
                stage(mods.path(), &out.files);
                out
            })
            .collect();
        to.mount_mods(&[mods.path()], &["data"]).unwrap();
        // A decal as the preview draws it: no mesh file, a draped grid.
        let decals = |cfg: &str| -> Vec<String> {
            let pieces = super::super::models::load(&to, &plain(), cfg, 1.).unwrap();
            (pieces.iter().filter(|p| p.rdm.is_empty()))
                .map(|p| {
                    let d = p.mesh.data();
                    format!(
                        "{:?} {:?} {:?} {:?} {:?} {:?} maps {} {} {} {}",
                        p.local,
                        d.positions,
                        d.uvs,
                        d.colors,
                        d.indices,
                        d.material,
                        d.texture.is_some(),
                        d.normal_map.is_some(),
                        d.surface_map.is_some(),
                        d.alpha_map.is_some(),
                    )
                })
                .collect()
        };
        for (cfg, out) in cases.iter().zip(&written) {
            let original = decals(cfg);
            assert!(!original.is_empty(), "{cfg}: no decal drawn");
            let again: Vec<String> = (out.pieces.iter())
                .filter(|(path, _)| path.contains("_decal"))
                .flat_map(|(path, at)| {
                    assert_eq!(*at, Transform::IDENTITY, "{path}");
                    decals(path)
                })
                .collect();
            assert_eq!(again, original, "{cfg}");
        }
    }

    // Needs both games. 1800's ground-quad decal prps become 117 box
    // decals that load from the mod with their texture; its mesh decals
    // (ice floes) are turned away. An effect cfg an 1800 island places
    // becomes 117's own effect, placed as it ships.
    #[test]
    fn decals_and_effects_of_1800_come_into_117() {
        let (Some(a), Some(b)) = (
            anno_formats::vfs::install_dir("ANNO1800_DIR"),
            anno_formats::vfs::install_dir("ANNO117_DIR"),
        ) else {
            eprintln!("skipping: ANNO1800_DIR or ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno1800).archive_families;
        let from = Install::open(a, families).unwrap();
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let to = Install::open(b, families).unwrap();
        let games: [&dyn Module; 2] = [&::anno1800::MODULE, &::anno117::MODULE];
        let mods = tempfile::tempdir().unwrap();
        let decal = port(
            games[0],
            games[1],
            &from,
            &to,
            "data/cdlc05/graphics/props/decals/dirt_01.prp",
        )
        .unwrap();
        assert_eq!(decal.pieces.len(), 1, "{:?}", decal.left_out);
        assert!(decal.effects.is_empty() && decal.left_out.is_empty());
        stage(mods.path(), &decal.files);
        to.mount_mods(&[mods.path()], &["data"]).unwrap();
        let (cfg, at) = &decal.pieces[0];
        assert_eq!(*at, Transform::IDENTITY);
        let loaded = Model::load(&to, cfg).unwrap();
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert!(loaded.parts.is_empty() && loaded.decals.len() == 1);
        let d = &loaded.decals[0];
        assert_eq!(
            (d.kind.as_str(), d.extents[0], d.extents[2]),
            ("Terrain", 0.5, 0.5)
        );
        assert_eq!((d.opacity, d.uv_rect), (1., [0., 0., 1., 1.]));
        let diffuse = d.material.as_ref().and_then(|m| m.diffuse.clone());
        assert!(
            diffuse
                .as_deref()
                .is_some_and(|t| is_ported(t) && to.exists(t)),
            "{diffuse:?}"
        );
        let pieces = super::super::models::load(&to, &plain(), cfg, 1.).unwrap();
        assert_eq!(pieces.len(), 1, "the preview draws the decal");

        let floe = "data/dlc03/graphics/landscape/terrain/decals/ice_floes/icefloe_arctic_coast_fine_01.prp";
        let e = port(games[0], games[1], &from, &to, floe).unwrap_err();
        assert!(format!("{e:#}").contains("mesh decal"), "{e:#}");

        let torch = "data/graphics/effects/environment/fps_torch.cfg";
        let fire = port(games[0], games[1], &from, &to, torch).unwrap();
        assert!(fire.pieces.is_empty() && fire.files.is_empty());
        assert_eq!(
            fire.effects,
            [(
                "data/base/graphics/library/effects/fire/fire_torch_01.cfg".to_owned(),
                Transform::IDENTITY
            )]
        );
    }

    // Anno 117 build 26607411. A game update changes these; PORT_DIGESTS=<file>
    // writes the per-prop digests to diff against the run before.
    const GOLDEN_TO_117: (usize, u64) = (0x43, 0xff9faa8c13f054a5);
    const GOLDEN_TO_1800: (usize, u64) = (0x6c, 0xe14eacb9fbb9a349);
}
