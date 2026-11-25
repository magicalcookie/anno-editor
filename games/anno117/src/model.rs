// Converts 117's model cfgs to and from the neutral model (core::neutral).
// Reading flattens a cfg: every mesh part and decal of it and of its
// sub-models (houses carry torches, wall ends are stone blocks) becomes a
// piece at its offset, and its particles, lights and effect sub-models
// become effects. Decals, particles and lights keep their cfg entry whole.
// Writing makes one cfg per mesh piece, modelled on a shipped prop: the
// palm for plants (vertex colours carry wind and AO), a rock otherwise. A
// decal becomes a decal-only cfg and a particle or light a small effect
// cfg, each with the game's own entry, so a 117 model written again keeps
// every field of them.
// 117 loads 1800's rdm meshes unchanged (it ships 1800's palm,
// `palm_tree_01_04_lod0.rdm`, byte for byte), so a mesh is copied as it is
// unless its part placement has to be baked in.

use anno_core::neutral::{
    Effect, Material, Model, Piece, Ported, Shape, Source, texture_files, texture_source,
};
use anno_formats::model::{self, Transform};
use anno_formats::rdm::{self, Rdm};
use anno_formats::vfs::Install;
use anno_formats::xml::{self, set, set_point, tag};
use anyhow::{Context, Result, bail};

/// How deep we follow sub-models. Shipped groups nest two or three deep
/// (house, torch, the torch's fire), and a cfg that names itself would
/// loop forever.
const MAX_DEPTH: usize = 6;

/// 117 props whose cfg the written ones copy, by the mesh's kind.
const PLANT_TEMPLATE: &str = "data/base/graphics/library/environment/props/vegetation/trees/tree_palm_01/tree_palm_06_03.cfg";
const STATIC_TEMPLATE: &str =
    "data/base/graphics/roman/environment/props/rocks/roman_rock_07/roman_rock_07_e.cfg";

/// Reads `source`, a 117 cfg, into the neutral model. An effect cfg placed
/// on its own (chimney smoke, waterfall foam) is one effect.
pub fn read(install: &Install, source: &str) -> Result<Model> {
    let mut out = Model::default();
    if source.contains("/effects/") {
        out.effects.push(Effect {
            name: source.to_owned(),
            at: Transform::IDENTITY,
            missing: source.to_owned(),
            light: false,
            cfg: None,
        });
        return Ok(out);
    }
    flatten(install, source, Transform::IDENTITY, 0, &mut out)?;
    Ok(out)
}

