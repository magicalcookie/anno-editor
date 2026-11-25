// The Model workspace in GPUI's test platform: open a shipped building,
// edit it as a modder would, save it into the project. They need the game
// (ANNO117_DIR).

use super::playback::Source;
use crate::editor::{Editor, Leave, Workspace};
use anno_formats::model::{Model, Play, Sound};
use anno_formats::xml::Doc;
use gpui::{TestApp, TestAppWindow};
use gpui_editor_kit::math;

pub(super) const INSTITUTION: &str =
    "data/base/graphics/roman/buildings/institution/institution_01/institution_01.cfg";

pub(super) type Window = TestAppWindow<Editor>;

/// The editor with `path` open in the Model workspace, on the 117 install.
pub(super) fn open(path: &str) -> Option<(TestApp, Window, tempfile::TempDir)> {
    open_in("ANNO117_DIR", &anno117::MODULE, path)
}

/// The same on the 2070 install (ANNO2070_DIR).
pub(super) fn open_2070(path: &str) -> Option<(TestApp, Window, tempfile::TempDir)> {
    open_in("ANNO2070_DIR", &anno2070::MODULE, path)
}

/// [`open`] on the install `var` names, of `module`'s game.
pub(super) fn open_in(
    var: &str,
    module: &'static dyn anno_core::game::Module,
    path: &str,
) -> Option<(TestApp, Window, tempfile::TempDir)> {
    let game = anno_formats::vfs::install_dir(var)?;
    let mut app = TestApp::new();
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_path_buf();
    let mut w = app.open_window(|window, cx| {
        Editor::new(
            crate::project::Project::load(&dir, module).unwrap(),
            dir,
            Some(game),
            None,
            window,
            cx,
        )
    });
    app.run_until_parked();
    let path = path.to_owned();
    w.update(|ed, _, cx| ed.open_model(&path, cx));
    app.run_until_parked();
    Some((app, w, config))
}

// A prop moved with the inspector's step button and a part duplicated with
// Ctrl D save into the project as a cfg the game reads with both edits;
// the rest of the file is byte for byte the game's.
#[test]
fn edits_save_into_the_project_as_the_games_cfg() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    let (prop, part, shipped) = w.read(|ed, _| {
        assert_eq!(ed.workspace, Workspace::Model);
        let m = ed.model.as_ref().expect("the model opened");
        assert!(!m.pieces.is_empty(), "nothing to draw");
        assert!(!m.dirty() && !m.in_project, "an untouched game original");
        let shipped = ed.install.as_ref().unwrap().read(INSTITUTION).unwrap();
        (m.model.props[0].element, m.model.parts[0].element, shipped)
    });
    let before = w.read(|ed, _| ed.model.as_ref().unwrap().placement(prop).unwrap());
    w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        m.select(Some(prop));
        let mut t = before;
        t.position[0] += 1.5;
        m.set_placement(prop, t, true);
        m.select(Some(part));
        cx.notify();
    });
    w.simulate_keystrokes("ctrl-d");
    app.run_until_parked();
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert!(!m.dirty() && m.in_project, "saved");
        let saved = ed
            .project
            .read_file(INSTITUTION)
            .expect("the cfg is in the mod");
        let again = Model::from_doc(&Doc::parse(&saved)).unwrap();
        let shipped = Model::from_doc(&Doc::parse(&shipped)).unwrap();
        assert_eq!(again.parts.len(), shipped.parts.len() + 1);
        assert_eq!(again.parts[1].rdm, shipped.parts[0].rdm);
        let moved = again.props[0].transform.position[0] - shipped.props[0].transform.position[0];
        assert!((moved - 1.5).abs() < 1e-4, "moved {moved}");
    });
    // Undo takes back the duplicate, then the move.
    w.simulate_keystrokes("ctrl-z ctrl-z");
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(
            m.doc.write(),
            shipped,
            "two undos are the game's file again"
        );
    });
}

// The hierarchy lists what the cfg holds; Delete removes the selection and
// Escape clears it.
#[test]
fn delete_and_escape_act_on_the_selection() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    let (part, count) = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        (m.model.parts[0].element, m.model.parts.len())
    });
    w.update(|ed, _, cx| {
        ed.model.as_mut().unwrap().select(Some(part));
        cx.notify();
    });
    w.simulate_keystrokes("delete");
    app.run_until_parked();
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.model.parts.len(), count - 1);
        assert_eq!(m.selected, None);
    });
    w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let first = m.model.parts[0].element;
        m.select(Some(first));
        cx.notify();
    });
    w.simulate_keystrokes("escape");
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().selected, None));
}

pub(super) const DOG: &str = "data/base/graphics/library/wildlife/dog/dog-big_01.cfg";
pub(super) const WORKER: &str =
    "data/base/graphics/celtic/units/inhabitants/production/worker_lean_f_01/worker_lean_f_01.cfg";
pub(super) const FIREFLIES: &str =
    "data/cdlc02/graphics/library/effects/animals/firefly_swarm_01.cfg";

// The dog opens on its first sequence; playing it skins the body away from
// its bind pose, and the selected part shows its bones.
#[test]
fn a_sequence_skins_the_model_and_shows_its_bones() {
    let Some((_app, mut w, _config)) = open(DOG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().expect("the dog opened");
        assert!(!m.model.sequences.is_empty());
        assert_eq!(m.playback.source, super::playback::Source::Sequence(0));
        assert!(
            (m.clip_length() - 5.333).abs() < 0.01,
            "{}",
            m.clip_length()
        );
        // Named from the game's own enums.
        assert_eq!(m.names.name(100691457), "Neutral Idle 1");
        assert_eq!(m.label(m.model.sequences[0].element), "Neutral Idle 1");
        let body = m.model.parts[0].element;
        m.select(Some(body));
        m.advance(1.);
        let skinned = m.view.posed.iter().filter(|p| p.mesh.is_some()).count();
        assert!(skinned > 0, "nothing skinned");
        let rest = m.pieces.iter().find(|p| p.element == Some(body)).unwrap();
        let posed = m.view.posed.iter().find_map(|p| p.mesh.clone()).unwrap();
        assert_ne!(
            posed.bounds(),
            rest.mesh.bounds(),
            "the pose is the bind pose"
        );
        assert!(m.view.overlay.len() > 10, "{} bones", m.view.overlay.len());
    });
}

// The worker's hand props hang on its hand joints: playing moves them off
// the placement the cfg writes in joint space.
#[test]
fn bone_linked_props_follow_their_joint() {
    let Some((_app, mut w, _config)) = open(WORKER) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().expect("the worker opened");
        let linked: Vec<_> = (m.model.props.iter())
            .filter(|p| p.rig.link.is_some())
            .map(|p| p.element)
            .collect();
        assert!(!linked.is_empty(), "no prop on a joint");
        m.play(super::playback::Source::Sequence(0));
        m.advance(0.5);
        let moved = (m.pieces.iter().zip(&m.view.posed))
            .filter(|(p, _)| p.element.is_some_and(|e| linked.contains(&e)))
            .any(|(p, q)| p.local != q.local);
        assert!(moved, "no linked prop moved onto its joint");
    });
}

// A particle effect plays as sprites while nothing else does.
#[test]
fn particles_play_as_sprites() {
    let Some((_app, mut w, _config)) = open(FIREFLIES) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().expect("the fireflies opened");
        assert!(!m.model.particles.is_empty());
        m.advance(2.);
        assert!(!m.view.sprites.is_empty(), "{:?}", m.timeline_notes());
    });
}

