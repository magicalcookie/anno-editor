// The Model workspace's hierarchy pane in GPUI's test platform, on the 117 install
// (see `tests::open`).

use super::Field;
use super::hierarchy::{Row, visible_rows};
use super::tests::{DOG, INSTITUTION, WORKER, open};
use anno_formats::model::Model;
use anno_formats::xml::{Doc, Id};
use gpui_editor_kit::field::{Filter, TextEdit};

/// What a click in the Hierarchy pane does to the keys: they go to it.
fn focus_hierarchy(w: &mut super::tests::Window) {
    w.update(|ed, _, cx| {
        let i = ed.dock_index();
        ed.docks[i].focus(crate::editor::Pane::ModelHierarchy);
        cx.notify();
    });
}

// Typing in the search narrows the tree to the dog's head joints, under
// their part, whatever was open; Escape brings the whole tree back.
#[test]
fn search_finds_joints_under_their_part() {
    let Some((app, mut w, _config)) = open(DOG) else {
        return;
    };
    let all = w.read(|ed, _| visible_rows(ed.model.as_ref().unwrap()).len());
    // What a click on the search field does.
    w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        m.typing = Some(TextEdit::new(Field::HierarchyFilter, "", Filter::Text));
        cx.notify();
    });
    w.simulate_keystrokes("h e a d");
    app.run_until_parked();
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.hierarchy.filter, "head");
        let rows = visible_rows(m);
        let head = |i: usize| {
            let label = super::hierarchy::row_label(m, rows[i].node).0;
            label.to_lowercase().contains("head")
        };
        let joints: Vec<usize> = (0..rows.len())
            .filter(|&i| matches!(rows[i].node, Row::Joint(..)))
            .collect();
        assert!(joints.iter().any(|&i| head(i)), "no head joint found");
        // A joint that does not match is on the way to one that does.
        for &i in &joints {
            let deeper = rows.get(i + 1).is_some_and(|r| r.depth > rows[i].depth);
            assert!(
                head(i) || deeper,
                "row {i} neither matches nor leads to one"
            );
        }
        assert!(joints.len() < 40, "the whole skeleton shows");
        // Every joint row sits under its part's Joints heading.
        let first = rows
            .iter()
            .position(|r| matches!(r.node, Row::Joint(..)))
            .unwrap();
        assert!(
            rows[..first]
                .iter()
                .any(|r| matches!(r.node, Row::Joints(_)))
        );
    });
    w.simulate_keystrokes("escape");
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert!(m.hierarchy.filter.is_empty() && m.typing.is_none());
        assert_eq!(visible_rows(m).len(), all);
    });
}

// Down moves the selection to the next entry, stepping over a list's
// header; F2 renames it in place and Enter commits one undo step.
#[test]
fn arrows_walk_the_entries_and_f2_renames() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    let (first, second, last_part, first_prop) = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let parts = m.entries("Models");
        let props = m.entries("Files");
        (parts[0], parts[1], *parts.last().unwrap(), props[0])
    });
    focus_hierarchy(&mut w);
    w.update(|ed, _, cx| {
        ed.model.as_mut().unwrap().select(Some(first));
        cx.notify();
    });
    w.simulate_keystrokes("down");
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().selected, Some(second)));
    // From the last mesh, down crosses the Props header to the first prop.
    w.update(|ed, _, cx| {
        ed.model.as_mut().unwrap().select(Some(last_part));
        cx.notify();
    });
    w.simulate_keystrokes("down");
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().selected, Some(first_prop)));

    let before = w.read(|ed, _| ed.model.as_ref().unwrap().doc.write());
    w.simulate_keystrokes("f2");
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let name = m.doc.child(first_prop, "Name").expect("a prop has a Name");
        assert_eq!(m.typing.as_ref().map(|t| t.field), Some(Field::Value(name)));
    });
    w.simulate_keystrokes("ctrl-a w e l l enter");
    app.run_until_parked();
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.label(first_prop), "well");
        assert!(m.typing.is_none());
    });
    w.simulate_keystrokes("ctrl-z");
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().doc.write(), before));
}

