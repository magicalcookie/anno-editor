// Window tests: the editor in GPUI's test platform, driven by keys as a
// modder would, each with a config folder of its own. They need the game
// (ANNO117_DIR).

use super::{Editor, Game, Pane};
use crate::ui::Keys;
use gpui::{MouseButton, TestApp, TestAppWindow, point, px};
use gpui_editor_kit::dock::{close_pane, fold_pane};

const ICON_ROW: &str = "data/ui/studio/generated/5dc6f943-738a-4d18-b2b9-b686277c3d15";

type Window = TestAppWindow<Editor>;

/// The editor showing the icon row with its Icon selected, and a fresh
/// config folder (layouts, recent projects, the UI index), never the
/// modder's and never another test's: kept until the test ends (round 13:
/// one shared folder, wiped by every test process, broke concurrent runs).
fn icon_row() -> Option<(TestApp, Window, tempfile::TempDir)> {
    let game = std::env::var_os("ANNO117_DIR")?;
    let mut app = TestApp::new();
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_path_buf();
    let mut w = app.open_window(|window, cx| {
        let open = Some(ICON_ROW.into());
        Editor::new(Game::Anno117, dir, Some(game.into()), open, window, cx)
    });
    app.run_until_parked();
    w.update(|ed, _, _| ed.ui.as_mut().unwrap().select_named("Icon"));
    Some((app, w, config))
}

/// Shows `pane` maximized.
fn maximize(ed: &mut Editor, pane: Pane) {
    ed.show_pane(pane);
    let i = ed.dock_index();
    ed.docks[i].toggle_maximized(pane);
}

/// Runs until parked, then draws the frame after the pane's size is known
/// (the window asks for it on the next frame, which the test platform never
/// draws by itself): the graph is framed.
fn draw(app: &mut TestApp, w: &mut Window) {
    app.run_until_parked();
    w.update(|_, _, cx| cx.notify());
    app.run_until_parked();
}

// The "24 × 24 follows MOBO_02" link in the inspector shows the graph, and
// the graph takes the keys: Delete there never deletes the widget (round
// 12: the switch handed the keys back to the inspector it was clicked in).
// Esc in the graph hands the keys to the canvas when it shows, through the
// dock; in a maximized Bindings pane or Timeline they stay, and Delete
// never reaches the hidden widget (round 12: it deleted Icon).
#[test]
fn a_shown_or_maximized_pane_keeps_the_keys() {
    let Some((_app, mut w, _config)) = icon_row() else {
        return;
    };
    let count = w.read(|ed, _| ed.ui.as_ref().unwrap().widget_count());
    let nothing_deleted = |w: &Window, keys: Keys| {
        w.read(|ed, _| {
            let ui = ed.ui.as_ref().unwrap();
            assert_eq!(ui.widget_count(), count, "{:?}", ui.message);
            assert_eq!(ui.keys, keys);
        })
    };
    // A press in the inspector, then the link.
    w.update(|ed, _, cx| {
        let i = ed.dock_index();
        ed.docks[i].focus(Pane::UiInspector);
        ed.keys_to(Keys::Canvas);
        ed.show_pane(Pane::UiBindings);
        cx.notify();
    });
    w.simulate_keystrokes("delete");
    nothing_deleted(&w, Keys::Graph);
    w.update(|ed, _, cx| {
        maximize(ed, Pane::UiBindings);
        cx.notify();
    });
    w.simulate_keystrokes("escape delete");
    nothing_deleted(&w, Keys::Graph);
    // Docked again, Esc gives the canvas the keys and the dock's focus.
    w.update(|ed, _, cx| {
        let i = ed.dock_index();
        ed.docks[i].toggle_maximized(Pane::UiBindings);
        cx.notify();
    });
    w.simulate_keystrokes("escape");
    w.read(|ed, _| {
        assert_eq!(ed.ui.as_ref().unwrap().keys, Keys::Canvas);
        assert_eq!(ed.docks[ed.dock_index()].focused, Pane::UiCanvas);
    });
    // The same from a maximized Timeline.
    w.update(|ed, _, cx| {
        maximize(ed, Pane::UiTimeline);
        cx.notify();
    });
    w.simulate_keystrokes("escape delete");
    nothing_deleted(&w, Keys::Timeline);
}