// An effect a sequence starts runs only while its clip does: the axe
// infantry's two effects stay out of a sequence that starts neither (they
// all ran there before), and show inside the clip of one that starts one.
#[test]
fn effects_run_only_in_the_sequences_that_start_them() {
    const AXE: &str = "data/base/graphics/celtic/units/army/axe_infantry_01/axe_infantry_01.cfg";
    let Some((_app, mut w, _config)) = open(AXE) else {
        return;
    };
    w.update(|ed, _, _| {
        use anno_formats::model::Play;
        let m = ed.model.as_mut().expect("the axe infantry opened");
        let starts = |s: &anno_formats::model::Sequence| {
            (s.clips.iter())
                .find(|c| matches!(c.what, Play::Particle(_)))
                .copied()
        };
        let quiet = (m.model.sequences.iter())
            .position(|s| starts(s).is_none())
            .expect("a sequence that starts no effect");
        let (loud, clip) = (m.model.sequences.iter().enumerate())
            .find_map(|(k, s)| starts(s).map(|c| (k, c)))
            .expect("one that starts one");
        m.play(super::playback::Source::Sequence(quiet));
        for t in [0.1, 0.5, 1.] {
            m.playback.time = t;
            m.pose();
            assert!(m.view.sprites.is_empty(), "an effect ran at {t} s");
        }
        m.play(super::playback::Source::Sequence(loud));
        let shown = (1..10).any(|k| {
            m.playback.time = clip.start + (clip.end - clip.start) * k as f32 / 10.;
            m.pose();
            !m.view.sprites.is_empty()
        });
        assert!(shown, "nothing drew inside {clip:?}");
    });
}

// Dragging a clip retimes it in the cfg; one undo takes the drag back.
#[test]
fn a_retimed_clip_saves_its_new_times() {
    let Some((_app, mut w, _config)) = open(DOG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let clip = m.model.sequences[0].clips[0].element;
        let start = m.doc.child(clip, "StartTime").unwrap();
        let end = m.doc.child(clip, "EndTime").unwrap();
        let before = m.doc.write();
        m.retime(start, end, "250", "5583", true);
        m.retime(start, end, "500", "5833", false);
        assert!((m.model.sequences[0].clips[0].start - 0.5).abs() < 1e-6);
        let again = Model::from_doc(&Doc::parse(&m.doc.write())).unwrap();
        assert!((again.sequences[0].clips[0].end - 5.833).abs() < 1e-6);
        assert!(m.undo(false));
        assert_eq!(m.doc.write(), before, "one undo for the whole drag");
    });
}

// Taking the worker's basket off its hand and hanging it back keeps it
// where it shows in the bind pose, and the link reads back as it was.
#[test]
fn attaching_keeps_an_entry_where_it_shows() {
    let Some((_app, mut w, _config)) = open(WORKER) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let (prop, link) = (m.model.props.iter())
            .find_map(|p| Some((p.element, p.rig.link?)))
            .expect("a prop on a joint");
        let shown = |m: &super::ModelEditor| {
            let i = m
                .pieces
                .iter()
                .position(|p| p.element == Some(prop))
                .unwrap();
            let local = m.view.posed.get(i).map_or(m.pieces[i].local, |p| p.local);
            gpui_editor_kit::math::translation(&local)
        };
        m.play(super::playback::Source::Still);
        let before = shown(m);
        m.attach(prop, None);
        assert_eq!(m.rig(prop).unwrap().link, None);
        let off = shown(m);
        m.attach(prop, Some(link));
        assert_eq!(m.rig(prop).unwrap().link, Some(link));
        let back = shown(m);
        for (a, b) in [(before, off), (before, back)] {
            let d = (0..3).map(|k| (a[k] - b[k]).abs()).fold(0f32, f32::max);
            assert!(d < 1e-3, "moved {d} m: {a:?} → {b:?}");
        }
    });
}

// A shipped effect made larger saves beside the cfg as a file the game
// reads, larger; undo takes the edit back. A new effect lands in the cfg
// and the mod with its own file.
#[test]
fn effects_edit_and_save_into_the_mod() {
    let Some((app, mut w, _config)) = open(FIREFLIES) else {
        return;
    };
    let (path, before) = w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let p = m.model.particles[0].clone();
        m.select(Some(p.element));
        let before = m.effect(&p.file).expect("the fireflies read");
        let scale = |r: &anno_formats::rdp::Rdp| r.scales.iter().map(|s| s[0]).fold(0f32, f32::max);
        m.change_effect(&p.file, super::effect::Change::Size(1.25))
            .unwrap();
        let after = m.effect(&p.file).unwrap();
        assert!((scale(&after) / scale(&before) - 1.25).abs() < 0.02);
        (p.file, before)
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.update(|ed, _, _| {
        let saved = ed
            .project
            .read_file(&path)
            .expect("the effect is in the mod");
        let saved = anno_formats::rdp::Rdp::parse(&saved).unwrap();
        assert_eq!(saved.starts.len(), before.starts.len());
        let m = ed.model.as_mut().unwrap();
        assert!(m.undo(false));
        assert!(m.effects.is_empty(), "undo kept the edit");
        let mut emitter = super::effect::Emitter::default();
        emitter.effect.texture = "data/maps/spark.psd".into();
        let entry = m.new_effect(emitter).expect("an effect made");
        let file = m
            .model
            .particles
            .iter()
            .find(|p| p.element == entry)
            .unwrap()
            .file
            .clone();
        assert!(file.ends_with(".rdp") && file.contains("/rdp/"), "{file}");
        assert!(m.effect(&file).is_some());
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let file = &m.model.particles.last().unwrap().file;
        let bytes = ed
            .project
            .read_file(file)
            .expect("the new effect is in the mod");
        assert!(anno_formats::rdp::Rdp::parse(&bytes).is_ok());
        // The size edit, undone before this save, left the mod again.
        assert!(
            ed.project.read_file(&path).is_none(),
            "{path} stayed in the mod"
        );
        // Its emitter is in the project's settings, on disk too, so it
        // stays editable once the editor closes.
        assert!(ed.project.effects.iter().any(|e| &e.path == file));
        let again = crate::project::Project::open(&ed.project.folder(), &anno117::MODULE).unwrap();
        let kept = again
            .effects
            .iter()
            .find(|e| &e.path == file)
            .expect("kept");
        assert_eq!(kept.emitter.effect.texture, "data/maps/spark.psd");
        assert_eq!(
            kept.emitter.launch,
            super::effect::Emitter::default().launch
        );
    });
}

// A new effect's emitter edits after it is made and bakes again: a faster
// rate gives more particles, a flipbook sets the entry's grid, and undo
// takes back the emitter with the effect.
#[test]
fn a_new_effect_rebakes_from_its_emitter() {
    use super::effect::Knob;
    let Some((_app, mut w, _config)) = open(FIREFLIES) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let entry = m.new_effect(super::effect::Emitter::default()).unwrap();
        let file = m
            .model
            .particles
            .iter()
            .find(|p| p.element == entry)
            .unwrap()
            .file
            .clone();
        let count = |m: &mut super::ModelEditor| m.effect(&file).unwrap().starts.len();
        let before = count(m);
        let mut e = m.emitter(&file).unwrap();
        for knob in [Knob::Rate(false), Knob::Rate(true)] {
            let faster = knob.get(&e) * 2.;
            knob.set(&mut e, faster);
        }
        Knob::Book(0).set(&mut e, 8.);
        Knob::Book(1).set(&mut e, 8.);
        m.rebake(&file, e).unwrap();
        assert_eq!(count(m), before * 2);
        let p = m
            .model
            .particles
            .iter()
            .find(|p| p.element == entry)
            .unwrap();
        assert_eq!(p.sub_images, Some([8, 8, 64]), "the entry plays the grid");
        m.advance(0.5);
        assert!(!m.view.sprites.is_empty());
        assert!(m.undo(false));
        assert_eq!(count(m), before);
        assert_eq!(
            m.emitter(&file).unwrap().flipbook,
            None,
            "undo took the emitter back"
        );
    });
}

const CYBERNETICS: &str = "addondata/graphics/buildings/techs/production/cybernetic_factory_techs/cybernetic_factory_techs.cfg";

