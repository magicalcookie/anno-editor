use super::{MapEditor, Selection, Tool, snap};
use crate::project::Project;
use anno_core::game::SizeClass;
use anno_core::map::{Map, MapRules, Registers, Role, Setup, Slot, Variant};
use std::sync::Arc;

static CLASSES: &[SizeClass] = &[
    SizeClass {
        name: "Small",
        slot: 192,
        start: false,
    },
    SizeClass {
        name: "Medium",
        slot: 272,
        start: false,
    },
    SizeClass {
        name: "Large",
        slot: 384,
        start: true,
    },
];

static RULES: MapRules = MapRules {
    size_classes: CLASSES,
    setups: &[
        Setup {
            island_size: "Large",
            world_size: "",
        },
        Setup {
            island_size: "Small",
            world_size: "",
        },
    ],
    roles: &[(Role::Normal, 0), (Role::Start, 1), (Role::Decoration, 2)],
    grid: 16,
    fixed_turns: true,
    pool_slots: true,
    free_starts: false,
    min_starts: 1,
    min_gap: 0,
    min_edge: 0,
    layout_folder: ["data/maps/", "/"],
    registers: Registers::NewMapType,
};

fn slot(at: [i32; 2], source: u16) -> Slot {
    Slot {
        at,
        island: Slot::POOL,
        role: Role::Normal,
        class: 1,
        turns: 0,
        raw_type: 0,
        source,
    }
}

/// Two variants of three slots each, from the same three elements.
fn map() -> MapEditor {
    let variant = |setup| Variant {
        setup,
        size: [2048, 2048],
        playable: [0, 0, 2048, 2048],
        slots: vec![slot([0, 0], 0), slot([400, 0], 1), slot([800, 800], 2)],
        starts: Vec::new(),
        source: Arc::from(&[][..]),
        base: "data/base".into(),
        replaces: Vec::new(),
    };
    MapEditor::new(
        Map {
            name: "Twin".into(),
            region: "Roman".into(),
            size: [2048, 2048],
            playable: [0, 0, 2048, 2048],
            islands: Vec::new(),
            variants: RULES.setups.iter().map(|&s| variant(s)).collect(),
        },
        None,
        false,
    )
}

#[test]
fn a_selection_holds_slots_past_one_word() {
    let mut s = Selection::default();
    for i in [0, 63, 64, 511] {
        s.set(i, true);
    }
    assert_eq!(s.iter().collect::<Vec<_>>(), [0, 63, 64, 511]);
    s.set(63, false);
    assert_eq!(s.count(), 3);
    assert!(!s.has(63) && s.has(511) && !s.has(600));
}

#[test]
fn a_press_selects_the_topmost_slot_and_shift_adds() {
    let mut m = map();
    // A Medium slot is 272 m a side: (100, 100) is in the first.
    let hit = m.hit([100., 100.], &RULES);
    assert_eq!(hit, Some(0));
    m.select(hit, false);
    m.select(m.hit([500., 100.], &RULES), true);
    assert_eq!(m.selected.iter().collect::<Vec<_>>(), [0, 1]);
    m.select(None, false);
    assert!(m.selected.is_empty(), "a press on open sea clears");
}

#[test]
fn a_drag_moves_the_selection_on_the_grid_and_undoes_in_one_step() {
    let mut m = map();
    m.select(Some(1), false);
    m.begin_move([500., 100.]);
    m.drag_to([505., 103.], RULES.grid);
    assert_eq!(
        m.slots()[1].at,
        [400, 0],
        "under half a grid step is no move"
    );
    m.drag_to([530., 140.], RULES.grid);
    m.drag_to([541., 171.], RULES.grid);
    assert_eq!(m.slots()[1].at, [448, 64], "snapped to 16 m");
    assert!(m.dirty());
    assert_eq!(
        m.map.variants[1].slots[1].at,
        [400, 0],
        "the other variant stays"
    );
    assert!(m.undo(false));
    assert_eq!(m.slots()[1].at, [400, 0], "one drag, one undo");
    assert!(m.undo(true));
    assert_eq!(m.slots()[1].at, [448, 64]);
}

#[test]
fn edit_all_variants_follows_the_shared_element() {
    let mut m = map();
    m.all_variants = true;
    m.select(Some(2), false);
    m.rotate_selected(&RULES);
    assert_eq!(m.slots()[2].turns, 0, "a pool slot doesn't turn");
    m.set_role(Role::Start, &RULES);
    assert_eq!(m.map.variants[1].slots[2].role, Role::Start);
    assert_eq!(m.map.variants[1].slots[2].raw_type, 1);
    assert_eq!(m.delete_selected(), 1);
    assert_eq!(m.slots().len(), 2);
    assert_eq!(m.map.variants[1].slots.len(), 2, "deleted in both");
}

#[test]
fn add_places_a_new_pool_slot_centred_and_classes_step() {
    let mut m = map();
    m.tool = Tool::Add;
    m.add_role = Role::Decoration;
    m.add_class = 0;
    let i = m.add_slot([1000., 1000.], &RULES).unwrap();
    let s = m.slots()[i];
    assert_eq!(s.at, [912, 912], "192 m centred on the press, on the grid");
    assert_eq!(
        (s.role, s.raw_type, s.source),
        (Role::Decoration, 2, Slot::NEW)
    );
    assert!(m.selected.has(i));
    m.step_class(1, &RULES);
    m.step_class(5, &RULES);
    assert_eq!(m.slots()[i].class, 2, "clamped at the largest class");
}

