// New entries made from shipped ones, on the 117 install (see
// `tests::open`): each lands at the origin, selected, as one undo step, and
// the cfg saves into the project as one the game's reader takes.

use super::ModelEditor;
use super::tests::{INSTITUTION, open};
use anno_formats::model::{Model, Transform, mesh_path, texture_path};
use anno_formats::xml::{Doc, Id};

/// The texts of `entry`'s child leaf `name`.
fn field(m: &ModelEditor, entry: Id, name: &str) -> String {
    (m.doc.child(entry, name)).map_or_else(String::new, |c| m.doc.text(c).to_owned())
}

/// The ConfigTypes of `entry`'s transformer configs.
fn transforms(m: &ModelEditor, entry: Id) -> Vec<String> {
    let doc = &m.doc;
    (doc.child(entry, "Transformer").into_iter())
        .flat_map(|t| doc.children(t))
        .filter_map(|c| doc.child(c, "ConfigType").map(|k| doc.text(k).to_owned()))
        .collect()
}

/// `entry`'s first material config.
fn material(m: &ModelEditor, entry: Id) -> Id {
    let list = m.doc.child(entry, "Materials").expect("a Materials list");
    m.doc.children(list).next().expect("a material")
}

// A point light, a spot, a cloth, a mesh with a shipped cfg beside it and
// one without, and a decal: each in its list, selected, on its placement
// alone at the origin, one undo taking it back. Saved, the cfg reads back
// with all six.
#[test]
fn added_entries_stand_at_the_origin_and_save_as_the_game_reads() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    // A plain grey ground no cfg in its folder or the one above draws.
    const FAR_MESH: &str = "data/base/graphics/library/systems/polygon_base/rdm/ground_02x02.rdm";
    const DECAL: &str =
        "data/base/graphics/library/environment/props/decals/beach/maps/beach_decal_02_diff_0.dds";
    let shipped = w.read(|ed, _| ed.model.as_ref().unwrap().model.clone());
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let near_mesh = m.model.parts[0].rdm.clone();
        type Add = Box<dyn Fn(&mut ModelEditor) -> anyhow::Result<Id>>;
        let adds: [(&str, Add); 6] = [
            ("Lights", Box::new(|m| m.add_light(false))),
            ("Lights", Box::new(|m| m.add_light(true))),
            ("Clothes", Box::new(ModelEditor::add_cloth)),
            ("Models", Box::new(move |m| m.add_mesh(&near_mesh))),
            ("Models", Box::new(|m| m.add_mesh(FAR_MESH))),
            ("Decals", Box::new(|m| m.add_decal(DECAL))),
        ];
        for (list, add) in &adds {
            let before = m.entries(list).len();
            let entry = add(m).unwrap_or_else(|e| panic!("{list}: {e:#}"));
            assert_eq!(m.list_of(entry), Some(*list));
            assert_eq!(m.selected, Some(entry));
            assert_eq!(m.entries(list).len(), before + 1);
            assert_eq!(transforms(m, entry), ["ORIENTATION_TRANSFORM"], "{list}");
            assert_eq!(m.placement(entry), Some(Transform::IDENTITY), "{list}");
            assert!(m.undo(false), "{list}: no undo step");
            assert_eq!(m.entries(list).len(), before, "{list}: undo left it");
            assert!(m.undo(true));
        }
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    let install = w.read(|ed, _| ed.install.clone().unwrap());
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let saved = ed
            .project
            .read_file(INSTITUTION)
            .expect("the cfg is in the mod");
        let again = Model::from_doc(&Doc::parse(&saved)).unwrap();
        assert_eq!(again.lights.len(), shipped.lights.len() + 2);
        assert_eq!(again.parts.len(), shipped.parts.len() + 2);
        assert_eq!(again.decals.len(), shipped.decals.len() + 1);

        let lights = m.entries("Lights");
        let [point, spot] = &lights[lights.len() - 2..] else {
            unreachable!()
        };
        assert_eq!(
            (field(m, *point, "Type"), field(m, *spot, "Type")),
            ("0".into(), "2".into())
        );

        // The mesh beside its shipped cfg keeps that cfg's materials; the
        // far one gets the white default on each of its slots, in its own
        // vertex layout.
        let parts = &again.parts[again.parts.len() - 2..];
        assert_eq!(parts[0].rdm, shipped.parts[0].rdm);
        assert_eq!(parts[0].materials.len(), shipped.parts[0].materials.len());
        assert_eq!(parts[1].rdm, mesh_path(FAR_MESH));
        let rdm = anno_formats::rdm::Rdm::parse(&install.read(FAR_MESH).unwrap()).unwrap();
        let slots = rdm.groups.iter().map(|g| g.material + 1).max().unwrap() as usize;
        assert_eq!(parts[1].materials.len(), slots);
        let models = m.entries("Models");
        let far = material(m, *models.last().unwrap());
        assert_eq!(field(m, far, "VertexFormat"), rdm.format.to_string());
        assert_eq!(field(m, far, "cDiffuseColor.g"), "1");
        assert!(
            field(m, far, "cModelDiffTex").ends_with("default_model_diffuse.png"),
            "{}",
            field(m, far, "cModelDiffTex")
        );

        // The decal shows the picked texture, its normal map on only where
        // one lies beside it.
        let decal = again.decals.last().unwrap();
        let diffuse = decal.material.as_ref().and_then(|m| m.diffuse.clone());
        assert_eq!(diffuse.as_deref(), Some(DECAL));
        let mat = material(m, *m.entries("Decals").last().unwrap());
        let norm = DECAL.replace("_diff_0.dds", "_norm.psd");
        let has_norm = install.exists(&texture_path(&norm));
        assert_eq!(field(m, mat, "NORMAL_ENABLED") == "1", has_norm);
        if has_norm {
            assert_eq!(field(m, mat, "cModelNormalTex"), norm);
        }
    });
}

// The inspector's Add mesh… picks through the picker: Enter on a mesh adds
// it, selected.
#[test]
fn the_picker_adds_a_mesh() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    let count = w.read(|ed, _| ed.model.as_ref().unwrap().model.parts.len());
    w.update(|ed, _, cx| {
        ed.open_model_picker(super::picker::Want::Add(super::picker::Kind::Mesh), cx)
    });
    let first = w.read(|ed, _| ed.model_picker.as_ref().unwrap().hits.first().cloned());
    let first = first.expect("meshes beside the model");
    assert!(first.ends_with(".rdm"), "{first}");
    w.simulate_keystrokes("enter");
    app.run_until_parked();
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.model.parts.len(), count + 1, "{first} did not add");
        let s = m.selected.expect("the new mesh is selected");
        assert_eq!(m.list_of(s), Some("Models"));
    });
}