// A 2070 factory's steam plays from its `.efc` emitters. Copied for this
// model and given a faster rate, it bakes more particles and saves beside
// the cfg as an `.efc` the game reads, with the cfg pointing at it; undo
// takes both back.
#[test]
fn a_2070_effect_plays_edits_and_saves_as_efc() {
    use super::effect::{Emitter, Knob};
    let Some((app, mut w, _config)) = open_2070(CYBERNETICS) else {
        return;
    };
    let (copy, rate) = w.update(|ed, _, _| {
        let m = ed.model.as_mut().expect("the factory opened");
        let steam = m.model.particles[0].clone();
        assert!(steam.file.ends_with(".efc"), "{}", steam.file);
        m.advance(1.5);
        assert!(!m.view.sprites.is_empty(), "{:?}", m.timeline_notes());
        let runs = m.efc_emitters(&steam.file).unwrap().runs.clone();
        assert!(runs.iter().flatten().count() > 0, "nothing drawn");
        let before = runs.iter().flatten().map(|r| r.starts.len()).sum::<usize>();
        m.select(Some(steam.element));
        let copy = m.copy_effect(steam.element).unwrap();
        assert!(copy.ends_with(".efc") && copy.contains("/efc/"), "{copy}");
        let mut e = Emitter {
            effect: m.efc(&copy).unwrap().effects().remove(0),
            launch: Default::default(),
            flipbook: None,
        };
        let rate = Knob::Rate(false).get(&e) * 2.;
        Knob::Rate(false).set(&mut e, rate);
        Knob::Rate(true).set(&mut e, rate);
        m.set_efc_effect(&copy, 0, &e.effect).unwrap();
        let runs = m.efc_emitters(&copy).unwrap().runs.clone();
        let after = runs.iter().flatten().map(|r| r.starts.len()).sum::<usize>();
        assert!(after > before, "{after} particles, {before} before");
        (copy, rate)
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.update(|ed, _, _| {
        let saved = ed.project.read_file(&copy).expect("the copy is in the mod");
        let efc = anno_formats::efc::Efc::parse(&saved).unwrap();
        let e = Emitter {
            effect: efc.effects().remove(0),
            launch: Default::default(),
            flipbook: None,
        };
        assert!((Knob::Rate(false).get(&e) - rate).abs() < rate * 0.1);
        let cfg = ed
            .project
            .read_file(CYBERNETICS)
            .expect("the cfg is in the mod");
        let named = copy.replace('/', "\\");
        assert!(
            String::from_utf8_lossy(&cfg).contains(&named),
            "the cfg names {named}"
        );
        let m = ed.model.as_mut().unwrap();
        assert!(m.undo(false) && m.undo(false));
        assert!(m.efcs.is_empty(), "undo kept the copy");
    });
    // Another effect dropped from the Assets browser lands as an EFFECT
    // entry, written in the file's spelling, and plays.
    w.update(|ed, _, cx| {
        let before = ed.model.as_ref().unwrap().model.particles.len();
        ed.model_drop("data/graphics/effects/particles/ambient_steam.efc", cx);
        let m = ed.model.as_mut().unwrap();
        assert_eq!(m.model.particles.len(), before + 1);
        let entry = m.model.particles.last().unwrap().element;
        assert_eq!(m.list_of(entry), Some("Effects"));
        assert!(String::from_utf8_lossy(&m.doc.write()).contains("<m_ConfigType>EFFECT"));
    });
}

// An animation added to the dog's body gets the next AnimationID, and a
// new sequence from it plays it once through, written as the game reads.
#[test]
fn animations_and_sequences_add_to_a_part() {
    let Some((_app, mut w, _config)) = open(DOG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let body = m.model.parts[0].element;
        let (count, sequences) = (m.model.parts[0].animations.len(), m.model.sequences.len());
        let walk = m.model.parts[0].animations[1].clone();
        m.add_animation(body, &walk).unwrap();
        assert_eq!(m.model.parts[0].animations.len(), count + 1);
        assert!(m.add_animation(body, "data/none.rdm").is_err());
        m.new_sequence(0, count).unwrap();
        let again = Model::from_doc(&Doc::parse(&m.doc.write())).unwrap();
        assert_eq!(again.sequences.len(), sequences + 1);
        let clip = again.sequences.last().unwrap().clips[0];
        assert_eq!(
            clip.what,
            anno_formats::model::Play::Animation {
                model: 0,
                animation: count as u32
            }
        );
        assert!((m.clip_length() - clip.end).abs() < 1e-3);
        assert_eq!(
            m.playback.source,
            super::playback::Source::Sequence(sequences)
        );
    });
}

// A 2070 building moves and saves in its own `m_` spelling: the game reads
// the rest of the file as it shipped.
#[test]
fn a_2070_cfg_edits_in_its_own_spelling() {
    const BAKERY: &str = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
    let Some((app, mut w, _config)) = open_2070(BAKERY) else {
        return;
    };
    let shipped = w.update(|ed, _, _| {
        let m = ed.model.as_mut().expect("the bakery opened");
        assert_eq!(m.doc.prefix(), "m_");
        assert!(!m.pieces.is_empty());
        let part = m.model.parts[0].element;
        let mut t = m.placement(part).expect("a part places");
        t.position[1] += 2.;
        m.set_placement(part, t, true);
        ed.install.as_ref().unwrap().read(BAKERY).unwrap()
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.read(|ed, _| {
        let saved = ed.project.read_file(BAKERY).expect("the cfg is in the mod");
        let text = String::from_utf8_lossy(&saved);
        assert!(
            !text.contains("<Position") && text.contains("<m_Position"),
            "written unprefixed"
        );
        let (a, b) = (
            Model::from_doc(&Doc::parse(&saved)).unwrap(),
            Model::from_doc(&Doc::parse(&shipped)).unwrap(),
        );
        let rose = a.parts[0].transform.position[1] - b.parts[0].transform.position[1];
        assert!((rose - 2.).abs() < 1e-4, "rose {rose}");
        assert_eq!(a.parts.len(), b.parts.len());
    });
}

// Off the Model workspace (islands, the asset preview), a soldier's spear
// stands where its bind pose holds it: where the workspace draws it at rest.
#[test]
fn islands_hang_props_on_the_bind_pose() {
    const SPEAR: &str =
        "data/base/graphics/celtic/units/feedback_military/spear_thrower_lean_m_01.cfg";
    let Some((_app, mut w, _config)) = open(SPEAR) else {
        return;
    };
    w.update(|ed, _, _| {
        let install = ed.install.clone().unwrap();
        let m = ed.model.as_mut().unwrap();
        m.play(super::playback::Source::Still);
        let cache = crate::island::models::Cache::new(&ed.module.model_look());
        let flat = crate::island::models::build(&install, &cache, &m.model, 1., false);
        let linked: Vec<_> = (m.model.props.iter())
            .filter(|p| p.rig.link.is_some() && p.rig.shown_if.is_none())
            .map(|p| p.element)
            .collect();
        let mut compared = 0;
        for (i, piece) in m.pieces.iter().enumerate() {
            if !piece.element.is_some_and(|e| linked.contains(&e)) {
                continue;
            }
            let rested = gpui_editor_kit::math::translation(&m.view.posed[i].local);
            let island = (flat.iter())
                .find(|f| {
                    f.element == piece.element && f.rdm == piece.rdm && f.group == piece.group
                })
                .expect("the island draws the prop");
            let at = gpui_editor_kit::math::translation(&island.local);
            let d = (0..3)
                .map(|a| (at[a] - rested[a]).abs())
                .fold(0f32, f32::max);
            assert!(d < 1e-3, "{}: {at:?} vs {rested:?}", piece.rdm);
            compared += 1;
        }
        assert!(compared > 0, "no linked prop drawn");
    });
}

// The worker's 25 hand props are all tools a condition shows while she
// works. Nothing raises one on an island, so none is drawn there, rather
// than every tool in her hands at once.
#[test]
fn islands_leave_out_the_tools_a_condition_shows() {
    let Some((_app, mut w, _config)) = open(WORKER) else {
        return;
    };
    w.update(|ed, _, _| {
        let install = ed.install.clone().unwrap();
        let m = ed.model.as_ref().unwrap();
        let cache = crate::island::models::Cache::new(&ed.module.model_look());
        let flat = crate::island::models::build(&install, &cache, &m.model, 1., false);
        let tools: Vec<_> = (m.model.props.iter())
            .filter(|p| p.rig.link.is_some() && p.rig.shown_if.is_some())
            .map(|p| p.element)
            .collect();
        assert!(!tools.is_empty());
        assert!(!flat.is_empty(), "the worker herself is drawn");
        for f in &flat {
            assert!(
                !f.element.is_some_and(|e| tools.contains(&e)),
                "{} is a tool a condition shows",
                f.rdm
            );
        }
    });
}

// A dog's joint keyed mid-walk holds that pose there, in the animation the
// mod carries; undo takes the key and the file back out; a copy for this
// model only points its cfg at a new animation beside it.
#[test]
fn joints_key_into_their_animation() {
    let Some((app, mut w, _config)) = open(DOG) else {
        return;
    };
    let (path, joint) = w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        m.play(super::playback::Source::Animation {
            part: 0,
            animation: 1,
        });
        m.playback.time = 0.1;
        m.pose();
        let body = m.model.parts[0].element;
        let joint = 5u16;
        m.joint = Some((body, joint));
        let k = m.keying().expect("the walk plays on the body");
        let before = m.joint_local().unwrap();
        let mut turned = before;
        let mut e = super::inspector::euler(turned.rotation);
        e[0] += 30.;
        turned.rotation = super::inspector::quaternion(e);
        m.key_joint(turned, true).unwrap();
        assert!(m.joint_key().is_some(), "keyed at the playhead");
        let now = super::inspector::euler(m.joint_local().unwrap().rotation);
        assert!((now[0] - e[0]).abs() < 0.5, "{now:?} vs {e:?}");
        (k.path, joint)
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.update(|ed, _, _| {
        let saved = ed
            .project
            .read_file(&path)
            .expect("the animation is in the mod");
        let a = anno_formats::rdm::Animation::parse(&saved).unwrap();
        let m = ed.model.as_mut().unwrap();
        let skeleton = m.model.parts[0]
            .mesh
            .as_ref()
            .unwrap()
            .skeleton
            .as_ref()
            .unwrap();
        let track = a
            .track_for(skeleton, joint as usize)
            .expect("the joint has a track");
        let (times, ..) = a.keys(track);
        assert!(times.iter().any(|t| (t - 0.1).abs() < 0.001), "{times:?}");
        assert!(m.undo(false));
        assert!(m.animations.is_empty());
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.update(|ed, _, _| {
        assert!(
            ed.project.read_file(&path).is_none(),
            "the undone key left the mod"
        );
        let m = ed.model.as_mut().unwrap();
        let to = m.copy_animation(0, 1).unwrap();
        assert!(to.contains("/anim/") && to.ends_with("_01.rdm"), "{to}");
        assert_eq!(m.model.parts[0].animations[1], to);
    });
}

// The gizmo keys a joint from its matrix in model space: keyed where it
// already is, its pose in its parent's space stays.
#[test]
fn a_joint_keyed_where_it_stands_keeps_its_pose() {
    let Some((_app, mut w, _config)) = open(DOG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        m.play(super::playback::Source::Animation {
            part: 0,
            animation: 1,
        });
        m.playback.time = 0.2;
        m.pose();
        m.joint = Some((m.model.parts[0].element, 9));
        let (before, at) = (m.joint_local().unwrap(), m.joint_matrix().unwrap());
        m.key_joint_at(&at, true).unwrap();
        let after = m.joint_local().unwrap();
        for a in 0..3 {
            assert!((after.translation[a] - before.translation[a]).abs() < 1e-4);
        }
        let dot: f32 = (0..4).map(|k| after.rotation[k] * before.rotation[k]).sum();
        assert!(dot.abs() > 0.9999, "turned: {before:?} → {after:?}");
    });
}

// A banner's cloth draws at rest and moves once playback runs under wind,
// pinned where it hangs from its pole.
#[test]
fn cloth_hangs_and_moves_in_the_wind() {
    const BANNER: &str = "data/base/graphics/library/props/decoration/banner/banner_01.cfg";
    let Some((_app, mut w, _config)) = open(BANNER) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let cloth = (anno_formats::cloth::clothes(&m.doc).first())
            .map(|c| c.element)
            .expect("the banner has cloth");
        let drawn = |m: &super::ModelEditor| {
            let ids = Some(u64::from(cloth.raw()));
            (m.view.sprites.iter())
                .find(|b| b.instances.ids.first() == Some(&ids))
                .map(|b| b.mesh.bounds())
        };
        m.pose();
        let rest = drawn(m).expect("the cloth draws at rest");
        m.playback.wind = 2.;
        for _ in 0..30 {
            m.advance(1. / 30.);
        }
        let blown = drawn(m).unwrap();
        assert_ne!(rest, blown, "the wind moved nothing");
        let finite = blown.iter().flatten().all(|v| v.is_finite());
        assert!(finite, "{blown:?}");
    });
}

// A building's props load off the UI thread, four at a time: rebuilt with
// none loaded, the model draws its own meshes at once and takes edits
// meanwhile; each prop's pieces come in as its load lands, where its entry
// stands by then.
#[test]
fn props_load_in_the_background_a_few_at_a_time() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    let shown = |m: &super::ModelEditor, entry| -> Vec<[f32; 3]> {
        (m.pieces.iter().filter(|p| p.element == Some(entry)))
            .map(|p| [p.local[12], p.local[13], p.local[14]])
            .collect()
    };
    let (whole, prop, before) = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let (whole, prop) = (m.pieces.len(), m.model.props[0].element);
        let before = shown(m, prop);
        assert!(!before.is_empty(), "the first prop draws nothing");
        m.nested.clear();
        m.rebuild();
        assert!(m.pieces.len() < whole, "the props drew before loading");
        assert!(shown(m, prop).is_empty());
        let mut t = m.placement(prop).unwrap();
        t.position[0] += 1.;
        m.set_placement(prop, t, true);
        ed.load_nested(cx);
        let m = ed.model.as_ref().unwrap();
        let running = (m.nested.iter())
            .filter(|n| matches!(n.load, super::Load::Running))
            .count();
        assert!(
            m.nested.len() > super::NESTED_LOADS,
            "{} prop cfgs",
            m.nested.len()
        );
        assert_eq!(running, super::NESTED_LOADS);
        (whole, prop, before)
    });
    app.run_until_parked();
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert!(
            (m.nested.iter()).all(|n| matches!(n.load, super::Load::Ready(_))),
            "loads left over"
        );
        assert_eq!(m.pieces.len(), whole);
        let after = shown(m, prop);
        assert_eq!(after.len(), before.len());
        for (a, b) in after.iter().zip(&before) {
            let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
            assert!(
                (d[0] - 1.).abs() < 1e-4 && d[1].abs() < 1e-4 && d[2].abs() < 1e-4,
                "{d:?}"
            );
        }
    });
}