#[test]
fn an_island_dropped_on_a_slot_fixes_it_and_on_sea_adds_one() {
    let mut m = map();
    let facts = anno_core::map::IslandFacts {
        side: 256,
        regular: true,
        start: false,
        underwater: false,
        broken: false,
    };
    let path = "data/islands/a/a.a7m";
    m.drop_island([100., 100.], path, Some(facts), &RULES);
    let s = m.slots()[0];
    assert_eq!(m.map.island_of(&s), Some(path));
    assert_eq!(s.class, 1, "256 m takes the Medium class");
    assert_eq!(m.side(&s, &RULES), 256, "drawn at the island's own side");
    let i = m
        .drop_island([1600., 200.], path, Some(facts), &RULES)
        .unwrap();
    assert_eq!(m.slots()[i].at, [1472, 80], "200 - 128 = 72 rounds to 80");
    assert_eq!(m.map.islands.len(), 1, "one entry per island");
    m.select(Some(i), false);
    m.rotate_selected(&RULES);
    m.rotate_selected(&RULES);
    assert_eq!(m.slots()[i].turns, 2);
    m.back_to_pool(&RULES);
    assert!(!m.slots()[i].fixed());
}

#[test]
fn copying_to_the_other_variants_makes_new_elements_there() {
    let mut m = map();
    m.select(Some(0), false);
    m.delete_selected();
    m.copy_to_other_variants();
    let other = &m.map.variants[1].slots;
    assert_eq!(other.len(), 2);
    assert!(other.iter().all(|s| s.source == Slot::NEW));
    assert_eq!(other[0].at, [400, 0]);
}

#[test]
fn undo_keeps_at_most_its_ring() {
    let mut m = map();
    m.select(Some(0), false);
    for _ in 0..300 {
        m.rotate_selected(&RULES);
        m.step_class(1, &RULES);
    }
    let mut n = 0;
    while m.undo(false) {
        n += 1;
    }
    assert_eq!(n, super::UNDO);
}

#[test]
fn undo_back_to_the_save_is_clean_and_a_new_layout_stays_unsaved() {
    let mut m = map();
    assert!(m.dirty(), "a new layout is unsaved from the start");
    m.select(Some(0), false);
    m.step_class(1, &RULES);
    assert!(m.undo(false));
    assert!(m.dirty(), "undo can't make a never-saved layout saved");
    m.saved("data/maps/roman/twin/twin.a7t".into());
    assert!(!m.dirty());
    m.step_class(1, &RULES);
    assert!(m.dirty());
    assert!(m.undo(false));
    assert!(!m.dirty(), "back at the save");
    assert!(m.undo(true));
    assert!(m.dirty());
    assert!(m.undo(false));
    // A press on a slot or a start that never moves, and a typed value
    // entered as it was, are no edits.
    m.begin_move([100., 100.]);
    m.drag_to([101., 99.], RULES.grid);
    m.drag = None;
    m.set_position(0, m.slots()[0].at[0]);
    m.map.variants[0].starts.push([64, 64]);
    m.begin_start_move(0, [64., 64.]);
    m.drag_to([65., 64.], RULES.grid);
    assert!(!m.dirty());
    m.drag_to([200., 64.], RULES.grid);
    m.drag_to([300., 64.], RULES.grid);
    assert!(m.dirty());
    assert!(m.undo(false), "one step for the drag");
    assert!(!m.dirty());
}

#[test]
fn a_layout_clashes_by_name_or_by_the_files_it_writes() {
    use anno_core::map::LayoutEntry;
    use anno_core::project::MapEntry;
    let entry = |name: &str, stem: &str| LayoutEntry {
        name: name.into(),
        path: format!("data/maps/roman/{stem}_easy/{stem}_easy.a7t"),
        region: "Roman".into(),
        variants: vec![MapEntry {
            path: format!("data/maps/roman/{stem}_easy/{stem}_easy.a7t"),
            region: "Roman".into(),
            island_size: "Large".into(),
            source_guids: Vec::new(),
        }],
        guids: Vec::new(),
        fill_with_islands: false,
    };
    let layouts = [entry("Astra Straits", "astra_straits")];
    let files = |stem: &str| vec![format!("data/maps/roman/{stem}_easy/{stem}_easy.a7t")];
    let clash = |name: &str, own: Option<&str>| {
        let stem = crate::island::file_name(name);
        super::layout_clash(&layouts, name, &files(&stem), own)
    };
    assert!(
        clash("astra straits", None)
            .unwrap()
            .contains("named Astra Straits")
    );
    assert!(
        clash("Astra-Straits", None)
            .unwrap()
            .contains("same file name"),
        "two names, one stem"
    );
    assert_eq!(clash("Twin Straits", None), None);
    assert_eq!(
        clash("Astra Straits", Some(&layouts[0].path)),
        None,
        "the open layout saves over itself"
    );
}

#[test]
fn snapping_and_classes() {
    assert_eq!(snap(23.9, 16), 16);
    assert_eq!(snap(24., 16), 32);
    assert_eq!(snap(-9., 16), -16);
    assert_eq!(RULES.class_for(100), 0);
    assert_eq!(RULES.class_for(272), 1);
    assert_eq!(RULES.class_for(900), 2);
}