/// Pairs of (referring element, the mesh entry it names).
type Refs = Vec<(Id, Id)>;

/// Every reference by place to a mesh, as the mesh it names: each clip
/// that plays a part's animation, each entry hung on a part's joint.
fn part_refs(m: &super::ModelEditor) -> (Refs, Refs) {
    use anno_formats::model::Play;
    let part = |k: u32| m.model.parts[k as usize].element;
    let clips = (m.model.sequences.iter().flat_map(|s| &s.clips))
        .filter_map(|c| match c.what {
            Play::Animation { model, .. } => Some((c.element, part(model))),
            _ => None,
        })
        .collect();
    let m2 = &m.model;
    let links = (m2.parts.iter().map(|p| (p.element, p.rig)))
        .chain(m2.props.iter().map(|p| (p.element, p.rig)))
        .chain(m2.particles.iter().map(|p| (p.element, p.rig)))
        .filter_map(|(e, r)| Some((e, part(r.link?.model))))
        .collect();
    (clips, links)
}

// Dragging the worker's body mesh to the end of its list moves it there,
// and its sequences and the props on its hands still name the body: their
// ModelIDs follow the new order. One undo puts the file back.
#[test]
fn reordering_meshes_keeps_tracks_and_links_on_them() {
    let Some((_app, mut w, _config)) = open(WORKER) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let parts = m.entries("Models");
        assert!(parts.len() > 1, "the worker has one mesh");
        let before = m.doc.write();
        let (clips, links) = part_refs(m);
        assert!(!clips.is_empty() && !links.is_empty());
        let list = m.doc.parent(parts[0]).unwrap();
        m.drop_entry(parts[0], Row::List(list));
        assert_eq!(m.entries("Models").last(), Some(&parts[0]));
        assert_eq!(part_refs(m), (clips, links), "a reference lost its mesh");
        // The saved file says the same.
        let again = Model::from_doc(&Doc::parse(&m.doc.write())).unwrap();
        assert_eq!(again.parts.len(), parts.len());
        assert!(m.undo(false));
        assert_eq!(m.doc.write(), before);
        // Dropped on the next row down, it lands under it.
        m.drop_entry(parts[0], Row::Entry(parts[1]));
        assert_eq!(&m.entries("Models")[..2], &[parts[1], parts[0]]);
    });
}

// Dropping a prop on one of the dog's joints hangs it there; another drop
// on a mesh row of another list does nothing.
#[test]
fn dropping_on_a_joint_hangs_the_entry() {
    let Some((_app, mut w, _config)) = open(WORKER) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let prop = m.model.props[0].element;
        let part = (m.model.parts.iter())
            .find(|p| p.mesh.as_ref().is_some_and(|r| r.skeleton.is_some()))
            .map(|p| p.element)
            .expect("a skinned part");
        let before = m.doc.write();
        m.drop_entry(prop, Row::Entry(part));
        assert_eq!(m.doc.write(), before, "a prop does not go into Meshes");
        m.drop_entry(prop, Row::Joint(part, 1));
        assert_eq!(m.rig(prop).and_then(|r| r.link), m.link_to(part, 1));
        assert!(m.undo(false));
        assert_eq!(m.doc.write(), before);
    });
}

// Enter on a selected prop opens its own cfg in the workspace.
#[test]
fn enter_opens_the_selected_prop() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    let cfg = w.update(|ed, _, cx| {
        // A game original opens unsaved; saving it lets another cfg open.
        ed.save();
        let m = ed.model.as_mut().unwrap();
        let prop = m.model.props[0].element;
        m.select(Some(prop));
        cx.notify();
        m.prop_cfg(prop).unwrap()
    });
    app.run_until_parked();
    focus_hierarchy(&mut w);
    w.simulate_keystrokes("enter");
    app.run_until_parked();
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().path, cfg));
}
