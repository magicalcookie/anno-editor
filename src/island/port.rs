// Brings models from outside into the mod: Anno 1800 props into a 117 mod,
// and model files from disk (a cfg with its meshes and textures, or a bare
// mesh). Also lists the models a project carries and the files each needs.
//
// 1800 props: 117 loads 1800's meshes as they are (it ships 1800's palm,
// `palm_tree_01_04_lod0.rdm`, byte for byte, and every 1800 terrain prop uses
// its 92-byte mesh layout), so the meshes, their distance levels and
// textures are copied unchanged under `FOLDER`, and a 117 cfg is written
// from a shipped 117 prop of the same kind.
use anno_formats::{
    game::Game,
    model::{mesh_path, texture_path},
    rda::normalize_path,
    rdm::Rdm,
    vfs::Install,
};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where ported files live in the mod: the 1800 path without its `data/`.
pub const FOLDER: &str = "data/editor/anno1800/";

/// 117 props whose cfg the ported ones copy, by the 1800 mesh's kind.
const PLANT_TEMPLATE: &str = "data/base/graphics/library/environment/props/vegetation/trees/tree_palm_01/tree_palm_06_03.cfg";
const STATIC_TEMPLATE: &str =
    "data/base/graphics/roman/environment/props/rocks/roman_rock_07/roman_rock_07_e.cfg";

/// Whether `source`, a model of game `from`, can be brought into `to`:
/// only `.prp` props, and only from the game `to` ports props from.
pub fn portable(from: Game, to: Game, source: &str) -> bool {
    to.profile().ports_props_from == Some(from) && source.ends_with(".prp")
}

/// The cfg a ported `source` is placed as.
pub fn target(source: &str) -> String {
    let stem = source.strip_suffix(".prp").unwrap_or(source);
    moved(&format!("{stem}.cfg"))
}

fn moved(path: &str) -> String {
    let rel = path.strip_prefix("data/").unwrap_or(path);
    format!("{FOLDER}{}", rel.to_ascii_lowercase())
}

/// One ported prop: every file to stage, its cfg (`target`) last.
pub struct Ported {
    pub files: Vec<(String, Vec<u8>)>,
}

/// One `<Material>` of a prp.
#[derive(Clone)]
struct PrpMaterial {
    diffuse: Option<String>,
    normal: Option<String>,
    color: [String; 3],
    double_sided: String,
}

/// First `<tag>value</…>` in `xml`.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    crate::terrain::tag(xml, name)
}

/// Replaces the content of every `<name>…</name>` in `xml`.
fn set(xml: &str, name: &str, value: &str) -> String {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find(&close) else { break };
        out.push_str(&rest[..i + open.len()]);
        out.push_str(value);
        rest = &body[j..];
    }
    out.push_str(rest);
    out
}

/// Sets `<name><x>…</x><y>…</y><z>…</z></name>`, when present.
fn set_point(xml: &str, name: &str, p: [f32; 3]) -> String {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let (Some(a), Some(b)) = (xml.find(&open), xml.find(&close)) else {
        return xml.to_owned();
    };
    let mut body = xml[a..b].to_owned();
    for (axis, v) in ["x", "y", "z"].iter().zip(p) {
        body = set(&body, axis, &v.to_string());
    }
    format!("{}{body}{}", &xml[..a], &xml[b..])
}

/// The `<stem>_N.dds` files the game reads for texture `source`.
fn texture_files(from: &Install, source: &str) -> Vec<(String, Vec<u8>)> {
    let source = source.replace('\\', "/");
    let stem = source.rsplit_once('.').map_or(source.as_str(), |(s, _)| s);
    (0..4)
        .map_while(|level| {
            let path = format!("{stem}_{level}.dds");
            let bytes = from.read(&path).ok()?;
            Some((moved(&path), bytes))
        })
        .collect()
}