// Folding or closing a pane is a press on the dock: a value being typed in
// the inspector is applied and takes no more digits (round 12: 123, fold
// the Timeline, 4 gave 1234; round 13: X 77, close the Widget tree, 4 gave
// 774). Reset panel layout ends it too and hands the keys to the reset
// dock's focus, from the graph as well (round 13: 12, reset, 4 gave 124).
#[test]
fn dock_changes_end_typing() {
    let Some((_app, mut w, _config)) = icon_row() else {
        return;
    };
    let applied = |w: &Window, x: f32| {
        w.read(|ed, _| {
            let ui = ed.ui.as_ref().unwrap();
            assert_eq!(ui.typed(), None);
            assert_eq!(ui.position_x(), x, "{:?}", ui.message);
        })
    };
    w.update(|ed, _, cx| {
        let i = ed.dock_index();
        ed.docks[i].focus(Pane::UiInspector);
        ed.keys_to(Keys::Canvas);
        ed.ui.as_mut().unwrap().type_position_x("123");
        fold_pane(ed, Pane::UiTimeline, cx);
    });
    w.simulate_keystrokes("4 enter");
    applied(&w, 123.);
    w.update(|ed, _, cx| {
        let i = ed.dock_index();
        ed.docks[i].focus(Pane::UiInspector);
        ed.keys_to(Keys::Canvas);
        ed.ui.as_mut().unwrap().type_position_x("77");
        close_pane(ed, Pane::UiTree, cx);
    });
    w.simulate_keystrokes("4 enter");
    applied(&w, 77.);
    w.update(|ed, _, _| {
        ed.show_pane(Pane::UiBindings);
        assert_eq!(ed.ui.as_ref().unwrap().keys, Keys::Graph);
        ed.ui.as_mut().unwrap().type_position_x("12");
        ed.reset_layout();
    });
    w.simulate_keystrokes("4 enter");
    applied(&w, 12.);
    w.read(|ed, _| {
        let dock = &ed.docks[ed.dock_index()];
        assert_eq!(
            Some(ed.ui.as_ref().unwrap().keys),
            ed.pane_keys(dock.focused)
        );
    });
}

// Folding the pane with the keys hands them on, as a close does: Delete
// then acts on what shows, never on the folded Timeline (round 12 deleted
// a key out of sight). A maximized pane folded is restored first: folded
// over the whole window it left it blank, and Delete reached the hidden
// Timeline (round 13).
#[test]
fn folding_a_pane_hands_its_keys_on() {
    let Some((_app, mut w, _config)) = icon_row() else {
        return;
    };
    let handed_on = |w: &Window| {
        w.read(|ed, _| {
            let dock = &ed.docks[ed.dock_index()];
            assert_ne!(dock.focused, Pane::UiTimeline);
            assert!(dock.visible_panes().contains(&dock.focused));
            assert_eq!(ed.ui.as_ref().unwrap().keys, Keys::Canvas);
        })
    };
    w.update(|ed, _, cx| {
        ed.show_pane(Pane::UiTimeline);
        assert_eq!(ed.ui.as_ref().unwrap().keys, Keys::Timeline);
        fold_pane(ed, Pane::UiTimeline, cx);
    });
    handed_on(&w);
    w.update(|ed, _, cx| {
        maximize(ed, Pane::UiTimeline);
        fold_pane(ed, Pane::UiTimeline, cx);
    });
    handed_on(&w);
    w.read(|ed, _| {
        let dock = &ed.docks[ed.dock_index()];
        assert_eq!(dock.maximized, None, "restored");
        assert!(!dock.visible_panes().contains(&Pane::UiTimeline));
    });
}