// The asset picker searches the game as it is typed and adds the pick; a
// file dropped from the Asset browser adds the same way; the field filter
// narrows as it is typed.
#[test]
fn props_add_from_the_picker_and_from_drops() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    let before = w.read(|ed, _| ed.model.as_ref().unwrap().model.props.len());
    w.update(|ed, _, cx| {
        let add = super::picker::Want::Add(super::picker::Kind::Prop);
        ed.open_model_picker(add, cx);
        let p = ed.model_picker.as_ref().expect("the picker opened");
        assert!(!p.hits.is_empty(), "nothing near the model");
    });
    w.simulate_keystrokes("b u c k e t");
    app.run_until_parked();
    w.read(|ed, _| {
        let p = ed.model_picker.as_ref().unwrap();
        assert!(
            !p.hits.is_empty()
                && p.hits
                    .iter()
                    .all(|h| h.ends_with(".cfg") || h.ends_with(".prp"))
        );
        assert!(
            p.hits.iter().all(|h| h.contains("bucket")),
            "{:?}",
            &p.hits[..3.min(p.hits.len())]
        );
    });
    w.simulate_keystrokes("enter");
    app.run_until_parked();
    let dropped = "data/base/graphics/library/effects/fire/fire_big_01.cfg";
    w.update(|ed, _, cx| {
        assert!(ed.model_picker.is_none());
        assert_eq!(ed.model.as_ref().unwrap().model.props.len(), before + 1);
        ed.model_drop(dropped, cx);
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.model.props.len(), before + 2);
        assert!(m.model.props.iter().any(|p| p.cfg == dropped));
    });
}