/// Layouts take GUIDs from the top of the islands' span, keep them across
/// saves, and islands numbered after them skip them.
#[test]
fn layouts_hold_guids_from_the_top_and_reopen_with_them() {
    let module: &'static dyn anno_core::game::Module = &anno117::MODULE;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("layouts");
    let mut project = Project::create(&folder, "Layouts", "layouts", module).unwrap();
    let base = project.guid_base().unwrap();
    let span = module.packaging().unwrap().guid_range.islands;
    let entry = |name: &str| anno_core::map::LayoutEntry {
        name: name.into(),
        path: format!("data/x/{name}/{name}_easy.a7t"),
        region: "Roman".into(),
        variants: ["easy", "medium", "hard"]
            .iter()
            .map(|v| anno_core::project::MapEntry {
                path: format!("data/x/{name}/{name}_{v}.a7t"),
                region: "Roman".into(),
                island_size: (*v).into(),
                source_guids: Vec::new(),
            })
            .collect(),
        guids: Vec::new(),
        fill_with_islands: false,
    };
    project.set_layout(entry("one"), Registers::NewMapType);
    let one = project.layouts[0].guids.clone();
    assert_eq!(
        one,
        [
            base + span - 1,
            base + span - 2,
            base + span - 3,
            base + span - 4
        ]
    );
    project.set_layout(entry("one"), Registers::NewMapType);
    assert_eq!(project.layouts.len(), 1);
    assert_eq!(project.layouts[0].guids, one, "a layout keeps its GUIDs");
    project.set_layout(entry("two"), Registers::NewMapType);
    assert_eq!(project.layouts[1].guids[0], base + span - 5);
    assert_eq!(
        project.island_guid("data/i/i.a7m"),
        base,
        "islands from the bottom"
    );
    project.write().unwrap();
    let back = Project::open(&folder, module).unwrap();
    assert_eq!(back.layouts, project.layouts);
}

/// The texts name each registered layout's map type, escaped.
#[test]
fn a_layout_names_its_map_type_in_every_language() {
    let module: &'static dyn anno_core::game::Module = &anno117::MODULE;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("named");
    let mut project = Project::create(&folder, "Named", "named", module).unwrap();
    project.set_layout(
        anno_core::map::LayoutEntry {
            name: "Rocks & Reefs".into(),
            path: "data/x/r/r.a7t".into(),
            region: "Roman".into(),
            variants: vec![anno_core::project::MapEntry {
                path: "data/x/r/r.a7t".into(),
                region: "Roman".into(),
                island_size: "Large".into(),
                source_guids: Vec::new(),
            }],
            guids: Vec::new(),
            fill_with_islands: false,
        },
        Registers::NewMapType,
    );
    let ty = project.layouts[0].guids[0];
    let folder = project.write().unwrap();
    let texts =
        std::fs::read_to_string(folder.join("data/base/config/gui/texts_german.xml")).unwrap();
    assert!(texts.contains(&format!("<LineId>{ty}</LineId>")), "{texts}");
    assert!(texts.contains("Rocks &amp; Reefs"), "{texts}");
}
/// The design's save and reopen test, in the editor: New map on the game's
/// default base, a start, a pool and a fixed slot (2070: a start island, an
/// island and an ark, having no pool), Ctrl S, then the project opened
/// again lists the layout with its GUIDs and reads back the same layout.
/// The editor on a new project in a fresh config folder, so no thumbnail
/// is cached, with the New map dialog open on the game's default base and
/// the island list loaded. None without the game's install. Bind `app` and
/// `_config` by name: a field left to `..` drops with the pattern, which
/// ends the app and deletes the folder.
struct Session {
    app: gpui::TestApp,
    w: gpui::TestAppWindow<crate::editor::Editor>,
    folder: std::path::PathBuf,
    _config: tempfile::TempDir,
}

impl Session {
    fn open(var: &str, module: &'static dyn anno_core::game::Module) -> Option<Session> {
        use crate::editor::Editor;
        let Some(dir) = anno_formats::vfs::install_dir(var) else {
            eprintln!("skip: {var} unset");
            return None;
        };
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().join("maps");
        let project = Project::create(&folder, "Maps", "maps-test", module).unwrap();
        let mut app = gpui::TestApp::new();
        let cfg = config.path().to_owned();
        let mut w = app.open_window(|window, cx| {
            Editor::new(project, cfg, Some(dir.clone()), None, window, cx)
        });
        app.run_until_parked();
        // The shipped maps for the dialog, and the island list the slots
        // take their islands' sides from.
        w.update(|ed, _, cx| {
            ed.open_new_map(cx);
            ed.map_facts_need_list(cx);
        });
        app.run_until_parked();
        Some(Session {
            app,
            w,
            folder,
            _config: config,
        })
    }
}

/// New map on the dialog's base, named `name` (the name its files take,
/// so the map read back names itself the same).
fn create_map(
    ed: &mut crate::editor::Editor,
    name: &str,
    cx: &mut gpui::Context<crate::editor::Editor>,
) {
    use gpui_editor_kit::field::{Filter, TextEdit};
    match &mut ed.map_dialog {
        Some(super::panels::Dialog::New(d)) => {
            assert!(!d.base.is_empty(), "a base sea is chosen");
            d.name = TextEdit::new((), name, Filter::Text);
        }
        _ => panic!("the New map dialog is open"),
    }
    ed.create_map(cx);
    assert!(ed.map.is_some(), "{:?}", ed.status_line());
}