/// A 117 cfg for `mesh` (its virtual path in the mod), modelled on a
/// shipped prop: the palm for plants (vertex colors carry wind and AO), a
/// rock otherwise. One material block per entry of `materials`, whose
/// textures are virtual paths too.
fn wrap(
    to: &Install,
    mesh: &str,
    rdm: &Rdm,
    format: &str,
    materials: &[PrpMaterial],
) -> Result<String> {
    let plant = format.contains("C4b");
    let template = if plant {
        PLANT_TEMPLATE
    } else {
        STATIC_TEMPLATE
    };
    let xml = String::from_utf8_lossy(&to.read(template)?).into_owned();
    let (lo, hi) = rdm.bounds;
    let center: [f32; 3] = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
    let extent: [f32; 3] = std::array::from_fn(|i| (hi[i] - lo[i]) / 2.);
    let radius = extent.iter().map(|v| v * v).sum::<f32>().sqrt();
    let mut cfg = xml.clone();
    for (axis, i) in [("x", 0), ("y", 1), ("z", 2)] {
        for prefix in ["Center", "MeshCenter"] {
            cfg = set(&cfg, &format!("{prefix}.{axis}"), &center[i].to_string());
        }
        for prefix in ["Extent", "MeshExtent"] {
            cfg = set(&cfg, &format!("{prefix}.{axis}"), &extent[i].to_string());
        }
    }
    cfg = set(&cfg, "Radius", &radius.to_string());
    cfg = set(&cfg, "MeshRadius", &radius.to_string());

    // One material block per prp material, in the mesh's material order.
    let open = "<Materials>";
    let (a, b) = (
        cfg.find(open).context("template has no Materials")? + open.len(),
        cfg.find("</Materials>")
            .context("template has no Materials")?,
    );
    let block = cfg[a..b].to_owned();
    let blocks: String = (materials.iter().enumerate())
        .map(|(i, m)| {
            let mut out = set(&block, "Name", &format!("material_{i}"));
            out = set(&out, "VertexFormat", format);
            if let Some(t) = &m.diffuse {
                out = set(&out, "cModelDiffTex", t);
            }
            // Without one the template's stays named but off.
            match &m.normal {
                Some(t) => out = set(&out, "cModelNormalTex", t),
                None => out = set(&out, "NORMAL_ENABLED", "0"),
            }
            for (c, v) in ["r", "g", "b"].iter().zip(&m.color) {
                out = set(&out, &format!("cDiffuseColor.{c}"), v);
            }
            set(&out, "DOUBLE_SIDED", &m.double_sided)
        })
        .collect();
    cfg.replace_range(a..b, &blocks);
    // Each distance level draws every material.
    let indices: String = (0..materials.len())
        .map(|i| format!("<i>{i}</i>\n"))
        .collect();
    let lod = format!("<i><Indices>{indices}</Indices>\n</i>\n");
    if let (Some(a), Some(b)) = (
        cfg.find("<MaterialLODInfos>"),
        cfg.find("</MaterialLODInfos>"),
    ) {
        cfg.replace_range(a + "<MaterialLODInfos>".len()..b, &lod.repeat(5));
    }
    // The model's own name and mesh come after its materials.
    let after = cfg.find("</MaterialLODInfos>").unwrap_or(0);
    let name = mesh
        .rsplit('/')
        .next()
        .unwrap_or(mesh)
        .trim_end_matches(".rdm")
        .trim_end_matches("_lod0");
    let tail = set(&cfg[after..], "Name", name);
    let tail = set(&tail, "FileName", mesh);
    let tail = set_point(&set_point(&tail, "AABBMin", lo), "AABBMax", hi);
    cfg.replace_range(after.., &tail);

    Ok(cfg)
}