/// Reads the parts and decals of `cfg`, placed at `at` in the source model,
/// then its sub-models. Animated, skinned and bone-linked parts are left
/// out: the neutral model holds static meshes only.
fn flatten(
    install: &Install,
    cfg: &str,
    at: Transform,
    depth: usize,
    out: &mut Model,
) -> Result<()> {
    let model = model::Model::load(install, cfg)?;
    let name = cfg.rsplit('/').next().unwrap_or(cfg);
    let stem = cfg.strip_suffix(".cfg").unwrap_or(cfg);
    let text = String::from_utf8_lossy(&install.read(cfg)?).into_owned();
    let tints = tints(&text);
    for (k, part) in model.parts.iter().enumerate() {
        let label = format!("{name} {}", part.name);
        let refused = if part.bone_linked {
            Some("attached to a bone")
        } else {
            match &part.mesh {
                None => Some("the mesh does not load"),
                Some(mesh) if mesh.skinned => Some("skinned"),
                Some(_) => None,
            }
        };
        if let Some(why) = refused {
            out.left_out.push(format!("{label}: {why}"));
            continue;
        }
        out.pieces.push(Piece {
            // With one part the piece keeps the cfg's own name, which is
            // the path the import wizard shows.
            name: match model.parts.len() {
                1 => stem.to_owned(),
                _ => format!("{stem}_part{k}"),
            },
            label,
            at,
            shape: Shape::Mesh {
                rdm: part.rdm.clone(),
                transform: part.transform,
            },
            materials: part.materials.iter().map(material).collect(),
            tints: tints.clone(),
        });
    }
    // The entries as the file holds them, for a 117 writer to copy whole.
    // The parser reads every entry of these lists, so they pair by index.
    let [decals, particles, lights] =
        ["Decals", "Particles", "Lights"].map(|l| xml::configs(&text, l));
    let block = |list: &[&str], k: usize| list.get(k).map(|b| b.to_string());
    for (k, decal) in model.decals.iter().enumerate() {
        out.pieces.push(Piece {
            name: format!("{stem}_decal{k}"),
            label: format!("{name} decal {k}"),
            at,
            shape: Shape::Decal {
                extents: decal.extents,
                transform: decal.transform,
                uv_rect: decal.uv_rect,
                cfg: block(&decals, k),
            },
            materials: decal.material.iter().map(material).collect(),
            tints: Vec::new(),
        });
    }
    for (k, p) in model.particles.iter().enumerate() {
        out.effects.push(Effect {
            name: format!("{cfg} {}", p.file),
            at: at.then(&p.transform),
            missing: format!("{name}: particle effect {}", p.file),
            light: false,
            cfg: block(&particles, k),
        });
    }
    for (k, l) in model.lights.iter().enumerate() {
        out.effects.push(Effect {
            name: format!("{cfg} light {}", l.name),
            at: at.then(&l.transform),
            missing: format!("{name}: light {}", l.name),
            light: true,
            cfg: block(&lights, k),
        });
    }
    // A rig's sub-models are whatever its animations attach (a villager's
    // hammer, broom and buckets, all hidden at rest), and standing them all
    // around the spot would look silly. A part that only spins or sways,
    // like a crane arm or a sign, keeps its static pose and its attachments.
    let rigged = (model.parts.iter()).any(|p| p.mesh.as_ref().is_some_and(|m| m.skinned));
    if rigged && !model.props.is_empty() {
        out.left_out
            .push(format!("{name}: {} attachments", model.props.len()));
    }
    for child in model.props.iter().filter(|_| !rigged) {
        let child_name = child.cfg.rsplit('/').next().unwrap_or(&child.cfg);
        let here = at.then(&child.transform);
        // A torch's flame or a brazier's fire: an effect cfg inside the model.
        if child.cfg.contains("/effects/") {
            out.effects.push(Effect {
                name: child.cfg.clone(),
                at: here,
                missing: format!("{name}: {child_name}"),
                light: false,
                cfg: None,
            });
            continue;
        }
        if depth + 1 >= MAX_DEPTH || !child.cfg.ends_with(".cfg") {
            out.left_out.push(format!("{name}: {child_name}"));
            continue;
        }
        if let Err(e) = flatten(install, &child.cfg, here, depth + 1, out) {
            out.left_out.push(format!("{child_name}: {e:#}"));
        }
    }
    Ok(())
}

fn material(m: &model::Material) -> Material {
    Material {
        diffuse: m.diffuse.clone(),
        normal: m.normal.clone(),
        metallic: m.metallic.clone(),
        color: m.diffuse_color,
        double_sided: m.double_sided,
        alpha: m.alpha,
        // Shader 24, `Terrain`: stone and ruins that take tint and grit
        // from the ground under them.
        ground_shaded: m.shader == model::Shader::Id(24),
        terrain_tint: m.terrain_tint,
    }
}