fn a_new_map_saves_and_reopens(var: &str, module: &'static dyn anno_core::game::Module) {
    let Some(Session {
        mut w,
        folder,
        app: _app,
        _config,
    }) = Session::open(var, module)
    else {
        return;
    };
    let saved = w.update(|ed, _, cx| {
        let maps = ed.maps().expect("the game has maps");
        let rules = maps.rules();
        let install = ed.install.clone().unwrap();
        create_map(ed, "test_straits", cx);
        let islands = module.islands().unwrap();
        let shipped = islands.shipped(&install, ed.assets.as_deref());
        // The smallest, so a row of them keeps the game's gaps.
        let island = (shipped.iter())
            .filter(|s| s.pool && s.world[0] <= rules_largest(islands.rules().size_classes))
            .min_by_key(|s| s.world[0])
            .expect("a pool island");
        let classes = islands.rules().size_classes;
        let facts = super::island_facts(ed, &island.path);
        assert!(
            facts.is_some(),
            "the island list gives {} its side",
            island.path
        );
        let m = ed.map.as_mut().expect("the map opened");
        assert!(m.dirty(), "a new map opens unsaved");
        assert!(m.slots().is_empty(), "an empty sea");
        // In every variant at its own size (1800's worlds differ): as many
        // starts as the game's maps have at least, then a pool and a fixed
        // slot, each centred in a cell of a square grid, so no two come
        // closer than the game allows.
        let starts = usize::from(rules.min_starts.max(1));
        let cols = ((starts + 2) as f32).sqrt().ceil();
        for v in 0..m.map.variants.len() {
            m.show_variant(v);
            // Inside the playable area, which 1800 keeps well within the map.
            let [x0, y0, x1, y1] = m.playable().map(|v| v as f32);
            let cell = |n: usize| {
                let (c, r) = ((n as f32) % cols, (n as f32 / cols).floor());
                [
                    x0 + (x1 - x0) * (c + 0.5) / cols,
                    y0 + (y1 - y0) * (r + 0.5) / cols,
                ]
            };
            for n in 0..starts {
                let i = if rules.pool_slots {
                    m.add_role = Role::Start;
                    m.add_class = (classes.len() - 1) as u8;
                    m.add_slot(cell(n), rules).unwrap()
                } else {
                    m.drop_island(cell(n), &island.path, facts, rules).unwrap()
                };
                m.select(Some(i), false);
                m.set_role(Role::Start, rules);
            }
            if rules.pool_slots {
                m.add_role = Role::Normal;
                m.add_class = 0;
                m.add_slot(cell(starts), rules).unwrap();
            }
            m.drop_island(cell(starts + 1), &island.path, facts, rules)
                .unwrap();
        }
        m.show_variant(0);
        let [w, h] = m.size().map(|v| v as f32);
        if rules.free_starts {
            m.add_start([w * 0.3, h * 0.6], rules.grid);
        }
        ed.save();
        let (status, error) = ed.status_line();
        assert!(!error, "{status}");
        let m = ed.map.as_ref().unwrap();
        assert!(!m.dirty(), "saved");
        m.map.clone()
    });
    let back = Project::open(&folder, module).unwrap();
    assert_eq!(back.layouts.len(), 1, "the layout is listed");
    let layout = &back.layouts[0];
    let rules = module.islands().unwrap().maps().unwrap().rules();
    assert_eq!(
        layout.guids.len(),
        rules.registers.guids(saved.variants.len())
    );
    assert_eq!(layout.variants.len(), saved.variants.len());
    w.update(|ed, _, _| {
        let install = ed.install.clone().unwrap();
        let read = super::read_layout(ed.maps().unwrap(), &install, layout).unwrap();
        assert!(
            read.same_layout(&saved),
            "read back:\n{read:#?}\nsaved:\n{saved:#?}"
        );
    });
}

fn rules_largest(classes: &[SizeClass]) -> u32 {
    classes.iter().map(|c| c.slot).max().unwrap_or(0)
}

#[test]
fn a_new_117_map_saves_and_reopens() {
    a_new_map_saves_and_reopens("ANNO117_DIR", &anno117::MODULE);
}

#[test]
fn a_new_1800_map_saves_and_reopens() {
    a_new_map_saves_and_reopens("ANNO1800_DIR", &anno1800::MODULE);
}

#[test]
fn a_new_2070_map_saves_and_reopens() {
    a_new_map_saves_and_reopens("ANNO2070_DIR", &anno2070::MODULE);
}

/// What a problem names is what a click on it shows: its slots selected in
/// their variant, a variant shown with nothing selected, or for the whole
/// map nothing selected; its words name the variants or setups.
#[test]
fn a_problem_shows_the_slots_variant_or_map_it_names() {
    use anno_core::island::{Level, Names, Problem};
    use anno_core::map::slot_item;
    let mut m = map();
    let problem = |level, what: &str, names, items: Vec<u64>| Problem {
        level,
        what: what.into(),
        count: items.len(),
        items,
        names,
    };
    m.problems = vec![
        problem(
            Level::Broken,
            "Slots too close to another",
            Names::Slots,
            vec![slot_item(1, 0), slot_item(1, 2)],
        ),
        problem(
            Level::Broken,
            "Variants with too few starts",
            Names::Variants,
            vec![1],
        ),
        problem(
            Level::Gameplay,
            "Setup options no variant answers",
            Names::Setups,
            vec![1],
        ),
    ];
    assert!(!m.variant_has_problems(0) && m.variant_has_problems(1));
    assert_eq!(m.problems_of(0).count(), 0, "variant 0 is shown");

    m.select_problem(0);
    assert_eq!(m.variant, 1, "the variant its slots are in");
    assert_eq!(m.selected.iter().collect::<Vec<_>>(), [0, 2]);
    assert_eq!(m.problems_of(2).count(), 1);
    assert_eq!(
        m.problems_of(1).count(),
        0,
        "a variant's problem is no slot's"
    );

    m.show_variant(0);
    m.select(Some(1), false);
    m.select_problem(1);
    assert_eq!(m.variant, 1);
    assert!(
        m.selected.is_empty(),
        "the variant shown, no slot taken for it"
    );

    m.select(Some(1), false);
    m.select_problem(2);
    assert_eq!(m.variant, 1, "the whole map's keeps the variant shown");
    assert!(m.selected.is_empty(), "the inspector shows the map");

    let words: Vec<String> = (m.problems.iter())
        .map(|p| m.problem_words(p, &RULES))
        .collect();
    assert_eq!(
        words,
        [
            "2 × Slots too close to another",
            "Variants with too few starts · Small islands",
            "Setup options no variant answers · the whole map lacks Small islands",
        ]
    );
    m.show_variant(0);
    assert_eq!(
        m.problem_words(&m.problems[0], &RULES),
        "2 × Slots too close to another · Small islands",
        "slots of a variant not shown name it"
    );
}

