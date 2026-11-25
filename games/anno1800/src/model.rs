// Converts 1800's prop prps to and from the neutral model (core::neutral).
// A prp holds one static mesh with no part placement, so reading one gives
// one piece, or a decal when the mesh is the unit ground quad; an effect cfg
// reads as one effect. Writing makes a prp per piece of whatever kind its materials
// call for, copied from a shipped prp of that `<Type>`, with the piece's
// placement and 1800's vertex layout baked into its own mesh; projector
// decals become 1800's unit ground quad. Particle effects become 1800's
// own effects, by name.

use anno_core::neutral::{
    Effect, Material, Model, Piece, Ported, Shape, Source, texture_files, texture_source,
};
use anno_formats::model::{Alpha, Transform};
use anno_formats::rdm::{self, Rdm};
use anno_formats::vfs::Install;
use anno_formats::xml::{set, tag};
use anyhow::{Context, Result, ensure};

/// Reads `source`, an 1800 prp, as one piece: its mesh and materials. A
/// prp on 1800's unit ground quad is a decal; an effect cfg placed on its
/// own (torch fire, smoke) is one effect.
pub fn read(install: &Install, source: &str) -> Result<Model> {
    let name = source.rsplit('/').next().unwrap_or(source);
    if source.ends_with(".cfg") && source.contains("/effects/") {
        ensure!(install.exists(source), "{source} does not ship");
        return Ok(Model {
            effects: vec![Effect {
                name: source.to_owned(),
                at: Transform::IDENTITY,
                missing: source.to_owned(),
                light: false,
                cfg: None,
            }],
            ..Model::default()
        });
    }
    let prp = String::from_utf8_lossy(&install.read(source)?).into_owned();
    let mesh = tag(&prp, "MeshFileName")
        .context("prp has no MeshFileName")?
        .replace('\\', "/");
    if mesh.ends_with("/1x1_decal.rdm") {
        return Ok(Model {
            pieces: vec![quad_decal(source, &prp)?],
            ..Model::default()
        });
    }
    // Ice floes and plant-shaded dirt planes: decals drawn by their own
    // mesh, which the other games' writers have no decal kind for.
    ensure!(
        !source.contains("/decals/") && !source.contains("/effects/"),
        "{name}: a mesh decal or effect mesh, which only comes over on the unit ground quad"
    );
    let materials: Vec<Material> = (prp.split("<Material>").skip(1))
        .map(|m| Material {
            diffuse: tag(m, "cPropDiffuseTex")
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            normal: tag(m, "cPropNormalTex")
                .filter(|t| !t.is_empty() && tag(m, "NORMAL_MAP_ENABLED") != Some("0"))
                .map(str::to_owned),
            metallic: None,
            color: ["r", "g", "b"].map(|c| {
                tag(m, &format!("cDiffuseColor.{c}"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1.)
            }),
            double_sided: tag(m, "DOUBLE_SIDED").is_some_and(|v| v != "0"),
            alpha: Alpha::Opaque,
            ground_shaded: false,
            terrain_tint: 0.,
        })
        .collect();
    ensure!(!materials.is_empty(), "prp has no materials");
    Ok(Model {
        pieces: vec![Piece {
            name: source.strip_suffix(".prp").unwrap_or(source).to_owned(),
            label: name.to_owned(),
            at: Transform::IDENTITY,
            shape: Shape::Mesh {
                rdm: mesh,
                transform: Transform::IDENTITY,
            },
            materials,
            tints: Vec::new(),
        }],
        ..Model::default()
    })
}

/// A prp on 1800's unit ground quad (x and z ±0.5, the texture across it
/// once) as the box decal it stands for, half a metre deep: what
/// [`write_decal`] makes of a box, read back. Its SimplePBR material names
/// the cfg-style `cModelDiffTex`.
fn quad_decal(source: &str, prp: &str) -> Result<Piece> {
    let texture = |names: [&str; 2]| {
        names
            .iter()
            .find_map(|n| tag(prp, n).filter(|t| !t.is_empty()))
            .map(str::to_owned)
    };
    let diffuse =
        texture(["cModelDiffTex", "cPropDiffuseTex"]).context("the decal has no texture")?;
    let normal = (tag(prp, "NORMAL_ENABLED") == Some("1"))
        .then(|| texture(["cModelNormalTex", "cPropNormalTex"]))
        .flatten();
    let color = ["r", "g", "b"].map(|c| {
        tag(prp, &format!("cDiffuseColor.{c}"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.)
    });
    Ok(Piece {
        name: source.strip_suffix(".prp").unwrap_or(source).to_owned(),
        label: source.rsplit('/').next().unwrap_or(source).to_owned(),
        at: Transform::IDENTITY,
        shape: Shape::Decal {
            extents: [0.5, 0.25, 0.5],
            transform: Transform::IDENTITY,
            uv_rect: [0., 0., 1., 1.],
            cfg: None,
        },
        materials: vec![Material {
            diffuse: Some(diffuse),
            normal,
            metallic: None,
            color,
            double_sided: false,
            alpha: Alpha::Decal,
            ground_shaded: false,
            terrain_tint: 0.,
        }],
        tints: Vec::new(),
    })
}

/// What a written piece becomes, decided by its materials. Each kind
/// copies a shipped 1800 prp of that `<Type>`:
/// - `Plant`: cutout foliage. All 537 shipped plant prps use the two-color
///   layout.
/// - `Rock`: ground-shaded stone and ruins (117's shader 24, `Terrain`,
///   which takes tint and grit from the ground under it).
/// - `Building`: any other opaque mesh (`SimplePBR`, 1,445 shipped, mostly
///   building props). It reads cfg-style diffuse, normal and metallic maps.
/// - `Decal`: mesh and projector decals (`Decal`, like the arctic ice floes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Plant,
    Rock,
    Building,
    Decal,
}

impl Kind {
    fn of(materials: &[Material]) -> Kind {
        let visible = |m: &&Material| m.alpha != Alpha::Hidden;
        if materials
            .iter()
            .filter(visible)
            .all(|m| m.alpha == Alpha::Decal)
        {
            Kind::Decal
        } else if materials
            .iter()
            .any(|m| matches!(m.alpha, Alpha::Cutout(_)))
        {
            Kind::Plant
        } else if materials.iter().filter(visible).all(|m| m.ground_shaded) {
            Kind::Rock
        } else {
            Kind::Building
        }
    }

    fn template(self) -> &'static str {
        match self {
            Kind::Plant => {
                "data/graphics/props/terrain_props/vegetation/trees/moderate/tree_moderate_01_var0.prp"
            }
            Kind::Rock => {
                "data/graphics/props/terrain_props/rocks/rocks_moderate/rock_moderate_13.prp"
            }
            Kind::Building => "data/cdlc06/graphics/props/neon_sign_01.prp",
            Kind::Decal => {
                "data/dlc03/graphics/landscape/terrain/decals/ice_floes/icefloe_arctic_coast_fine_01.prp"
            }
        }
    }

    /// The vertex layout the kind's shader reads, as its shipped meshes
    /// store it. `P4h_N4b_G4b_B4b_T2h`, plus two colors for plants (a 0/255
    /// mask, then a signed vector).
    fn layout(self) -> rdm::VertexFormat {
        use rdm::{Attribute, Semantic, Storage};
        let a = |semantic, storage, interpretation, count| Attribute {
            semantic,
            storage,
            interpretation,
            count,
        };
        let mut v = vec![
            a(Semantic::Position, Storage::F16, 0, 4),
            a(Semantic::Normal, Storage::U32, 6, 1),
            a(Semantic::Tangent, Storage::U32, 6, 1),
            a(Semantic::Bitangent, Storage::U32, 6, 1),
            a(Semantic::Texcoord, Storage::F16, 0, 2),
        ];
        if self == Kind::Plant {
            v.push(a(Semantic::Color, Storage::U32, 2, 1));
            v.push(a(Semantic::Color, Storage::U32, 6, 1));
        }
        rdm::VertexFormat(v)
    }
}

