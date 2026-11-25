// Reads 2070's model cfgs into the neutral model (core::neutral), so other
// games can bring them in as props. Each mesh part becomes a piece with
// its placement and the first material's diffuse texture; 2070's normal,
// mask and dye maps, animations and effects are not carried. 2070 has no
// writer: its models are read only.

use anno_core::neutral::{Material, Model, Piece, Shape};
use anno_formats::model::{self, Alpha, Transform};
use anno_formats::vfs::Install;
use anyhow::{Context, Result, bail};

/// Reads `source`, a 2070 model cfg, as one piece per mesh part.
pub fn read(install: &Install, source: &str) -> Result<Model> {
    let bytes = install.read(source)?;
    // Parsed, not loaded: each part keeps the mesh the cfg names, whose
    // shipped `_lod<N>` levels port beside it.
    let parts = model::Model::parse(&bytes)
        .with_context(|| source.to_owned())?
        .parts;
    let name = source.rsplit('/').next().unwrap_or(source);
    let stem = source.strip_suffix(".cfg").unwrap_or(source);
    let mut out = Model::default();
    if parts.is_empty() {
        out.left_out.push(format!("{name}: no mesh parts"));
    }
    for (k, part) in parts.iter().enumerate() {
        // The other games' writers copy meshes byte for byte, and neither
        // game reads a big-endian one.
        if install.read(&part.rdm).is_ok_and(|b| b.get(3) == Some(&0)) {
            bail!(
                "mesh {}: big-endian, which 117 and 1800 do not read",
                part.rdm
            );
        }
        out.pieces.push(Piece {
            name: match parts.len() {
                1 => stem.to_owned(),
                _ => format!("{stem}_part{k}"),
            },
            label: format!(
                "{name} {}",
                part.rdm.rsplit('/').next().unwrap_or(&part.rdm)
            ),
            at: Transform::IDENTITY,
            shape: Shape::Mesh {
                rdm: part.rdm.clone(),
                transform: part.transform,
            },
            materials: vec![Material {
                diffuse: part.materials.first().and_then(|m| m.diffuse.clone()),
                normal: None,
                metallic: None,
                color: [1.; 3],
                double_sided: false,
                // What the diffuse alpha means depends on the 2070 shader:
                // drawn opaque, nothing vanishes.
                alpha: Alpha::Opaque,
                ground_shaded: false,
                terrain_tint: 0.,
            }],
            tints: Vec::new(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Needs Anno 2070: a building with several parts reads as one piece per
    // part, named after the cfg with the part's number, each placed by its
    // cfg and naming its mesh and shipped diffuse texture.
    #[test]
    fn a_model_reads_as_one_piece_per_part() {
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap();
        let cfg = "addondata/graphics/buildings/others/public/underwater_markethouse_small_others/underwater_markethouse_small_others.cfg";
        let parts = model::Model::parse(&install.read(cfg).unwrap())
            .unwrap()
            .parts;
        assert!(parts.len() >= 2, "{parts:?}");
        let model = read(&install, cfg).unwrap();
        assert_eq!(model.pieces.len(), parts.len());
        assert!(model.left_out.is_empty() && model.effects.is_empty());
        for (k, (piece, part)) in model.pieces.iter().zip(&parts).enumerate() {
            assert_eq!(
                piece.name,
                format!("{}_part{k}", cfg.trim_end_matches(".cfg"))
            );
            let Shape::Mesh { rdm, transform } = &piece.shape else {
                panic!("{}: not a mesh", piece.name);
            };
            assert_eq!(rdm, &part.rdm);
            assert_eq!(transform, &part.transform);
            let diffuse = piece.materials[0].diffuse.as_deref().unwrap();
            assert!(
                diffuse.ends_with("_0.dds") && install.exists(diffuse),
                "{diffuse}"
            );
        }
    }

    // Needs Anno 2070: a model whose mesh is big-endian does not port, as
    // neither target game reads one.
    #[test]
    fn big_endian_meshes_do_not_port() {
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap();
        let cfg = "data/graphics/buildings/tycoons/props/submarine/submarine.cfg";
        let e = read(&install, cfg).unwrap_err();
        assert!(format!("{e:#}").contains("big-endian"), "{e:#}");
    }
}