// A resident's sequence starts its sounds as the playhead crosses them:
// each clip once per pass, again after the loop wraps.
#[test]
fn sequence_sounds_start_as_the_playhead_crosses_them() {
    const RESIDENT: &str = "data/base/graphics/celtic/units/inhabitants/residents/resident_tier03_lean_m_01/resident_tier03_lean_m_01.cfg";
    let Some((_app, mut w, _config)) = open(RESIDENT) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let (k, s) = (m.model.sequences.iter().enumerate())
            .find(|(_, s)| {
                s.clips
                    .iter()
                    .any(|c| matches!(c.what, anno_formats::model::Play::Sound(_)))
            })
            .expect("a sequence with sounds");
        let sounds = (s.clips.iter())
            .filter(|c| matches!(c.what, anno_formats::model::Play::Sound(_)))
            .count();
        let length = s.length();
        m.play(super::playback::Source::Sequence(k));
        m.playback.looping = true;
        let mut t = 0.;
        while t < length {
            m.advance(0.01);
            t += 0.01 * m.model.sequences[k].speed;
        }
        let heard = std::mem::take(&mut m.sounds_due).len();
        assert_eq!(heard, sounds, "each sound once a pass");
        for _ in 0..((length / 0.01) as usize + 2) {
            m.advance(0.01 / m.model.sequences[k].speed);
        }
        assert!(m.sounds_due.len() >= sounds, "again after the wrap");
    });
}

const CRANE: &str = "data/base/graphics/library/props/cranes/roman_harbor_crane_large_01/roman_harbor_crane_large_01.cfg";

/// One playback frame of `dt` seconds, as the model's tick runs it.
fn sound_frame(ed: &mut Editor, dt: f32) {
    let m = ed.model.as_mut().unwrap();
    let before = (m.playback.source, m.playback.time);
    if m.playback.playing {
        m.advance(dt);
    }
    let due = std::mem::take(&mut m.sounds_due);
    ed.model_sound_frame(before, due);
}

/// Frames until the playhead passes `t` on the sequence's clock.
fn play_to(ed: &mut Editor, t: f32) {
    for _ in 0..10_000 {
        if ed.model.as_ref().unwrap().playback.time > t {
            return;
        }
        sound_frame(ed, 0.02);
    }
    panic!("the playhead never passed {t}");
}

/// Frames until the playhead wraps past the sequence's end.
fn play_round(ed: &mut Editor) {
    for _ in 0..10_000 {
        let before = ed.model.as_ref().unwrap().playback.time;
        sound_frame(ed, 0.02);
        if ed.model.as_ref().unwrap().playback.time < before {
            return;
        }
    }
    panic!("the playhead never wrapped");
}

/// Voices the clip started.
fn voices_of(ed: &Editor, clip: anno_formats::xml::Id) -> usize {
    (ed.model_sound.voices().iter())
        .filter(|v| v.0 == Some(clip.raw()))
        .count()
}

// A Looped clip starts as the playhead crosses it and runs on through the
// sequence's wraps, one voice; a rewind, a pause or another source stops
// it, and playing from past its start hears it straight away. A clip that
// isn't Looped starts again at every crossing, unless its event loops by
// itself (the festival drum, 112160): that one runs on as a Looped one.
#[test]
fn looped_sound_clips_run_until_playback_stops() {
    let Some((_app, mut w, _config)) = open(CRANE) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        assert!(m.sounds.is_some(), "the game's sounds loaded");
        let k = (m.model.sequences.iter())
            .position(|s| s.length() > 1.)
            .expect("a sequence");
        m.play(Source::Sequence(k));
        let track = m.add_track(m.model.sequences[k].element);
        let looped = Sound {
            looped: true,
            ..Sound::of(51235)
        };
        let looped = m.add_clip(track, Play::Sound(looped), 0.3).unwrap();
        let once = m
            .add_clip(track, Play::Sound(Sound::of(51235)), 0.2)
            .unwrap();
        let drum = m
            .add_clip(track, Play::Sound(Sound::of(112160)), 0.25)
            .unwrap();
        m.playback.looping = true;
        m.playback.playing = true;

        play_to(ed, 0.35);
        assert_eq!(voices_of(ed, looped), 1);
        assert_eq!(voices_of(ed, once), 1);
        assert!(
            ed.model_sound
                .voices()
                .iter()
                .any(|v| v.0 == Some(looped.raw()) && v.1)
        );
        // Past the end and round to the clips again.
        play_round(ed);
        play_to(ed, 0.35);
        assert_eq!(voices_of(ed, looped), 1, "the wrap keeps the one voice");
        assert_eq!(voices_of(ed, once), 2, "the plain clip starts again");
        assert_eq!(voices_of(ed, drum), 1, "a looping event keeps its voice");
        assert!(
            ed.model_sound
                .voices()
                .iter()
                .any(|v| v.0 == Some(drum.raw()) && v.1)
        );

        let m = ed.model.as_mut().unwrap();
        m.playback.time = 0.;
        m.pose();
        sound_frame(ed, 0.02);
        assert_eq!(
            voices_of(ed, looped) + voices_of(ed, once),
            0,
            "a rewind stops them"
        );

        play_to(ed, 0.35);
        assert_eq!(voices_of(ed, looped), 1);
        ed.model.as_mut().unwrap().playback.playing = false;
        sound_frame(ed, 0.02);
        assert_eq!(voices_of(ed, looped), 0, "a pause stops it");

        let m = ed.model.as_mut().unwrap();
        m.playback.time = 0.5;
        m.pose();
        m.playback.playing = true;
        sound_frame(ed, 0.02);
        assert_eq!(voices_of(ed, looped), 1, "playing from past it hears it");
        assert_eq!(
            voices_of(ed, once),
            0,
            "a plain clip waits for its crossing"
        );

        ed.model.as_mut().unwrap().play(Source::Still);
        sound_frame(ed, 0.02);
        assert_eq!(voices_of(ed, looped), 0, "another source stops it");
    });
}

