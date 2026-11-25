// Behavioural tests of the layout evaluator and the bindings it evaluates:
// hand-built trees, and the installed game's screens (each skips without
// ANNO117_DIR).
use super::*;
use crate::ui::evaluate::Builtin;
use crate::ui::test_support::screens;
use anno_formats::phx::animation::{NewKey, TrackKind};
use anno_formats::phx::binding::{NewBinding, NewSlot, Slot};
use anno_formats::vfs::Install;

/// The installed Anno 117, or None (the test skips) without ANNO117_DIR.
fn install() -> Option<Install> {
    let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
        eprintln!("skipping: ANNO117_DIR unset");
        return None;
    };
    let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
    Some(Install::open(dir, families).unwrap())
}

/// Generated UI file `file` (a scene name or a guid) as the install ships it.
fn open(install: &Install, file: &str) -> Asset {
    Asset::parse(
        install
            .read(&format!("data/ui/studio/generated/{file}"))
            .unwrap(),
    )
    .unwrap()
}

/// What layout trees load nested templates with: the install's file of
/// that guid, None where none ships.
fn loader(install: &Install) -> impl FnMut(Guid) -> Option<Arc<Asset>> + '_ {
    |g| {
        let b = install
            .read(&format!("data/ui/studio/generated/{g}"))
            .ok()?;
        Asset::parse(b).ok().map(Arc::new)
    }
}

/// A whole layout from scratch: every node descended into, nothing to take
/// over.
fn evaluate(nodes: &[LayoutNode], root: usize, root_size: [f32; 2]) -> Vec<Placed> {
    evaluate_since(nodes, root, root_size, &|_| true, &[], None, &|_| false).placed
}

/// The tree node of widget `w` of the file itself (not of a template).
fn own_node(tree: &Tree, w: usize) -> usize {
    tree.source
        .iter()
        .position(|&(f, k)| f == 0 && k == w)
        .unwrap()
}

/// Built-in property `id` (Visible, Alpha) of `widget`.
fn builtin_prop(widget: usize, id: Builtin) -> NewSlot {
    NewSlot::Widget {
        widget,
        path: vec![(1, id.uid())],
    }
}

// A scene's cross-file layout tree, evaluated at 1920×1080, with the
// widget names per node.
fn install_scene(install: &Install, scene: &str) -> (Tree, Vec<Placed>, Vec<String>) {
    let root = open(install, scene);
    let t = Tree::build_with(&root, &mut loader(install), true);
    let out = evaluate(&t.nodes, 0, [1920.0, 1080.0]);
    let names = t
        .source
        .iter()
        .map(|&(file, k)| {
            let a = if file == 0 {
                &root
            } else {
                &*t.templates[file - 1]
            };
            a.widgets()[k].name.clone().unwrap_or_default()
        })
        .collect();
    (t, out, names)
}

fn find(names: &[String], name: &str) -> usize {
    names
        .iter()
        .position(|n| n == name)
        .unwrap_or_else(|| panic!("no widget {name}"))
}

fn node(intrinsic: [f32; 2], pos: [f32; 2]) -> LayoutNode {
    LayoutNode {
        geometry: Geometry {
            position: [pos[0], pos[1], 0.0],
            ..Geometry::default()
        },
        intrinsic: Some(intrinsic),
        ..Default::default()
    }
}

/// A plain group of `children`: no size, no components.
fn group(children: Vec<usize>) -> LayoutNode {
    LayoutNode {
        children,
        ..Default::default()
    }
}

// A vertical stack: items in reverse array order, spacing between visible
// items, cross-axis Middle alignment. Items start at the group's origin;
// the margins grow its box outward (origin −left/−top).
#[test]
fn layout_stack() {
    use {Align, LayoutComponent};
    let mut stack = group(vec![1, 2, 3]);
    stack.components = vec![LayoutComponent::Stack {
        horizontal: false,
        align: Align::Middle,
        invert: false,
        spacing: 10.0,
        margin: [5.0, 5.0, 5.0, 5.0],
    }];
    stack.geometry.position = [100.0, 50.0, 0.0];
    let mut hidden = node([50.0, 50.0], [0.0, 0.0]);
    hidden.geometry.visible = false;
    let nodes = vec![
        group(vec![4]),
        node([40.0, 20.0], [300.0, 300.0]),
        hidden,
        node([80.0, 30.0], [-7.0, 9.0]),
        stack,
    ];
    let out = evaluate(&nodes, 0, [1920.0, 1080.0]);
    // Last array item first: node 3 (80×30) at top, then node 1 (40×20).
    assert_eq!(out[3].rect.min, [100.0, 50.0]);
    assert_eq!(out[1].rect.min, [120.0, 90.0]);
    assert_eq!(out[4].local.size(), [90.0, 70.0]);
    assert_eq!(out[4].rect.min, [95.0, 45.0]);
}