/// 1800's Plays as is the project's to choose after New map: a layout made
/// on one Archipelago map plays as another of its region and setup instead,
/// saves, reopens playing as it, and registers as a redirect of that map
/// alone. Undo takes the choice back.
#[test]
fn an_1800_layout_plays_as_another_map_after_saving_and_reopening() {
    let Some(Session {
        mut w,
        folder,
        app: _app,
        _config,
    }) = Session::open("ANNO1800_DIR", &anno1800::MODULE)
    else {
        return;
    };
    let (old, new) = w.update(|ed, _, cx| {
        create_map(ed, "test_plays_as", cx);
        let maps = ed.maps().unwrap();
        ed.list_plays_as(maps);
        let m = ed.map.as_mut().unwrap();
        let old = m.map.variants[0].replaces.clone();
        assert!(
            !old.is_empty(),
            "a new layout plays as the map it was made on"
        );
        let offered = &m.plays_as.as_ref().expect("assets are in")[0];
        let new = (offered.iter())
            .find(|p| !old.contains(&p.guid))
            .expect("another map of the region and setup")
            .guid;
        for &g in &old {
            m.set_plays_as(g, false);
        }
        m.set_plays_as(new, true);
        assert_eq!(m.map.variants[0].replaces, [new]);
        // Each click is one step: undo takes back the last.
        assert!(m.undo(false));
        assert!(m.map.variants[0].replaces.is_empty(), "undo");
        assert!(m.undo(true));
        ed.save();
        assert!(!ed.map.as_ref().unwrap().dirty(), "{:?}", ed.status_line());
        (old, new)
    });
    let back = Project::open(&folder, &anno1800::MODULE).unwrap();
    let layout = &back.layouts[0];
    assert_eq!(layout.variants[0].source_guids, [new]);
    w.update(|ed, _, _| {
        let maps = ed.maps().unwrap();
        let install = ed.install.clone().unwrap();
        let read = super::read_layout(maps, &install, layout).unwrap();
        assert_eq!(read.variants[0].replaces, [new], "reopened, it plays as it");
        let xml = maps.register(&back.layouts, &[], 0);
        assert!(xml.contains(&format!("GUID=\"{new}\"")), "{xml}");
        for g in &old {
            assert!(
                !xml.contains(&format!("{g}")),
                "{g} is left to the game:\n{xml}"
            );
        }
    });
}