// The crane's wheel sound (BoneID 2) plays from kontor_crane_wheel_01 as
// the crane moves it; a joint the skeleton lacks falls back to the mesh's
// origin, a mesh the cfg lacks to the model's. Heard from the camera, a
// sound on the right of the view is louder on the right.
#[test]
fn sound_clips_play_from_their_joint() {
    let Some((_app, mut w, _config)) = open(CRANE) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let (k, clip, wheel) = (m.model.sequences.iter().enumerate())
            .find_map(|(k, s)| {
                s.clips.iter().find_map(|c| match c.what {
                    Play::Sound(s) if s.bone == 2 => Some((k, c.element, s)),
                    _ => None,
                })
            })
            .expect("the wheel's clip");
        m.play(Source::Sequence(k));
        m.advance(0.5);
        assert!(
            m.sound_place(wheel).ends_with("› kontor_crane_wheel_01"),
            "{}",
            m.sound_place(wheel)
        );
        let posed = super::playback::joint_at(&m.model, &m.player.poses, 0, 2).unwrap();
        assert_eq!(m.sound_at(wheel), math::transform(&posed, [0.; 3]));
        let off = Sound { bone: 99, ..wheel };
        assert_eq!(m.sound_at(off), m.model.parts[0].transform.position);
        assert!(m.sound_place(off).contains("has no joint 99"));
        assert_eq!(m.sound_at(Sound { model: 99, ..wheel }), [0.; 3]);

        // The view centred a unit to the joint's left: the joint shows on
        // the right.
        let at = m.sound_at(wheel);
        let right = m.view.camera.right();
        m.view.camera.center = Some(math::sub(at, right));
        m.view.camera.pan = [0.; 2];
        let reach = (m.sounds.as_ref().unwrap().event(wheel.guid))
            .unwrap()
            .max_attenuation;
        assert!(reach > 1., "the wheel is heard to {reach}");
        let [l, r] = m.cue(clip, wheel).gain;
        assert!(r > l && r > 0., "left {l}, right {r}");
        m.view.camera.center = Some(math::add(at, right));
        let [l, r] = m.cue(clip, wheel).gain;
        assert!(l > r, "left {l}, right {r}");
        // Out of reach: silent.
        m.view.camera.center = Some(math::add(at, [0., 0., reach * 2.]));
        assert_eq!(m.cue(clip, wheel).gain, [0., 0.]);
    });
}

// The inspector's Sound section sets a clip's joint from the joint picked
// in the hierarchy and its Looped tick, as edits the cfg keeps and undo
// takes back.
#[test]
fn a_sound_clips_joint_and_loop_are_edited() {
    let Some((app, mut w, _config)) = open(CRANE) else {
        return;
    };
    let clip = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let clip = (m.model.sequences.iter().flat_map(|s| &s.clips))
            .find(|c| matches!(c.what, Play::Sound(s) if s.bone == 2))
            .unwrap()
            .element;
        m.select(Some(clip));
        m.joint = Some((m.model.parts[0].element, 1));
        cx.notify();
        clip
    });
    app.run_until_parked();
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let s = m.sound_clip(clip).unwrap();
        assert_eq!(m.picked_sound_joint(), Some((0, 1)));
        m.set_sound(
            clip,
            Sound {
                model: 0,
                bone: 1,
                ..s
            },
        );
        m.set_sound(
            clip,
            Sound {
                looped: true,
                model: 0,
                bone: 1,
                ..s
            },
        );
        let now = m.sound_clip(clip).unwrap();
        assert_eq!((now.looped, now.model, now.bone), (true, 0, 1));
        assert!(m.sound_place(now).ends_with("› kontor_crane_rope_01"));
        let again = Model::from_doc(&Doc::parse(&m.doc.write())).unwrap();
        assert!(
            (again.sequences.iter().flat_map(|s| &s.clips)).any(|c| c.what == Play::Sound(now))
        );
        m.undo(false);
        m.undo(false);
        assert_eq!(m.sound_clip(clip), Some(s));
    });
}

// 1800's sound clips name its sounds and start voices: a worker's hammer
// blow plays from the hammer (ModelID 7, its one joint), and the train's
// Looped engine loops.
#[test]
fn anno1800_sound_clips_play() {
    const HAMMERING: &str = "data/cdlc10/graphics/units/residents/asian_01_normal_m.cfg";
    let Some((_app, mut w, _config)) = open_in("ANNO1800_DIR", &anno1800::MODULE, HAMMERING) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let library = m.sounds.clone().expect("1800's sounds loaded");
        let (k, clip, hammer) = (m.model.sequences.iter().enumerate())
            .find_map(|(k, s)| {
                s.clips.iter().find_map(|c| match c.what {
                    Play::Sound(s) if s.model == 7 && library.event(s.guid).is_some() => {
                        Some((k, c.element, s))
                    }
                    _ => None,
                })
            })
            .expect("the hammer's clip");
        assert!(
            library
                .name(hammer.guid)
                .is_some_and(|n| n.starts_with("ww"))
        );
        assert!(
            m.sound_place(hammer).ends_with("› Hammer_lod2"),
            "{}",
            m.sound_place(hammer)
        );
        let start = (m.model.sequences[k].clips.iter())
            .find(|c| c.element == clip)
            .unwrap()
            .start;
        m.play(Source::Sequence(k));
        m.playback.playing = true;
        play_to(ed, start + 0.01);
        assert_eq!(voices_of(ed, clip), 1);
    });
}

// Sequences name meshes by place: after the dog's body is duplicated and
// then moved behind its copy, its sequences still play on the body.
#[test]
fn sequences_follow_their_mesh_through_duplicate_and_reorder() {
    let Some((_app, mut w, _config)) = open(DOG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let body = m.model.parts[0].element;
        let plays_on = |m: &super::ModelEditor| match m.model.sequences[0].clips[0].what {
            anno_formats::model::Play::Animation { model, .. } => model as usize,
            ref other => panic!("{other:?}"),
        };
        m.duplicate(body);
        assert_eq!(m.model.parts[plays_on(m)].element, body, "after duplicate");
        m.reorder(body, None);
        assert_eq!(plays_on(m), 1);
        assert_eq!(m.model.parts[plays_on(m)].element, body, "after reorder");
        let copy = m.model.parts[0].element;
        m.remove(copy);
        assert_eq!(plays_on(m), 0);
        assert_eq!(m.model.parts[0].element, body, "after remove");
    });
}

// Opening the worker and then one of her props (as the hierarchy's
// double-click does) switches models: nothing was edited, so there is
// nothing to save first. Back returns to the worker. An edit makes the next
// open ask to save or discard, keeping the edit until answered.
#[test]
fn an_untouched_model_gives_way_to_the_next() {
    let Some((app, mut w, _config)) = open(WORKER) else {
        return;
    };
    let prop = w.read(|ed, _| ed.model.as_ref().unwrap().model.props[0].cfg.clone());
    w.update(|ed, _, cx| ed.open_model(&prop, cx));
    app.run_until_parked();
    w.read(|ed, _| {
        assert_eq!(ed.model.as_ref().unwrap().path, prop);
        assert_eq!(ed.previous_model(), Some(WORKER));
    });
    w.simulate_keystrokes("alt-left");
    app.run_until_parked();
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().path, WORKER, "back"));
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let part = m.model.parts[0].element;
        m.duplicate(part);
    });
    w.update(|ed, _, cx| ed.open_model(&prop, cx));
    app.run_until_parked();
    w.read(|ed, _| {
        assert_eq!(ed.model.as_ref().unwrap().path, WORKER, "the edit is kept");
        assert_eq!(ed.leaving, Some(Leave::Model(prop.clone())), "asked");
    });
}

// Ctrl P opens another model by name from the Model workspace, the ones
// opened before first.
#[test]
fn ctrl_p_opens_a_model_by_name() {
    let Some((app, mut w, _config)) = open(WORKER) else {
        return;
    };
    w.simulate_keystrokes("ctrl-p");
    w.read(|ed, _| {
        let p = ed.model_picker.as_ref().expect("the picker opened");
        assert!(!p.hits.iter().any(|h| h == WORKER), "not the open one");
        assert!(
            p.hits
                .iter()
                .all(|h| h.ends_with(".cfg") || h.ends_with(".prp"))
        );
    });
    w.simulate_keystrokes("d o g");
    app.run_until_parked();
    let at = w.read(|ed, _| {
        let hits = &ed.model_picker.as_ref().unwrap().hits;
        hits.iter()
            .position(|h| h == DOG)
            .expect("the dog among the hits")
    });
    for _ in 0..at {
        w.simulate_keystrokes("down");
    }
    w.simulate_keystrokes("enter");
    app.run_until_parked();
    w.read(|ed, _| {
        assert!(ed.model_picker.is_none());
        assert_eq!(ed.model.as_ref().unwrap().path, DOG);
    });
    w.simulate_keystrokes("ctrl-p");
    w.read(|ed, _| {
        let hits = &ed.model_picker.as_ref().unwrap().hits;
        assert_eq!(
            hits.first().map(String::as_str),
            Some(WORKER),
            "recent first"
        );
    });
}

