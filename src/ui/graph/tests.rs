use super::view::GraphView;
use super::wiring::Drop;
use super::{Graph, Kind, Node};
use crate::ui::UiEditor;
use crate::ui::bindings::{T_BOOL, T_EVENT, T_FLOAT, Target, Transform};
use crate::ui::test_support::{
    BUTTON, ICON_ROW, QUEST_TRACKER, bindings, event_ending, install, key, labels, node, open,
    path, reopen, select, slot_ending,
};
use anno_formats::phx::Asset;
use anno_formats::phx::binding::{Bindings, Slot, builtin};
use gpui_editor_kit::gpui::{Bounds, Pixels, point, px, size};
use std::sync::Arc;

/// What `overlaps` and `backward` find in a sound layout.
const NONE: [&str; 0] = [];

/// A pane `w` × `h` at the window's corner.
fn pane(w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(0.), px(0.)), size(px(w), px(h)))
}

/// Whether rectangle `r` (graph units) lies wholly on a `w` × `h` pane as
/// `view` shows it.
fn on_pane(view: &GraphView, r: [f32; 4], [w, h]: [f32; 2]) -> bool {
    let (a, b) = (
        view.to_screen([r[0], r[1]]),
        view.to_screen([r[0] + r[2], r[1] + r[3]]),
    );
    a[0] >= 0. && a[1] >= 0. && b[0] <= w && b[1] <= h
}

/// The graph the pane shows now, as `shown_graph` draws it.
fn shown(ui: &mut UiEditor) -> Graph {
    ui.shown_graph().unwrap();
    ui.graph.cache.as_ref().unwrap().1.clone()
}

/// What feeds `node`'s widget (`feeds`: and what it feeds), built fresh.
fn widget_graph(ui: &mut UiEditor, node: usize, feeds: bool) -> Graph {
    let ft = ui.file_table(node).unwrap();
    Graph::build(&ft, Some(ui.nodes[node].widget), &[], false, feeds)
}

/// The whole file `node` is part of, built fresh.
fn file_graph(ui: &mut UiEditor, node: usize) -> Graph {
    Graph::build(&ui.file_table(node).unwrap(), None, &[], false, false)
}

/// `node`'s graph as the pane draws it: built, placed (nodes keep where
/// they were drawn before), cached as drawn.
fn draw(ui: &mut UiEditor, node: usize, open: &[Target]) -> Graph {
    let (file, widget) = (ui.node_file(node), Some(ui.nodes[node].widget));
    let ft = ui.file_table(node).unwrap();
    let mut g = Graph::build(&ft, widget, open, false, false);
    let laid: Vec<[f32; 2]> = g.nodes.iter().map(|n| [n.rect[0], n.rect[1]]).collect();
    ui.graph.place((file, widget, false), &mut g, &laid);
    let key = (file, widget, false, open.to_vec(), false, ui.layout_gen);
    ui.graph.cache = Some((key, g.clone(), laid));
    g
}

/// The properties of `node`'s widget a wire can be dropped on unbound.
fn open_targets(ui: &UiEditor, node: usize) -> Vec<Target> {
    Target::ALL
        .into_iter()
        .filter(|t| t.fits(ui.nodes[node].kind))
        .collect()
}

fn focus(n: &Node) -> bool {
    matches!(n.kind, Kind::Widget { focus: true, .. })
}

/// The first node whose title holds `part`.
fn titled<'a>(g: &'a Graph, part: &str) -> &'a Node {
    g.nodes
        .iter()
        .find(|n| n.title.contains(part))
        .unwrap_or_else(|| panic!("no node {part}"))
}

/// The drop onto input `pin` of the first node titled `part`'s.
fn arg(g: &Graph, part: &str, pin: usize) -> Drop {
    let (binding, k) = titled(g, part).ins[pin]
        .arg
        .expect("an input that takes wires");
    Drop::Arg { binding, k }
}

/// The pin a drop lands on, by its node's title and its name, in the
/// graph the pane shows.
fn drop_on(ui: &mut UiEditor, title: &str, pin: &str) -> Drop {
    let g = shown(ui);
    g.drop_targets()
        .into_iter()
        .find(|&(n, k, _)| g.nodes[n].title == title && g.nodes[n].ins[k].name == pin)
        .unwrap_or_else(|| panic!("no {pin} on {title}"))
        .2
}

/// Template input `name`'s slot in the file as stored now: slots
/// renumber with each edit, so found again each time.
fn input(ui: &UiEditor, name: &str) -> usize {
    let t = bindings(&ui.asset);
    (0..t.slots.len())
        .find(|&s| {
            matches!(t.slots[s], Slot::Template(p, 1)
                if ui.asset.property_name(&ui.asset.properties()[p]) == name)
        })
        .unwrap_or_else(|| panic!("no input {name}"))
}

/// The slot of `widget`'s Size.x in the file as stored now.
fn size_x(ui: &UiEditor, widget: usize) -> usize {
    let t = bindings(&ui.asset);
    (0..t.slots.len())
        .find(|&s| {
            matches!(&t.slots[s], Slot::Widget { widget: Some(w), path }
                if *w == widget && path[..] == [(2, 0x8000_0000 | builtin::SIZE)])
        })
        .expect("the widget's Size.x slot")
}

/// Binds the selected widget from template input `input` through the
/// binder, as its Bind does.
fn bind(ui: &mut UiEditor, input: usize, transform: Transform) {
    ui.open_binder();
    let b = ui.binder.as_mut().unwrap();
    b.source = Some(crate::ui::bindings::Source::Input(input));
    b.transform = transform;
    ui.bind().unwrap();
}

/// Selects Icon_Group and gives its file's IsScalable_Content input, the
/// source the insert tests bind from.
fn icon_group(ui: &mut UiEditor) -> usize {
    select(ui, "Icon_Group");
    ui.asset
        .properties()
        .iter()
        .position(|p| ui.asset.property_name(p).ends_with("IsScalable_Content"))
        .unwrap()
}

/// Nodes that cover one another, by title.
fn overlaps(g: &Graph) -> Vec<String> {
    let mut out = Vec::new();
    for (i, a) in g.nodes.iter().enumerate() {
        for b in &g.nodes[i + 1..] {
            let (r, q) = (a.rect, b.rect);
            if r[0] < q[0] + q[2] && q[0] < r[0] + r[2] && r[1] < q[1] + q[3] && q[1] < r[1] + r[3]
            {
                out.push(format!("{} {r:?} over {} {q:?}", a.title, b.title));
            }
        }
    }
    out
}

/// Wires between two nodes whose target does not start right of where
/// their source ends.
fn backward(g: &Graph) -> Vec<String> {
    g.wires
        .iter()
        .filter(|w| w.from.0 != w.to.0)
        .map(|w| (&g.nodes[w.from.0], &g.nodes[w.to.0]))
        .filter(|(a, b)| a.rect[0] + a.rect[2] > b.rect[0])
        .map(|(a, b)| format!("{} → {}", a.title, b.title))
        .collect()
}

// Needs the game: with the keys on the graph, Delete and every chord
// stop there, and only the editor's own shortcuts go on (round 8:
// Ctrl+Delete went on to the tree and deleted the selected widget).
#[test]
fn graph_keys_never_reach_the_canvas() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let own = (1..ui.nodes.len()).find(|&n| !ui.nodes[n].foreign).unwrap();
    ui.select_widget(own);
    ui.take_keys(crate::ui::Keys::Graph);
    let bytes = ui.asset.encode();
    for k in [
        "delete",
        "ctrl-delete",
        "ctrl-backspace",
        "alt-delete",
        "ctrl-d",
        "ctrl-v",
        "alt-down",
    ] {
        assert!(ui.graph_key_down(&key(k)), "{k} stops in the graph");
    }
    assert_eq!(ui.asset.encode(), bytes, "the file as it was");
    for k in ["ctrl-z", "ctrl-y", "ctrl-s", "ctrl-3", "f5", "space"] {
        assert!(!ui.graph_key_down(&key(k)), "{k} is the editor's");
    }
    // The List: Delete says where removal lives, arrows leave the
    // hidden graph alone.
    ui.graph.on = false;
    let pan = ui.graph.pan;
    assert!(ui.graph_key_down(&key("delete")) && ui.graph_key_down(&key("left")));
    assert_eq!(ui.graph.pan, pan);
    assert!(
        ui.message
            .as_ref()
            .is_some_and(|m| m.0.contains("Data section"))
    );
    assert_eq!(ui.asset.encode(), bytes);
}