#[test]
fn layout_pivot_anchor_background() {
    use LayoutComponent;
    let mut child = node([100.0, 40.0], [-10.0, -20.0]);
    child.geometry.anchor = 8; // bottom-right
    child.components = vec![LayoutComponent::ProportionalPivot([1.0, 1.0, 0.0])];
    let mut boxed = group(vec![3, 4]);
    boxed.components = vec![LayoutComponent::Background {
        margin: [4.0, 4.0, 4.0, 4.0],
    }];
    boxed.geometry.position = [10.0, 10.0, 0.0];
    let nodes = vec![
        group(vec![1, 2]),
        child,
        boxed,
        node([8.0, 8.0], [0.0, 0.0]),
        node([30.0, 10.0], [5.0, 5.0]),
    ];
    let out = evaluate(&nodes, 0, [1920.0, 1080.0]);
    // Bottom-right anchor, pivot at the child's bottom-right corner.
    assert_eq!(out[1].rect.max, [1910.0, 1060.0]);
    assert_eq!(out[1].rect.min, [1810.0, 1020.0]);
    // Background spans the other child grown by the margin.
    assert_eq!(out[3].rect.min, [11.0, 11.0]);
    assert_eq!(out[3].rect.max, [49.0, 29.0]);
}

// Containers anchor their children in the template's ReferenceSize even
// when their bounds are the children's union; plain groups have no Size,
// so an anchor inside one adds nothing.
#[test]
fn layout_anchor_in_parent_size() {
    let mut corner = node([10.0, 10.0], [-10.0, -10.0]);
    corner.geometry.anchor = 8;
    let mut in_group = node([10.0, 10.0], [5.0, 5.0]);
    in_group.geometry.anchor = 8;
    // A container without the fixed-size flag: no intrinsic box.
    let mut container = group(vec![2]);
    container.size = Some([400.0, 300.0]);
    container.geometry.position = [100.0, 100.0, 0.0];
    let nodes = vec![
        group(vec![1, 3]),
        container,
        corner,
        group(vec![4]),
        in_group,
    ];
    let out = evaluate(&nodes, 0, [1920.0, 1080.0]);
    assert_eq!(out[2].rect.min, [490.0, 390.0]);
    assert_eq!(out[1].rect.max, [500.0, 400.0]);
    assert_eq!(out[4].rect.min, [5.0, 5.0]);
}

// Metrics cancel ancestor scale: anchor metric 1 divides Position by the
// ancestors' scale, resize metric 2 the parent's, AspectRatio 1 makes the
// widget uniform at the smaller axis.
#[test]
fn layout_metrics_cancel_ancestor_scale() {
    let mut scaled = group(vec![2, 3, 4]);
    scaled.geometry.scale = [2.0, 4.0, 1.0];
    let mut pinned = node([10.0, 10.0], [100.0, 100.0]);
    pinned.geometry.anchor_metrics = [1, 1];
    pinned.geometry.resize_metrics = [2, 2];
    let mut uniform = node([10.0, 10.0], [0.0, 0.0]);
    uniform.geometry.aspect_ratio = 1;
    let plain = node([10.0, 10.0], [10.0, 10.0]);
    let nodes = vec![group(vec![1]), scaled, pinned, uniform, plain];
    let out = evaluate(&nodes, 0, [1920.0, 1080.0]);
    assert_eq!(out[2].rect.min, [100.0, 100.0]);
    assert_eq!(out[2].rect.size(), [10.0, 10.0]);
    assert_eq!(out[3].rect.size(), [20.0, 20.0]);
    assert_eq!(out[4].rect.min, [20.0, 40.0]);
    assert_eq!(out[4].rect.size(), [20.0, 40.0]);
}

// The main title menu: bottom-left once ConsiderSize-off pattern layers
// stay out of the bounds that ProportionalPivotPoint (0,1) measures. Its
// full-screen pieces: the scene container takes its template's 1920×1080,
// and the title video (VideoAsset 160×160 × Scale 12 × 6.75, centre anchor,
// proportional pivot) covers the screen.
#[test]
fn install_layout_main_title() {
    let Some(install) = install() else { return };
    let (_, out, names) = install_scene(&install, "maintitlescene");
    let r = out[find(&names, "MainTitle_BtnContainer")].rect;
    assert!((r.max[1] - 1080.0).abs() < 1.0, "{r:?}");
    assert!(r.min[0] < 0.0 && r.max[0] < 960.0, "{r:?}");
    let screen = Rect {
        min: [0.0, 0.0],
        max: [1920.0, 1080.0],
    };
    assert_eq!(out[find(&names, "MainTitleSceneContainer")].rect, screen);
    let video = out[find(&names, "title")].rect;
    assert!(
        (video.min[0]).abs() < 1.0 && (video.max[0] - 1920.0).abs() < 1.0,
        "{video:?}"
    );
    assert!(
        (video.min[1]).abs() < 1.0 && (video.max[1] - 1080.0).abs() < 1.0,
        "{video:?}"
    );
}