// The Models list files every game model under its folders: the worker
// among 117's Celtic units, before the Roman ones.
#[test]
fn the_models_list_files_every_game_model() {
    let Some((_app, w, _config)) = open(WORKER) else {
        return;
    };
    w.read(|ed, _| {
        let all = super::list::game_models(ed.install.as_ref().unwrap());
        assert!(all.len() > 10_000, "{} models", all.len());
        let worker = all
            .iter()
            .position(|p| p == WORKER)
            .expect("the worker is listed");
        let roman = (all.iter())
            .position(|p| p.contains("/graphics/roman/units/"))
            .expect("Roman units");
        assert!(worker < roman, "Celtic files before Roman");
        assert!(
            all.iter()
                .all(|p| p.ends_with(".cfg") || p.ends_with(".prp"))
        );
    });
}

// New animation makes a bind-pose animation beside the cfg as one edit: it
// plays at once, holds every joint where the bind pose does, takes a key,
// and saves as a file the reader takes back with every joint tracked at
// the shipped rate.
#[test]
fn a_new_animation_starts_in_the_bind_pose() {
    use super::playback::Source;
    let Some((app, mut w, _config)) = open(DOG) else {
        return;
    };
    let (path, count) = w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let count = m.model.parts[0].animations.len();
        let path = m.new_animation(0, 1.5).unwrap();
        assert!(path.ends_with("/anim/dog-big_01_anim_01.rdm"), "{path}");
        assert_eq!(m.model.parts[0].animations[count], path);
        let source = Source::Animation {
            part: 0,
            animation: count,
        };
        assert_eq!(m.playback.source, source);
        assert!((m.clip_length() - 1.5).abs() < 1e-6);
        let bind = (m.model.parts[0].mesh.as_ref())
            .and_then(|r| r.skeleton.as_ref())
            .map(|s| s.bind.clone())
            .unwrap();
        for t in [0., 0.75, 1.5] {
            m.playback.time = t;
            m.pose();
            for (posed, bind) in m.player.poses[0].iter().zip(&bind) {
                let off = (0..3).map(|a| (posed.translation[a] - bind.translation[a]).abs());
                assert!(
                    off.fold(0f32, f32::max) < 1e-4,
                    "{t}: {posed:?} vs {bind:?}"
                );
            }
        }
        // One undo takes it all back; redo brings it again.
        assert!(m.undo(false));
        assert_eq!(m.model.parts[0].animations.len(), count);
        assert!(m.animations.is_empty());
        assert!(m.undo(true));
        assert_eq!(m.model.parts[0].animations[count], path);
        // A joint keys into it at the playhead.
        m.play(source);
        m.playback.time = 0.5;
        m.pose();
        m.joint = Some((m.model.parts[0].element, 5));
        let mut turned = m.joint_local().unwrap();
        let mut e = super::inspector::euler(turned.rotation);
        e[0] += 30.;
        turned.rotation = super::inspector::quaternion(e);
        m.key_joint(turned, true).unwrap();
        (path, count)
    });
    w.update(|ed, _, _| ed.save());
    app.run_until_parked();
    w.read(|ed, _| {
        let saved = ed
            .project
            .read_file(&path)
            .expect("the animation is in the mod");
        let a = anno_formats::rdm::Animation::parse(&saved).unwrap();
        assert_eq!((a.duration, a.sample_rate), (1.5, 15));
        let m = ed.model.as_ref().unwrap();
        let skeleton = (m.model.parts[0].mesh.as_ref())
            .and_then(|r| r.skeleton.as_ref())
            .unwrap();
        for j in 0..skeleton.len() {
            assert!(a.track_for(skeleton, j).is_some(), "joint {j} has no track");
        }
        let track = a.track_for(skeleton, 5).unwrap();
        assert_eq!(a.keys(track).0, [0., 0.5, 1.5]);
        let cfg = Model::from_doc(&Doc::parse(&ed.project.read_file(DOG).unwrap())).unwrap();
        assert_eq!(cfg.parts[0].animations[count], path);
    });
}

// A clip three times its animation's length repeats the animation in a
// looped sequence; in one that is not, it plays once and holds the last
// pose, as the game steps it.
#[test]
fn long_clips_repeat_when_looped_and_hold_when_not() {
    let Some((_app, mut w, _config)) = open(DOG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        m.new_sequence(0, 0).unwrap();
        let k = m.model.sequences.len() - 1;
        let path = m.model.parts[0].animations[0].clone();
        let d = m.animation(&path).unwrap().duration;
        assert_eq!(m.model.parts[0].animation_scales[0], 1.);
        let entry = m.model.sequences[k].element;
        let element = (m.doc.child(entry, "Track"))
            .and_then(|t| m.doc.child(t, "TrackElement"))
            .unwrap();
        let end = m.doc.child(element, "EndTime").unwrap();
        let looped = m.doc.child(entry, "Looped").unwrap();
        m.set_value(end, &((d * 3000.).round() as u32).to_string());
        let shown = |m: &mut super::ModelEditor| {
            m.play(super::playback::Source::Sequence(k));
            m.playback.time = 2.5 * d;
            m.pose();
            m.player.playing[0].2
        };
        assert!((shown(m) - 0.5 * d).abs() < 1e-3, "looped repeats");
        m.set_value(looped, "0");
        assert!((shown(m) - d).abs() < 1e-6, "unlooped holds");
    });
}

// An info-layer arrow faces the camera, its -Z toward the viewer, and turns
// with it as the view orbits.
#[test]
fn billboards_face_the_camera() {
    const ARROW: &str = "data/base/graphics/ui/3d_objects/infolayer/arrow_green.cfg";
    let Some((_app, mut w, _config)) = open(ARROW) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        assert!(m.faces_camera());
        let part = m.model.parts[0].element;
        for yaw in [0.5, 1.7] {
            m.view.camera.yaw = yaw;
            m.pose();
            let at = (m.pieces.iter().zip(&m.view.posed))
                .find(|(p, _)| p.element == Some(part))
                .map(|(_, q)| q.local)
                .unwrap();
            let column = |c: usize| {
                let v = [at[c * 4], at[c * 4 + 1], at[c * 4 + 2]];
                gpui_editor_kit::math::normalize(v).unwrap()
            };
            let camera = m.view.camera;
            let toward = camera.toward();
            let z = column(2);
            let facing: f32 = (0..3).map(|a| z[a] * toward[a]).sum();
            assert!(
                facing < -0.999,
                "yaw {yaw}: -Z faces away ({z:?} vs {toward:?})"
            );
            let right = camera.right();
            let along: f32 = (0..3).map(|a| column(0)[a] * right[a]).sum();
            assert!(along > 0.999, "yaw {yaw}: +X is not screen right");
        }
    });
}