// A vector and its axes as the walks see them: a change to the whole
// reaches what reads either axis, a change to x only what reads x, and
// what feeds a copy of y meets the whole but not x. A field of an object
// input (Layout.TextPlane) and its axes follow the same rule, two steps
// down a widget's path (round 7 only knew one-step paths, so a loop or a
// second writer through Layout.TextPlane.x went unseen).
#[test]
fn places_share_values_by_axis() {
    use anno_formats::phx::binding::Binding;
    let copy = |from: usize, to: usize| Binding {
        mode: 0,
        converter: crate::ui::bindings::ASSIGN,
        sources: vec![from],
        targets: vec![to],
        rules: Vec::new(),
        entry: 0,
        enum_rule: None,
    };
    // A, A.x, A.y, then B and C: B copies A.x, C copies A.y.
    let bindings = vec![copy(1, 3), copy(2, 4)];
    let others = [Slot::Template(1, 1), Slot::Template(2, 1)];
    let one_step = [1, 2, 3].map(|f| Slot::Template(0, f));
    let two_step = [1, 2, 3].map(|f| Slot::Widget {
        widget: Some(3),
        path: vec![(1, 0x1234), (f, 0x5678)],
    });
    for axes in [one_step, two_step] {
        let slots: Vec<Slot> = axes.into_iter().chain(others.clone()).collect();
        let places = super::wiring::Places::new(&slots, &bindings);
        let read = |from: usize| {
            places
                .readers(&bindings, [from])
                .into_iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(read(0), [0, 1]);
        assert_eq!(read(1), [0]);
        assert_eq!(read(2), [1]);
        let up = places.upstream(&bindings, 4);
        assert!(up.meets(places.of(0)) && up.meets(places.of(2)));
        assert!(!up.meets(places.of(1)) && !up.meets(places.of(3)));
        // A slot past the table meets nothing.
        assert!(!up.meets(places.of(99)));
    }
}

// Needs the game: one axis of an object input's field or of a
// container's input is a number. The Text container's
// Layout.TextPlane.x in the icon row was judged as the whole Vector2. In
// 0027a4b3 a Float onto the Label container's SizeIn.x is wired, as the
// game's files do over a thousand times (round 6 refused it as "takes
// Vector2").
#[test]
fn an_axis_of_an_input_is_a_number() {
    let Some(install) = install() else { return };
    let mut ui = UiEditor::load(install.clone(), &path(ICON_ROW)).unwrap();
    let ft = ui.file_table(0).unwrap();
    assert_eq!(
        ft.kind(slot_ending(&ft, "Layout.TextPlane.x")),
        Some(T_FLOAT)
    );
    let file = install
        .list("data/ui/studio/generated")
        .into_iter()
        .find(|f| f.contains("0027a4b3"))
        .expect("0027a4b3");
    let mut ui = UiEditor::load(install, &file).unwrap();
    let ft = ui.file_table(0).unwrap();
    let size_x = (0..ft.table.slots.len())
        .find(|&s| ft.label(s).starts_with("Label · ") && ft.label(s).ends_with("SizeIn.x"))
        .unwrap();
    assert_eq!(ft.kind(size_x), Some(T_FLOAT));
    let from = slot_ending(&ft, "_BtnSizeX_S2");
    let v = ui
        .wire_verdicts(from, None, &[Drop::Slot(size_x)])
        .remove(0);
    assert!(v.is_ok(), "{v:?}");
    // The drop does it, and says it by the names the graph shows.
    let said = ui.wire_up(from, None, Drop::Slot(size_x)).unwrap();
    assert!(
        said.contains("SizeIn.x") && said.contains("replacing MOBO"),
        "{said}"
    );
}

// Needs the game: the construction bar, kept from Anno 1800, reads game
// data by Anno 1800's ids and a scene transition's events, and starts on
// the Started of tweens whose template does not ship; its pins name all
// of them but the one field no exe has (round 12 showed eight as hex).
#[test]
fn pins_name_ids_from_every_source_the_game_has() {
    let Some(mut ui) = open("constructionbarscene") else {
        return;
    };
    let labels = labels(&ui.file_table(0).unwrap());
    for want in [
        "ConstructionBarData.IsExpanded",
        "ConstructionBarData.IsHidden",
        "SceneTransition.Show",
        "SceneTransition.Hide",
        "Tween 72c007a3.Started",
        "Tween 72216e56.Started",
    ] {
        assert!(
            labels.iter().any(|l| l.ends_with(want)),
            "{want} in {labels:#?}"
        );
    }
    let hex = |l: &String| {
        l.split(['.', ' '])
            .any(|w| w.len() == 8 && w.chars().all(|c| c.is_ascii_hexdigit()))
    };
    // Game data: all named but 2534e5ce. (ConstrBarWrapper's own inputs
    // stay by id: its template is the one that does not ship.)
    let left: Vec<&String> = labels
        .iter()
        .filter(|l| l.contains("GameContext") && hex(l) && !l.contains("2534e5ce"))
        .collect();
    assert!(left.is_empty(), "{left:#?}");
}

// Only what is on the pane becomes elements: a node off to the left
// does not, a wire crossing the pane with both ends off it does, and
// before the pane's first layout everything does.
#[test]
fn only_what_the_pane_shows_is_built() {
    let mut g = GraphView::default();
    assert!(
        g.shows([-5000., -5000.], [-4900., -4900.]),
        "size unknown yet"
    );
    g.bounds = Bounds::new(point(px(300.), px(700.)), size(px(800.), px(240.)));
    assert!(
        !g.shows([-500., 10.], [-400., 60.]),
        "a node off to the left"
    );
    assert!(!g.shows([10., 400.], [100., 460.]), "a node below");
    assert!(g.shows([-20., 10.], [30., 60.]), "a node half on");
    assert!(
        g.shows([-300., 100.], [1200., 120.]),
        "a wire across the pane"
    );
}

// A fit fills the pane: a small graph in a maximized pane reads larger
// than docked (round 11 framed both at 100%), up to the cap, and a
// docked pane still fits it whole. A resize fits an untouched view
// again; once panned or zoomed by hand, the view is the modder's: a new
// pane size keeps the zoom and the graph point at the pane's centre,
// there and back (round 12: maximize, pan, restore refitted). A new view
// still fits.
#[test]
fn a_fit_fills_the_pane_until_the_view_is_moved_by_hand() {
    let fit = |w: f32, h: f32| {
        let mut g = GraphView {
            bounds: pane(w, h),
            ..Default::default()
        };
        g.fit([0., 0.], [1000., 160.]);
        g.zoom
    };
    let (docked, maximized) = (fit(1140., 300.), fit(1920., 1000.));
    assert!(docked <= 1.1, "{docked}");
    assert!(maximized > docked * 1.4, "{maximized} vs {docked}");
    assert!(maximized <= super::layout::MAX_FIT_ZOOM);

    let mut g = GraphView::default();
    let resize = |g: &mut GraphView, w: i32, h: i32, widget: usize| {
        g.bounds = pane(w as f32, h as f32);
        let key = super::layout::FitKey {
            file: None,
            widget: Some(widget),
            all: false,
            feeds: false,
            size: [w, h],
        };
        g.frame(key, || Some(([0., 0.], [1000., 160.])));
    };
    let centre = |g: &GraphView| {
        let (w, h) = (
            f32::from(g.bounds.size.width),
            f32::from(g.bounds.size.height),
        );
        [(w / 2. - g.pan[0]) / g.zoom, (h / 2. - g.pan[1]) / g.zoom]
    };
    resize(&mut g, 1140, 300, 1);
    let docked = g.zoom;
    resize(&mut g, 1920, 1000, 1);
    assert!(g.zoom > docked * 1.4, "untouched: the larger pane fits");
    g.zoom_by(240., [300., 200.]);
    g.pan[0] -= 50.;
    let (zoom, at) = (g.zoom, centre(&g));
    for (w, h) in [(1140, 300), (1920, 1000)] {
        resize(&mut g, w, h, 1);
        assert_eq!(g.zoom, zoom);
        let c = centre(&g);
        assert!(
            (c[0] - at[0]).abs() < 0.5 && (c[1] - at[1]).abs() < 0.5,
            "{c:?} {at:?}"
        );
    }
    resize(&mut g, 1140, 300, 2);
    assert_eq!(g.zoom, docked, "another widget fits");
    // A reveal's pan counts as a view moved: the next resize keeps it.
    g.reveal(&[[5000., 0., 100., 60.]], 1);
    let at = centre(&g);
    resize(&mut g, 1920, 1000, 2);
    assert_eq!(g.zoom, docked);
    let c = centre(&g);
    assert!((c[0] - at[0]).abs() < 0.5, "{c:?} {at:?}");
}

// Needs the game: All in file opens at the selected widget, outlined,
// in a docked pane at a readable zoom; the whole-file framing it opened
// with before left the Icon out of view (round 12).
#[test]
fn all_in_file_opens_at_the_selected_widget() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = select(&mut ui, "Icon");
    ui.bindings_all = true;
    let g = shown(&mut ui);
    // What it reads out and what is set on it.
    let outlined: Vec<usize> = (0..g.nodes.len()).filter(|&i| focus(&g.nodes[i])).collect();
    assert!(!outlined.is_empty() && outlined.iter().all(|&i| g.nodes[i].title == "Icon"));
    let docked = [1140., 300.];
    let mut view = GraphView {
        bounds: pane(docked[0], docked[1]),
        ..Default::default()
    };
    let key = super::layout::FitKey {
        file: None,
        widget: Some(ui.nodes[icon].widget),
        all: true,
        feeds: false,
        size: [1140, 300],
    };
    view.frame(key, || g.opening(&outlined, docked));
    assert!(
        outlined
            .iter()
            .any(|&i| on_pane(&view, g.nodes[i].rect, docked))
    );
    assert!(view.zoom >= super::layout::MIN_FIT_ZOOM, "{}", view.zoom);
    // Maximized: the band most of Icon is set in, at a zoom its text
    // reads at, not every band a copy of Icon sits in with the gaps
    // between (round 15: it opened on empty space).
    let maxed = [1880., 960.];
    let (_, opened) = g.opening(&outlined, maxed).unwrap();
    let (top, bottom) = outlined.iter().fold((f32::MAX, f32::MIN), |(t, b), &i| {
        let r = g.nodes[i].rect;
        (t.min(r[1]), b.max(r[1] + r[3]))
    });
    assert!(
        outlined.len() > 1 && opened[1] < bottom - top,
        "{opened:?} vs {}",
        bottom - top
    );
    let zoom = ((maxed[0] - 40.) / opened[0]).min((maxed[1] - 40.) / opened[1]);
    assert!(zoom >= 0.9, "{zoom}");

    // As the pane does it: opened at Icon, then a click on another
    // widget of the file outlines and shows it, zoom kept (round 14:
    // every click framed anew, 0.6 to 1.6).
    ui.graph.bounds = pane(docked[0], docked[1]);
    ui.graph.fitted = None;
    let g = shown(&mut ui);
    let zoom = ui.graph.zoom;
    let widget = ui.nodes[icon].widget;
    let file = ui.node_file(icon);
    let other = g
        .nodes
        .iter()
        .filter_map(|n| match n.kind {
            Kind::Widget { widget: w, .. } if w != widget => Some(w),
            _ => None,
        })
        .find_map(|w| {
            (1..ui.nodes.len()).find(|&n| ui.nodes[n].widget == w && ui.node_file(n) == file)
        })
        .expect("another widget in the file's graph");
    ui.select_widget(other);
    let g = shown(&mut ui);
    assert_eq!(ui.graph.zoom, zoom, "a selection keeps the zoom");
    assert!(
        g.nodes
            .iter()
            .any(|n| focus(n) && on_pane(&ui.graph, n.rect, docked)),
        "the new selection is on the pane"
    );
    // Selecting it again moves nothing.
    let pan = ui.graph.pan;
    ui.select_widget(other);
    ui.shown_graph().unwrap();
    assert_eq!((ui.graph.zoom, ui.graph.pan), (zoom, pan));
    // Back to Icon: shown again, still the same zoom.
    ui.select_widget(icon);
    ui.shown_graph().unwrap();
    assert_eq!(ui.graph.zoom, zoom);
}

// Needs the game: All in file reads as bands, one per chain. A source
// or widget that separate chains run through is drawn in each, so no
// wire crosses the file; the widgets set line up in one column on the
// right; no node covers another (round 14: 85 bindings through one
// Inputs node read as spaghetti).
#[test]
fn all_in_file_lays_out_one_band_per_chain() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let g = shown(&mut ui);
    assert!(g.wires.len() > 100, "the whole file");
    let inputs = g.nodes.iter().filter(|n| n.kind == Kind::Inputs).count();
    assert!(inputs > 1, "the inputs, once per chain: {inputs}");
    let tallest = g
        .wires
        .iter()
        .map(|w| (g.out_at(w.from.0, w.from.1)[1] - g.in_at(w.to.0, w.to.1)[1]).abs())
        .fold(0., f32::max);
    assert!(tallest < g.size[1] / 4., "{tallest} of {}", g.size[1]);
    // A widget set by a chain stands in the one widget column.
    let set: Vec<f32> = g
        .nodes
        .iter()
        .enumerate()
        .filter(|&(i, n)| {
            matches!(n.kind, Kind::Widget { .. })
                && g.wires.iter().any(|w| w.to.0 == i)
                && !g.wires.iter().any(|w| w.from.0 == i)
        })
        .map(|(_, n)| n.rect[0])
        .collect();
    assert!(set.len() > 3 && set.iter().all(|&x| x == set[0]), "{set:?}");
    let over = overlaps(&g);
    assert!(over.is_empty(), "{over:#?}");
}

// Needs the game: a wire is an event or a value by what flows in it,
// not by its binding. A "when it turns true" reads a value and sends
// an event; a "true on event" takes an event and writes a value; a
// copy or pass if of an event carries one. Event pins hold no value.
// (round 15: one flag per binding drew FalseToTrueChange's event as
// data and EventToTrue's yes/no as an event; most events read as data.)
// An event dropped on a yes/no, or a yes/no on an event, is refused, and
// the refusal says why and which converters bridge the two; a logic
// node's input refuses an event in the graph's words and names the
// bridges, as a property pin does (round 15: "Logical Or takes Boolean
// there, and TruetoFalseChange_02 is Event").
#[test]
fn events_and_values_flow_apart_and_refusals_name_the_bridge() {
    let Some(install) = install() else { return };
    let mut ui = UiEditor::load(install.clone(), &path("minimapscene")).unwrap();
    // The screen and its templates, each once, as All in file shows it.
    let (mut files, mut tables) = (Vec::new(), Vec::new());
    for n in 0..ui.nodes.len() {
        let file = ui.node_file(n);
        if !files.contains(&file) {
            files.push(file);
            tables.extend(ui.file_table(n));
        }
    }
    let (mut seen, mut events) = ([0; 2], 0);
    for ft in &tables {
        let g = Graph::build(ft, None, &[], false, false);
        let wires = |n: usize, into: bool| {
            g.wires
                .iter()
                .filter(move |w| if into { w.to.0 == n } else { w.from.0 == n })
        };
        for (n, node) in g.nodes.iter().enumerate() {
            let (sends, takes) = match node.title.as_str() {
                "when it turns true" | "when it turns false" => (true, false),
                "true on event" | "false on event" => (false, true),
                _ => continue,
            };
            seen[usize::from(takes)] += 1;
            assert!(
                wires(n, false).all(|w| w.event == sends),
                "{} out",
                node.title
            );
            assert!(
                wires(n, true).all(|w| w.event == takes),
                "{} in",
                node.title
            );
            assert_eq!(node.outs[0].event, sends);
        }
        // An event never feeds a yes/no, number or text pin, and the
        // pins it ends at are events.
        for w in g.wires.iter().filter(|w| w.event) {
            events += 1;
            let to = &g.nodes[w.to.0].ins[w.to.1];
            assert!(to.event, "{} {}", g.nodes[w.to.0].title, to.name);
            assert!(
                !matches!(to.kind, Some(0x00..=0x08)),
                "an event into {} {} ({:?})",
                g.nodes[w.to.0].title,
                to.name,
                to.kind
            );
        }
    }
    assert!(
        seen[0] > 0 && seen[1] > 0,
        "both kinds on the minimap: {seen:?}"
    );
    assert!(events > 20, "{events}");

    // Templates of the minimap: one with a "when it turns …" result and
    // an or, one with a template event and a yes/no target, one the
    // other way round; each opened to edit.
    let (mut or, mut to_value, mut to_event) = (None, None, None);
    for ft in &tables {
        let Some(file) = ft.file.0 else { continue };
        let (slots, b) = (&ft.table.slots, &ft.table.bindings);
        let conv = |i: usize| ft.converter(i);
        let change =
            (0..b.len()).find(|&i| conv(i).ends_with("Change") && !b[i].targets.is_empty());
        if or.is_none()
            && let (Some(c), Some(o)) = (change, (0..b.len()).find(|&i| conv(i) == "LogicalOr"))
        {
            or = Some((file, b[c].targets[0], Drop::Arg { binding: o, k: 0 }));
        }
        let target = |s: usize| b.iter().any(|x| x.targets.contains(&s));
        let input = |k: u32| {
            (0..slots.len())
                .find(|&s| ft.kind(s) == Some(k) && matches!(slots[s], Slot::Template(_, 1)))
        };
        let set = |k: u32| (0..slots.len()).find(|&s| ft.kind(s) == Some(k) && target(s));
        if to_value.is_none()
            && let (Some(e), Some(t)) = (input(T_EVENT), set(T_BOOL))
        {
            to_value = Some((file, e, Drop::Slot(t)));
        }
        if to_event.is_none()
            && let (Some(v), Some(t)) = (input(T_BOOL), set(T_EVENT))
        {
            to_event = Some((file, v, Drop::Slot(t)));
        }
    }
    let verdict = |(file, from, to): (anno_formats::phx::Guid, usize, Drop)| {
        let mut t = UiEditor::load(install.clone(), &path(&file.to_string())).unwrap();
        t.wire_verdicts(from, None, &[to]).remove(0)
    };
    let said = verdict(or.expect("a template with both")).unwrap_err();
    assert!(said.starts_with("or's a holds Boolean"), "{said}");
    assert!(
        said.contains("when it turns"),
        "the source by its node: {said}"
    );
    assert!(said.contains("true on event or false on event"), "{said}");
    let v = verdict(to_value.expect("an event and a yes/no"));
    assert!(
        v.as_ref()
            .is_err_and(|e| e.contains("is an event: it happens")),
        "{v:?}"
    );
    let v = verdict(to_event.expect("a yes/no and an event"));
    assert!(
        v.as_ref()
            .is_err_and(|e| e.contains("is an event, and") && e.contains("when it turns true")),
        "{v:?}"
    );
}

// Needs the game: an object's field that is an event only by what reads
// it (BtnOverlay_DS's BtnEvents.Leave, copied into a clip's Start and
// written by nothing) is refused on a pass if's condition, and the file
// and its history stay as they were (round 16 saved it there).
#[test]
fn an_event_known_by_its_reader_is_no_condition() {
    let Some(mut ui) = open("2d3010a3-807d-4c39-b1a5-488001ac2c1d") else {
        return;
    };
    let ft = ui.file_table(0).unwrap();
    let (slots, b) = (&ft.table.slots, &ft.table.bindings);
    let clip = |s: usize| matches!(slots[s], Slot::Clip { .. });
    let leave = slot_ending(&ft, "BtnEvents.Leave");
    assert_eq!(ft.kind(leave), None, "no kind of its own");
    assert!(
        !b.iter().any(|x| x.targets.contains(&leave)),
        "nothing writes it"
    );
    assert!(
        b.iter()
            .any(|x| x.sources.contains(&leave) && x.targets.iter().any(|&t| clip(t)))
    );
    let gate = (0..b.len())
        .find(|&i| {
            ft.converter(i) == "IfTrueAssign"
                && b[i].targets.iter().any(|&t| clip(t))
                && b[i].sources.get(1) != Some(&leave)
        })
        .expect("a pass if that starts a clip");
    let (bytes, revision) = (ui.asset.encode(), ui.history.revision());
    let condition = Drop::Arg {
        binding: gate,
        k: 1,
    };
    let said = ui.wire_up(leave, None, condition).unwrap_err().to_string();
    assert!(said.starts_with("pass if's if holds Boolean"), "{said}");
    assert!(said.contains("BtnEvents.Leave is an event"), "{said}");
    assert!(said.contains("true on event or false on event"), "{said}");
    assert_eq!(ui.asset.encode(), bytes);
    assert_eq!(ui.history.revision(), revision);
}