/// Ports 1800 prop `source` (a `.prp`) for `to`, a 117 install that has
/// the template cfgs.
pub fn port(from: &Install, to: &Install, source: &str) -> Result<Ported> {
    ensure!(
        portable(from.game(), to.game(), source),
        "{source}: only Anno 1800 props can be brought into Anno 117"
    );
    let prp = String::from_utf8_lossy(&from.read(source)?).into_owned();
    let mesh = tag(&prp, "MeshFileName")
        .context("prp has no MeshFileName")?
        .replace('\\', "/");
    let format = tag(&prp, "VertexFormat").unwrap_or_default();
    let materials: Vec<PrpMaterial> = (prp.split("<Material>").skip(1))
        .map(|m| PrpMaterial {
            diffuse: tag(m, "cPropDiffuseTex")
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            normal: tag(m, "cPropNormalTex")
                .filter(|t| !t.is_empty() && tag(m, "NORMAL_MAP_ENABLED") != Some("0"))
                .map(str::to_owned),
            color: ["r", "g", "b"].map(|c| {
                tag(m, &format!("cDiffuseColor.{c}"))
                    .unwrap_or("1")
                    .to_owned()
            }),
            double_sided: tag(m, "DOUBLE_SIDED").unwrap_or("0").to_owned(),
        })
        .collect();
    ensure!(!materials.is_empty(), "prp has no materials");

    let mut files = Vec::new();
    let bytes = from.read(&mesh).with_context(|| format!("mesh {mesh}"))?;
    let rdm = Rdm::parse(&bytes).with_context(|| format!("mesh {mesh}"))?;
    files.push((moved(&mesh), bytes));
    // Distance levels sit next to the mesh; the game finds them by name.
    let stem = mesh.strip_suffix(".rdm").unwrap_or(&mesh);
    let stem = stem.strip_suffix("_lod0").unwrap_or(stem);
    for level in 1..=4 {
        let path = format!("{stem}_lod{level}.rdm");
        if let Ok(bytes) = from.read(&path) {
            files.push((moved(&path), bytes));
        }
    }
    for m in &materials {
        for t in [&m.diffuse, &m.normal].into_iter().flatten() {
            files.extend(texture_files(from, t));
        }
    }

    let moved_materials: Vec<PrpMaterial> = (materials.iter())
        .map(|m| PrpMaterial {
            diffuse: m.diffuse.as_deref().map(|t| moved(&t.replace('\\', "/"))),
            normal: m.normal.as_deref().map(|t| moved(&t.replace('\\', "/"))),
            ..m.clone()
        })
        .collect();
    let cfg = wrap(to, &moved(&mesh), &rdm, format, &moved_materials)?;
    files.push((target(source), cfg.into_bytes()));
    Ok(Ported { files })
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

/// The models among the project's files. A model's files are the ones its
/// cfg references (with their distance levels and mips), when staged.
pub fn project_models(files: &BTreeMap<String, Vec<u8>>) -> Vec<ProjectModel> {
    files
        .iter()
        .filter(|(p, _)| p.ends_with(".cfg") || p.ends_with(".prp"))
        .map(|(cfg, bytes)| {
            let text = String::from_utf8_lossy(bytes);
            let mut own = vec![cfg.clone()];
            for r in references(&text) {
                for wanted in expected(&r) {
                    own.extend(
                        family(&wanted)
                            .into_iter()
                            .filter(|f| files.contains_key(f)),
                    );
                }
            }
            own.sort();
            own.dedup();
            ProjectModel {
                kind: if cfg.starts_with(FOLDER) {
                    ModelKind::Ported
                } else {
                    ModelKind::Custom
                },
                size: own.iter().filter_map(|f| files.get(f)).map(Vec::len).sum(),
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

/// Extensions a cfg names files with: meshes as exported (fbx, max) or
/// shipped, textures as authored or shipped, and sub-models and effects.
const REFERENCED: [&str; 14] = [
    "rdm", "fbx", "max", "dds", "psd", "png", "tga", "cfg", "prp", "rdp", "anim", "cf7", "ifo",
    "fc",
];

/// File references in a cfg or prp: every tag body that is a path with one
/// of the `REFERENCED` extensions, as written.
fn references(xml: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for body in xml.split('>').filter_map(|s| s.split('<').next()) {
        let body = body.trim();
        let Some((_, ext)) = body.rsplit_once('.') else {
            continue;
        };
        let ok = REFERENCED.contains(&ext.to_ascii_lowercase().as_str())
            && !body.contains(char::is_whitespace)
            && body.len() > ext.len() + 1;
        if ok && !out.iter().any(|o| o == body) {
            out.push(body.to_owned());
        }
    }
    out
}

/// The files the game reads for reference `r`: the shipped mesh for an
/// exporter path, the top mip for an authored texture.
fn expected(r: &str) -> Vec<String> {
    let ext = r
        .rsplit_once('.')
        .map_or("", |(_, e)| e)
        .to_ascii_lowercase();
    match ext.as_str() {
        "rdm" | "fbx" | "max" => vec![mesh_path(r)],
        "dds" | "psd" | "png" | "tga" => vec![texture_path(r)],
        _ => vec![normalize_path(r).into_owned()],
    }
}

/// `file` and the siblings the game finds by name: distance levels of a
/// mesh, lower mips of a texture.
fn family(file: &str) -> Vec<String> {
    let mut out = vec![file.to_owned()];
    if let Some(stem) = file.strip_suffix(".rdm") {
        let stem = stem.strip_suffix("_lod0").unwrap_or(stem);
        out.extend((1..=4).map(|k| format!("{stem}_lod{k}.rdm")));
    } else if let Some(stem) = file.strip_suffix("_0.dds") {
        out.extend((1..=4).map(|k| format!("{stem}_{k}.dds")));
    }
    out
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
pub fn import(source: &Path, mod_id: &str, install: &Install) -> Result<Imported> {
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "cfg" | "prp" => import_cfg(source, &ext, mod_id, install),
        "rdm" => import_mesh(source, mod_id, install),
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
    for r in references(&text) {
        let wanted = expected(&r);
        let in_root = root.and_then(|root| {
            wanted
                .iter()
                .find_map(|w| Some((find_ci(root, w)?, w.clone())))
        });
        let beside = || {
            wanted.iter().find_map(|w| {
                let name = w.rsplit('/').next()?;
                Some((find_ci(dir, name)?, w.clone()))
            })
        };
        let (found, wanted_path, keep) = match in_root {
            Some((path, w)) => (path, w, true),
            None => match beside() {
                Some((path, w)) => (path, w, false),
                None if wanted.iter().any(|w| install.file(w).is_some()) => {
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
            (wanted_path.clone(), r.clone())
        } else {
            let name = r.rsplit(['/', '\\']).next().unwrap_or(&r);
            let new_ref = format!("{target}/{name}");
            (expected(&new_ref).remove(0), new_ref)
        };
        if install.file(&dest).is_some() {
            shipped += 1;
            continue;
        }
        let dest_family = family(&dest);
        for (n, sibling) in family(&wanted_path).iter().enumerate() {
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
fn import_mesh(source: &Path, mod_id: &str, install: &Install) -> Result<Imported> {
    ensure!(
        install.game().profile().mesh_import,
        "Anno 1800 props need a .prp; pick the model’s .prp or .cfg"
    );
    let bytes = std::fs::read(source).with_context(|| format!("Reading {}", source.display()))?;
    let rdm = Rdm::parse(&bytes).with_context(|| source.display().to_string())?;
    ensure!(
        !rdm.skinned,
        "skinned meshes need an animated cfg; pick the model’s .cfg"
    );
    let name = stem_of(source);
    let target = custom_folder(mod_id, name.trim_end_matches("_lod0"));
    let mesh = format!("{target}/{name}.rdm");
    let mut files = vec![(mesh.clone(), bytes)];
    let dir = source.parent().context("the file has a folder")?;
    let stem = name.trim_end_matches("_lod0");
    for k in 1..=4 {
        if let Some(p) = find_ci(dir, &format!("{stem}_lod{k}.rdm")) {
            files.push((format!("{target}/{stem}_lod{k}.rdm"), std::fs::read(p)?));
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
        for (n, sibling) in family(&texture_path(&reference)).iter().enumerate() {
            let on_disk = found.iter().find(|p| {
                let f = p
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_ascii_lowercase();
                f == format!("{base}_{n}.dds") || (n == 0 && f == format!("{base}.dds"))
            });
            if let Some(p) = on_disk {
                files.push((sibling.clone(), std::fs::read(p)?));
            }
        }
        Ok(Some(reference))
    };
    let diffuse = texture(&["diff", "_d.", "albedo", "color"])?;
    let normal = texture(&["norm", "_n."])?;
    let count = (rdm.groups.iter().map(|g| g.material + 1).max()).unwrap_or(1) as usize;
    let material = PrpMaterial {
        diffuse,
        normal,
        color: ["1".into(), "1".into(), "1".into()],
        double_sided: "0".into(),
    };
    // The rdm's own layout name, as cfgs spell it (P4h_N4b_…); see rdm.rs.
    let format = rdm.format.to_string();
    let cfg = wrap(install, &mesh, &rdm, &format, &vec![material; count.max(1)])?;
    let cfg_path = format!("{target}/{}.cfg", name.trim_end_matches("_lod0"));
    files.insert(0, (cfg_path.clone(), cfg.into_bytes()));
    Ok(Imported {
        cfg: cfg_path,
        files,
        shipped: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_every_tag_body() {
        assert_eq!(
            set("<a>1</a><b>2</b><a>3</a>", "a", "x"),
            "<a>x</a><b>2</b><a>x</a>"
        );
        assert_eq!(set("<a>1</a>", "c", "x"), "<a>1</a>");
    }

    #[test]
    fn ported_paths_live_under_the_folder() {
        assert_eq!(
            target("data/graphics/props/x/Tree_A.prp"),
            "data/editor/anno1800/graphics/props/x/tree_a.cfg"
        );
        assert!(portable(Game::Anno1800, Game::Anno117, "a/b.prp"));
        assert!(!portable(Game::Anno1800, Game::Anno117, "a/b.cfg"));
        assert!(!portable(Game::Anno117, Game::Anno1800, "a/b.prp"));
    }

    // Needs both games: a ported jungle tree and rock load through the
    // editor's own model reader from a mod folder over the 117 install,
    // with their mesh, distance levels and textures.
    #[test]
    fn ported_props_load_from_the_mod() {
        let (Some(a), Some(b)) = (
            std::env::var_os("ANNO1800_DIR"),
            std::env::var_os("ANNO117_DIR"),
        ) else {
            eprintln!("skipping: ANNO1800_DIR or ANNO117_DIR unset");
            return;
        };
        let from = Install::open_game(Game::Anno1800, a).unwrap();
        let to = Install::open_game(Game::Anno117, b).unwrap();
        let dir = tempfile::tempdir().unwrap();
        for source in [
            "data/graphics/props/terrain_props/vegetation/trees/jungle/tree_jungle_very_large_01.prp",
            "data/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_23.prp",
        ] {
            let Ok(ported) = port(&from, &to, source) else {
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
            to.mount_mod(dir.path()).unwrap();
            let cfg = target(source);
            let model = anno_formats::model::Model::load(&to, &cfg).unwrap();
            assert!(model.problems.is_empty(), "{:?}", model.problems);
            let part = model.parts.iter().find(|p| p.mesh.is_some()).unwrap();
            let diffuse = part.materials[0].diffuse.as_deref();
            assert!(diffuse.is_some_and(|t| to.exists(t)), "{diffuse:?}");
            assert!(model.mesh_extent[1] > 0.1, "{:?}", model.mesh_extent);
            let pieces = super::super::models::load(&to, &Default::default(), &cfg, 1.).unwrap();
            assert!(!pieces.is_empty());
        }
    }

    /// An install whose only archive ships `files`.
    fn shipped(game_files: &[(&str, &[u8])]) -> (tempfile::TempDir, Install) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        let mut p = anno_formats::rda::Packer::new(Default::default());
        for (path, data) in game_files {
            p.add(path, data).unwrap();
        }
        p.write(&dir.path().join("maindata/data0.rda")).unwrap();
        let install = Install::open(dir.path()).unwrap();
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
        let model = import(&dir.path().join("Tree.cfg"), "isles", &install).unwrap();
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
        let models = project_models(&files);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].kind, ModelKind::Custom);
        assert_eq!(removable(&models, &models[0]).len(), 5);
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
        let e = import(&dir.path().join("tree.cfg"), "isles", &install).unwrap_err();
        assert!(
            format!("{e:#}").contains("neither beside it nor in the game"),
            "{e:#}"
        );
        assert!(import(&dir.path().join("tree.txt"), "isles", &install).is_err());
    }

    // Needs 117: a shipped rock mesh and its textures, copied out and
    // brought back in as a bare mesh, load through the model reader.
    #[test]
    fn a_bare_mesh_becomes_a_loadable_prop() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Install::open_game(Game::Anno117, dir).unwrap();
        let shipped = anno_formats::model::Model::load(&install, STATIC_TEMPLATE).unwrap();
        let part = shipped.parts.iter().find(|p| p.mesh.is_some()).unwrap();
        // The cfg's layout name is what `wrap` writes from the rdm.
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
            for f in family(t.as_deref().unwrap()) {
                if install.exists(&f) {
                    copy(&f);
                }
            }
        }
        let model = import(&mesh, "isles", &install).unwrap();
        let folder = tempfile::tempdir().unwrap();
        for (path, bytes) in &model.files {
            let p = folder.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        install.mount_mod(folder.path()).unwrap();
        let loaded = anno_formats::model::Model::load(&install, &model.cfg).unwrap();
        assert!(loaded.parts.iter().any(|p| p.mesh.is_some()), "mesh loads");
    }
}