// A flag on the wind's alignment turns +X to where the timeline's wind
// blows.
#[test]
fn flags_turn_into_the_wind() {
    const FLAG: &str =
        "data/base/graphics/library/props/flags/animated_flags/flag_anim_horizontal_var01.cfg";
    let Some((_app, mut w, _config)) = open(FLAG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let part = (m.model.parts.iter())
            .find(|p| {
                p.rig
                    .motion
                    .is_some_and(|mo| mo.alignment != anno_formats::model::Alignment::None)
            })
            .map(|p| p.element)
            .expect("a part aligns");
        for (degrees, to) in [(90., [0., 0., 1.]), (0., [1., 0., 0.])] {
            m.playback.wind_direction = degrees;
            m.pose();
            let at = (m.pieces.iter().zip(&m.view.posed))
                .find(|(p, _)| p.element == Some(part))
                .map(|(_, q)| q.local)
                .unwrap();
            let x = gpui_editor_kit::math::normalize([at[0], at[1], at[2]]).unwrap();
            let along: f32 = (0..3).map(|a| x[a] * to[a]).sum();
            assert!(along > 0.999, "{degrees}°: +X is {x:?}");
        }
    });
}

// 117's Egyptian warship: its wake churn (particle 0) and its floating
// straw (two prop effects) start invisible (COLOR_TRANSFORM) and show only
// while a sequence raises condition 1024; its two damage smokes
// (DAMAGE_TRANSFORM from scale 0) never show on a whole ship, though the
// condition their colours wait on (8192) is up, and draw once that
// transform is gone. The happy face over a house shows while condition 32 is up
// and its mesh hides as it fades out. Paused, the colours stand where the
// playhead's conditions take them.
#[test]
fn color_and_damage_transforms_hide_until_raised_or_damaged() {
    use super::{ModelEditor, playback::Source};
    use anno_formats::{model::Play, xml::Id};
    const SHIP: &str = "data/dlc03/graphics/egypt/units/ships/ship_01/ship_01.cfg";
    const HAPPY: &str = "data/base/graphics/ui/infolayer3d/happy.cfg";
    let Some((app, mut w, _config)) = open(SHIP) else {
        return;
    };
    let at = |m: &mut ModelEditor, t: f32| {
        m.playback.time = t;
        m.pose();
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let smokes = [2, 3].map(|k| m.model.particles[k].element);
        for &s in &smokes {
            let d = m.rig(s).and_then(|r| r.damage).expect("a damage smoke");
            assert_eq!(d.scale_at(0.), 0.);
        }
        let waiting: Vec<Id> = (m.model.particles.iter().map(|p| (p.element, p.rig)))
            .chain(m.model.props.iter().map(|p| (p.element, p.rig)))
            .filter(|(_, r)| {
                r.color
                    .is_some_and(|c| c.start_invisible && c.conditions == 1024)
            })
            .map(|(e, _)| e)
            .collect();
        assert!(waiting.len() >= 3, "{waiting:?}");
        let (k, (from, to)) = (m.model.sequences.iter().enumerate())
            .find_map(|(k, s)| {
                let c = s.clips.iter().find(|c| c.what == Play::Condition(1024))?;
                Some((k, (c.start, c.end)))
            })
            .expect("a sequence raises 1024");
        let drawn = |m: &ModelEditor, e: Id| {
            let id = Some(u64::from(e.raw()));
            (m.view.sprites.iter()).any(|b| b.instances.ids.first() == Some(&id))
        };
        m.play(Source::Sequence(k));
        at(m, from - 0.5);
        assert!(
            waiting.iter().all(|&e| m.player.color(e)[3] == 0.),
            "clear before 1024"
        );
        at(m, (from + to) / 2.);
        assert!(
            waiting.iter().all(|&e| m.player.color(e)[3] == 1.),
            "shown while it is up"
        );
        let smoking = (m.model.sequences[k].clips.iter())
            .find(|c| c.what == Play::Condition(8192))
            .map(|c| (c.start + c.end) / 2.)
            .expect("8192 raised");
        at(m, smoking);
        assert!(smokes.iter().all(|&s| m.player.color(s)[3] == 1.));
        assert!(
            smokes.iter().all(|&s| !drawn(m, s)),
            "no smoke on a whole ship"
        );
        // Without their damage transforms the smokes draw.
        for &s in &smokes {
            let t = m.doc.child(s, "Transformer").unwrap();
            let damage = (m.doc.children(t)).find(|&c| {
                m.doc
                    .child(c, "ConfigType")
                    .is_some_and(|n| m.doc.text(n) == "DAMAGE_TRANSFORM")
            });
            m.doc.remove(damage.unwrap());
        }
        m.rebuild();
        at(m, smoking);
        assert!(
            smokes.iter().any(|&s| drawn(m, s)),
            "{:?}",
            m.timeline_notes()
        );
    });
    w.update(|ed, _, cx| {
        // Edited behind undo's back: dropped, so the next opens at once.
        ed.model = None;
        ed.open_model(HAPPY, cx);
    });
    app.run_until_parked();
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let face = m.model.parts[0].element;
        let shown = |m: &ModelEditor| {
            (m.pieces.iter().zip(&m.view.posed))
                .any(|(p, pose)| p.element == Some(face) && pose.shown)
        };
        at(m, 2.);
        assert!(shown(m), "while 32 is up");
        at(m, 5.5);
        assert!(!shown(m), "faded out");
    });
}

// The model's own effects, the particles no clip runs, keep their own
// clock. The 1800 airship's one-shot explosion on its wreck fired each
// time one of its sequences started or looped; now its idle sequences draw
// nothing, while its destruction still runs the effects its clips start.
// The 117 dye works' idle sequence turns its own fumes off, and its
// working one runs them.
#[test]
fn own_effects_keep_their_clock_and_stay_off_where_disabled() {
    const AIRSHIP: &str = "data/dlc03/graphics/vehicles/airships/airships.cfg";
    const DYEWORKS: &str = "data/base/graphics/roman/buildings/production/chemical/tyrian_purple/chemical_tyrian_purple.cfg";
    let drawn = |m: &mut super::ModelEditor, t: f32| {
        m.playback.time = t;
        m.pose();
        !m.view.sprites.is_empty()
    };
    if let Some((_app, mut w, _config)) = open_in("ANNO1800_DIR", &anno1800::MODULE, AIRSHIP) {
        w.update(|ed, _, _| {
            let m = ed.model.as_mut().unwrap();
            let (idle, wreck): (Vec<usize>, Vec<usize>) =
                (0..m.model.sequences.len()).partition(|&k| {
                    !(m.model.sequences[k].clips.iter())
                        .any(|c| matches!(c.what, Play::Particle(_)))
                });
            assert!(idle.len() >= 2 && wreck.len() == 1, "{idle:?} {wreck:?}");
            for k in idle {
                m.play(Source::Sequence(k));
                let length = m.clip_length();
                assert!(m.model.sequences[k].looped && length > 0.);
                // Through the start, and again after the loop wraps.
                for t in (0..40).map(|i| i as f32 * 0.05) {
                    assert!(!drawn(m, t), "sequence {k} drew an effect at {t} s");
                }
                m.playback.playing = true;
                m.advance(length + 0.1);
                assert!(
                    m.view.sprites.is_empty(),
                    "sequence {k} drew one as it looped"
                );
            }
            m.play(Source::Sequence(wreck[0]));
            let fire = (m.model.sequences[wreck[0]].clips.iter())
                .find(|c| matches!(c.what, Play::Particle(_)))
                .copied()
                .unwrap();
            assert!(
                drawn(m, (fire.start + fire.end) / 2.),
                "nothing inside {fire:?}"
            );
        });
    }
    let Some((_app, mut w, _config)) = open(DYEWORKS) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        assert!(!m.model.particles.is_empty());
        let off = (m.model.sequences.iter())
            .position(|s| s.disable_effects)
            .expect("an idle sequence that turns effects off");
        let on = (m.model.sequences.iter())
            .position(|s| !s.disable_effects)
            .expect("a working one");
        m.play(Source::Sequence(off));
        for t in [0., 0.5, 1., 2.] {
            assert!(!drawn(m, t), "an effect drew at {t} s while idle");
        }
        m.play(Source::Sequence(on));
        assert!(
            (0..20).any(|i| drawn(m, i as f32 * 0.1)),
            "no effect while working"
        );
    });
}