// Needs the game: a nested template's pins name what they reach inside
// ("clip Enter · Stop", not three bare Stop / Start rows), and a clip's
// node is titled once "clip Leave" (round 15; "clip clip" before). A
// widget type's own properties say on hover what they are for, in the
// words of the game's Phoenix sources (the quest tracker button's
// OnMouseLeave, inherited from the Button type).
#[test]
fn pins_name_what_they_reach_and_say_what_they_are_for() {
    let Some(mut ui) = open(BUTTON) else { return };
    let g = shown(&mut ui);
    let overlay: Vec<&str> = g
        .nodes
        .iter()
        .filter(|n| n.title == "BtnOverlay_DS")
        .flat_map(|n| &n.ins)
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        overlay
            .iter()
            .any(|p| p.starts_with("clip ") && p.ends_with(" · Start")),
        "{overlay:?}"
    );
    assert!(g.nodes.iter().any(|n| n.title.starts_with("clip ")));
    assert!(!g.nodes.iter().any(|n| n.title.starts_with("clip clip")));
    let about: Vec<(&str, Option<&str>)> = g
        .nodes
        .iter()
        .flat_map(|n| n.ins.iter().chain(&n.outs))
        .filter(|p| p.name.starts_with("OnMouse") || p.name.starts_with("OnRelease"))
        .map(|p| (p.name.as_str(), p.about.as_deref()))
        .collect();
    assert!(!about.is_empty());
    assert!(
        about.iter().all(|(_, a)| a.is_some_and(|a| a.len() > 20)),
        "{about:?}"
    );
}

// Needs the game: wires of the wrong kind or that close a loop are
// refused while dragging and on the drop, saying why, and leave the
// file as it was with no undo step.
// - Kinds come from where the property is defined: the Icon's
//   Icon3D.IconType is an enum by its component's definition, refused on
//   + X (round 5 wrote Position.x = 1610016757).
// - A converter's input takes the kinds the game feeds it: Icon_Size (a
//   number) onto EnumToBoolean's input is refused (round 5 rewired it and
//   the output lost its value), the IconType enum there is fine; a yes/no
//   into a multiplication is refused.
// - No wire closes a loop: Multiply's result into its own input, a later
//   result into an earlier node, and the Icon's drawn size back into the
//   Icon_Size input name the source (round 4 saved "Multiplication reads
//   Multiplication").
// - A yes/no dropped on a tint names the way the game's files make one
//   from it: an if / else (round 15).
#[test]
fn wrong_kinds_and_loops_are_refused_and_change_nothing() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = select(&mut ui, "Icon");
    let widget = ui.nodes[icon].widget;
    let bytes = ui.asset.encode();
    let refuse = |ui: &mut UiEditor, from: usize, to: Drop, says: &str| {
        let why = ui.wire_verdicts(from, None, &[to]).remove(0).unwrap_err();
        assert!(why.contains(says), "{why}");
        let err = ui.wire_up(from, None, to).unwrap_err();
        assert!(format!("{err:#}").contains(says), "{err:#}");
        assert_eq!(ui.asset.encode(), bytes);
    };
    let ft = ui.file_table(icon).unwrap();
    let icon_type = slot_ending(&ft, "Icon3D.IconType");
    assert_eq!(ft.kind(icon_type), Some(0x11), "an enum by its definition");
    let x = Drop::Open {
        widget,
        target: Target::X,
    };
    refuse(&mut ui, icon_type, x, "Enum");

    let g = widget_graph(&mut ui, icon, false);
    let to_bool = arg(&g, "EnumToBoolean", 0);
    let says = "EnumToBoolean's enum takes Enum";
    refuse(&mut ui, slot_ending(&ft, "Icon_Size"), to_bool, says);
    let v = ui.wire_verdicts(icon_type, None, &[to_bool]).remove(0);
    assert!(
        v.is_ok() || v.as_ref().is_err_and(|e| e.contains("already reads")),
        "{v:?}"
    );
    let yes_no = slot_ending(&ft, "IsScalable_Content");
    let v = ui
        .wire_verdicts(yes_no, None, &[arg(&g, "Multiply", 1)])
        .remove(0);
    assert!(
        v.as_ref()
            .is_err_and(|e| e.starts_with("Multiply's b takes Int32") && !e.contains("type 13")),
        "{v:?}"
    );

    let result = titled(&g, "Multiply").outs[0]
        .slot
        .expect("Multiply's result");
    for to in [arg(&g, "Multiply", 1), arg(&g, "max", 0)] {
        refuse(&mut ui, result, to, "would read itself");
    }
    // A copy back up the chain: Icon · Size.x onto the Icon_Size input.
    let (drawn, back_up) = (size_x(&ui, widget), Drop::Slot(input(&ui, "-Icon_Size")));
    refuse(&mut ui, drawn, back_up, "would read itself");

    let debug = select(&mut ui, "TEST_Debug_text_01");
    let ft = ui.file_table(debug).unwrap();
    let yes_no = (0..ft.table.slots.len())
        .find(|&s| ft.kind(s) == Some(T_BOOL) && matches!(ft.table.slots[s], Slot::Template(_, 1)))
        .unwrap();
    let tint = Drop::Open {
        widget: ui.nodes[debug].widget,
        target: Target::Color,
    };
    let says = "is a yes/no (an if / else turns one into Color";
    refuse(&mut ui, yes_no, tint, says);
    assert!(!ui.undo(false), "and leave no undo step");
}

// Needs the game: a wire of the wrong kind is refused and changes
// nothing; one onto a pin a copy fills replaces that copy, one undo
// step. A wire onto a converter's input rewires that one argument
// (max(Icon_Size, 16) becomes max(IconText_Padding, 16)) as one undo
// step; text into it is refused.
#[test]
fn wires_replace_copies_and_rewire_arguments_in_place() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = node(&ui, "Icon");
    let widget = ui.nodes[icon].widget;
    let count = |ui: &UiEditor| bindings(&ui.asset).bindings.len();
    let before = count(&ui);
    // Text (the icon's image path, as the graph's Data · Icon pin
    // holds it) onto Opacity and into max: refused.
    let g = widget_graph(&mut ui, icon, false);
    let text = g
        .nodes
        .iter()
        .filter(|n| n.kind == Kind::Data)
        .flat_map(|n| &n.outs)
        .find(|p| p.name == "Icon")
        .expect("the Data · Icon pin")
        .clone();
    let (binding, k) = g.nodes.iter().find(|n| n.title == "max").unwrap().ins[0]
        .arg
        .expect("max's input a takes wires");
    let max_a = Drop::Arg { binding, k };
    let alpha = Drop::Open {
        widget,
        target: Target::Alpha,
    };
    let err = ui
        .wire_up(text.slot.unwrap(), text.kind, alpha)
        .unwrap_err();
    assert!(format!("{err:#}").contains("takes"), "{err:#}");
    assert!(ui.wire_up(text.slot.unwrap(), text.kind, max_a).is_err());
    assert_eq!(count(&ui), before);
    // Icon_Size onto Size.x, which a copy fills: that copy goes.
    let size_x = size_x(&ui, widget);
    let writers = |ui: &UiEditor| {
        let t = bindings(&ui.asset);
        let key = super::place_key(&t.slots, size_x);
        t.bindings
            .iter()
            .filter(|b| {
                b.targets
                    .iter()
                    .any(|&x| super::place_key(&t.slots, x) == key)
            })
            .count()
    };
    assert_eq!(writers(&ui), 1);
    let said = ui
        .wire_up(input(&ui, "-Icon_Size"), None, Drop::Slot(size_x))
        .unwrap();
    assert!(said.contains(", replacing "), "{said}");
    assert_eq!(writers(&ui), 1, "replaced, not a second writer");
    assert_eq!(count(&ui), before);
    assert!(ui.undo(false));
    assert_eq!(count(&ui), before);
    // max's a: the same binding, in its place, reads the padding now.
    let padding = "-IconText_Padding";
    let said = ui.wire_up(input(&ui, padding), None, max_a).unwrap();
    assert!(said.starts_with("Rewired"), "{said}");
    let t = bindings(&ui.asset);
    assert_eq!(t.bindings.len(), before);
    assert!(
        matches!(t.slots[t.bindings[binding].sources[k]], Slot::Template(p, _)
        if ui.asset.property_name(&ui.asset.properties()[p]) == padding)
    );
    assert!(ui.undo(false));
    assert_eq!(count(&ui), before);
}

// Needs the game: enum values read as the entry names the game ships
// in its Phoenix sources, on pins and fixed values alike (round 15:
// the icon row showed Orientation as 1505389363 and Equal compared
// 1505389363 with 1410225626). A fixed enum value is edited as the name
// it shows, takes a name typed in any case and refuses one its enum
// lacks with the ones it has; a template input's default reads by name
// in the inspector (round 15: Equal's Horizontal refused "Vertical", and
// the inspector said 1403909053 where Bindings said Begin).
#[test]
fn enum_values_read_and_take_their_names() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let g = shown(&mut ui);
    let values: Vec<(&str, &str)> = g
        .nodes
        .iter()
        .flat_map(|n| n.ins.iter().chain(&n.outs))
        .filter_map(|p| Some((p.name.as_str(), p.value.as_deref()?)))
        .collect();
    let has =
        |name: &str, value: &str| values.iter().any(|&(n, v)| n.ends_with(name) && v == value);
    assert!(has("Orientation", "Horizontal"), "{values:?}");
    assert!(has("AlignmentY_IconTextToPlane", "Begin"), "{values:?}");
    assert!(
        values.iter().any(|&(n, v)| n == "b" && v == "Vertical"),
        "Equal's fixed value"
    );
    assert!(
        !values
            .iter()
            .any(|&(_, v)| v.contains("1505389363") || v.contains("1403909053")),
        "{values:?}"
    );
    let span = g
        .nodes
        .iter()
        .filter(|n| n.title.contains("Equal"))
        .flat_map(|n| &n.ins)
        .find(|p| p.value.as_deref() == Some("Vertical") && p.literal)
        .and_then(|p| p.constant)
        .expect("Equal's fixed Vertical");
    assert_eq!(ui.edit_text(span).as_deref(), Some("Vertical"));
    ui.commit(span, "horizontal").unwrap();
    assert_eq!(
        ui.asset.spans()[span].record,
        anno_formats::phx::Record::Default(anno_formats::phx::DefaultValue::U32(1505389363))
    );
    let said = ui.commit(span, "sideways").unwrap_err().to_string();
    assert_eq!(said, "Orientation takes Horizontal, Vertical");
    let p = ui
        .asset
        .properties()
        .iter()
        .find(|p| ui.asset.property_name(p) == "-Alignment_IconToText")
        .unwrap()
        .clone();
    assert_eq!(crate::ui::property_default_text(&ui.asset, &p), "Begin");
}

// Wheel steps from an overview Fit (below the readable zoom) go the way
// the wheel turns, and the point under the pointer stays put.
#[test]
fn the_wheel_zooms_out_from_an_overview() {
    let mut g = GraphView {
        zoom: 0.05,
        pan: [10., 20.],
        ..Default::default()
    };
    let at = [300., 200.];
    let graph_point = |g: &GraphView| [(at[0] - g.pan[0]) / g.zoom, (at[1] - g.pan[1]) / g.zoom];
    let before = graph_point(&g);
    g.zoom_by(-120., at);
    assert!(
        g.zoom < 0.05 && g.zoom >= super::layout::MIN_ZOOM,
        "{}",
        g.zoom
    );
    let after = graph_point(&g);
    assert!((after[0] - before[0]).abs() < 1e-2 && (after[1] - before[1]).abs() < 1e-2);
    g.zoom_by(120., at);
    assert!(g.zoom > 0.04);
}

// Needs the game: what feeds the quest tracker icon row's Icon is a
// graph whose wires all run left to right, whose nodes do not overlap,
// with the widget as one node and fixed values on pins, not as boxes.
#[test]
fn a_widgets_graph_flows_left_to_right() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = node(&ui, "Icon");
    let g = widget_graph(&mut ui, icon, false);
    assert!(!g.wires.is_empty());
    let own: Vec<_> = g.nodes.iter().filter(|n| focus(n)).collect();
    assert_eq!(own.len(), 1, "the widget is one node");
    assert!(
        own[0].ins.len() >= 3,
        "Image, ColorTint and Size are its pins"
    );
    assert!(g.nodes.iter().any(|n| matches!(n.kind, Kind::Logic { .. })));
    assert!(
        g.nodes.iter().flat_map(|n| &n.ins).any(|p| p.literal),
        "fixed values sit on pins"
    );
    // The two gates on `_hub01.x` read as one if / else whose output is
    // that property; no relay node for it.
    assert!(!g.nodes.iter().any(|n| n.title.starts_with("pass ")));
    assert!(
        g.nodes
            .iter()
            .any(|n| n.title == "if / else" && n.outs[0].name == "_hub01.x"),
        "{:?}",
        g.nodes
            .iter()
            .map(|n| (&n.title, &n.outs))
            .collect::<Vec<_>>()
    );
    assert!(!g.nodes.iter().any(|n| n.title == "_hub01.x"));
    // Both kinds of if / else (a gate pair, IfTrueFalseAssign) read the
    // same way down: if, then, else.
    let ifs: Vec<_> = g.nodes.iter().filter(|n| n.title == "if / else").collect();
    assert!(ifs.len() >= 2);
    for n in ifs {
        let names: Vec<&str> = n.ins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["if", "then", "else"]);
    }
    // Component properties by name, not `7f18814e.7b1e8330`.
    assert!(
        g.nodes
            .iter()
            .flat_map(|n| n.ins.iter().chain(&n.outs))
            .any(|p| p.name == "UIScaling.ScalingFactor"),
        "the icon's scale is named"
    );
    // Wires run left to right, but for the one closing a loop (a
    // binding that reads what its own chain writes).
    let reaches = |from: usize, to: usize| {
        let mut seen = vec![false; g.nodes.len()];
        let mut stack = vec![from];
        while let Some(v) = stack.pop() {
            if v == to {
                return true;
            }
            if std::mem::replace(&mut seen[v], true) {
                continue;
            }
            stack.extend(g.wires.iter().filter(|w| w.from.0 == v).map(|w| w.to.0));
        }
        false
    };
    for w in &g.wires {
        let (a, b) = (g.nodes[w.from.0].rect, g.nodes[w.to.0].rect);
        assert!(
            a[0] + a[2] <= b[0] || reaches(w.to.0, w.from.0),
            "{} → {}",
            g.nodes[w.from.0].title,
            g.nodes[w.to.0].title
        );
    }
    let over = overlaps(&g);
    assert!(over.is_empty(), "nodes overlap: {over:#?}");
}