// HUD scenes against a 1920×1080 in-game screenshot: minimap ring bottom-left,
// governors' top bar centred on the top edge, quest tracker on the left
// below it, the resource bar's right element pinned to the top-right
// corner (its template is not shipped, so only the anchor point is known).
#[test]
fn install_layout_hud() {
    let Some(install) = install() else { return };
    let (_, out, names) = install_scene(&install, "minimapscene");
    let map = out[find(&names, "Marble")].rect;
    assert!(map.min[0] > 0.0 && map.max[0] < 400.0, "{map:?}");
    assert!(map.min[1] > 700.0 && map.max[1] < 1080.0, "{map:?}");

    let (_, out, names) = install_scene(&install, "hudgovernorsbarscene");
    let bar = out[find(&names, "HUDGovernorsBarSceneContainer")].rect;
    // Hangs from the top edge (the band starts at y 0).
    assert!(bar.min[1] <= 0.5 && bar.max[1] < 400.0, "{bar:?}");
    let mid = (bar.min[0] + bar.max[0]) * 0.5;
    assert!((mid - 960.0).abs() < 100.0, "{bar:?}");

    let (_, out, names) = install_scene(&install, "questtrackerscene");
    let quest = out[find(&names, "TrackedQuest_Entry")].rect;
    assert!(quest.min[0] >= 0.0 && quest.max[0] < 700.0, "{quest:?}");
    assert!(quest.min[1] > 150.0 && quest.max[1] < 600.0, "{quest:?}");

    let (_, out, names) = install_scene(&install, "resourcebarscene");
    let right = out[find(&names, "GlobalRightBarElement")].transform;
    assert_eq!([right[4], right[5]], [1919.0, -2.0]);
}

// The build panel places its header glow through two Positioning
// components: the header's turns its centre (a binding feeds it half the
// header's size) global, the glow group's turns that local again, and a
// binding sets the glow there. Native code does the conversions;
// unconverted, the glow sat at its group's origin, a dark disc beside the
// panel.
#[test]
fn install_positioning_converts_through_the_layout() {
    let Some(install) = install() else { return };
    let (t, out, names) = install_scene(&install, "dc58f534-a08e-409b-bf0d-63ece3ac0194");
    let group = find(&names, "G_gradiant_Image_Header");
    let glow = out[t.nodes[group].children[0]].rect;
    let header = out[find(&names, "G_Header")].rect;
    let mid = |r: Rect| [(r.min[0] + r.max[0]) * 0.5, (r.min[1] + r.max[1]) * 0.5];
    let (a, b) = (mid(glow), mid(header));
    assert!(
        // The header's box has its stack margin around the size it reads.
        (a[0] - b[0]).abs() < 8. && (a[1] - b[1]).abs() < 8.,
        "glow {glow:?}, header {header:?}"
    );
}

// Needs the game: the quest tracker's ScrollbarAppear fades the scroll bar
// in: played from start to end, some widget's alpha rises. A new clip with
// a new AlphaTint track plays too: at its end the widget is transparent.
#[test]
fn quest_tracker_clips_play() {
    let Some(install) = install() else { return };
    let asset = open(&install, crate::ui::test_support::QUEST_TRACKER);
    let play = |a: &Asset, clip: usize, time: f32, bindings: bool| {
        let opts = Options {
            bindings,
            play: Some(Play {
                file: None,
                clip,
                time,
            }),
            ..Default::default()
        };
        Tree::build_opts(a, &mut loader(&install), &mut |_, _| None, &opts)
    };
    let alphas = |t: Tree| {
        t.nodes
            .iter()
            .map(|n| n.geometry.alpha)
            .collect::<Vec<f32>>()
    };
    let scroll = asset
        .clips()
        .iter()
        .position(|c| c.name == "ScrollbarAppear")
        .unwrap();
    let start = alphas(play(&asset, scroll, 0.0, false));
    let end = alphas(play(&asset, scroll, 1.0, false));
    assert!(
        start
            .iter()
            .zip(&end)
            .any(|(s, e)| *s < 0.01 && (*e - 1.0).abs() < 0.01),
        "no widget faded in"
    );

    let widgets = asset.widgets();
    let n = asset.clips().len();
    let a = asset.add_clip("EditorFade", 60, 30).unwrap();
    let w = (1..widgets.len())
        .find(|&w| widgets[w].name.is_some())
        .unwrap();
    let key = |time: f32, v: f32| NewKey {
        time,
        ease: 1,
        value: DefaultValue::F32(v.to_bits()),
    };
    let alpha = builtin_prop(w, Builtin::AlphaTint);
    let a = a
        .add_track(
            &a.widgets(),
            n,
            &alpha,
            TrackKind::Float,
            &[key(0., 1.), key(0.5, 0.)],
        )
        .unwrap();
    let tree = play(&a, n, 0.5, true);
    let faded = tree.nodes[own_node(&tree, w)].geometry.alpha;
    assert!(faded < 0.01, "{faded}");
}