// Ctrl S applies a half-typed value and saves; one the field refuses stays
// as typed, with its error, and nothing is saved (round 12: "-" was
// dropped and the status said "Saved 0 documents").
#[test]
fn a_refused_value_holds_the_save() {
    let Some((_app, mut w, _config)) = icon_row() else {
        return;
    };
    w.update(|ed, _, _| {
        ed.keys_to(Keys::Canvas);
        ed.ui.as_mut().unwrap().type_position_x("-");
    });
    w.simulate_keystrokes("ctrl-s");
    w.read(|ed, _| {
        let ui = ed.ui.as_ref().unwrap();
        assert_eq!(ui.typed().as_deref(), Some("-"), "kept as typed");
        assert!(
            ed.error && ed.status.starts_with("Not saved"),
            "{}",
            ed.status
        );
        assert!(!ui.dirty());
    });
    // Corrected, the same chord applies it and saves.
    w.update(|ed, _, _| ed.ui.as_mut().unwrap().type_position_x("7"));
    w.simulate_keystrokes("ctrl-s");
    w.read(|ed, _| {
        let ui = ed.ui.as_ref().unwrap();
        assert_eq!(ui.typed(), None);
        assert_eq!(ui.position_x(), 7.);
        assert!(
            !ui.dirty() && ed.status.starts_with("Saved"),
            "{}",
            ed.status
        );
    });
}

// A click inside a pin's field being typed places the caret: the value is
// not applied and typing goes on (round 13: the node box's press applied
// it, and the next keys went to the graph).
#[test]
fn a_click_inside_a_pin_field_places_the_caret() {
    let Some((app, mut w, _config)) = icon_row() else {
        return;
    };
    w.update(|ed, window, cx| {
        ed.ui.as_mut().unwrap().show_graph();
        maximize(ed, Pane::UiBindings);
        let ui = ed.ui.as_mut().unwrap();
        let span = ui.pin_constant("Equal", "b").expect("b's text");
        let text = gpui_editor_kit::field::Filter::Text;
        ed.start_span_edit(span, Keys::Graph, text, window, cx);
        cx.notify();
    });
    w.simulate_input("Long");
    app.run_until_parked();
    let caret = gpui_editor_kit::field::caret_bounds().expect("the field is drawn");
    w.simulate_click(caret.center(), MouseButton::Left);
    w.simulate_input("er");
    w.read(|ed, _| {
        let ui = ed.ui.as_ref().unwrap();
        let typed = ui.typed_record();
        assert_eq!(typed.as_deref(), Some("Longer"), "{:?}", ui.message);
    });
}

// In All in file, a click on a widget node's title selects that widget as
// one on its body does, and the view keeps its zoom (round 14: the title
// press started a move and the click never came; every selection framed
// the graph anew).
#[test]
fn a_click_on_a_widget_title_selects_it_where_the_view_is() {
    let Some((mut app, mut w, _config)) = icon_row() else {
        return;
    };
    w.update(|ed, _, cx| {
        ed.ui.as_mut().unwrap().show_all_in_file();
        maximize(ed, Pane::UiBindings);
        cx.notify();
    });
    draw(&mut app, &mut w);
    // A widget wired with Icon, on the pane as All in file opens (Text's
    // layout inputs are its view model's node since round 17).
    let (title, at, zoom) = w.read(|ed, _| {
        let ui = ed.ui.as_ref().unwrap();
        let (title, at) = ["Text", "Group_IconText", "BgIconText", "TEST_Debug_Holder"]
            .into_iter()
            .find_map(|t| Some((t, ui.node_title_at(t)?)))
            .expect("a widget wired with Icon shows");
        (title, at, ui.graph_zoom())
    });
    w.simulate_click(at, MouseButton::Left);
    app.run_until_parked();
    w.read(|ed, _| {
        let ui = ed.ui.as_ref().unwrap();
        assert_eq!(ui.selected_name().as_deref(), Some(title));
        assert_eq!(ui.graph_zoom(), zoom);
        // It was on the pane: nothing moves under the pointer.
        assert_eq!(ui.node_title_at(title), Some(at));
    });
}