// Needs the game: the minimap's inner_stroke feeds Stroke_Mask's size
// through a Subtract; What it feeds shows that, the plain slice not.
#[test]
fn what_it_feeds_follows_values_downstream() {
    let Some(mut ui) = open("minimapscene") else {
        return;
    };
    let stroke = (1..ui.nodes.len())
        .find(|&n| &*ui.nodes[n].name == "inner_stroke")
        .expect("inner_stroke");
    let has_mask = |g: &Graph| g.nodes.iter().any(|n| n.title == "Stroke_Mask");
    assert!(!has_mask(&widget_graph(&mut ui, stroke, false)));
    assert!(has_mask(&widget_graph(&mut ui, stroke, true)));
}

// Needs the game: nodes keep their keys across rebuilds (where they
// were dragged follows them) and no two share one. The file's binding
// table is built once per layout (reused while nothing changes) and
// never outlives an edit. Removing the first logic binding renumbers
// every binding after it, and a widget added before the Icon renumbers
// the widgets after it; the other nodes keep their keys (a dragged node
// stays where it was put).
#[test]
fn node_keys_hold_across_rebuilds_and_renumbering() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = node(&ui, "Icon");
    let keys = |g: &Graph| g.nodes.iter().map(|n| n.key.clone()).collect::<Vec<_>>();
    let (ft, again) = (ui.file_table(icon).unwrap(), ui.file_table(icon).unwrap());
    assert!(Arc::ptr_eq(&ft, &again), "built again with nothing changed");
    let a = Graph::build(&ft, None, &[], false, false);
    assert_eq!(keys(&a), keys(&Graph::build(&ft, None, &[], false, false)));
    let mut unique = keys(&a);
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), a.nodes.len());
    let first = a
        .nodes
        .iter()
        .find_map(|n| match n.kind {
            Kind::Logic {
                binding,
                also: None,
                ..
            } => Some(binding),
            _ => None,
        })
        .expect("a logic node");
    let gone = a
        .nodes
        .iter()
        .find(|n| matches!(n.kind, Kind::Logic { binding, .. } if binding == first))
        .unwrap()
        .key
        .clone();
    ui.unbind(&[first]).unwrap();
    let edited = ui.file_table(icon).unwrap();
    assert!(!Arc::ptr_eq(&ft, &edited), "an edit builds it anew");
    assert_eq!(edited.table.bindings.len(), ft.table.bindings.len() - 1);
    let after: std::collections::HashSet<String> =
        keys(&Graph::build(&edited, None, &[], false, false))
            .into_iter()
            .collect();
    let kept = keys(&a)
        .into_iter()
        .filter(|k| *k != gone && after.contains(k))
        .count();
    assert!(
        kept + 2 >= a.nodes.len() - 1,
        "{kept} of {} keys survived",
        a.nodes.len() - 1
    );
    let sorted = |ui: &mut UiEditor| {
        let n = node(ui, "Icon");
        let mut k = keys(&widget_graph(ui, n, false));
        k.sort();
        k
    };
    let before = sorted(&mut ui);
    let icon = node(&ui, "Icon");
    let was = ui.nodes[icon].widget;
    let first = (1..icon)
        .find(|&n| !ui.nodes[n].foreign)
        .expect("an own widget before the Icon");
    ui.select_widget(first);
    ui.duplicate_selected().unwrap();
    assert_ne!(
        ui.nodes[node(&ui, "Icon")].widget,
        was,
        "the Icon was renumbered"
    );
    assert_eq!(sorted(&mut ui), before);
}

// Needs the game: every plain copy the shipped UI files make between
// properties of known kinds passes the graph's type check (a golden
// check over all 4,824 files: the check may not refuse what the game
// itself writes), and the kinds no shipped copy crosses are refused:
// an enum's entry id into Opacity, a colour into Position.
#[test]
fn the_type_check_takes_what_the_game_writes() {
    use anno_formats::phx::{Guid, binding::Converter};
    use std::collections::{BTreeMap, HashMap};
    let Some(install) = install() else { return };
    // Templates as the editor loads them, once each.
    let mut cache: HashMap<Guid, Option<Arc<Asset>>> = HashMap::new();
    let mut load = |g: Guid| {
        cache
            .entry(g)
            .or_insert_with(|| crate::ui::scene::read_asset(&install, g).map(Arc::new))
            .clone()
    };
    let (mut copies, mut args) = (0, 0);
    let mut refused: BTreeMap<String, usize> = BTreeMap::new();
    // Every (from, to) kind pair a shipped copy, pass or comparison makes.
    let mut wrote = std::collections::BTreeSet::new();
    for f in install.list("data/ui/studio/generated") {
        let Ok(a) = Asset::parse(install.read(&f).unwrap()) else {
            continue;
        };
        let widgets = a.widgets();
        let t = a.bindings(&widgets);
        // The kinds the editor decides with (`FileTable::kind`): a
        // container's inputs by its template, components (its own or
        // its template root's) by theirs.
        let mut inputs: HashMap<usize, Arc<Asset>> = HashMap::new();
        let mut components: HashMap<(usize, u32), Arc<Asset>> = HashMap::new();
        for (w, wd) in widgets.iter().enumerate() {
            let container = a.container_template(wd).and_then(|r| load(r.id));
            let root = container.as_ref().and_then(|c| {
                let cw = c.widgets();
                let r = cw.first()?;
                Some(
                    c.components(r)
                        .into_iter()
                        .chain(c.effects(r))
                        .collect::<Vec<_>>(),
                )
            });
            for c in a
                .components(wd)
                .into_iter()
                .chain(a.effects(wd))
                .chain(root.into_iter().flatten())
            {
                if let Some(t) = c.template.and_then(&mut load) {
                    components.insert((w, c.uid), t);
                }
            }
            if let Some(c) = container {
                inputs.insert(w, c);
            }
        }
        let defined = |w: usize, c: Option<u32>, uid: u32| {
            let t = match c {
                None => inputs.get(&w)?,
                Some(c) => components.get(&(w, c))?,
            };
            t.properties().iter().find(|p| p.uid == uid).map(|p| p.kind)
        };
        let kind = |s: usize| crate::ui::bindings::defined_kind(&a, &t.slots, s, &defined);
        for b in &t.bindings {
            let Some(c) = Converter::of(b.converter) else {
                continue;
            };
            if c == Converter::Assign && b.sources.len() == 1 && b.mode == 0 {
                for &to in &b.targets {
                    if let (Some(x), Some(y)) = (kind(b.sources[0]), kind(to)) {
                        copies += 1;
                        wrote.insert((x, y));
                        if !super::wiring::fits(x, y) {
                            *refused.entry(format!("copy {x:#x} -> {y:#x}")).or_default() += 1;
                        }
                    }
                }
                continue;
            }
            for (k, &s) in b.sources.iter().enumerate() {
                let Some(given) = kind(s) else { continue };
                args += 1;
                if let Some(takes) = c.takes(k)
                    && !takes.contains(&given)
                {
                    *refused
                        .entry(format!("{c:?} arg {k} {given:#x}"))
                        .or_default() += 1;
                }
                if let Some(to) = c.passes(k).then(|| kind(b.targets[0])).flatten() {
                    wrote.insert((given, to));
                    if !super::wiring::fits(given, to) {
                        *refused
                            .entry(format!("{c:?} passes {given:#x} -> {to:#x}"))
                            .or_default() += 1;
                    }
                }
                let other = b.sources.get(1 - k.min(1)).and_then(|&o| kind(o));
                if matches!(c, Converter::Equal | Converter::NotEqual)
                    && let Some(other) = other
                {
                    wrote.extend([(given, other), (other, given)]);
                    if !super::wiring::comparable(given, other) {
                        *refused
                            .entry(format!("compare {given:#x} with {other:#x}"))
                            .or_default() += 1;
                    }
                }
            }
        }
    }
    assert!(
        copies > 10000 && args > 10000,
        "{copies} copies, {args} arguments"
    );
    assert!(
        refused.is_empty(),
        "refused what the game writes: {refused:#?}"
    );
    // And no more than it writes: each cross-family pair allowed is one
    // a shipped file makes.
    let unused: Vec<_> = super::wiring::SHIPPED
        .iter()
        .filter(|p| !wrote.contains(p))
        .collect();
    assert!(unused.is_empty(), "allowed, never written: {unused:x?}");
    let (enumeration, colour, vec3) = (0x11, 0x12, 0x06);
    let fits = super::wiring::fits;
    assert!(!fits(enumeration, T_FLOAT), "enum into Opacity");
    assert!(!fits(colour, vec3), "colour into Position");
    assert!(fits(vec3, colour), "a Vector3 input into ColorTint");
    let text = crate::ui::bindings::T_STRING;
    assert!(!fits(text, T_FLOAT), "text into Opacity");
}

// Needs the game: a rewire changes one wire, and places hold across any
// number of edits and undos in a view. Icon_Size onto Multiply's a
// (round 4: Multiply jumped from column 3 to 1 and a wire ran
// backwards): the binding keeps its place, mode and other sources, and
// every node on screen keeps its place and its pins their rows; a look
// at another view and back keeps them (round 6 reflowed). A second
// rewire and two undos keep them all the same (round 5: the second edit
// reflowed everything, rows swapped). Tidy lets go. A fresh layout draws
// every wire left to right before and after the rewire (round 5
// suspected a node left of its own source after a rewire and Tidy).
#[test]
fn a_rewire_moves_no_node() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = node(&ui, "Icon");
    let file = ui.node_file(icon);
    let fresh = |ui: &mut UiEditor, feeds: bool| backward(&widget_graph(ui, icon, feeds));
    assert_eq!(fresh(&mut ui, false), NONE, "icon row");
    let g0 = draw(&mut ui, icon, &[]);
    let names = |p: &[super::Pin]| p.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    let same = |g: &Graph, when: &str| {
        for n in &g.nodes {
            if let Some(o) = g0.nodes.iter().find(|o| o.key == n.key) {
                assert_eq!(n.rect[..2], o.rect[..2], "{when}: {} moved", n.title);
                assert_eq!(
                    (names(&n.ins), names(&n.outs)),
                    (names(&o.ins), names(&o.outs)),
                    "{when}: {} rows",
                    n.title
                );
            }
        }
    };
    let multiply_a = arg(&g0, "Multiply", 0);
    let Drop::Arg { binding, k } = multiply_a else {
        unreachable!()
    };
    let t0 = bindings(&ui.asset);
    let icon_size = input(&ui, "-Icon_Size");
    let said = ui.wire_up(icon_size, None, multiply_a).unwrap();
    assert!(said.starts_with("Rewired"), "{said}");
    let t1 = bindings(&ui.asset);
    assert_eq!(t1.bindings.len(), t0.bindings.len());
    let (b0, b1) = (&t0.bindings[binding], &t1.bindings[binding]);
    assert_eq!((b0.mode, b0.converter), (b1.mode, b1.converter), "in place");
    let place = |t: &Bindings, s: usize| format!("{:?}", t.slots[s]);
    assert_eq!(place(&t1, b1.sources[k]), place(&t0, icon_size));
    for (i, (&a, &b)) in b0.sources.iter().zip(&b1.sources).enumerate() {
        if i != k {
            assert_eq!(place(&t0, a), place(&t1, b), "source {i} kept");
        }
    }
    assert_eq!(fresh(&mut ui, false), NONE, "after the rewire");
    let feeds = fresh(&mut ui, true);
    assert_eq!(feeds, NONE, "after the rewire, what it feeds");
    let g1 = draw(&mut ui, icon, &[]);
    same(&g1, "first edit");
    // A look at another view and back.
    let mut other = g1.clone();
    let laid: Vec<[f32; 2]> = other.nodes.iter().map(|n| [n.rect[0], n.rect[1]]).collect();
    ui.graph.place((file, None, false), &mut other, &laid);
    same(&draw(&mut ui, icon, &[]), "after a look away");
    let from = g1
        .nodes
        .iter()
        .flat_map(|n| &n.outs)
        .find(|p| p.name == "Maximum_01")
        .and_then(|p| p.slot)
        .unwrap();
    ui.wire_up(from, None, multiply_a).unwrap();
    same(&draw(&mut ui, icon, &[]), "second edit");
    assert!(ui.undo(false));
    same(&draw(&mut ui, icon, &[]), "first undo");
    assert!(ui.undo(false));
    same(&draw(&mut ui, icon, &[]), "second undo");
    ui.graph.let_go(file.0);
    assert!(ui.graph.held.is_empty());
}

// Needs the game: an undo leaves the view drawn as it is, also one the
// edit was not made in (round 7: rewire in This widget, look at What
// it feeds, Ctrl Z: Icon jumped a column, Inputs swapped two rows).
#[test]
fn an_undo_keeps_the_view_drawn() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    select(&mut ui, "Icon");
    // Draws `feeds`'s view as the pane does.
    let draw = |ui: &mut UiEditor, feeds: bool| {
        ui.graph.feeds = feeds;
        shown(ui)
    };
    let multiply_a = arg(&draw(&mut ui, false), "Multiply", 0);
    ui.wire_up(input(&ui, "-Icon_Size"), None, multiply_a)
        .unwrap();
    let before = draw(&mut ui, true);
    assert!(ui.undo(false));
    let after = draw(&mut ui, true);
    let mut same = 0;
    for n in &after.nodes {
        let Some(o) = before.nodes.iter().find(|o| o.key == n.key) else {
            continue;
        };
        assert_eq!(n.rect[..2], o.rect[..2], "{} stays", n.title);
        for (now, then) in [(&n.ins, &o.ins), (&n.outs, &o.outs)] {
            let names = |p: &[super::Pin]| p.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
            let kept: Vec<String> = names(then)
                .into_iter()
                .filter(|x| now.iter().any(|p| &p.name == x))
                .collect();
            assert_eq!(
                names(now)[..kept.len()],
                kept[..],
                "{} keeps its rows",
                n.title
            );
        }
        same += 1;
    }
    assert!(same > 3, "the views share nodes");
}