// Needs the game: a binding added to a shipped template takes effect in a
// layout, and removing it gives the template's behaviour back.
#[test]
fn installed_bindings_lay_out_and_come_off() {
    let Some(install) = install() else { return };
    // The HUD icon-text: bindings over constants and template properties.
    let asset = open(&install, "ae3fe300-1235-46e6-a9c0-c9e68720b44a");
    let widgets = asset.widgets();
    let before = asset.bindings(&widgets);
    // A drawn widget with no Visible binding of its own.
    let w = (1..widgets.len())
        .find(|&w| {
            !before.slots.contains(&Slot::Widget {
                widget: Some(w),
                path: vec![(1, Builtin::Visible.uid())],
            })
        })
        .unwrap();
    let visible = |a: &Asset| {
        let tree = Tree::build_with(a, &mut loader(&install), true);
        tree.nodes[own_node(&tree, w)].geometry.visible
    };
    assert!(visible(&asset));
    let off = NewBinding {
        converter: name_crc(b"AssignConverterInstance"),
        sources: vec![NewSlot::Constant {
            kind: 0,
            value: DefaultValue::Bool(0),
        }],
        targets: vec![builtin_prop(w, Builtin::Visible)],
    };
    let edited = asset.add_binding(&widgets, &off).unwrap();
    assert!(!visible(&edited), "the new binding hides the widget");
    let ew = edited.widgets();
    let back = edited
        .remove_binding(&ew, edited.bindings(&ew).bindings.len() - 1)
        .unwrap();
    assert!(visible(&back));
}

// What the warm/fresh test's sampler answers for every IsVisible flag.
static FLAGS_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn flag_sampler(name: &str, kind: u32, _: &DefaultValue) -> Option<DefaultValue> {
    (kind == 0 && name.ends_with("IsVisible")).then(|| {
        DefaultValue::Bool(u8::from(
            FLAGS_ON.load(std::sync::atomic::Ordering::Relaxed),
        ))
    })
}

// Needs the game: a warm layout after game data changes (a new sampling
// generation) draws what a fresh one draws, through on, off and on again
// (round 9: the minimap kept its tutorial content after Reset all, from
// the previous layout's evaluations of nested instances).
#[test]
fn a_warm_layout_follows_changed_game_data() {
    use std::sync::atomic::Ordering;
    let Some(install) = install() else { return };
    let root = open(&install, "minimapscene");
    let mut load = loader(&install);
    let mut cache = Cache::default();
    let shown = |t: &Tree| {
        t.nodes
            .iter()
            .map(|n| n.geometry.visible)
            .collect::<Vec<_>>()
    };
    let mut seen = std::collections::BTreeSet::new();
    for (generation, on) in [(1, false), (2, true), (3, false), (4, true), (5, false)] {
        FLAGS_ON.store(on, Ordering::Relaxed);
        let opts = Options {
            bindings: true,
            sample: Some(flag_sampler),
            list_items: crate::ui::preview::Sample::LIST_ITEMS,
            fields: Some(screens().data_fields),
            sample_generation: generation,
            ..Default::default()
        };
        let warm = Tree::build_cached(&root, &mut load, &mut |_, _| None, &opts, &mut cache);
        let fresh = Tree::build_cached(
            &root,
            &mut load,
            &mut |_, _| None,
            &opts,
            &mut Cache::default(),
        );
        assert_eq!(
            shown(&warm),
            shown(&fresh),
            "generation {generation}, flags {on}"
        );
        seen.insert(shown(&fresh));
        cache.keep(warm);
    }
    assert!(seen.len() > 1, "the flags change what the minimap shows");
}

#[test]
fn opening_clips_by_name() {
    assert!(opens("Anim_In"));
    assert!(opens("FadeIn"));
    // Swaps the governors bar's second row to another party's island.
    assert!(!opens("Data_Change_in"));
    assert!(!opens("ScrollbarAppear"));
}