// The colour picker sits where it opened, in pane pixels: a wheel turn over
// the graph, an arrow pan or a switch of workspace closes it rather than
// leave it over another node or another workspace (round 14).
#[test]
fn the_colour_picker_closes_when_the_view_moves() {
    let Some((app, mut w, _config)) = icon_row() else {
        return;
    };
    w.update(|ed, _, cx| {
        ed.ui.as_mut().unwrap().show_graph();
        ed.show_pane(Pane::UiBindings);
        cx.notify();
    });
    app.run_until_parked();
    let open = |w: &mut Window| {
        w.update(|ed, _, cx| {
            let ui = ed.ui.as_mut().unwrap();
            let span = ui.pin_constant("if / else", "then").expect("a fixed then");
            ui.open_pick_for_test(span, [255, 0, 0]);
            ed.keys_to(Keys::Graph);
            cx.notify();
        });
    };
    let picking = |w: &Window| w.read(|ed, _| ed.ui.as_ref().unwrap().picking());
    open(&mut w);
    let middle = w.read(|ed, _| ed.ui.as_ref().unwrap().graph_middle());
    w.simulate_scroll(middle, point(px(0.), px(-40.)));
    assert!(!picking(&w), "a wheel turn closes it");
    open(&mut w);
    w.simulate_keystrokes("left");
    assert!(!picking(&w), "an arrow pan closes it");
    open(&mut w);
    w.simulate_keystrokes("ctrl-1 ctrl-3");
    assert!(!picking(&w), "a switch of workspace closes it");
}

// A gate pair (pass if one value, pass unless another, drawn as one if /
// else) loses one half by a right-click on its then or else row: the menu
// offers that one binding, and the other stays as a pass unless (round 14:
// only both could go).
#[test]
fn a_right_click_on_a_gate_pairs_then_removes_that_half() {
    let Some((mut app, mut w, _config)) = icon_row() else {
        return;
    };
    w.update(|ed, _, cx| {
        ed.ui.as_mut().unwrap().show_graph();
        maximize(ed, Pane::UiBindings);
        cx.notify();
    });
    draw(&mut app, &mut w);
    let (at, before) = w.read(|ed, _| {
        let ui = ed.ui.as_ref().unwrap();
        let at = ui.pin_row_at("if / else", "2 bindings", "then");
        (at, ui.binding_count())
    });
    let at = at.expect("the gate pair's then shows");
    w.simulate_mouse_down(at, MouseButton::Right);
    w.simulate_mouse_up(at, MouseButton::Right);
    app.run_until_parked();
    let menu = w.read(|ed, _| ed.ui.as_ref().unwrap().unbind_menu());
    let menu = menu.expect("the remove menu");
    assert_eq!(menu.len(), 1, "that half alone: {menu:?}");
    w.update(|ed, _, cx| {
        ed.ui.as_mut().unwrap().remove_from_menu().unwrap();
        cx.notify();
    });
    app.run_until_parked();
    w.read(|ed, _| {
        let ui = ed.ui.as_ref().unwrap();
        assert_eq!(ui.binding_count(), before - 1);
        let titles = ui.logic_titles();
        assert!(titles.iter().any(|t| t == "pass unless"), "{titles:?}");
    });
}

// Names and the widget library come from the UI index, which loads when a
// screen opens, not only once the UI files tab has been shown (round 15:
// a session reopened on the widget tree showed guids in the scope bar and
// an empty Widgets tab until UI files was clicked).
#[test]
fn the_ui_index_loads_without_the_files_tab() {
    let Some(game) = std::env::var_os("ANNO117_DIR") else {
        return;
    };
    let mut app = TestApp::new();
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_path_buf();
    let w = app.open_window(|window, cx| {
        let open = Some(ICON_ROW.into());
        let mut ed = Editor::new(Game::Anno117, dir, Some(game.into()), open, window, cx);
        // The left dock on the widget tree: UI files never draws.
        ed.show_pane(Pane::UiTree);
        ed
    });
    app.run_until_parked();
    w.read(|ed, _| {
        let index = ed.ui_find.index.clone().expect("the index loaded");
        let mut library = crate::ui::library::Library::default();
        let found = library.search("button", &index);
        assert!(found.contains(&"# Buttons".to_owned()));
    });
}