// Needs the game: a + pin that gets bound keeps its row and its name,
// and gets both back on undo, for properties whose friendly name is
// not the file's (round 8: "+ Opacity" came back as AlphaTint at the
// bottom, every row below shifting up). A template input dropped onto
// a widget's open pin binds it as one undo step and says what it wired
// to what; it is bound in the "+ " row it was dropped on, which is back
// there after the undo (round 6 moved it to the bottom of the node and
// the undo kept it there). Open pins are named as they will be once
// bound: the file's AlphaTint.
#[test]
fn a_bound_open_pin_keeps_its_row_and_name() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = select(&mut ui, "Icon");
    let widget = ui.nodes[icon].widget;
    let rows = |ui: &mut UiEditor| {
        let g = shown(ui);
        let n = g.nodes.iter().find(|n| focus(n)).unwrap();
        n.ins
            .iter()
            .map(|p| p.name.trim_start_matches("+ ").to_owned())
            .collect::<Vec<_>>()
    };
    let before = rows(&mut ui);
    for (target, name) in [(Target::Alpha, "AlphaTint"), (Target::X, "Position.x")] {
        let at = before.iter().position(|r| r == name).expect(name);
        let said = ui
            .wire_up(
                input(&ui, "-Icon_Size"),
                None,
                Drop::Open { widget, target },
            )
            .unwrap();
        assert!(said.ends_with(&format!("Icon · {name}")), "{said}");
        let now = rows(&mut ui);
        assert_eq!(now, before, "{name} bound: every row where it was");
        assert_eq!(now[at], name);
    }
    assert!(ui.undo(false) && ui.undo(false));
    assert_eq!(rows(&mut ui), before, "undone: the + rows where they were");

    let targets = open_targets(&ui, icon);
    let row = |g: &Graph, name: &str| {
        g.nodes
            .iter()
            .filter(|n| focus(n))
            .find_map(|n| n.ins.iter().position(|p| p.name == name))
            .unwrap_or_else(|| panic!("{name}"))
    };
    let at = row(&draw(&mut ui, icon, &targets), "+ Visible");
    let count = |ui: &UiEditor| bindings(&ui.asset).bindings.len();
    let before = count(&ui);
    let from = slot_ending(&ui.file_table(icon).unwrap(), "IsScalable_Content");
    let visible = Drop::Open {
        widget,
        target: Target::Visible,
    };
    let said = ui.wire_up(from, None, visible).unwrap();
    assert!(
        said.contains("IsScalable") && said.ends_with("Icon · Visible"),
        "{said}"
    );
    assert_eq!(count(&ui), before + 1);
    assert_eq!(
        row(&draw(&mut ui, icon, &targets), "Visible"),
        at,
        "bound in its row"
    );
    // Bound now: no "+ Visible" pin offers it again.
    let ft = ui.file_table(node(&ui, "Icon")).unwrap();
    let g = Graph::build(&ft, Some(widget), &Target::ALL, false, false);
    let pins: Vec<&String> = g
        .nodes
        .iter()
        .filter(|n| focus(n))
        .flat_map(|n| n.ins.iter().map(|p| &p.name))
        .collect();
    assert!(!pins.iter().any(|p| *p == "+ Visible"), "{pins:?}");
    assert!(pins.iter().any(|p| *p == "+ AlphaTint"), "{pins:?}");
    assert!(ui.undo(false));
    assert_eq!(count(&ui), before);
    assert_eq!(
        row(&draw(&mut ui, icon, &targets), "+ Visible"),
        at,
        "back in its row"
    );
}

// Needs the game: a fixed colour shows as a colour, and the picker puts
// colours into it live, all of one opening as one undo step. The picker
// says what it edits, and when a pick there cannot show on the canvas,
// why: the icon row's colour if / else has its if false, so its then is
// the branch not taken (round 14: a pick there changed nothing visible
// and nothing said so).
#[test]
fn a_colour_pin_takes_picked_colours_as_one_step() {
    use gpui_editor_kit::widgets::Hsv;
    let Some(mut ui) = open(ICON_ROW) else { return };
    select(&mut ui, "Icon");
    let g = shown(&mut ui);
    let fixed = |title: &str, pin: &str, colour: bool| {
        g.nodes
            .iter()
            .filter(|n| n.title.contains(title))
            .flat_map(|n| &n.ins)
            .find(|p| p.name == pin && p.constant.is_some() && p.rgb.is_some() == colour)
            .and_then(|p| p.constant)
            .expect("a fixed value")
    };
    let (then, factor) = (fixed("if / else", "then", true), fixed("max", "b", false));
    let (about, idle) = ui.pick_context(then);
    assert_eq!(about, "then of if / else → Icon · ColorTint");
    assert!(
        idle.as_deref()
            .is_some_and(|w| w.contains("condition is false")),
        "{idle:?}"
    );
    // A max's fixed bound always counts.
    let (about, idle) = ui.pick_context(factor);
    assert!(about.starts_with("fixed value → "), "{about}");
    assert_eq!(idle, None);

    // The colour if / else's then: (span, colour, shown value).
    let then = |ui: &mut UiEditor| {
        let g = shown(ui);
        let p = g
            .nodes
            .iter()
            .filter(|n| n.title.contains("if / else") && n.sub.is_empty())
            .flat_map(|n| &n.ins)
            .find(|p| p.name == "then" && p.rgb.is_some())
            .expect("a colour then");
        (
            p.constant.unwrap(),
            p.rgb,
            p.value.clone().unwrap_or_default(),
        )
    };
    let hue = |h: f32| Hsv { h, s: 1., v: 1. };
    let (red, green, blue) = (hue(0.), hue(1. / 3.), hue(2. / 3.));
    let (span, rgb, _) = then(&mut ui);
    assert_eq!(rgb, Some([255, 255, 255]));
    ui.open_pick_for_test(span, [255, 255, 255]);
    ui.pick_colour(red);
    assert_eq!(then(&mut ui).1, Some([255, 0, 0]));
    ui.pick_colour(green);
    let (_, rgb, text) = then(&mut ui);
    assert_eq!((rgb, text.as_str()), (Some([0, 255, 0]), "[0, 255, 0]"));
    ui.graph.pick = None;
    assert!(ui.undo(false), "one step for the whole opening");
    assert_eq!(then(&mut ui).1, Some([255, 255, 255]));
    // A save mid-opening ends that step: the next pick is unsaved again,
    // and undo goes back to what was saved (round 14: it was lost).
    ui.open_pick_for_test(span, [255, 255, 255]);
    ui.pick_colour(red);
    ui.history.mark_saved();
    assert!(!ui.dirty());
    ui.pick_colour(blue);
    assert!(ui.dirty(), "a pick after the save is unsaved");
    assert_eq!(then(&mut ui).1, Some([0, 0, 255]));
    assert!(ui.undo(false));
    assert_eq!(then(&mut ui).1, Some([255, 0, 0]), "back to what was saved");
    assert!(!ui.dirty());

    // Esc puts back the colour the picker opened on, as every Esc here
    // cancels (round 14: it kept the pick); a click elsewhere keeps it.
    ui.open_pick_for_test(span, [255, 0, 0]);
    ui.pick_colour(green);
    ui.pick_colour(blue);
    assert!(ui.graph_key_down(&key("escape")));
    assert!(!ui.picking());
    assert_eq!(then(&mut ui).1, Some([255, 0, 0]));
    assert!(!ui.dirty(), "as saved again");
    // A save in between: put back all the same, and unsaved, since the
    // file on disk has the green.
    ui.open_pick_for_test(span, [255, 0, 0]);
    ui.pick_colour(green);
    ui.history.mark_saved();
    ui.pick_colour(blue);
    assert!(ui.graph_key_down(&key("escape")));
    assert_eq!(then(&mut ui).1, Some([255, 0, 0]));
    assert!(ui.dirty());
    // Nothing picked: Esc only closes.
    ui.history.mark_saved();
    ui.open_pick_for_test(span, [255, 0, 0]);
    assert!(ui.graph_key_down(&key("escape")));
    assert!(!ui.picking() && !ui.dirty());
    assert_eq!(then(&mut ui).1, Some([255, 0, 0]));
    // A click elsewhere keeps the pick.
    ui.open_pick_for_test(span, [255, 0, 0]);
    ui.pick_colour(green);
    ui.graph.pick = None;
    assert_eq!(then(&mut ui).1, Some([0, 255, 0]));
}

// Needs the game: fixed values on pins are typed over in the graph. A
// fixed colour and a fixed text (the icon's colour if / else, the
// Equal's empty string): a colour takes three numbers or hex, else it is
// refused and kept. A fixed value on a logic node's input (the icon's
// if / else of two bindings feeds _hub01.x 0 when its condition holds):
// Enter applies it as one undo step, the pin and the evaluation follow,
// undo brings the old value back.
#[test]
fn fixed_values_on_pins_are_typed_over() {
    use gpui_editor_kit::field::{Filter, TextEdit};
    let Some(mut ui) = open(ICON_ROW) else { return };
    select(&mut ui, "Icon");
    // (span, value) of the first fixed value on a pin of a node titled
    // `title` (one binding: the pair is "if / else" too).
    let pin = |ui: &mut UiEditor, title: &str| {
        shown(ui)
            .nodes
            .iter()
            .filter(|n| n.title.contains(title) && n.sub.is_empty())
            .flat_map(|n| &n.ins)
            .find(|p| p.constant.is_some())
            .map(|p| (p.constant.unwrap(), p.value.clone().unwrap_or_default()))
            .unwrap()
    };
    let type_over = |ui: &mut UiEditor, span: usize, text: &str| {
        ui.editing = Some(TextEdit::new(span, text.to_owned(), Filter::Text));
        ui.flush_edits();
    };
    let (colour, value) = pin(&mut ui, "if / else");
    assert_eq!(value, "[255, 255, 255]");
    assert_eq!(ui.edit_text(colour).as_deref(), Some("[255, 255, 255]"));
    type_over(&mut ui, colour, "[255, 0, 0]");
    assert_eq!(pin(&mut ui, "if / else").1, "[255, 0, 0]");
    // Refused whole, never stored with a number dropped (round 13:
    // [255, 300, 0, 0] became [255, 0, 0]).
    for (typed, says) in [
        ("1, 2", "three numbers"),
        ("[255, 300, 0, 0]", "three numbers"),
        ("255 300 0", "300 is not 0 to 255"),
        ("0 0 x", "x is not a number"),
    ] {
        type_over(&mut ui, colour, typed);
        let said = ui.message.take();
        assert!(
            said.as_ref().is_some_and(|(m, e)| *e && m.contains(says)),
            "{typed}: {said:?}"
        );
        assert_eq!(pin(&mut ui, "if / else").1, "[255, 0, 0]", "{typed}: kept");
    }
    let (text, value) = pin(&mut ui, "Equal");
    assert_eq!(value, "\"\"");
    type_over(&mut ui, text, "Icon_Placeholder");
    assert_eq!(pin(&mut ui, "Equal").1, "\"Icon_Placeholder\"");
    assert!(ui.undo(false) && ui.undo(false));
    assert_eq!(pin(&mut ui, "if / else").1, "[255, 255, 255]");

    // The pair's `then`: (span, shown value).
    let then = |ui: &mut UiEditor| {
        let g = shown(ui);
        let n = g
            .nodes
            .iter()
            .find(|n| matches!(n.kind, Kind::Logic { also: Some(_), .. }))
            .expect("an if / else of two bindings");
        let p = n.ins.iter().find(|p| p.name == "then").unwrap();
        (
            p.constant.expect("an editable fixed value"),
            p.value.clone(),
        )
    };
    let (span, before) = then(&mut ui);
    assert_eq!(before.as_deref(), Some("0"));
    // What a click on it starts (`Editor::start_span_edit`), then typing.
    ui.editing = Some(TextEdit::new(
        span,
        ui.edit_text(span).unwrap(),
        Filter::Text,
    ));
    for k in ["ctrl-a", "7", "enter"] {
        ui.key_down(&key(k));
    }
    assert!(ui.editing.is_none());
    assert_eq!(then(&mut ui).1.as_deref(), Some("7"));
    // Refused in plain words, not the parser's; Tab and Enter alike keep
    // the text to fix (round 14: Tab did nothing); Esc then keeps what
    // was there.
    ui.editing = Some(TextEdit::new(span, "abc", Filter::Text));
    for k in ["tab", "enter"] {
        ui.message = None;
        ui.key_down(&key(k));
        let said = ui.message.take().map(|m| m.0).unwrap_or_default();
        assert!(said.starts_with("Not applied: Enter a"), "{k}: {said}");
        assert_eq!(ui.editing.as_ref().map(|e| e.text.as_str()), Some("abc"));
    }
    ui.key_down(&key("escape"));
    assert!(ui.editing.is_none());
    assert_eq!(then(&mut ui).1.as_deref(), Some("7"));
    assert!(ui.undo(false));
    assert_eq!(then(&mut ui).1.as_deref(), Some("0"));
}