/// Colors a plant lacks for 1800's plant layout, by slot. Slot 0 is the
/// wind mask (0 trunk, 255 leaves, alpha 0). A plant with no colors at all,
/// like bushes and wheat fields, gets 0 and stands still; making up a mask
/// seemed worse. Slot 1 is a signed vector around 128, and zero bends
/// nothing. Not checked in game yet.
const PLANT_COLOR_FILL: [[u8; 4]; 2] = [[0; 4], [128; 4]];

/// 1800's unit ground quad (x and z ±0.5, texcoords 0..1). Projector decals
/// become this quad, scaled to the decal's box and mapped to its atlas cell.
const DECAL_QUAD: &str = "data/cdlc05/graphics/props/decals/rdm/1x1_decal.rdm";

/// Files to stage: virtual path and bytes.
type Files = Vec<(String, Vec<u8>)>;

/// Writes every piece of `model` as a prp named after it, and its effects
/// as 1800's own. A piece that cannot be written is left out with why; a
/// piece shared by several placements is written once.
pub fn write(model: &Model, from: &dyn Source, to: &Install) -> Result<Ported> {
    let mut out = Ported {
        left_out: model.left_out.clone(),
        ..Ported::default()
    };
    let mut done = std::collections::HashSet::new();
    for piece in &model.pieces {
        let prp = format!("{}.prp", from.place(&piece.name));
        if !done.contains(&prp) {
            let written = match &piece.shape {
                Shape::Mesh { rdm, transform } => {
                    write_mesh(piece, rdm, *transform, &prp, from, to)
                }
                Shape::Decal {
                    extents,
                    transform,
                    uv_rect,
                    ..
                } => write_decal(piece, *extents, *transform, *uv_rect, &prp, from, to).map(Some),
            };
            match written {
                Ok(Some(files)) => {
                    out.files.extend(files);
                    done.insert(prp.clone());
                }
                // Hidden volume that cuts terrain or water. Nothing to draw.
                Ok(None) => continue,
                Err(e) => {
                    out.left_out.push(format!("{}: {e:#}", piece.label));
                    continue;
                }
            }
        }
        out.pieces.push((prp, piece.at));
    }
    for effect in &model.effects {
        // 1800's effects light themselves (its torch carries a light), so a
        // light on its own has no counterpart.
        if effect.light {
            out.left_out.push(effect.missing.clone());
            continue;
        }
        match effect_counterpart(to, &effect.name) {
            Some(path) => out.effects.push((path, effect.at)),
            None => out.left_out.push(effect.missing.clone()),
        }
    }
    out.files.sort_by(|a, b| a.0.cmp(&b.0));
    out.files.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

/// The 1800 effect closest to another game's, going by the name. 117's
/// particle files don't load in 1800, so 1800's shipped effects stand in.
/// `None` if 1800 has nothing like it or the cfg is missing.
pub(crate) fn effect_counterpart(to: &Install, name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    let pick = if n.contains("waterfall") || n.contains("splash") {
        // 117 and 1800 share the waterfall foam names and numbering.
        let k = ["01", "02", "03"]
            .into_iter()
            .find(|k| n.contains(&format!("waterfall_{k}")))
            .unwrap_or("01");
        format!("data/graphics/effects/water/waterfall_{k}_bttm.cfg")
    } else if n.contains("smoke") {
        "data/graphics/effects/city_incidents/fire/fire_smoke_01.cfg".to_owned()
    } else if ["fire", "torch", "candle", "brazier", "flame"]
        .iter()
        .any(|w| n.contains(w))
    {
        "data/graphics/effects/environment/fps_torch.cfg".to_owned()
    } else {
        return None;
    };
    to.exists(&pick).then_some(pick)
}

/// A mesh piece as the prp `prp_path`, of whatever [`Kind`] its materials
/// call for. Returns `None` for a hidden volume (it cuts terrain or water
/// and draws nothing). The placement and layout are baked into a mesh
/// named after the prp, since two models may place the same rdm
/// differently. Returns the files to stage, prp last.
fn write_mesh(
    piece: &Piece,
    rdm: &str,
    transform: Transform,
    prp_path: &str,
    from: &dyn Source,
    to: &Install,
) -> Result<Option<Files>> {
    ensure!(!piece.materials.is_empty(), "no materials");
    if piece.materials.iter().all(|m| m.alpha == Alpha::Hidden) {
        return Ok(None);
    }
    let kind = Kind::of(&piece.materials);
    let layout = kind.layout();
    let how = rdm::Rebake {
        transform,
        layout: Some(layout.clone()),
        color_fill: PLANT_COLOR_FILL.to_vec(),
        uv_rect: None,
    };
    let mut files = Vec::new();
    let stem = prp_path.trim_end_matches(".prp");
    let mesh_out = format!("{stem}.rdm");
    let mut format = None;
    for (level, path) in anno_core::files::siblings(rdm).iter().enumerate() {
        let Ok(bytes) = from.read(path) else {
            ensure!(level > 0, "mesh {path} does not ship");
            continue;
        };
        // Every level shares the top level's layout.
        if format.is_none() {
            let mesh = Rdm::parse(&bytes).with_context(|| format!("mesh {path}"))?;
            ensure!(!mesh.skinned, "skinned");
            format = Some(mesh.format);
        }
        let bytes = if format.as_ref() == Some(&layout) && transform == Transform::IDENTITY {
            bytes
        } else {
            rdm::rebake(&bytes, &how).with_context(|| format!("mesh {path}"))?
        };
        let name = match level {
            0 => mesh_out.clone(),
            _ => format!("{stem}_lod{level}.rdm"),
        };
        files.push((name, bytes));
    }
    let prp = prp_text(
        from,
        to,
        kind,
        &mesh_out,
        &piece.materials,
        &piece.tints,
        &mut files,
    )?;
    files.push((prp_path.to_owned(), prp.into_bytes()));
    Ok(Some(files))
}

/// A projector decal (a box that paints its texture onto whatever is below)
/// as an 1800 decal prp: 1800's unit ground quad, scaled to the box, placed
/// by its transform, texcoords mapped to the decal's atlas cell. Fine on
/// flat ground. On a slope you see the flat quad.
fn write_decal(
    piece: &Piece,
    extents: [f32; 3],
    transform: Transform,
    uv_rect: [f32; 4],
    prp_path: &str,
    from: &dyn Source,
    to: &Install,
) -> Result<Files> {
    let material = piece
        .materials
        .first()
        .context("the decal has no material")?;
    let e = extents;
    let box_scale = Transform {
        scale: [2. * e[0], 1., 2. * e[2]],
        ..Transform::IDENTITY
    };
    let how = rdm::Rebake {
        transform: transform.then(&box_scale),
        layout: None,
        color_fill: Vec::new(),
        uv_rect: Some(uv_rect),
    };
    let quad = rdm::rebake(&to.read(DECAL_QUAD)?, &how).context("decal quad")?;
    let mesh_out = format!("{}.rdm", prp_path.trim_end_matches(".prp"));
    let mut files = vec![(mesh_out.clone(), quad)];
    let prp = prp_text(
        from,
        to,
        Kind::Decal,
        &mesh_out,
        std::slice::from_ref(material),
        &[],
        &mut files,
    )?;
    files.push((prp_path.to_owned(), prp.into_bytes()));
    Ok(files)
}

/// The prp text of `kind` for `mesh` drawn with `materials` (in the mesh's
/// material order), copied from the kind's shipped template, with `tints`
/// as its per-instance colours. Adds the textures it names to `files`.
fn prp_text(
    from: &dyn Source,
    to: &Install,
    kind: Kind,
    mesh: &str,
    materials: &[Material],
    tints: &[[f32; 3]],
    files: &mut Files,
) -> Result<String> {
    // A prp names textures by their source; the game reads `<stem>_0.dds`.
    let mut texture = |named: &String| {
        let source = texture_source(named);
        files.extend(texture_files(from, &source));
        from.place(&source)
    };
    let template = String::from_utf8_lossy(&to.read(kind.template())?).into_owned();
    let mut prp = set(&template, "VertexFormat", &kind.layout().to_string());
    prp = set(&prp, "MeshFileName", mesh);

    // One `<Material>` per material. The template's block runs from its
    // `<Material>` to the `</>` closing `<Materials>`, the last one before
    // `<Colors>`.
    let (a, colors) = (
        prp.find("<Material>").context("template has no Material")?,
        prp.find("<Colors>").context("template has no Colors")?,
    );
    let end = prp[..colors].rfind("</>").context("template Materials")?;
    let block = prp[a..end].to_owned();
    let mut blocks = String::new();
    let on = |b: bool| if b { "1" } else { "0" };
    for m in materials {
        let mut out = block.clone();
        let diffuse = m.diffuse.as_ref().map(&mut texture);
        let normal = m.normal.as_ref().map(&mut texture);
        if kind == Kind::Building {
            // SimplePBR reads the cfg's own fields.
            let metallic = m.metallic.as_ref().map(&mut texture);
            out = set(&out, "DIFFUSE_ENABLED", on(diffuse.is_some()));
            out = set(&out, "cModelDiffTex", diffuse.as_deref().unwrap_or(""));
            out = set(&out, "NORMAL_ENABLED", on(normal.is_some()));
            if let Some(n) = &normal {
                out = set(&out, "cModelNormalTex", n);
            }
            out = set(&out, "METALLIC_TEX_ENABLED", on(metallic.is_some()));
            if let Some(t) = &metallic {
                out = set(&out, "cModelMetallicTex", t);
            }
            // The template is a neon sign.
            out = set(&out, "GLOW_ENABLED", "0");
            // Decal and see-through materials inside a building part blend
            // over what's under them instead of drawing a solid sheet.
            let blend = matches!(m.alpha, Alpha::Decal | Alpha::Blend(_) | Alpha::Hidden);
            out = set(&out, "FORCE_ALPHA_BLEND", on(blend));
        } else {
            let diffuse = diffuse.context("a material has no diffuse texture")?;
            out = set(&out, "cPropDiffuseTex", &diffuse);
            out = set(&out, "cPropNormalTex", normal.as_deref().unwrap_or(""));
            out = set(&out, "NORMAL_MAP_ENABLED", on(normal.is_some()));
        }
        for (c, v) in ["r", "g", "b"].iter().zip(m.color) {
            out = set(&out, &format!("cDiffuseColor.{c}"), &v.to_string());
        }
        out = set(&out, "DOUBLE_SIDED", on(m.double_sided));
        if let Alpha::Cutout(r) = m.alpha {
            out = set(&out, "cAlphaRef", &r.to_string());
        }
        let tint = m.terrain_tint > 0.;
        out = set(&out, "cUseTerrainTinting", on(tint));
        if tint {
            out = set(&out, "cTerrainTintIntensity", &m.terrain_tint.to_string());
        }
        // The rock template mixes 1800's moderate mountain texture into the
        // rock. A ported rock should keep its own look.
        out = set(&out, "ENABLE_TERRAIN_MATERIAL", "0");
        // The decal template lies on water (ice floes); ours lie on land.
        out = set(&out, "cAlignOnWater", "0");
        blocks.push_str(&out);
    }
    prp.replace_range(a..end, &blocks);

    // The source's per-instance tints. The template's would turn the model
    // green.
    let colors = prp.find("<Colors>").context("template has no Colors")?;
    let mut text = format!("<Colors><Count>{}</>\n", tints.len());
    for (k, tint) in tints.iter().enumerate() {
        for (c, v) in ["r", "g", "b"].iter().zip(tint) {
            text.push_str(&format!("<Color{k}.{c}>{v}</>\n"));
        }
    }
    text.push_str("</>\n</>\n");
    prp.replace_range(colors.., &text);
    Ok(prp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn material(alpha: Alpha, ground_shaded: bool) -> Material {
        Material {
            diffuse: None,
            normal: None,
            metallic: None,
            color: [1.; 3],
            double_sided: false,
            alpha,
            ground_shaded,
            terrain_tint: 0.,
        }
    }

    // What a piece becomes follows its materials: any cutout makes a
    // plant, ground shading alone a rock, all-decal a decal, else a
    // building; hidden volumes are ignored when deciding.
    #[test]
    fn materials_pick_the_prp_kind() {
        let kind = |m: &[Material]| Kind::of(m);
        assert_eq!(kind(&[material(Alpha::Cutout(0.5), false)]), Kind::Plant);
        assert_eq!(kind(&[material(Alpha::Opaque, true)]), Kind::Rock);
        assert_eq!(
            kind(&[
                material(Alpha::Opaque, true),
                material(Alpha::Hidden, false)
            ]),
            Kind::Rock
        );
        assert_eq!(kind(&[material(Alpha::Decal, false)]), Kind::Decal);
        assert_eq!(kind(&[material(Alpha::Opaque, false)]), Kind::Building);
        assert_eq!(
            Kind::Plant.layout().to_string(),
            "P4h_N4b_G4b_B4b_T2h_C4b_C4b"
        );
        assert_eq!(Kind::Rock.layout().to_string(), "P4h_N4b_G4b_B4b_T2h");
    }
}