/// A cfg's per-instance tints (`<Colors>`); a missing channel is 1.
fn tints(cfg: &str) -> Vec<[f32; 3]> {
    let count: usize = (cfg.find("<Colors>"))
        .and_then(|at| tag(&cfg[at..], "Count")?.parse().ok())
        .unwrap_or(0);
    (0..count)
        .map(|k| {
            ["r", "g", "b"].map(|c| {
                tag(cfg, &format!("Color{k}.{c}"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1.)
            })
        })
        .collect()
}

/// Writes every piece of `model` as a 117 cfg named after it: a mesh with
/// its distance levels and textures, or a decal as a decal-only cfg like
/// the shipped `props/decals` ones. Particles and lights become small
/// effect cfgs, placed as effects; an effect cfg 117 ships is placed as it
/// is. A mesh that cannot be written fails the whole model, as a prop has
/// nothing else to show; a decal or effect that cannot is noted.
pub fn write(model: &Model, from: &dyn Source, to: &Install) -> Result<Ported> {
    let mut out = Ported {
        left_out: model.left_out.clone(),
        ..Ported::default()
    };
    let mut done = std::collections::HashSet::new();
    for piece in &model.pieces {
        let cfg_path = format!("{}.cfg", from.place(&piece.name));
        if !done.contains(&cfg_path) {
            let written = match &piece.shape {
                Shape::Mesh { rdm, transform } => {
                    write_piece(piece, rdm, *transform, &cfg_path, from, to)?
                }
                Shape::Decal { .. } => match write_decal(piece, &cfg_path, from, to) {
                    Ok(files) => files,
                    Err(e) => {
                        out.left_out.push(format!("{}: {e:#}", piece.label));
                        continue;
                    }
                },
            };
            out.files.extend(written);
            done.insert(cfg_path.clone());
        }
        out.pieces.push((cfg_path, piece.at));
    }
    for effect in &model.effects {
        match write_effect(effect, from, to) {
            Ok(Some((path, at, files))) => {
                out.files.extend(files);
                out.effects.push((path, at));
            }
            Ok(None) => out.left_out.push(effect.missing.clone()),
            Err(e) => out.left_out.push(format!("{}: {e:#}", effect.missing)),
        }
    }
    // Two effects of one torch share their cfg; textures are shared too.
    let mut seen = std::collections::HashSet::new();
    out.files.retain(|(path, _)| seen.insert(path.clone()));
    Ok(out)
}

/// A shipped decal-only cfg (a beach decal: `Terrain`, ShaderID 1, drawn
/// over the ground at run time). Written decals copy its header, and a
/// decal from another game its `DECAL` entry too.
const DECAL_TEMPLATE: &str =
    "data/base/graphics/library/environment/props/decals/beach/beach_decal_02.cfg";
/// A shipped effect cfg (a candle: one particle and one light) whose header
/// written effects copy.
const EFFECT_TEMPLATE: &str = "data/base/graphics/library/effects/fire/fire_candle_01.cfg";

/// Decal piece `piece` as the decal-only cfg `cfg_path`, cfg last. A 117
/// source's own `DECAL` entry is kept byte for byte, apart from texture
/// paths the target lacks; another game's decal fills the template's.
fn write_decal(
    piece: &Piece,
    cfg_path: &str,
    from: &dyn Source,
    to: &Install,
) -> Result<Vec<(String, Vec<u8>)>> {
    let Shape::Decal {
        extents,
        transform,
        uv_rect,
        cfg,
    } = &piece.shape
    else {
        bail!("not a decal");
    };
    let template = String::from_utf8_lossy(&to.read(DECAL_TEMPLATE)?).into_owned();
    let block = match cfg {
        Some(block) => block.clone(),
        None => {
            let m = piece
                .materials
                .first()
                .context("the decal has no material")?;
            let diffuse = m.diffuse.as_ref().context("the decal has no texture")?;
            let shipped = *xml::configs(&template, "Decals")
                .first()
                .context("decal template has no DECAL")?;
            let mut b = with_orientation(shipped, transform);
            for (axis, v) in ["x", "y", "z"].iter().zip(extents) {
                b = set(&b, &format!("Extents.{axis}"), &num(*v));
            }
            for (axis, v) in ["x", "y", "z", "w"].iter().zip(uv_rect) {
                b = set(&b, &format!("TexCoords.{axis}"), &num(*v));
            }
            b = set(&b, "cModelDiffTex", diffuse);
            b = set(
                &b,
                "NORMAL_ENABLED",
                if m.normal.is_some() { "1" } else { "0" },
            );
            if let Some(n) = &m.normal {
                b = set(&b, "cModelNormalTex", n);
            }
            for (c, v) in ["r", "g", "b"].iter().zip(m.color) {
                b = set(&b, &format!("cDiffuseColor.{c}"), &num(v));
            }
            // The beach's own height map, parallax, opacity and ground tint
            // are not the other game's.
            b = set(&b, "HEIGHT_MAP_ENABLED", "0");
            b = set(&b, "PARALLAX_MAPPING_ENABLED", "0");
            b = set(&b, "cOpacity", "1");
            b = set(
                &b,
                "cUseTerrainTinting",
                if m.terrain_tint > 0. { "1" } else { "0" },
            );
            set(&b, "cTerrainTintIntensity", &num(m.terrain_tint))
        }
    };
    let mut files = Vec::new();
    let block = carry_files(&block, from, to, &mut files)?;

    // The header bounds the box as its transform turns it.
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for corner in 0..8 {
        let p = std::array::from_fn(|i| match corner >> i & 1 {
            0 => -extents[i],
            _ => extents[i],
        });
        let p = transform.apply(p);
        for i in 0..3 {
            (lo[i], hi[i]) = (lo[i].min(p[i]), hi[i].max(p[i]));
        }
    }
    let center: [f32; 3] = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
    let extent: [f32; 3] = std::array::from_fn(|i| (hi[i] - lo[i]) / 2.);
    let open = template
        .find("<Decals>")
        .context("decal template has no Decals")?;
    let close = template
        .find("</Decals>")
        .context("decal template has no Decals")?;
    let mut header = template[..open].to_owned();
    for (axis, i) in [("x", 0), ("y", 1), ("z", 2)] {
        header = set(&header, &format!("Center.{axis}"), &num(center[i]));
        header = set(&header, &format!("Extent.{axis}"), &num(extent[i]));
    }
    let radius = extent.iter().map(|v| v * v).sum::<f32>().sqrt();
    header = set(&header, "Radius", &num(radius));
    let nl = newline(&template);
    let text = format!("{header}<Decals>{block}{nl}{}", &template[close..]);
    files.push((cfg_path.to_owned(), text.into_bytes()));
    Ok(files)
}

/// What one effect becomes in a 117 mod: the cfg to place, where, and the
/// files it needs. `None` if 117 has nothing like it.
type WrittenEffect = Option<(String, Transform, Vec<(String, Vec<u8>)>)>;

/// A particle or light kept whole becomes a cfg of its own, shaped like a
/// shipped effect cfg, with the effect's turn and scale in its transformer
/// and only its position left for the placement: effects are placed as
/// objects, which turn about the vertical only. An effect cfg 117 ships is
/// placed as it is; another game's goes by its name.
fn write_effect(effect: &Effect, from: &dyn Source, to: &Install) -> Result<WrittenEffect> {
    let Some(block) = &effect.cfg else {
        if effect.name.ends_with(".cfg") && to.exists(&effect.name) {
            return Ok(Some((effect.name.clone(), effect.at, Vec::new())));
        }
        if effect.light {
            return Ok(None);
        }
        return Ok(effect_counterpart(to, &effect.name).map(|p| (p, effect.at, Vec::new())));
    };
    let turn = Transform {
        position: [0.; 3],
        ..effect.at
    };
    let mut files = Vec::new();
    let block = carry_files(&with_orientation(block, &turn), from, to, &mut files)?;
    let template = String::from_utf8_lossy(&to.read(EFFECT_TEMPLATE)?).into_owned();
    let open = template
        .find("<Particles>")
        .context("effect template has no Particles")?;
    let nl = newline(&template);
    let list = if effect.light { "Lights" } else { "Particles" };
    let text = format!(
        "{}<{list}>{block}{nl}</{list}>{nl}</Config>{nl}",
        &template[..open]
    );
    // Named after the model it sat in and what it holds: the same torch
    // flame on five houses is one file, two differently turned flames two.
    let model = effect.name.split(' ').next().unwrap_or(&effect.name);
    let stem = from.place(model.strip_suffix(".cfg").unwrap_or(model));
    let kind = if effect.light { "light" } else { "particle" };
    let path = format!("{stem}_{kind}_{:08x}.cfg", fnv(text.as_bytes()) as u32);
    files.push((path.clone(), text.into_bytes()));
    let at = Transform {
        position: effect.at.position,
        ..Transform::IDENTITY
    };
    Ok(Some((path, at, files)))
}

/// The 117 effect closest to another game's, going by the name; `None` if
/// 117 has nothing like it or the cfg is missing.
fn effect_counterpart(to: &Install, name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    let pick = if n.contains("waterfall") || n.contains("splash") {
        "data/base/graphics/library/effects/water/splash_waterfall_mid_bottom_01.cfg"
    } else if n.contains("smoke") {
        "data/base/graphics/library/effects/air/smoke_billow_small_01.cfg"
    } else if ["fire", "torch", "candle", "brazier", "flame"]
        .iter()
        .any(|w| n.contains(w))
    {
        "data/base/graphics/library/effects/fire/fire_torch_01.cfg"
    } else {
        return None;
    };
    to.exists(pick).then(|| pick.to_owned())
}

/// `block` (a cfg list entry) with its own placement set to `t`: every
/// `ORIENTATION_TRANSFORM` of its transformer gives way to one for `t`,
/// first, so animation and state transforms after it still apply. An entry
/// without a transformer gets one.
fn with_orientation(block: &str, t: &Transform) -> String {
    let nl = newline(block);
    let s = if t.scale[0] == t.scale[1] && t.scale[1] == t.scale[2] {
        (t.scale[0], [1.; 3])
    } else {
        (1., t.scale)
    };
    let mut orientation = format!(
        "<Config><ConfigType>ORIENTATION_TRANSFORM</ConfigType>{nl}<Conditions>0</Conditions>{nl}"
    );
    for (axis, i) in [("x", 0), ("y", 1), ("z", 2)] {
        orientation.push_str(&format!(
            "<Position.{axis}>{}</Position.{axis}>{nl}",
            num(t.position[i])
        ));
    }
    for (axis, i) in [("x", 0), ("y", 1), ("z", 2), ("w", 3)] {
        orientation.push_str(&format!(
            "<Rotation.{axis}>{}</Rotation.{axis}>{nl}",
            num(t.rotation[i])
        ));
    }
    orientation.push_str(&format!("<Scale>{}</Scale>{nl}", num(s.0)));
    for (axis, i) in [("x", 0), ("y", 1), ("z", 2)] {
        orientation.push_str(&format!(
            "<NonUniformScale.{axis}>{}</NonUniformScale.{axis}>{nl}",
            num(s.1[i])
        ));
    }
    orientation.push_str(&format!("</Config>{nl}"));
    // The entry's own transformer comes before its materials; a material
    // has none.
    let own = block.find("<Materials>").unwrap_or(block.len());
    if let Some(at) = block[..own].find("<Transformer>") {
        let from = at + "<Transformer>".len();
        let inner = xml::block(&block[at..], "Transformer").unwrap_or("");
        let kept: String = (xml::configs(&block[at..], "Transformer").into_iter())
            .filter(|c| !c.contains("<ConfigType>ORIENTATION_TRANSFORM<"))
            .map(|c| format!("{c}{nl}"))
            .collect();
        return format!(
            "{}{orientation}{kept}{}",
            &block[..from],
            &block[from + inner.len()..]
        );
    }
    let head = (block.find("</ConfigType>")).map_or(0, |at| at + "</ConfigType>".len() + nl.len());
    format!(
        "{}<Transformer>{orientation}</Transformer>{nl}{}",
        &block[..head],
        &block[head..]
    )
}

/// `block` with every file it names that the target lacks copied into the
/// mod (into `files`) and renamed to its copy: textures as their `_N.dds`
/// levels, anything else (`.rdp` particles) as it is. Files 117 ships are
/// left as named, so a 117 source's entry stays byte for byte.
fn carry_files(
    block: &str,
    from: &dyn Source,
    to: &Install,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<String> {
    const TEXTURES: [&str; 4] = [".psd", ".png", ".tga", ".dds"];
    let mut out = String::with_capacity(block.len());
    let mut rest = block;
    while let Some(gt) = rest.find('>') {
        out.push_str(&rest[..=gt]);
        rest = &rest[gt + 1..];
        let end = rest.find('<').unwrap_or(rest.len());
        let value = &rest[..end];
        rest = &rest[end..];
        let lower = value.to_ascii_lowercase();
        let ext = lower.rfind('.').map_or("", |at| &lower[at..]);
        let path = value.replace('\\', "/");
        let carried = if TEXTURES.contains(&ext) && !to.exists(&model::texture_path(&path)) {
            // Engine defaults like `default_height.png` ship in no `_N.dds`
            // form the source can give; the game finds them by the name.
            let source = texture_source(&path);
            let levels = texture_files(from, &source);
            match levels.is_empty() {
                true => value.to_owned(),
                false => {
                    files.extend(levels);
                    from.place(&source)
                }
            }
        } else if ext == ".rdp" && !to.exists(&path) {
            let bytes = from
                .read(&path)
                .with_context(|| format!("particle {path}"))?;
            files.push((from.place(&path), bytes));
            from.place(&path)
        } else {
            value.to_owned()
        };
        out.push_str(&carried);
    }
    out.push_str(rest);
    Ok(out)
}

/// The line break `text` uses: shipped cfgs end lines with CRLF.
fn newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// A number as the game's serializer writes it: shortest form, no `-0`.
fn num(v: f32) -> String {
    if v == 0. {
        "0".to_owned()
    } else {
        v.to_string()
    }
}

/// FNV-1a, 64 bit: written file names must not change between builds.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325_u64;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

/// One piece as the cfg `cfg_path`: its mesh, levels and textures, cfg
/// last.
fn write_piece(
    piece: &Piece,
    rdm: &str,
    transform: Transform,
    cfg_path: &str,
    from: &dyn Source,
    to: &Install,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    let bytes = from.read(rdm).with_context(|| format!("mesh {rdm}"))?;
    // A part placement is baked into a mesh named after the piece, since
    // two models may place the same rdm differently.
    let baked = transform != Transform::IDENTITY;
    let how = rdm::Rebake {
        transform,
        layout: None,
        color_fill: Vec::new(),
        uv_rect: None,
    };
    let bytes = match baked {
        true => rdm::rebake(&bytes, &how).with_context(|| format!("mesh {rdm}"))?,
        false => bytes,
    };
    let mesh = Rdm::parse(&bytes).with_context(|| format!("mesh {rdm}"))?;
    if mesh.skinned {
        bail!("mesh {rdm}: skinned");
    }
    let stem = cfg_path.trim_end_matches(".cfg");
    let mesh_out = match baked {
        true => format!("{stem}.rdm"),
        false => from.place(rdm),
    };
    files.push((mesh_out.clone(), bytes));
    for (level, path) in anno_core::files::siblings(rdm).iter().enumerate().skip(1) {
        let Ok(bytes) = from.read(path) else { continue };
        let (name, bytes) = match baked {
            true => (
                format!("{stem}_lod{level}.rdm"),
                rdm::rebake(&bytes, &how).with_context(|| format!("mesh {path}"))?,
            ),
            false => (from.place(path), bytes),
        };
        files.push((name, bytes));
    }
    // The cfg names textures by their source; the game reads `<stem>_N.dds`.
    let mut texture = |named: &String| {
        let source = texture_source(named);
        files.extend(texture_files(from, &source));
        from.place(&source)
    };
    let materials: Vec<Written> = (piece.materials.iter())
        .map(|m| Written {
            diffuse: m.diffuse.as_ref().map(&mut texture),
            normal: m.normal.as_ref().map(&mut texture),
            material: m,
        })
        .collect();
    let cfg = cfg_text(to, &mesh_out, &mesh, &materials)?;
    files.push((cfg_path.to_owned(), cfg.into_bytes()));
    Ok(files)
}

/// A material with its textures as the cfg names them.
struct Written<'a> {
    diffuse: Option<String>,
    normal: Option<String>,
    material: &'a Material,
}

/// A 117 cfg for `mesh` (its virtual path in the mod), copied from a
/// shipped prop: the palm for plants, a rock otherwise. One material block
/// per entry of `materials`.
fn cfg_text(to: &Install, mesh: &str, rdm: &Rdm, materials: &[Written]) -> Result<String> {
    let format = rdm.format.to_string();
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

    // A mesh whose groups use more materials than the source names (2070
    // cfgs name only the first) repeats the last for the rest.
    let used = (rdm.groups.iter().map(|g| g.material as usize + 1).max()).unwrap_or(0);
    let count = match materials.len() {
        0 => 0,
        n => n.max(used),
    };
    let open = "<Materials>";
    let (a, b) = (
        cfg.find(open).context("template has no Materials")? + open.len(),
        cfg.find("</Materials>")
            .context("template has no Materials")?,
    );
    let block = cfg[a..b].to_owned();
    let on = |b: bool| if b { "1" } else { "0" };
    let blocks: String = (0..count)
        .filter_map(|i| Some((i, materials.get(i).or(materials.last())?)))
        .map(|(i, w)| {
            let m = w.material;
            let mut out = set(&block, "Name", &format!("material_{i}"));
            out = set(&out, "VertexFormat", &format);
            if let Some(t) = &w.diffuse {
                out = set(&out, "cModelDiffTex", t);
            }
            // Without one the template's stays named but off.
            match &w.normal {
                Some(t) => out = set(&out, "cModelNormalTex", t),
                None => out = set(&out, "NORMAL_ENABLED", "0"),
            }
            // Six decimals, as the games' own serializer writes colours
            // (every one of 1800's 12,579 prp material colours).
            for (c, v) in ["r", "g", "b"].iter().zip(m.color) {
                out = set(&out, &format!("cDiffuseColor.{c}"), &format!("{v:.6}"));
            }
            set(&out, "DOUBLE_SIDED", on(m.double_sided))
        })
        .collect();
    cfg.replace_range(a..b, &blocks);
    // Each distance level draws every material.
    let indices: String = (0..count).map(|i| format!("<i>{i}</i>\n")).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The 117 install as its own source, its copies going under a mod
    /// folder as an import's do. What 117 ships must keep its own path.
    struct Moved<'a>(&'a Install);

    const MOVED: &str = "data/editor/test/";

    impl Source for Moved<'_> {
        fn read(&self, path: &str) -> Result<Vec<u8>> {
            self.0.read(path)
        }
        fn place(&self, path: &str) -> String {
            format!("{MOVED}{path}")
        }
    }

    fn install() -> Option<Install> {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap())
    }

    /// The `kind` entries of a cfg's `list`, cut from the file by their
    /// `ConfigType` line rather than by tag depth, so a slip in
    /// `xml::configs` can't hide on both sides of a comparison.
    fn entries(cfg: &str, list: &str, kind: &str) -> Vec<String> {
        let (open, close) = (format!("<{list}>"), format!("</{list}>"));
        let Some(a) = cfg.find(&open) else {
            return Vec::new();
        };
        let body = &cfg[a + open.len()..a + cfg[a..].find(&close).unwrap()];
        let start = format!("<Config><ConfigType>{kind}</ConfigType>");
        (body.split(&start).skip(1))
            .map(|e| format!("{start}{}", e.trim_end()))
            .collect()
    }

    /// An entry without its orientation transforms, which the writer moves
    /// into the placement.
    fn unplaced(entry: &str) -> String {
        let mut out = entry.to_owned();
        let tag = "<Config><ConfigType>ORIENTATION_TRANSFORM</ConfigType>";
        while let Some(a) = out.find(tag) {
            let b = a + out[a..].find("</Config>").unwrap() + "</Config>".len();
            let b = b + out[b..].len() - out[b..].trim_start().len();
            out.replace_range(a..b, "");
        }
        // An entry left with an empty transformer had only its placement.
        out.replace("<Transformer></Transformer>\r\n", "")
    }

    fn close(a: &Transform, b: &Transform) -> bool {
        let near = |x: &[f32], y: &[f32]| x.iter().zip(y).all(|(x, y)| (x - y).abs() < 1e-5);
        let flip = b.rotation.map(|v| -v);
        near(&a.position, &b.position)
            && (near(&a.rotation, &b.rotation) || near(&a.rotation, &flip))
            && near(&a.scale, &b.scale)
    }

    // Needs Anno 117. Shipped models with ground decals, particles and
    // lights, read and written again into a mod folder: each decal comes
    // back as a decal-only cfg whose entry is the shipped one byte for
    // byte, each particle and light as an effect cfg whose entry matches
    // the shipped one but for its placement, which the effect's offset and
    // the entry's transformer put back where the shipped file has it. The
    // shipped side is cut from the file and parsed on its own, not through
    // the reader.
    #[test]
    fn shipped_decals_and_effects_survive_a_write() {
        let Some(install) = install() else { return };
        let cases = [
            // A baked crack: no transformer. A beach decal: one.
            "data/base/graphics/library/environment/props/decals/cracks/crack_decal_01.cfg",
            "data/base/graphics/library/environment/props/decals/beach/beach_decal_02.cfg",
            // A fireplace: mesh, ground decal, flames and a flickering light.
            "data/base/graphics/celtic/buildings/ornaments/hall_of_fame/fireplaces/fireplace_01.cfg",
            // Three tilted flames over a wood pile sub-model.
            "data/base/graphics/library/props/decoration/wood_pile_01/wood_pile_02_burning.cfg",
            // A portal: decal, particles, five lights and an effect cfg.
            "data/dlc03/graphics/egypt/buildings/ornaments/hall_of_fame/statues/ancient_portal_01.cfg",
        ];
        for cfg in cases {
            let shipped = String::from_utf8_lossy(&install.read(cfg).unwrap()).into_owned();
            let parsed = model::Model::parse(shipped.as_bytes()).unwrap();
            let model = read(&install, cfg).unwrap();
            let out = write(&model, &Moved(&install), &install).unwrap();
            let file = |path: &str| {
                let bytes = &out.files.iter().find(|f| f.0 == path).unwrap().1;
                String::from_utf8_lossy(bytes).into_owned()
            };
            let stem = format!("{MOVED}{}", cfg.trim_end_matches(".cfg"));

            let decals = entries(&shipped, "Decals", "DECAL");
            assert!(!decals.is_empty() || parsed.decals.is_empty(), "{cfg}");
            assert_eq!(decals.len(), parsed.decals.len(), "{cfg}");
            for (k, shipped_entry) in decals.iter().enumerate() {
                let path = format!("{stem}_decal{k}.cfg");
                let (_, at) = (out.pieces.iter().find(|p| p.0 == path))
                    .unwrap_or_else(|| panic!("{path} not placed"));
                assert_eq!(*at, Transform::IDENTITY, "{path}");
                let written = file(&path);
                assert_eq!(
                    entries(&written, "Decals", "DECAL"),
                    [shipped_entry.clone()]
                );
                let again = model::Model::parse(written.as_bytes()).unwrap();
                // Element ids are places in their own file: not compared.
                let unplaced = |d: &model::Decal| model::Decal {
                    element: anno_formats::xml::Id::ROOT,
                    ..d.clone()
                };
                let again: Vec<_> = again.decals.iter().map(unplaced).collect();
                assert_eq!(again, [unplaced(&parsed.decals[k])], "{path}");
            }

            // Particles and lights of the top file, in order; sub-models'
            // come after.
            let effects: Vec<(String, Transform)> = (out.effects.iter())
                .filter(|(path, _)| path.starts_with(&format!("{stem}_")))
                .cloned()
                .collect();
            let shipped_particles = entries(&shipped, "Particles", "PARTICLE");
            let shipped_lights = entries(&shipped, "Lights", "LIGHT");
            assert_eq!(shipped_particles.len(), parsed.particles.len(), "{cfg}");
            assert_eq!(shipped_lights.len(), parsed.lights.len(), "{cfg}");
            let wanted = (shipped_particles.iter().zip(&parsed.particles))
                .map(|(e, p)| ("Particles", "PARTICLE", e, p.transform))
                .chain(
                    (shipped_lights.iter().zip(&parsed.lights))
                        .map(|(e, l)| ("Lights", "LIGHT", e, l.transform)),
                );
            let mut n = 0;
            for ((list, kind, shipped_entry, shipped_at), (path, offset)) in wanted.zip(&effects) {
                let written = file(path);
                let entry = entries(&written, list, kind);
                assert_eq!(entry.len(), 1, "{path}");
                assert_eq!(unplaced(&entry[0]), unplaced(shipped_entry), "{path}");
                let again = model::Model::parse(written.as_bytes()).unwrap();
                let inner = match list {
                    "Particles" => again.particles[0].transform,
                    _ => again.lights[0].transform,
                };
                let placed = offset.then(&inner);
                assert!(
                    close(&placed, &shipped_at),
                    "{path}: {placed:?} {shipped_at:?}"
                );
                n += 1;
            }
            assert_eq!(n, shipped_particles.len() + shipped_lights.len(), "{cfg}");
            // Every texture and particle file they name ships, so nothing
            // but cfgs and the mesh pieces' own files is copied.
            for (path, _) in &out.files {
                assert!(
                    path.ends_with(".cfg") || path.ends_with(".rdm") || path.ends_with(".dds"),
                    "{path}"
                );
            }

            // Effect cfgs the model names are placed as they ship.
            for prop in parsed.props.iter().filter(|p| p.cfg.contains("/effects/")) {
                assert!(
                    out.effects
                        .iter()
                        .any(|(path, at)| *path == prop.cfg && close(at, &prop.transform)),
                    "{cfg}: {}",
                    prop.cfg
                );
            }
            eprintln!(
                "{cfg}: {} decals, {} particles, {} lights; {} pieces, {} effects, left out {:?}",
                decals.len(),
                shipped_particles.len(),
                shipped_lights.len(),
                out.pieces.len(),
                out.effects.len(),
                out.left_out
            );
        }
    }
}