// Needs the game: a gate pair's then and else take a wire, one binding's
// value each, in one undo step, and the if / else stays one (round 8
// refused both; removing it to wire anew lost the conditional). Its if,
// both bindings' condition, still says why not.
#[test]
fn a_gate_pairs_branches_rewire() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    select(&mut ui, "Icon");
    // The pair's pins as drops: (if, then, else).
    let pair = |ui: &mut UiEditor| {
        let g = shown(ui);
        let n = g
            .nodes
            .iter()
            .position(|n| matches!(n.kind, Kind::Logic { also: Some(_), .. }))
            .expect("an if / else of two bindings");
        let drops: Vec<Drop> = g
            .drop_targets()
            .into_iter()
            .filter(|d| d.0 == n)
            .map(|d| d.2)
            .collect();
        assert_eq!(
            g.nodes[n].sub, "2 bindings",
            "told apart from a one-binding if / else"
        );
        (drops, g.nodes[n].key.clone())
    };
    let (drops, key) = pair(&mut ui);
    assert!(matches!(drops[0], Drop::No(why) if why.contains("condition")));
    let bytes = ui.asset.encode();
    let mut rewired = 0;
    for (k, name) in [(1, "then"), (2, "else")] {
        let size = input(&ui, "-Icon_Size");
        let v = ui.wire_verdicts(size, None, &[drops[k]]).remove(0);
        let Ok(said) = &v else {
            // A branch already reading Icon_Size says so.
            assert!(v.unwrap_err().contains("already reads it"));
            continue;
        };
        assert!(
            said.contains(&format!("'s {name} reads Icon_Size")),
            "{said}"
        );
        let Drop::Arg { binding, .. } = drops[k] else {
            panic!("{name} takes a wire")
        };
        let count = bindings(&ui.asset).bindings.len();
        ui.wire_up(size, None, drops[k]).unwrap();
        let t = bindings(&ui.asset);
        assert_eq!(t.bindings.len(), count, "rewired in place");
        assert_eq!(
            super::place_key(&t.slots, t.bindings[binding].sources[0]),
            super::place_key(&t.slots, input(&ui, "-Icon_Size"))
        );
        let (_, still) = pair(&mut ui);
        assert_eq!(still, key, "still one if / else");
        assert!(ui.undo(false));
        assert_eq!(ui.asset.encode(), bytes, "one undo step");
        rewired += 1;
    }
    assert!(rewired > 0, "a branch took the wire");
}

// Needs the game: a node an edit inserts between two columns sits
// between them (what it feeds moves over to make room), every wire
// still runs left to right, and once placed it stays when what it feeds
// is dragged (round 9: an inverted copy's "not" sat under Inputs with
// its wire running back through it, and placed nodes followed their
// target until the next edit).
#[test]
fn a_node_an_edit_inserts_makes_room() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let input = icon_group(&mut ui);
    // Every wire left to right and no node over another, in the graph
    // the pane shows.
    let sound = |ui: &mut UiEditor, what: &str| {
        let g = shown(ui);
        assert_eq!(backward(&g), NONE, "{what}: runs backward");
        assert_eq!(overlaps(&g), NONE, "{what}: nodes overlap");
    };
    sound(&mut ui, "before");
    for (transform, what) in [(Transform::AsIs, "as is"), (Transform::Invert, "inverted")] {
        if transform == Transform::Invert {
            // The target dragged a little first (round 10: the insert
            // then landed under it, the drag snapping it back).
            let g = shown(&mut ui);
            let n = titled(&g, "Icon_Group");
            let file = ui.node_file(ui.subject()).0;
            let at = [n.rect[0] + 20., n.rect[1]];
            ui.graph.moved.insert((file, n.key.clone()), at);
        }
        bind(&mut ui, input, transform);
        sound(&mut ui, what);
        sound(&mut ui, what);
    }
    // Drag what "not" feeds: "not" stays where it was placed.
    let g = shown(&mut ui);
    let (not, target) = (titled(&g, "not"), titled(&g, "Icon_Group"));
    let file = ui.node_file(ui.subject()).0;
    let to = [target.rect[0] + 300., target.rect[1] + 200.];
    ui.graph.moved.insert((file, target.key.clone()), to);
    let now = titled(&shown(&mut ui), "not").rect;
    assert_eq!(now[..2], not.rect[..2], "placed once, it stays");
}

// Needs the game: a node an edit adds lands in view: when its place is
// off the pane, the view pans the least that shows it, the zoom as it
// was (round 9: a bind's new node landed below the view). A node that
// comes back (a redo, or the same bind made again after an undo) is
// revealed too, though it keeps the place it had (round 13: it came
// back off the pane).
#[test]
fn a_node_an_edit_adds_or_brings_back_is_in_view() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let input = icon_group(&mut ui);
    let docked = [900., 400.];
    let shows_not = |ui: &mut UiEditor| {
        let r = titled(&shown(ui), "not").rect;
        on_pane(&ui.graph, r, docked)
    };
    ui.shown_graph().unwrap();
    bind(&mut ui, input, Transform::AsIs);
    // The pane as drawn: framed with the new node in it, it shows.
    ui.graph.bounds = pane(docked[0], docked[1]);
    bind(&mut ui, input, Transform::Invert);
    assert!(shows_not(&mut ui));
    assert!(ui.undo(false));
    // Then looking far to the right of everything, zoomed out enough that
    // the three fit side by side.
    ui.shown_graph().unwrap();
    ui.graph.zoom = 1.;
    ui.graph.pan = [-3000., 0.];
    bind(&mut ui, input, Transform::Invert);
    let g = shown(&mut ui);
    let not = g
        .nodes
        .iter()
        .position(|n| n.title.contains("not"))
        .unwrap();
    // The new node, what it reads and what it feeds: the edit means
    // source -> not -> target.
    let near: Vec<usize> = g
        .wires
        .iter()
        .filter_map(|w| match (w.from.0 == not, w.to.0 == not) {
            (true, _) => Some(w.to.0),
            (_, true) => Some(w.from.0),
            _ => None,
        })
        .collect();
    assert!(near.len() >= 2, "{near:?}");
    for i in std::iter::once(not).chain(near) {
        let r = g.nodes[i].rect;
        assert!(on_pane(&ui.graph, r, docked), "{}: {r:?}", g.nodes[i].title);
    }
    assert_eq!(ui.graph.zoom, 1., "no refit");
    // Back by a redo, then by the same bind after an undo, each with the
    // view looking away.
    assert!(ui.undo(false));
    ui.shown_graph().unwrap();
    ui.graph.pan = [-3000., 0.];
    assert!(ui.undo(true), "redo");
    assert!(shows_not(&mut ui), "back by a redo");
    assert!(ui.undo(false));
    ui.shown_graph().unwrap();
    ui.graph.pan = [-3000., 0.];
    bind(&mut ui, input, Transform::Invert);
    assert!(shows_not(&mut ui), "back by the same bind");
}

// A chain wider than the pane: the new node and what it feeds show,
// the source is left out.
#[test]
fn a_reveal_too_wide_keeps_the_new_node_and_its_target() {
    let mut view = GraphView {
        bounds: pane(400., 300.),
        zoom: 1.,
        ..Default::default()
    };
    let new = [2000., 0., 100., 60.];
    let target = [2150., 0., 100., 60.];
    let source = [1000., 0., 100., 60.];
    view.reveal(&[new, target, source], 2);
    for r in [new, target] {
        let (a, b) = (
            view.to_screen([r[0], r[1]]),
            view.to_screen([r[0] + r[2], r[1] + r[3]]),
        );
        assert!(a[0] >= 0. && b[0] <= 400., "{a:?} {b:?}");
    }
    let a = view.to_screen([source[0], source[1]]);
    assert!(a[0] < 0., "{a:?}");
    // When everything fits, everything shows.
    view.reveal(&[new, [1900., 0., 60., 60.]], 1);
    assert!(view.to_screen([1900., 0.])[0] >= 0.);
    // A target taller than the pane: the source still shows beside it.
    let tall = [2150., 0., 100., 500.];
    let near = [1950., 0., 100., 60.];
    view.pan = [-5000., 0.];
    view.reveal(&[new, tall, near], 2);
    let a = view.to_screen([near[0], near[1]]);
    let b = view.to_screen([tall[0] + tall[2], 0.]);
    assert!(a[0] >= 0. && b[0] <= 400., "{a:?} {b:?}");
}

// Needs the game: in the whole file's graph, making room for an insert
// moves what the target feeds, not every node from its column right
// (round 10: one inverted bind moved six columns of unrelated chains).
#[test]
fn making_room_moves_only_what_lies_downstream() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let input = icon_group(&mut ui);
    ui.bindings_all = true;
    let places = |ui: &mut UiEditor| {
        shown(ui)
            .nodes
            .into_iter()
            .map(|n| (n.key, [n.rect[0], n.rect[1]]))
            .collect::<std::collections::HashMap<_, _>>()
    };
    places(&mut ui);
    bind(&mut ui, input, Transform::AsIs);
    let before = places(&mut ui);
    bind(&mut ui, input, Transform::Invert);
    let after = places(&mut ui);
    let moved: Vec<&String> = before
        .iter()
        .filter(|(k, at)| after.get(*k).is_some_and(|now| now != *at))
        .map(|(k, _)| k)
        .collect();
    assert!(
        moved.len() <= 2,
        "only Icon_Group and what it feeds: {moved:?}"
    );
}

// Needs the game: a node an edit adds covers no other, also after an
// undo and a redo (round 8: Visible equals -Icon_Size from the binder
// drew its Equal exactly over the Equal already there).
#[test]
fn a_node_an_edit_adds_covers_no_other() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    select(&mut ui, "Icon");
    let covered = |ui: &mut UiEditor| overlaps(&shown(ui));
    assert_eq!(covered(&mut ui), NONE, "the layout itself");
    let size = ui
        .asset
        .properties()
        .iter()
        .position(|p| ui.asset.property_name(p) == "-Icon_Size")
        .unwrap();
    ui.open_binder();
    let b = ui.binder.as_mut().unwrap();
    b.source = Some(crate::ui::bindings::Source::Input(size));
    b.transform = Transform::Equals;
    b.value = "24".into();
    ui.bind().unwrap();
    assert_eq!(covered(&mut ui), NONE, "after the bind");
    assert!(ui.undo(false));
    assert_eq!(covered(&mut ui), NONE, "after the undo");
    assert!(ui.undo(true));
    assert_eq!(covered(&mut ui), NONE, "after the redo");
}

// Needs the game: a drag shows what its drop does. For Icon_Size
// dragged over the Icon's graph, every input pin's verdict is what
// `wire_up` then does there: the refused ones refuse with the same
// words (ColorTint takes a colour; Multiply's result cannot read
// itself), the accepted ones wire (Size.x replacing MOBO_02), each
// undone again.
#[test]
fn a_drag_shows_what_its_drop_does() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = node(&ui, "Icon");
    let targets = open_targets(&ui, icon);
    let ft = ui.file_table(icon).unwrap();
    let g = Graph::build(&ft, Some(ui.nodes[icon].widget), &targets, false, false);
    let drops: Vec<Drop> = g.drop_targets().into_iter().map(|d| d.2).collect();
    let t = bindings(&ui.asset);
    let icon_size = input(&ui, "-Icon_Size");
    let verdicts = ui.wire_verdicts(icon_size, None, &drops);
    assert!(
        verdicts
            .iter()
            .any(|v| v.as_ref().is_err_and(|e| e.contains("takes Color"))),
        "{verdicts:?}"
    );
    assert!(
        verdicts
            .iter()
            .any(|v| v.as_ref().is_ok_and(|s| s.contains("replacing MOBO_02")))
    );
    // Both branches of ColorTint's if / else pass their value into
    // ColorTint and answer alike (round 7: then, a constant read as a
    // Vector3, was green; else, fed by a colour, red).
    let tint: Vec<_> = drops
        .iter()
        .zip(&verdicts)
        .filter(|(d, _)| {
            matches!(d, Drop::Arg { binding, k: 0 | 1 }
            if super::Converter::of(t.bindings[*binding].converter)
                == Some(super::Converter::IfTrueFalseAssign)
                && t.bindings[*binding].targets.iter().any(|&x| {
                    crate::ui::bindings::slot_kind(&ui.asset, &t.slots, x) == Some(0x12)
                }))
        })
        .map(|(_, v)| v.clone())
        .collect();
    assert_eq!(tint.len(), 2, "{tint:?}");
    assert!(
        tint.iter().all(|v| v
            .as_ref()
            .is_err_and(|e| e.contains("ColorTint takes Color"))),
        "{tint:?}"
    );
    let bytes = ui.asset.encode();
    for (drop, verdict) in drops.iter().zip(&verdicts) {
        match (verdict, ui.wire_up(icon_size, None, *drop)) {
            (Err(why), Err(e)) => assert_eq!(*why, format!("{e:#}")),
            (Ok(_), Ok(_)) => assert!(ui.undo(false)),
            (v, d) => panic!("{v:?} shown, {:?} done", d.map_err(|e| e.to_string())),
        }
        assert_eq!(
            ui.asset.encode(),
            bytes,
            "each try leaves the file as it was"
        );
    }
}

// Needs the game: a wire onto an event place adds a trigger and names
// the ones it joins (the quest button's clip Leave · Start already has
// two); a wire onto a value one copy fills replaces that copy where it
// is in the file (round 16: triggers were replaced, and a replaced copy
// moved to the end).
#[test]
fn events_take_another_trigger_and_values_keep_their_place() {
    let Some(mut ui) = open(BUTTON) else { return };
    let ft = ui.file_table(0).unwrap();
    let (slots, b) = (&ft.table.slots, &ft.table.bindings);
    let leave_start = slot_ending(&ft, "clip Leave · Start");
    let enter = event_ending(&ft, "OnMouseEnter");
    let triggers = |t: &Bindings, place: &Slot| {
        t.bindings
            .iter()
            .filter(|x| x.targets.iter().any(|&y| &t.slots[y] == place))
            .count()
    };
    let place = slots[leave_start].clone();
    assert_eq!(triggers(&ft.table, &place), 2);
    // A value one copy fills, and another source of its kind.
    let (copy, target, other) = (0..b.len())
        .find_map(|i| {
            let x = &b[i];
            let t = *x.targets.first()?;
            let k = ft.kind(t)?;
            (x.converter == crate::ui::bindings::ASSIGN
                && x.targets.len() == 1
                && ft.event_slot(t) != Some(true))
            .then_some(())?;
            let other = (0..slots.len()).find(|&s| {
                s != x.sources[0]
                    && s != t
                    && ft.kind(s) == Some(k)
                    && matches!(slots[s], Slot::Widget { .. })
            })?;
            Some((i, t, other))
        })
        .expect("a value copy and another source");
    let other_slot = slots[other].clone();
    drop(ft);

    let said = ui
        .wire_verdicts(enter, None, &[Drop::Slot(leave_start)])
        .remove(0)
        .unwrap();
    assert!(
        said.contains("OnMouseEnter also starts BtnOverlay_DS › clip Leave, with"),
        "{said}"
    );
    assert!(
        said.contains("OnMouseLeave and when it turns false"),
        "{said}"
    );
    ui.wire_up(enter, None, Drop::Slot(leave_start)).unwrap();
    let reopened = reopen(&ui.asset);
    assert_eq!(
        triggers(&bindings(&reopened), &place),
        3,
        "saved and opened again"
    );

    let shape = |a: &Asset| {
        let t = bindings(a);
        t.bindings
            .iter()
            .map(|x| {
                let targets: Vec<Slot> = x.targets.iter().map(|&y| t.slots[y].clone()).collect();
                (x.converter, targets)
            })
            .collect::<Vec<_>>()
    };
    let before = shape(&ui.asset);
    let said = ui.wire_up(other, None, Drop::Slot(target)).unwrap();
    assert!(said.contains(", replacing "), "{said}");
    assert_eq!(shape(&ui.asset), before, "every binding where it was");
    let t = bindings(&ui.asset);
    assert_eq!(
        t.slots[t.bindings[copy].sources[0]], other_slot,
        "the copy reads the new source"
    );
}