/// The Islands tab draws its islands' pictures without the Library ever
/// opening: it starts the Library's worker, whose cache both share, and
/// asks for the rows it shows.
#[test]
fn the_islands_tab_has_thumbnails_without_the_library() {
    let Some(Session {
        mut w,
        app: _app,
        _config,
        ..
    }) = Session::open("ANNO117_DIR", &anno117::MODULE)
    else {
        return;
    };
    let first = w.update(|ed, window, cx| {
        create_map(ed, "test_thumbs", cx);
        // As if no pane had started the worker (the project overview may
        // have, in drawing the first frame).
        ed.thumbs = None;
        let region = ed.map.as_ref().unwrap().map.region.to_ascii_lowercase();
        let first = (ed
            .island_list
            .as_deref()
            .expect("the island list is in")
            .iter())
        .find(|s| s.region.eq_ignore_ascii_case(&region) && s.path.ends_with(".a7m"))
        .expect("a shipped island of the region")
        .path
        .clone();
        let _ = ed.render_map_islands(window, cx);
        assert!(ed.thumbs.is_some(), "the tab started the worker");
        first
    });
    // The worker reads each island's map picture: no GPU, a few ms each,
    // and the rows shown ask first.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let got = w.update(|ed, _, _| {
            let thumbs = ed.thumbs.as_mut().unwrap();
            thumbs.receive();
            thumbs.get_small(&first).is_some()
        });
        if got {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no picture of {first} in 60 s"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The first shipped map the game lists, by path.
fn first_shipped(ed: &crate::editor::Editor) -> String {
    let list = ed
        .shipped_maps
        .as_deref()
        .expect("the shipped maps are listed");
    list.first().expect("a shipped map").path.clone()
}

/// An edit any layout takes: the shown variant's first slot a grid step
/// east, or on an empty sea a new pool slot in the middle.
fn nudge(ed: &mut crate::editor::Editor) {
    let rules = ed.maps().unwrap().rules();
    let m = ed.map.as_mut().expect("a map is open");
    if m.slots().is_empty() {
        let [x0, y0, x1, y1] = m.playable().map(|v| v as f32);
        m.add_slot([(x0 + x1) / 2., (y0 + y1) / 2.], rules).unwrap();
    } else {
        m.select(Some(0), false);
        let x = m.slots()[0].at[0];
        m.set_position(0, x + rules.grid as i32);
    }
    assert!(m.dirty());
}

/// Session::open with its New map dialog closed again.
fn session_117() -> Option<Session> {
    let mut s = Session::open("ANNO117_DIR", &anno117::MODULE)?;
    s.w.update(|ed, _, _| ed.map_dialog = None);
    Some(s)
}

// L1: New map layout over a map with unsaved edits asks first. Cancel, and
// Discard followed by cancelling the dialog, keep the map with its undo;
// Save writes it, then opens the dialog.
#[test]
fn a_new_map_over_an_edited_one_asks_and_keeps_it_until_create() {
    use crate::editor::{Answer, Leave};
    let Some(Session {
        mut w,
        folder,
        app: _app,
        _config,
    }) = Session::open("ANNO117_DIR", &anno117::MODULE)
    else {
        return;
    };
    w.update(|ed, _, cx| {
        create_map(ed, "Twin Straits", cx);
        nudge(ed);
        ed.open_new_map(cx);
        assert_eq!(ed.leaving, Some(Leave::NewMap), "asked first");
        assert!(ed.map_dialog.is_none());
        ed.answer_leave(Answer::Cancel, cx);
        let m = ed.map.as_ref().unwrap();
        assert_eq!(m.map.name, "Twin Straits");
        assert!(m.dirty() && m.slots().len() == 1, "Cancel keeps it");

        ed.open_new_map(cx);
        ed.answer_leave(Answer::Discard, cx);
        assert!(
            matches!(ed.map_dialog, Some(super::panels::Dialog::New(_))),
            "Discard opens the dialog"
        );
        assert!(ed.map.as_ref().unwrap().dirty(), "kept until Create");
    });
    w.simulate_keystrokes("escape");
    w.update(|ed, _, cx| {
        assert!(ed.map_dialog.is_none());
        let m = ed.map.as_mut().unwrap();
        assert_eq!(m.map.name, "Twin Straits", "cancelling the dialog keeps it");
        assert!(m.undo(false) && m.slots().is_empty(), "with its undo");
        assert!(m.undo(true));

        ed.open_new_map(cx);
        ed.answer_leave(Answer::Save, cx);
        assert!(!ed.map.as_ref().unwrap().dirty(), "{:?}", ed.status_line());
        assert!(matches!(ed.map_dialog, Some(super::panels::Dialog::New(_))));
    });
    let back = Project::open(&folder, &anno117::MODULE).unwrap();
    assert_eq!(back.layouts.len(), 1);
    assert_eq!(back.layouts[0].name, "Twin Straits");
}

// L2: Save and switch on an edited shipped map waits for the question of
// how it saves, writes it once answered and only then switches; switching
// project leaves no question behind. Save and quit, with the question
// cancelled, quits nothing and keeps the edits.
#[test]
fn save_and_go_on_a_shipped_map_waits_for_its_question() {
    use crate::editor::{Answer, Leave};
    let Some(Session {
        mut w,
        folder,
        app: _app,
        _config,
    }) = session_117()
    else {
        return;
    };
    let other = _config.path().join("other");
    Project::create(&other, "Other", "other-test", &anno117::MODULE).unwrap();
    w.update(|ed, _, cx| {
        let path = first_shipped(ed);
        ed.open_map(&path, cx);
        assert!(
            ed.map.as_ref().is_some_and(|m| m.shipped),
            "{:?}",
            ed.status_line()
        );
        nudge(ed);
        ed.leave(Leave::Quit, cx);
        ed.answer_leave(Answer::Save, cx);
        match &ed.map_dialog {
            Some(super::panels::Dialog::SaveShipped(d)) => {
                assert_eq!(d.then, Some(Leave::Quit), "the quit waits")
            }
            _ => panic!("the shipped-map question is open"),
        }
    });
    w.simulate_keystrokes("escape");
    w.update(|ed, _, cx| {
        assert!(ed.map_dialog.is_none() && ed.leaving.is_none());
        assert!(ed.map.as_ref().unwrap().dirty(), "Cancel keeps the edits");
        assert!(ed.project.layouts.is_empty(), "and saves no layout");

        ed.leave(Leave::Project(other.clone()), cx);
        ed.answer_leave(Answer::Save, cx);
        assert_eq!(ed.project.folder(), folder, "not switched yet");
    });
    // Enter answers Save as new layout, under the name it offers.
    w.simulate_keystrokes("enter");
    w.read(|ed, _| {
        assert_eq!(ed.project.folder(), other, "{:?}", ed.status_line());
        assert!(ed.map.is_none() && ed.map_dialog.is_none());
    });
    let back = Project::open(&folder, &anno117::MODULE).unwrap();
    assert_eq!(back.layouts.len(), 1, "the map was written first");
    assert!(
        back.layouts[0].name.ends_with(" custom"),
        "{}",
        back.layouts[0].name
    );
}

// L3: no save replaces another layout: two names that make one stem are
// refused at New, a second Save as new layout of the same shipped map
// offers a free name and refuses the taken one, and a reopened layout
// shows the name it was saved under.
#[test]
fn no_layout_replaces_another() {
    use gpui_editor_kit::field::{Filter, TextEdit};
    let Some(Session {
        mut w,
        folder,
        app,
        _config,
    }) = Session::open("ANNO117_DIR", &anno117::MODULE)
    else {
        return;
    };
    w.update(|ed, _, cx| {
        create_map(ed, "Astra Straits", cx);
        nudge(ed);
        assert_eq!(
            ed.save(),
            crate::editor::Saved::Written,
            "{:?}",
            ed.status_line()
        );
        ed.open_new_map(cx);
    });
    app.run_until_parked();
    w.update(|ed, _, cx| {
        if let Some(super::panels::Dialog::New(d)) = &mut ed.map_dialog {
            d.name = TextEdit::new((), "Astra-Straits", Filter::Text);
        }
        ed.create_map(cx);
        let (status, error) = ed.status_line();
        assert!(error && status.contains("same file name"), "{status}");
        assert!(ed.map_dialog.is_some(), "the dialog stays to rename");
        ed.map_dialog = None;

        let shipped = first_shipped(ed);
        for n in 0..2 {
            ed.open_map(&shipped, cx);
            assert!(ed.map.as_ref().unwrap().shipped, "{:?}", ed.status_line());
            nudge(ed);
            ed.save();
            let Some(super::panels::Dialog::SaveShipped(d)) = &mut ed.map_dialog else {
                panic!("the shipped-map question is open");
            };
            let offered = d.name.text.clone();
            if n == 1 {
                assert!(offered.ends_with(" custom 2"), "a free name: {offered}");
                let first = ed.project.layouts[1].name.clone();
                d.name = TextEdit::new((), first.to_uppercase(), Filter::Text);
                ed.save_shipped_as(super::SaveAs::NewLayout, cx);
                assert!(ed.map_dialog.is_some(), "a taken name is refused");
                assert!(ed.status_line().1);
                if let Some(super::panels::Dialog::SaveShipped(d)) = &mut ed.map_dialog {
                    d.name = TextEdit::new((), offered, Filter::Text);
                }
            }
            ed.save_shipped_as(super::SaveAs::NewLayout, cx);
            assert!(ed.map_dialog.is_none(), "{:?}", ed.status_line());
        }
        assert_eq!(ed.project.layouts.len(), 3);

        let astra = ed.project.layouts[0].path.clone();
        ed.open_map(&astra, cx);
        assert_eq!(ed.map.as_ref().unwrap().map.name, "Astra Straits");
    });
    let back = Project::open(&folder, &anno117::MODULE).unwrap();
    let names: Vec<&str> = back.layouts.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    let mut paths: Vec<&str> = back.layouts.iter().map(|l| l.path.as_str()).collect();
    paths.dedup();
    assert_eq!(paths.len(), 3, "three layouts, three files");
}

// L4: a value typed into X but not entered counts as an edit when leaving:
// it is applied, so the ask comes. One that isn't a number stops the leave
// and stays typed.
#[test]
fn a_typed_slot_value_is_applied_before_leaving() {
    use crate::editor::Leave;
    use gpui_editor_kit::field::{Filter, TextEdit};
    let Some(Session {
        mut w,
        app: _app,
        _config,
        ..
    }) = session_117()
    else {
        return;
    };
    w.update(|ed, _, cx| {
        let path = first_shipped(ed);
        ed.open_map(&path, cx);
        let m = ed.map.as_mut().unwrap();
        m.select(Some(0), false);
        let x = m.slots()[0].at[0] + 64;
        m.typing = Some(TextEdit::new(0, x.to_string(), Filter::Number));
        ed.leave(Leave::Game, cx);
        assert_eq!(ed.leaving, Some(Leave::Game), "the typed value is an edit");
        let m = ed.map.as_mut().unwrap();
        assert_eq!(m.slots()[0].at[0], x);
        assert!(m.typing.is_none() && m.dirty());

        ed.leaving = None;
        let m = ed.map.as_mut().unwrap();
        m.typing = Some(TextEdit::new(0, "-", Filter::Number));
        ed.leave(Leave::Game, cx);
        assert_eq!(ed.leaving, None, "not left");
        let m = ed.map.as_ref().unwrap();
        assert_eq!(m.typing.as_ref().map(|t| t.text.as_str()), Some("-"));
        assert!(ed.status_line().1);
    });
}

// Open another map, close, and a map that doesn't read: each asks about
// the edited map, and Cancel, Discard and Save do what they say. The same
// map opens with no ask, edits and all.
#[test]
fn opening_or_closing_over_an_edited_map_asks() {
    use crate::editor::{Answer, Leave, Workspace};
    let Some(Session {
        mut w,
        app: _app,
        _config,
        ..
    }) = Session::open("ANNO117_DIR", &anno117::MODULE)
    else {
        return;
    };
    w.update(|ed, _, cx| {
        create_map(ed, "Twin Straits", cx);
        nudge(ed);
        ed.switch(Workspace::Project);
        let shipped = first_shipped(ed);

        // A new layout has no path to open again; save it, then edit.
        assert_eq!(ed.save(), crate::editor::Saved::Written);
        nudge(ed);
        let own = ed.map.as_ref().unwrap().path.clone().unwrap();
        ed.open_map(&own, cx);
        assert_eq!(ed.leaving, None, "the same map just shows");
        assert_eq!(ed.workspace, Workspace::Map);
        assert!(ed.map.as_ref().unwrap().dirty());

        ed.open_map(&shipped, cx);
        assert_eq!(ed.leaving, Some(Leave::Map(shipped.clone())));
        ed.answer_leave(Answer::Cancel, cx);
        assert_eq!(ed.map.as_ref().unwrap().map.name, "Twin Straits");

        ed.open_map("data/base/no_such_map.a7t", cx);
        ed.answer_leave(Answer::Discard, cx);
        let m = ed.map.as_ref().unwrap();
        assert!(
            m.map.name == "Twin Straits" && m.dirty(),
            "a failed read keeps it"
        );

        ed.open_map(&shipped, cx);
        ed.answer_leave(Answer::Discard, cx);
        assert!(ed.map.as_ref().unwrap().shipped, "Discard opens the other");
        assert!(!ed.map.as_ref().unwrap().dirty());

        ed.open_map(&own, cx);
        nudge(ed);
        ed.leave(Leave::CloseMap, cx);
        assert_eq!(ed.leaving, Some(Leave::CloseMap));
        ed.answer_leave(Answer::Save, cx);
        assert!(ed.map.is_none(), "saved, then closed");
        ed.open_map(&own, cx);
        nudge(ed);
        ed.leave(Leave::CloseMap, cx);
        ed.answer_leave(Answer::Discard, cx);
        assert!(ed.map.is_none());
        ed.open_map(&own, cx);
        assert!(!ed.map.as_ref().unwrap().dirty(), "the edit was discarded");
    });
}

// One Open dialog: every project layout first, the open one marked, then
// the shipped maps by region. Typing filters with the field focused from
// the start; Down and Enter open the row picked.
#[test]
fn the_open_dialog_lists_layouts_first_and_filters_by_typing() {
    let Some(Session {
        mut w,
        app,
        _config,
        ..
    }) = Session::open("ANNO117_DIR", &anno117::MODULE)
    else {
        return;
    };
    w.update(|ed, _, cx| {
        create_map(ed, "Twin Straits", cx);
        nudge(ed);
        ed.save();
        ed.open_new_map(cx);
    });
    app.run_until_parked();
    w.update(|ed, _, cx| {
        create_map(ed, "Twin Reefs", cx);
        nudge(ed);
        ed.save();
        ed.open_map_dialog(cx);
        let rows = ed.open_rows("");
        let names: Vec<(&str, bool)> = (rows.iter())
            .filter_map(|r| match r {
                super::panels::OpenRow::Map { name, open, .. } => Some((name.as_str(), *open)),
                super::panels::OpenRow::Heading(_) => None,
            })
            .collect();
        assert_eq!(names[..2], [("Twin Straits", false), ("Twin Reefs", true)]);
        assert!(names.len() > 2, "the shipped maps follow");
        assert!(ed.active_edit().is_some(), "the filter takes the keys");
    });
    w.simulate_keystrokes("t w i n space s t r");
    w.read(|ed, _| {
        let Some(super::panels::Dialog::Open(d)) = &ed.map_dialog else {
            panic!("the Open dialog is open");
        };
        assert_eq!(d.filter.text, "twin str");
        let maps = (ed.open_rows(&d.filter.text).into_iter())
            .filter(|r| matches!(r, super::panels::OpenRow::Map { .. }))
            .count();
        assert_eq!(maps, 1, "only Twin Straits is left");
    });
    // Back to "twin": both layouts. Down twice stops on the last, Up
    // picks the first, which isn't the open one.
    w.simulate_keystrokes("backspace backspace backspace backspace down down");
    w.read(|ed, _| match &ed.map_dialog {
        Some(super::panels::Dialog::Open(d)) => assert_eq!(d.at, 1),
        _ => panic!("the Open dialog is open"),
    });
    w.simulate_keystrokes("up enter");
    w.read(|ed, _| {
        assert!(ed.map_dialog.is_none());
        assert_eq!(
            ed.map.as_ref().unwrap().map.name,
            "Twin Straits",
            "{:?}",
            ed.status_line()
        );
    });
}

// Assets opens a map as it does an island or a screen: a double-click or
// the preview's action goes through the same dispatch.
#[test]
fn assets_open_a_map() {
    use crate::editor::Workspace;
    let Some(Session {
        mut w,
        app: _app,
        _config,
        ..
    }) = session_117()
    else {
        return;
    };
    w.update(|ed, _, cx| {
        let path = first_shipped(ed);
        ed.switch(Workspace::Assets);
        ed.browser.select_path(&path);
        assert!(ed.browser.selected.is_some(), "{path} is in the browser");
        ed.open_selected(cx);
        assert_eq!(ed.workspace, Workspace::Map);
        assert!(
            ed.map.as_ref().is_some_and(|m| m.is_at(&path)),
            "{:?}",
            ed.status_line()
        );
    });
}

// A save that only warns says so in one message, not as an error.
#[test]
fn a_save_that_warns_says_saved_once() {
    let Some(Session {
        mut w,
        app: _app,
        _config,
        ..
    }) = Session::open("ANNO117_DIR", &anno117::MODULE)
    else {
        return;
    };
    w.update(|ed, _, cx| {
        // An empty sea: no starts, which the game can't load.
        create_map(ed, "Empty Sea", cx);
        assert_eq!(ed.save(), crate::editor::Saved::Written);
        let (status, error) = ed.status_line();
        assert!(!error, "{status}");
        assert_eq!(status.matches("Saved").count(), 1, "{status}");
        assert!(status.contains("can't load Empty Sea yet"), "{status}");
    });
}