// Needs the game: game data reads apart from the widgets' properties
// (user, round 17): a pin on a widget, Inputs or game-data node sits on a
// view-model node exactly when its place is a view model or one of its
// fields; the Inputs node holds value inputs only; and a container's view
// model (BtnOverlay_DS's BtnEvents) is a node of its own, not pins on the
// widget.
#[test]
fn view_models_read_apart_from_properties() {
    for file in [BUTTON, QUEST_TRACKER, ICON_ROW] {
        let Some(mut ui) = open(file) else { return };
        let ft = ui.file_table(0).unwrap();
        let g = file_graph(&mut ui, 0);
        let mut models = 0;
        for n in &g.nodes {
            if !matches!(n.kind, Kind::Widget { .. } | Kind::Inputs | Kind::Data) {
                continue;
            }
            for p in n.ins.iter().chain(&n.outs) {
                let Some(s) = p.slot else { continue };
                let model = ft.view_model(s).is_some();
                models += usize::from(model);
                assert_eq!(
                    matches!(n.kind, Kind::Data),
                    model,
                    "{file}: {} · {}",
                    n.title,
                    p.name
                );
            }
        }
        assert!(models > 0, "{file} reads game data");
    }
    let Some(mut ui) = open(BUTTON) else { return };
    let g = file_graph(&mut ui, 0);
    let events = titled(&g, "BtnEvents");
    assert!(matches!(events.kind, Kind::Data));
    assert_eq!(events.sub, "BtnOverlay_DS's view model");
    assert!(events.ins.iter().any(|p| p.name == "Leave"));
    assert!(
        !titled(&g, "BtnOverlay_DS")
            .ins
            .iter()
            .any(|p| p.name.contains("BtnEvents"))
    );
}

// Needs the game: view models keep to two lanes apart from the widgets
// (user, round 17): what the file reads at the graph's left edge, what it
// writes right of every widget, and no other node inside either lane.
#[test]
fn view_models_keep_to_their_lanes() {
    for file in [BUTTON, QUEST_TRACKER, ICON_ROW] {
        let Some(mut ui) = open(file) else { return };
        let g = file_graph(&mut ui, 0);
        let lanes = g.lanes();
        assert!(!lanes.is_empty(), "{file} has view models");
        let model = |n: &Node| matches!(n.kind, Kind::Data);
        let left = g.nodes.iter().map(|n| n.rect[0]).fold(f32::MAX, f32::min);
        let others_right = g
            .nodes
            .iter()
            .filter(|n| !(model(n) && !n.ins.is_empty()))
            .map(|n| n.rect[0] + n.rect[2])
            .fold(0., f32::max);
        for n in g.nodes.iter().filter(|n| model(n)) {
            if n.ins.is_empty() {
                assert_eq!(
                    n.rect[0], left,
                    "{file}: {} reads at the left edge",
                    n.title
                );
            } else {
                assert!(
                    n.rect[0] > others_right,
                    "{file}: {} written right of the widgets",
                    n.title
                );
            }
        }
        for ([l, r], label) in &lanes {
            for n in g.nodes.iter().filter(|n| !model(n)) {
                let (a, b) = (n.rect[0], n.rect[0] + n.rect[2]);
                assert!(b <= *l || a >= *r, "{file}: {} inside {label}", n.title);
            }
        }
    }
}

// Needs the game: with the whole file shown, the root's Shown nothing
// reads yet is a wire to start from, so a clip nothing starts can start
// when the screen shows (round 17: no pin offered it); the nameless root
// reads "root", as the List names it.
#[test]
fn the_screen_showing_starts_a_clip() {
    use super::wiring::Source;
    let Some(mut ui) = open(BUTTON) else { return };
    ui.graph.on = true;
    ui.canvas.selected = None;
    let g = shown(&mut ui);
    let shown_pin = g
        .nodes
        .iter()
        .filter(|n| n.title == "root")
        .flat_map(|n| &n.outs)
        .find(|p| p.name == "+ Shown")
        .and_then(|p| p.make)
        .expect("+ Shown on the root's values");
    let start = g
        .drop_targets()
        .into_iter()
        .find(|&(n, k, _)| {
            g.nodes[n].title == "clip Nothing" && g.nodes[n].ins[k].name == "+ Start"
        })
        .expect("clip Nothing's + Start")
        .2;
    let said = ui
        .wire_verdicts(Source::New(shown_pin), None, &[start])
        .remove(0)
        .unwrap();
    assert_eq!(said, "root · Shown starts clip Nothing");
    ui.wire_up(Source::New(shown_pin), None, start).unwrap();
    let t = bindings(&reopen(&ui.asset));
    assert!(t.bindings.iter().any(|b| {
        matches!(&t.slots[b.sources[0]], Slot::Widget { widget: Some(0), path } if path[..] == [(1, 0x8000_003c)])
            && matches!(&t.slots[b.targets[0]], Slot::Clip { path, .. } if path[..] == [(1, 0x8000_001f)])
    }));
    // Read now: the pin is the root's Shown, no longer a +.
    let g = shown(&mut ui);
    assert!(
        !g.nodes
            .iter()
            .flat_map(|n| &n.outs)
            .any(|p| p.name == "+ Shown")
    );
}

// Needs the game: a button added from the library sends events
// nothing reads yet (OnPress): its values node offers them, and the
// file's clips sit in the same view, so a wire from one onto a clip's +
// Start, through the pane's own drop target, makes both places in one
// undo step, exact bytes back and forth (round 17: the two ends were in
// different views, and this test built the target by hand). A clip's
// Completed nothing reads is a wire to start from too. An event is
// refused on a value (round 16: a new button had nothing to wire).
#[test]
fn a_new_buttons_press_starts_a_clip() {
    use super::wiring::{NewPlace, Source};
    use crate::ui::files::{Entry, Index};
    use crate::ui::hierarchy::Place;
    use crate::ui::library::Pick;
    let Some(mut ui) = open(BUTTON) else { return };
    let widgets = ui.asset.widgets();
    let button = widgets
        .iter()
        .position(|w| w.name.as_deref() == Some("AnnoButtonWT"))
        .unwrap();
    let template = ui.asset.container_template(&widgets[button]).unwrap().id;
    let nothing = ui
        .asset
        .clips()
        .iter()
        .position(|c| c.name == "Nothing")
        .unwrap();
    let index = Index::of(vec![Entry {
        file: template.to_string(),
        kind: "WI".into(),
        name: "AnnoButtonWT".into(),
        widgets: 3,
        used_by: 1,
        example: Some((BUTTON.into(), button as u32)),
        widget_type: None,
    }]);
    let r = ui.nodes[0].rect;
    let at = [r[0] + r[2] / 2., r[1] + r[3] / 2.];
    ui.add_from_library(Pick::Template(0), &index, Place::Canvas(at))
        .unwrap();
    let added = ui.canvas.selected.expect("the new button is selected");
    let w = ui.nodes[added].widget;
    assert_ne!(w, button);
    ui.graph.on = true;
    ui.bindings_all = false;
    let g = shown(&mut ui);
    let press = g
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, Kind::Widget { widget, .. } if widget == w))
        .flat_map(|n| &n.outs)
        .find(|p| p.name == "+ OnPress")
        .and_then(|p| p.make)
        .expect("+ OnPress on the new button's values");
    let start = g
        .drop_targets()
        .into_iter()
        .find(|&(n, k, _)| {
            g.nodes[n].title == "clip Nothing" && g.nodes[n].ins[k].name == "+ Start"
        })
        .expect("clip Nothing's + Start beside the new button")
        .2;
    let visible = Drop::Open {
        widget: w,
        target: Target::Visible,
    };
    let refused = ui
        .wire_verdicts(Source::New(press), None, &[visible])
        .remove(0)
        .unwrap_err();
    assert!(refused.contains("OnPress is an event"), "{refused}");
    let (revision, before) = (ui.history.revision(), ui.asset.encode());
    let said = ui.wire_up(Source::New(press), None, start).unwrap();
    assert!(said.ends_with("· OnPress → clip Nothing · Start"), "{said}");
    assert_eq!(ui.history.revision(), revision + 1, "one undo step");
    let after = ui.asset.encode();
    // The wire stays in the view it was made in: what the button's events
    // drive is part of its view.
    let g = shown(&mut ui);
    let nothing_node = titled(&g, "clip Nothing");
    assert!(
        nothing_node.ins.iter().any(|p| p.name == "Start"),
        "bound Start in view"
    );
    assert!(
        g.wires
            .iter()
            .any(|x| g.nodes[x.to.0].title == "clip Nothing" && x.event)
    );
    assert!(ui.undo(false));
    assert_eq!(ui.asset.encode(), before, "undo restores the bytes");
    assert!(ui.undo(true));
    assert_eq!(ui.asset.encode(), after, "redo restores the edit");
    let t = bindings(&reopen(&ui.asset));
    let NewPlace::Event { uid, .. } = press else {
        unreachable!()
    };
    assert!(t.bindings.iter().any(|b| {
        matches!(&t.slots[b.sources[0]], Slot::Widget { widget: Some(x), path } if *x == w && path[..] == [(1, uid)])
            && matches!(&t.slots[b.targets[0]], Slot::Clip { clip, path } if *clip == nothing && path[..] == [(1, 0x8000_001f)])
    }));
    // A clip's Completed nothing reads, with the whole file.
    ui.canvas.selected = None;
    let g = shown(&mut ui);
    let enter = titled(&g, "clip OnMouseEnter");
    assert!(
        enter
            .outs
            .iter()
            .any(|p| p.name == "+ Completed" && p.make.is_some())
    );
    // A sibling container in view offers its template's clips: a press
    // can start the overlay's clip from the button's own view.
    ui.select_named("AnnoButtonWT");
    ui.graph.feeds = true;
    let g = shown(&mut ui);
    assert!(
        titled(&g, "BtnOverlay_DS")
            .ins
            .iter()
            .any(|p| p.name == "+ clip ContentAplhaShow · Start"),
        "the overlay's clips beside the button"
    );
    ui.graph.feeds = false;
    assert!(ui.undo(false));
}

// Needs the game: a clip nothing starts yet has a + Start to drop an
// event on, and so does a clip of a container's template; the drop
// makes the place, one undo step, kept on save. A value is refused
// there (round 16: clip Nothing had no node, BtnOverlay_DS no
// "clip ContentAplhaShow · Start", a new button nothing). An event wires
// onto a clip's Start or Stop someone already sets (round 15: "clips
// cannot be wired yet", while Remove was offered on the same pin), as
// one undo step, through the container the file already has for that
// clip, so the binding is shaped as the game's own are, and it survives
// a save and reopen; and onto a nested template's Start.
#[test]
fn events_wire_onto_clips_start_and_stop() {
    let Some(mut ui) = open(BUTTON) else { return };
    let ft = ui.file_table(0).unwrap();
    let slots = &ft.table.slots;
    let (leave, enter) = (
        event_ending(&ft, "OnMouseLeave"),
        event_ending(&ft, "OnMouseEnter"),
    );
    let yes_no = (0..slots.len())
        .find(|&s| ft.kind(s) == Some(T_BOOL) && matches!(slots[s], Slot::Widget { .. }))
        .expect("a yes/no");
    let nothing = ft
        .asset
        .clips()
        .iter()
        .position(|c| c.name == "Nothing")
        .unwrap();
    let from = slots[leave].clone();
    drop(ft);
    let to = drop_on(&mut ui, "clip Nothing", "+ Start");
    let refused = ui.wire_verdicts(yes_no, None, &[to]).remove(0).unwrap_err();
    assert!(refused.contains("is an event, and"), "{refused}");
    assert!(refused.contains("when it turns true"), "{refused}");
    let ok = ui.wire_verdicts(leave, None, &[to]).remove(0).unwrap();
    assert!(ok.ends_with("OnMouseLeave starts clip Nothing"), "{ok}");
    let revision = ui.history.revision();
    let said = ui.wire_up(leave, None, to).unwrap();
    assert!(said.ends_with("→ clip Nothing · Start"), "{said}");
    assert_eq!(ui.history.revision(), revision + 1, "one undo step");
    let starts = |a: &Asset, target: &dyn Fn(&Slot) -> bool| {
        let t = bindings(a);
        t.bindings.iter().any(|b| {
            b.targets.iter().any(|&x| target(&t.slots[x]))
                && b.sources.first().map(|&x| &t.slots[x]) == Some(&from)
        })
    };
    let start = [(1, 0x8000_001f)];
    let own =
        |s: &Slot| matches!(s, Slot::Clip { clip, path } if *clip == nothing && path[..] == start);
    assert!(starts(&reopen(&ui.asset), &own), "saved and opened again");
    // Bound now: the pin reads Start, fed by a wire.
    let g = shown(&mut ui);
    let n = g
        .nodes
        .iter()
        .position(|n| n.title == "clip Nothing")
        .unwrap();
    let k = g.nodes[n]
        .ins
        .iter()
        .position(|p| p.name == "Start")
        .expect("bound");
    assert!(g.wires.iter().any(|w| w.to == (n, k) && w.event));
    assert!(!g.nodes[n].ins.iter().any(|p| p.name == "+ Start"));

    // The overlay's template clip, from the overlay's own node. The
    // hover clips this view shows are stopped by bindings it leaves
    // out: bound all the same, no + Stop.
    select(&mut ui, "BtnOverlay_DS");
    let to = drop_on(&mut ui, "BtnOverlay_DS", "+ clip ContentAplhaShow · Start");
    let g = shown(&mut ui);
    let leave_clip = titled(&g, "clip OnMouseLeave");
    assert!(
        !leave_clip.ins.iter().any(|p| p.name == "+ Stop"),
        "{:?}",
        leave_clip.ins.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
    let said = ui.wire_up(enter, None, to).unwrap();
    assert!(
        said.ends_with("→ BtnOverlay_DS › clip ContentAplhaShow · Start"),
        "{said}"
    );
    let t = bindings(&ui.asset);
    let b = t.bindings.last().unwrap();
    assert!(
        matches!(&t.slots[b.targets[0]], Slot::Nested { inner, path, .. }
        if inner.len() == 1 && path[..] == start)
    );
    assert!(ui.undo(false));
    assert!(ui.undo(false));
    assert!(!starts(&ui.asset, &own), "undone");

    // A clip's Stop some binding sets, and a button event (an Event by
    // its widget type) that does not set it yet.
    let ft = ui.file_table(0).unwrap();
    let (slots, set) = (&ft.table.slots, &ft.table.bindings);
    let stop = (0..slots.len())
        .find(|&s| {
            matches!(&slots[s], Slot::Clip { path, .. } if path.last().is_some_and(|&(_, id)| id == 0x8000_0020))
                && set.iter().any(|b| b.targets.contains(&s))
        })
        .expect("a clip's Stop that is set");
    let event = (0..slots.len())
        .find(|&s| {
            ft.kind(s) == Some(T_EVENT)
                && matches!(slots[s], Slot::Widget { .. })
                && !set
                    .iter()
                    .any(|b| b.sources.contains(&s) && b.targets.contains(&stop))
        })
        .expect("a button event");
    let wired = |a: &Asset, to: &Slot| {
        let t = bindings(a);
        t.bindings.iter().any(|b| {
            b.sources.first().map(|&s| &t.slots[s]) == Some(&slots[event])
                && b.targets.first().map(|&s| &t.slots[s]) == Some(to)
        })
    };
    let containers = |a: &Asset| {
        let t = bindings(a);
        (t.slots.len(), t.constants.len())
    };
    let (bytes, before) = (ui.asset.encode(), containers(&ui.asset));
    let said = ui.wire_up(event, None, Drop::Slot(stop)).unwrap();
    assert!(said.starts_with("Wired "), "{said}");
    assert!(wired(&ui.asset, &slots[stop]));
    let reopened = reopen(&ui.asset);
    assert!(wired(&reopened, &slots[stop]), "saved and opened again");
    assert_eq!(
        containers(&reopened),
        before,
        "the clip's own container, shared"
    );
    assert!(ui.undo(false));
    assert_eq!(ui.asset.encode(), bytes, "one undo step");
    // And onto a nested template's Start ("BtnOverlay_DS › · Start").
    let nested = (0..slots.len())
        .find(|&s| {
            let setters = || set.iter().filter(move |b| b.targets.contains(&s));
            matches!(slots[s], Slot::Nested { .. })
                && setters().count() > 0
                // Plain copies only: a converter's target is refused.
                && setters().all(|b| b.converter == crate::ui::bindings::ASSIGN)
                && !setters().any(|b| b.sources.contains(&event))
        })
        .expect("a nested target only copies set");
    ui.wire_up(event, None, Drop::Slot(nested)).unwrap();
    assert!(wired(&ui.asset, &slots[nested]));
}

// Needs the game: a vector and its axes are one place for the loop
// check, and sibling axes do not touch. In XIcon's template a Divide
// writes MOBO_01 and MOBO_01 sets XIcon's Size whole; wiring XIcon ·
// Size.x into that Divide closes a loop through the axis, which an
// exact-path check missed (round 5). In the quest tracker's objective
// template an Add sets Pattern · Size.y; Pattern · Size.x (fed on its
// own) into that Add is no loop (round 6 refused it), while Pattern ·
// Size.y into it is one.
#[test]
fn a_loop_through_an_axis_is_refused_and_siblings_are_none() {
    let Some(install) = install() else { return };
    let xicon = path("027cffcb-adec-48e2-a715-d815a12e32ea");
    let mut ui = UiEditor::load(install.clone(), &xicon).unwrap();
    let ft = ui.file_table(0).unwrap();
    let labelled = |name: &str| {
        (0..ft.table.slots.len())
            .find(|&s| ft.label(s) == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    let size_x = labelled("XIcon · Size.x");
    let mobo = (0..ft.table.slots.len())
        .find(|&s| matches!(ft.table.slots[s], Slot::Template(_, 1)) && ft.label(s) == "MOBO_01")
        .expect("MOBO_01");
    let divide = ft
        .table
        .bindings
        .iter()
        .position(|b| b.targets.contains(&mobo) && b.sources.len() == 2)
        .expect("the Divide writing MOBO_01");
    let drop = Drop::Arg {
        binding: divide,
        k: 0,
    };
    let v = ui.wire_verdicts(size_x, None, &[drop]).remove(0);
    assert!(
        v.as_ref().is_err_and(|e| e.contains("would read itself")),
        "{v:?}"
    );
    let bytes = ui.asset.encode();
    assert!(ui.wire_up(size_x, None, drop).is_err());
    assert_eq!(ui.asset.encode(), bytes);

    let mut ui = UiEditor::load(install, &path(BUTTON)).unwrap();
    let ft = ui.file_table(0).unwrap();
    let labelled = |name: &str| {
        (0..ft.table.slots.len())
            .find(|&s| ft.label(s) == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    let (x, y) = (labelled("Pattern · Size.x"), labelled("Pattern · Size.y"));
    let add = ft
        .table
        .bindings
        .iter()
        .position(|b| b.targets.contains(&y) && b.sources.len() == 2)
        .expect("the Add setting Pattern · Size.y");
    let drop = Drop::Arg { binding: add, k: 1 };
    let v = ui.wire_verdicts(x, None, &[drop]).remove(0);
    assert!(v.is_ok(), "{v:?}");
    let v = ui.wire_verdicts(y, None, &[drop]).remove(0);
    assert!(
        v.as_ref().is_err_and(|e| e.contains("would read itself")),
        "{v:?}"
    );
}

// Needs the game: on whole screens no node wires into itself (an
// object read and written is two nodes), and a root nothing binds gets
// no empty focus node. A fresh layout draws every wire left to right
// when there is no loop, also where paths meet again, in what every own
// widget feeds (round 5 suspected a node left of its own source after a
// rewire and Tidy).
#[test]
fn whole_screens_have_no_wire_into_itself_or_backward() {
    let Some(install) = install() else { return };
    for name in ["constructionmenuscene", "minimapscene", "questtrackerscene"] {
        let mut ui = UiEditor::load(install.clone(), &path(name)).unwrap();
        let g = file_graph(&mut ui, 0);
        for w in &g.wires {
            assert_ne!(
                w.from.0, w.to.0,
                "{name}: {} wires into itself",
                g.nodes[w.from.0].title
            );
        }
        for n in g.nodes.iter().filter(|n| n.kind == Kind::Data) {
            assert!(n.ins.is_empty() || n.outs.is_empty(), "{name}: {}", n.title);
        }
        let ft = ui.file_table(0).unwrap();
        let root = Graph::build(&ft, Some(0), &[], false, false);
        assert!(
            !root
                .nodes
                .iter()
                .any(|n| focus(n) && n.ins.is_empty() && n.outs.is_empty()),
            "{name}: an empty focus node"
        );
        for n in 1..ui.nodes.len().min(400) {
            if !ui.nodes[n].foreign {
                let back = backward(&widget_graph(&mut ui, n, true));
                assert!(back.is_empty(), "{name}: {back:?}");
            }
        }
    }
}

// Needs the game: a press and release in place on an output pin is a
// click, not a failed wire: nothing happens and nothing is said (round
// 5 put "Not wired" in the status bar for every click). A release away
// from any pin still says why it was not wired.
#[test]
fn a_click_on_a_pin_is_no_failed_wire() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let wire = || super::view::Wiring {
        source: super::wiring::Source::Slot(0),
        kind: None,
        from: [0., 0.],
        at: [100., 100.],
        start: [100., 100.],
        verdicts: Vec::new(),
    };
    let bytes = ui.asset.encode();
    ui.graph.wiring = Some(wire());
    assert!(ui.drop_wire([102., 101.]).is_none(), "a click");
    assert!(ui.graph.wiring.is_none());
    ui.graph.wiring = Some(wire());
    let r = ui.drop_wire([400., 300.]).expect("a drag");
    assert!(r.is_err_and(|e| e.to_string().contains("drop the wire")));
    assert_eq!(ui.asset.encode(), bytes);
}

// Needs the game: a right-click on the if / else that stands for two
// gates asks first, naming what it sets, and removes both as one undo
// step. Removing the Multiply that only sets a template property names
// the Icon's Size, which loses its value through it.
#[test]
fn removing_a_logic_node_asks_and_is_one_step() {
    let Some(mut ui) = open(ICON_ROW) else { return };
    let icon = node(&ui, "Icon");
    ui.canvas.selected = Some(icon);
    let g = widget_graph(&mut ui, icon, false);
    let at = point(px(10.), px(10.));
    let multiply = g
        .nodes
        .iter()
        .find_map(|n| match n.kind {
            Kind::Logic { binding, .. } if n.title.contains("Multiply") => Some(binding),
            _ => None,
        })
        .expect("the Multiply node");
    ui.ask_unbind(at, vec![multiply]);
    let menu = ui.graph.menu.clone().unwrap();
    assert!(
        menu.reach
            .iter()
            .any(|p| p.starts_with("Icon") && p.contains("Size")),
        "{:?} / {:?}",
        menu.sets,
        menu.reach
    );
    let (binding, also) = g
        .nodes
        .iter()
        .find_map(|n| match n.kind {
            Kind::Logic {
                binding,
                also: Some(also),
                ..
            } => Some((binding, also)),
            _ => None,
        })
        .expect("a gate pair");
    let count = |ui: &UiEditor| bindings(&ui.asset).bindings.len();
    let before = count(&ui);
    ui.ask_unbind(at, vec![binding, also]);
    let menu = ui.graph.menu.clone().expect("asks first");
    assert_eq!(count(&ui), before, "nothing removed yet");
    assert!(
        menu.sets.iter().any(|s| s.contains("_hub01")),
        "{:?}",
        menu.sets
    );
    // A layout that only re-evaluates (a playback frame, a preview
    // value) keeps it: binding indices hold until an edit (round 4: no
    // menu could open while a clip played).
    ui.relayout();
    assert!(ui.graph.menu.is_some(), "a playback frame closed the menu");
    // An edit, or its undo, closes it: its indices were the old file's.
    let other = (0..before).find(|i| ![binding, also].contains(i)).unwrap();
    ui.ask_unbind(at, vec![binding, also]);
    ui.unbind(&[other]).unwrap();
    assert!(ui.graph.menu.is_none(), "the menu outlived an edit");
    ui.ask_unbind(at, vec![binding, also]);
    assert!(ui.undo(false));
    assert!(ui.graph.menu.is_none(), "the menu outlived an undo");
    assert_eq!(count(&ui), before);
    // A removal of what is not there any more changes nothing.
    assert!(ui.unbind(&[before]).is_err());
    assert_eq!(count(&ui), before);
    let said = ui.unbind(&menu.bindings).unwrap();
    assert!(
        said.starts_with("Removed 2 bindings setting") && said.contains("_hub01"),
        "{said}"
    );
    assert_eq!(count(&ui), before - 2);
    assert!(ui.undo(false));
    assert_eq!(count(&ui), before, "one undo step");
}

mod layout_probe {
    use crate::ui::UiEditor;
    use crate::ui::graph::{Graph, Kind};
    use crate::ui::test_support::{ICON_ROW, install, path};
    use std::fmt::Write;

    // Scratch: the largest All in file graphs of a few screens as SVG, to
    // look at the layout without the window. LAYOUT_DIR names the folder.
    #[test]
    #[ignore]
    fn dump() {
        let install = install().expect("ANNO117_DIR");
        let out = std::env::var("LAYOUT_DIR").unwrap();
        for screen in [
            ICON_ROW,
            "constructionmenuscene",
            "minimapscene",
            "questtrackerscene",
        ] {
            let mut ui = UiEditor::load(install.clone(), &path(screen)).unwrap();
            let mut files = std::collections::HashMap::new();
            for n in 0..ui.nodes.len() {
                files.entry(ui.node_file(n)).or_insert(n);
            }
            let mut graphs: Vec<Graph> = files
                .values()
                .filter_map(|&n| ui.file_table(n))
                .map(|ft| Graph::build(&ft, None, &[], false, false))
                .collect();
            graphs.sort_by_key(|g| std::cmp::Reverse(g.wires.len()));
            let g = &graphs[0];
            let mut svg = String::new();
            let _ = write!(
                svg,
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" style="background:#16191e" font-family="sans-serif" font-size="11">"##,
                g.size[0] + 40.,
                g.size[1] + 40.
            );
            for w in &g.wires {
                let (a, b) = (g.out_at(w.from.0, w.from.1), g.in_at(w.to.0, w.to.1));
                let d = ((b[0] - a[0]).abs() / 2.).max(30.);
                let _ = write!(
                    svg,
                    r##"<path d="M{} {} C{} {} {} {} {} {}" stroke="#7aa2d8" fill="none" opacity="0.7"/>"##,
                    a[0] + 20.,
                    a[1] + 20.,
                    a[0] + 20. + d,
                    a[1] + 20.,
                    b[0] + 20. - d,
                    b[1] + 20.,
                    b[0] + 20.,
                    b[1] + 20.
                );
            }
            for n in &g.nodes {
                let fill = match n.kind {
                    Kind::Widget { .. } => "#22344d",
                    Kind::Inputs => "#3a2a4d",
                    Kind::Data => "#23402e",
                    Kind::Value => "#3d2a47",
                    _ => "#2a2f37",
                };
                let r = n.rect;
                let _ = write!(
                    svg,
                    r##"<rect x="{}" y="{}" width="{}" height="{}" fill="{fill}" stroke="#555"/><text x="{}" y="{}" fill="#ddd">{}</text>"##,
                    r[0] + 20.,
                    r[1] + 20.,
                    r[2],
                    r[3],
                    r[0] + 26.,
                    r[1] + 35.,
                    n.title.replace('&', "&amp;").replace('<', "&lt;")
                );
            }
            svg.push_str("</svg>");
            std::fs::write(format!("{out}/{screen}.svg"), svg).unwrap();
            println!(
                "{screen}: {} nodes, {} wires, {:?}",
                g.nodes.len(),
                g.wires.len(),
                g.size
            );
        }
    }
}
