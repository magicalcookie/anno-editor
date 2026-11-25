// The Model workspace's viewport in GPUI's test platform, on the 117 install
// (see `tests::open`). Picking meshes needs the GPU, which the test platform
// lacks; light and effect markers, joints and the gizmo are hit on the CPU.

use super::tests::{DOG, FIREFLIES, INSTITUTION, WORKER, Window, open};
use super::view::{Action, Menu, Overlays};
use super::{ModelEditor, playback::Source};
use anno_formats::xml::Id;
use gpui::{Modifiers, MouseButton, MouseMoveEvent, Pixels, Point, point, px};
use gpui_editor_kit::math::{self, Vec3};

/// The pane drawn, so the canvas knows its bounds.
fn draw(app: &gpui::TestApp, w: &mut Window) {
    app.run_until_parked();
    w.draw();
    app.run_until_parked();
}

/// A drag with the left button: press, a few moves held, release.
fn drag(w: &mut Window, from: Point<Pixels>, to: Point<Pixels>, button: MouseButton) {
    w.simulate_mouse_down(from, button);
    for k in 1..=4 {
        let t = k as f32 / 4.;
        w.simulate_event(MouseMoveEvent {
            position: from + (to - from) * t,
            modifiers: Modifiers::default(),
            pressed_button: Some(button),
        });
    }
    w.simulate_mouse_up(to, button);
}

/// The box around an entry's pieces as the cfg places them.
fn placed_box(m: &ModelEditor, entry: Id) -> [Vec3; 2] {
    math::bounds(
        (m.pieces.iter().filter(|p| p.element == Some(entry)))
            .flat_map(|p| math::corners(p.mesh.bounds()).map(|c| math::transform(&p.local, c))),
    )
}

/// How many pixels across a box shows, at its widest.
fn on_screen(m: &ModelEditor, b: [Vec3; 2]) -> f32 {
    let p = math::corners(b).map(|c| m.view.screen(c));
    let w = p.iter().map(|p| f32::from(p.x)).fold(f32::MIN, f32::max)
        - p.iter().map(|p| f32::from(p.x)).fold(f32::MAX, f32::min);
    let h = p.iter().map(|p| f32::from(p.y)).fold(f32::MIN, f32::max)
        - p.iter().map(|p| f32::from(p.y)).fold(f32::MAX, f32::min);
    w.max(h)
}

fn middle([lo, hi]: [Vec3; 2]) -> Vec3 {
    std::array::from_fn(|i| (lo[i] + hi[i]) / 2.)
}

// F brings a small prop to the middle of the pane, large; Ctrl Home goes
// back to the opening view of the whole model.
#[test]
fn f_frames_the_selection_and_home_resets() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    draw(&app, &mut w);
    let (prop, before) = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let prop = m.model.props[0].element;
        m.select(Some(prop));
        cx.notify();
        (prop, on_screen(m, placed_box(m, prop)))
    });
    w.simulate_keystrokes("f");
    draw(&app, &mut w);
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let b = m.view.painted();
        let side = f32::from(b.size.width.min(b.size.height));
        let at = m.view.screen(middle(placed_box(m, prop)));
        let off = f32::from((at - b.center()).x).hypot(f32::from((at - b.center()).y));
        assert!(off < side * 0.05, "{off} px off the middle of {b:?}");
        let after = on_screen(m, placed_box(m, prop));
        assert!(
            after > before * 2. && after > side * 0.25,
            "{before} → {after} px"
        );
    });
    // Ctrl Home: a plain Home goes to the hierarchy first, which takes it
    // to its first row.
    w.simulate_keystrokes("ctrl-home");
    w.read(|ed, _| {
        let c = ed.model.as_ref().unwrap().view.camera;
        assert_eq!((c.center, c.zoom, c.pan), (None, 1., [0.; 2]));
    });
}

// The overlay keys flip what is drawn, the number keys turn to the
// model's axis views, 5 swaps the lens; with markers off a click on an
// effect's marker no longer takes it.
#[test]
fn keys_flip_overlays_and_turn_the_view() {
    let Some((app, mut w, _config)) = open(FIREFLIES) else {
        return;
    };
    draw(&app, &mut w);
    let shown = |w: &Window| w.read(|ed, _| ed.model.as_ref().unwrap().view.shown);
    for (key, o) in [
        ("g", Overlays::GRID),
        ("b", Overlays::SKELETON),
        ("j", Overlays::JOINTS),
        ("c", Overlays::CLOTH),
    ] {
        assert!(shown(&w).has(o));
        w.simulate_keystrokes(key);
        assert!(!shown(&w).has(o), "{key} left it on");
        w.simulate_keystrokes(key);
        assert!(shown(&w).has(o), "{key} twice left it off");
    }
    let effect = w.read(|ed, _| ed.model.as_ref().unwrap().model.particles[0].clone());
    let at = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        m.view.screen(math::translation(&effect.transform.matrix()))
    });
    w.simulate_keystrokes("l");
    w.simulate_click(at, MouseButton::Left);
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().selected, None));
    w.simulate_keystrokes("l");
    w.simulate_click(at, MouseButton::Left);
    w.read(|ed, _| {
        assert_eq!(ed.model.as_ref().unwrap().selected, Some(effect.element));
    });
    let camera = |w: &Window| w.read(|ed, _| ed.model.as_ref().unwrap().view.camera);
    w.simulate_keystrokes("7");
    assert!(camera(&w).tilt < 0.01, "top: {:?}", camera(&w));
    w.simulate_keystrokes("1");
    let front = camera(&w);
    assert!(
        front.tilt > 1.5 && front.yaw.abs() < 1e-4,
        "front: {front:?}"
    );
    w.simulate_keystrokes("3");
    let right = camera(&w);
    assert!((right.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
    w.simulate_keystrokes("9");
    assert!((camera(&w).yaw - right.yaw - std::f32::consts::PI).abs() < 1e-4);
    let lens = camera(&w).fov;
    w.simulate_keystrokes("5");
    assert_eq!(camera(&w).fov > 0., lens == 0.);
}

// A right click on an effect's marker selects it and opens its menu; Esc
// closes it; Duplicate there adds a copy and selects it. A right drag
// turns the view and opens nothing.
#[test]
fn the_context_menu_duplicates_what_was_right_clicked() {
    let Some((app, mut w, _config)) = open(FIREFLIES) else {
        return;
    };
    draw(&app, &mut w);
    let (effect, count, at) = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let p = &m.model.particles[0];
        let at = m.view.screen(math::translation(&p.transform.matrix()));
        (p.element, m.model.particles.len(), at)
    });
    let yaw = w.read(|ed, _| ed.model.as_ref().unwrap().view.camera.yaw);
    drag(&mut w, at, at + point(px(60.), px(0.)), MouseButton::Right);
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.view.menu, None, "a drag opened the menu");
        assert_ne!(m.view.camera.yaw, yaw, "the drag did not turn the view");
    });
    w.simulate_keystrokes("ctrl-home");
    draw(&app, &mut w);
    w.simulate_click(at, MouseButton::Right);
    let menu = w.read(|ed, _| ed.model.as_ref().unwrap().view.menu);
    assert!(
        matches!(menu, Some(Menu::Entry { entry, .. }) if entry == effect),
        "{menu:?}"
    );
    w.simulate_keystrokes("escape");
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!((m.view.menu, m.selected), (None, Some(effect)));
    });
    w.simulate_click(at, MouseButton::Right);
    w.update(|ed, _, cx| ed.model_view_act(Action::Duplicate, cx));
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.model.particles.len(), count + 1);
        let copy = m.selected.expect("the copy is selected");
        assert_ne!(copy, effect);
        assert_eq!(m.list_of(copy), Some("Particles"));
        assert_eq!(m.view.menu, None);
    });
}

// The move gizmo drags an effect along X in steps once Snap is on; one
// undo takes the drag back.
#[test]
fn the_gizmo_moves_an_effect_in_snapped_steps() {
    let Some((app, mut w, _config)) = open(FIREFLIES) else {
        return;
    };
    let (effect, before) = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let p = m.model.particles[0].element;
        m.select(Some(p));
        m.view.snap = true;
        m.view.steps[0] = 0.25;
        cx.notify();
        (p, m.placement(p).unwrap())
    });
    draw(&app, &mut w);
    let (from, to) = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let (_, at, _) = m.handle_at().expect("the effect has a gizmo");
        let o = math::translation(&at);
        let origin = m.view.screen(o);
        let tip = m.view.screen(math::add(o, [0.01, 0., 0.]));
        let d = tip - origin;
        let len = f32::from(d.x).hypot(f32::from(d.y));
        let along = |px_: f32| origin + d * (px_ / len);
        (along(50.), along(140.))
    });
    drag(&mut w, from, to, MouseButton::Left);
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let after = m.placement(effect).unwrap();
        let dx = after.position[0] - before.position[0];
        assert!(dx > 0.2, "moved {dx}");
        assert!(
            ((dx / 0.25).round() * 0.25 - dx).abs() < 1e-3,
            "{dx} is off the step"
        );
        for a in [1, 2] {
            assert!((after.position[a] - before.position[a]).abs() < 1e-4);
        }
        assert!(m.undo(false));
        assert_eq!(m.placement(effect).unwrap().position, before.position);
    });
}

// The toolbar's turn and scale steps reach the gizmo: with 5° chosen an
// 11.5° drag round the Y ring turns the effect 10°, where the kit's own
// 15° made it 15°; with 0.25× a drag to 1.37 scales it 1.25.
#[test]
fn the_gizmo_turns_and_scales_in_the_chosen_steps() {
    let Some((app, mut w, _config)) = open(FIREFLIES) else {
        return;
    };
    let (effect, before) = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let p = m.model.particles[0].element;
        m.select(Some(p));
        m.view.snap = true;
        m.view.steps[1] = 5.;
        m.view.steps[2] = 0.25;
        m.gizmo = gpui_editor_kit::gizmo::Mode::Rotate;
        cx.notify();
        (p, m.placement(p).unwrap())
    });
    draw(&app, &mut w);
    // The kit lays a handle 92 px long: X, Y, Z on screen at that length.
    let axes = |w: &Window| {
        w.read(|ed, _| {
            let m = ed.model.as_ref().unwrap();
            let o = math::translation(&m.handle_at().unwrap().1);
            let origin = m.view.screen(o);
            let unit = m.view.screen(math::add(o, m.view.camera.right())) - origin;
            let length = 92. / f32::from(unit.x).hypot(f32::from(unit.y));
            let axis = |a: usize| {
                let mut d = [0.; 3];
                d[a] = length;
                m.view.screen(math::add(o, d)) - origin
            };
            (origin, [axis(0), axis(1), axis(2)])
        })
    };
    let (origin, [x, _, z]) = axes(&w);
    // The Y ring at 0.85 of the handle, from +Z toward +X.
    let ring = |t: f32| origin + (z * t.cos() + x * t.sin()) * 0.85;
    let mid = std::f32::consts::FRAC_PI_4;
    drag(&mut w, ring(mid - 0.1), ring(mid + 0.1), MouseButton::Left);
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let (a, b) = (before.rotation, m.placement(effect).unwrap().rotation);
        let dot: f32 = (0..4).map(|i| a[i] * b[i]).sum();
        let turned = 2. * dot.abs().min(1.).acos().to_degrees();
        assert!((turned - 10.).abs() < 0.05, "turned {turned}°");
        assert!(m.undo(false));
        m.gizmo = gpui_editor_kit::gizmo::Mode::Scale;
    });
    draw(&app, &mut w);
    let (origin, [_, y, _]) = axes(&w);
    drag(&mut w, origin + y, origin + y * 1.37, MouseButton::Left);
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let s = m.placement(effect).unwrap().scale;
        assert!((s[1] / before.scale[1] - 1.25).abs() < 1e-3, "scaled {s:?}");
    });
}

// The worker's basket hangs on a hand joint, its placement in that joint's
// space: the gizmo still moves it along the model's X, as drawn.
#[test]
fn the_gizmo_moves_an_entry_on_a_joint_along_the_models_axes() {
    let Some((app, mut w, _config)) = open(WORKER) else {
        return;
    };
    let shown = |m: &ModelEditor, prop: Id| -> Vec3 {
        let i = m
            .pieces
            .iter()
            .position(|p| p.element == Some(prop))
            .unwrap();
        math::translation(&m.view.posed.get(i).map_or(m.pieces[i].local, |p| p.local))
    };
    let (prop, before) = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        m.play(Source::Still);
        let prop = (m.model.props.iter())
            .find(|p| p.rig.link.is_some())
            .expect("a prop on a joint")
            .element;
        m.select(Some(prop));
        cx.notify();
        (prop, shown(m, prop))
    });
    draw(&app, &mut w);
    let (from, to) = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let (_, at, parent) = m.handle_at().unwrap();
        assert!(
            parent.is_some(),
            "the prop's joint is not its gizmo's space"
        );
        let o = math::translation(&at);
        let origin = m.view.screen(o);
        let d = m.view.screen(math::add(o, [0.01, 0., 0.])) - origin;
        let len = f32::from(d.x).hypot(f32::from(d.y));
        (origin + d * (50. / len), origin + d * (110. / len))
    });
    drag(&mut w, from, to, MouseButton::Left);
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let after = shown(m, prop);
        let d = math::sub(after, before);
        assert!(d[0] > 0.01, "moved {d:?}");
        assert!(
            d[1].abs() < d[0] * 0.02 && d[2].abs() < d[0] * 0.02,
            "off X: {d:?}"
        );
    });
}

// With the dog's body selected its joints show as dots; a click on one
// picks that joint, as the hierarchy's joint rows do.
#[test]
fn a_click_on_a_joint_dot_picks_the_joint() {
    let Some((app, mut w, _config)) = open(DOG) else {
        return;
    };
    let body = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let body = m.model.parts[0].element;
        m.select(Some(body));
        cx.notify();
        body
    });
    draw(&app, &mut w);
    let (j, at) = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let (part, dots) = m.joint_dots().expect("the body has a skeleton");
        assert_eq!(part, body);
        let screen: Vec<_> = dots.iter().map(|p| m.view.screen(*p)).collect();
        let gizmo = m.view.screen(math::translation(&m.handle_at().unwrap().1));
        let far =
            |a: Point<Pixels>, b: Point<Pixels>| f32::from((a - b).x).hypot(f32::from((a - b).y));
        // A dot clear of the others and of the gizmo's handles.
        (0..screen.len())
            .filter(|&j| far(screen[j], gizmo) > 130.)
            .find(|&j| (0..screen.len()).all(|k| k == j || far(screen[j], screen[k]) > 14.))
            .map(|j| (j, screen[j]))
            .expect("a joint dot stands alone")
    });
    w.simulate_click(at, MouseButton::Left);
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.joint, Some((body, j as u16)));
        assert_eq!(m.selected, Some(body), "the click took the selection");
    });
}

// The wheel stops zooming in at about 5 cm across the pane and out at the
// model a third of it.
#[test]
fn the_wheel_zooms_within_limits() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    draw(&app, &mut w);
    let middle = w.read(|ed, _| ed.model.as_ref().unwrap().view.painted().center());
    let zoom = |w: &mut Window, dy: f32| {
        for _ in 0..40 {
            w.simulate_scroll(middle, point(px(0.), px(dy)));
        }
        w.read(|ed, _| {
            let v = &ed.model.as_ref().unwrap().view;
            (v.camera.zoom, v.extent.size)
        })
    };
    let (z, size) = zoom(&mut w, 400.);
    assert!(z <= size / 0.05 + 1e-3, "in to {z} for a {size} m model");
    assert!(z > size / 0.05 * 0.9, "stopped short at {z}");
    let (z, _) = zoom(&mut w, -400.);
    assert!((0.3 - 1e-4..0.31).contains(&z), "out to {z}");
}

// A spot light carries its beam: a portrait's spot over the figure aims
// down and a little back, as its rotation turns -Y, with the cone its
// Theta and Phi give. Its line light has none.
#[test]
fn a_spot_light_aims_down_its_own_minus_y() {
    const PORTRAIT: &str = "data/base/graphics/portrait/hdri/portrait_light_diffuse_artificial.cfg";
    let Some((_app, w, _config)) = open(PORTRAIT) else {
        return;
    };
    w.read(|ed, _| {
        let m = ed.model.as_ref().expect("the model opened");
        let marks = m.marks();
        let beams: Vec<_> = marks.iter().filter_map(|k| k.cone).collect();
        assert_eq!((beams.len(), marks.len()), (1, 2));
        let c = beams[0];
        assert!(c.aim[1] < -0.95 && c.aim[2] < -0.2, "{:?}", c.aim);
        assert!((c.outer - 0.349066).abs() < 1e-5 && (c.inner - 0.174533).abs() < 1e-5);
    });
}

// Area lights carry their shape: a portrait's 2 m quad light outlines a
// 2 m square in its own X and Y, square to its -Z, and a line light 8 m
// along its own X; neither has a beam.
#[test]
fn area_lights_outline_their_shape() {
    use super::view::Shape;
    const QUAD: &str = "data/base/graphics/portrait/hdri/portrait_light_mystery_right.cfg";
    const LINE: &str = "data/base/graphics/portrait/hdri/portrait_light_experimental.cfg";
    let side = |a: Vec3, b: Vec3| math::length(math::sub(a, b));
    let Some((_app, w, _config)) = open(QUAD) else {
        return;
    };
    w.read(|ed, _| {
        let m = ed.model.as_ref().expect("the model opened");
        let k = (m.marks().into_iter())
            .find(|k| k.area.is_some_and(|a| a.shape == Shape::Quad))
            .expect("a quad light");
        assert!(k.cone.is_none());
        let corners = k.area.unwrap().outline(k.at);
        assert_eq!(corners.len(), 5, "a closed loop of four corners");
        for i in 0..4 {
            let s = side(corners[i], corners[i + 1]);
            assert!((s - 2.).abs() < 1e-3, "side {i}: {s}");
        }
        let light = m
            .model
            .lights
            .iter()
            .find(|l| l.element == k.entry)
            .unwrap();
        let t = light.transform.matrix();
        let normal = math::normalize([t[8], t[9], t[10]]).unwrap();
        for c in &corners {
            let off = math::dot(math::sub(*c, k.at), normal);
            assert!(off.abs() < 1e-4, "a corner {off} m off its plane");
        }
    });
    let Some((_app, w, _config)) = open(LINE) else {
        return;
    };
    w.read(|ed, _| {
        let m = ed.model.as_ref().expect("the model opened");
        let k = (m.marks().into_iter())
            .find(|k| k.area.is_some_and(|a| a.shape == Shape::Line))
            .expect("a line light");
        let ends = k.area.unwrap().outline(k.at);
        assert_eq!(ends.len(), 2);
        assert!((side(ends[0], ends[1]) - 8.).abs() < 1e-3);
        let light = m
            .model
            .lights
            .iter()
            .find(|l| l.element == k.entry)
            .unwrap();
        let t = light.transform.matrix();
        let along = math::normalize(math::sub(ends[1], ends[0])).unwrap();
        assert!(math::dot(along, math::normalize([t[0], t[1], t[2]]).unwrap()).abs() > 0.999);
    });
}

// A torch the game lights only by night (its DAYNIGHT_TRANSFORM) gives the
// view no light by day. N turns the view to night and lights it where its
// marker is; while playback runs its ColorGraph flickers it, and back to
// day it fades out over its FadeTime.
#[test]
fn night_lights_the_torch_and_playback_flickers_it() {
    const TORCH: &str = "data/base/graphics/library/effects/fire/fire_torch_daynight_01.cfg";
    let Some((app, mut w, _config)) = open(TORCH) else {
        return;
    };
    draw(&app, &mut w);
    let lights = |w: &Window| w.read(|ed, _| ed.model.as_ref().unwrap().view.lights.clone());
    assert!(lights(&w).is_empty(), "dark by day");
    w.simulate_keystrokes("n");
    let lit = lights(&w);
    assert_eq!(lit.len(), 1, "lit by night");
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert!(m.view.night);
        let mark = m.marks()[0];
        assert!(math::length(math::sub(mark.at, lit[0].position)) < 0.01);
        assert!(
            (mark.range - lit[0].reach).abs() < 1e-4,
            "the marker shows its reach"
        );
    });
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        m.playback.playing = true;
        m.advance(0.9);
    });
    let later = lights(&w);
    assert_ne!(later[0].color, lit[0].color, "the graph moved on");
    w.simulate_keystrokes("n");
    assert_eq!(lights(&w).len(), 1, "fading, not gone");
    w.update(|ed, _, _| ed.model.as_mut().unwrap().advance(0.3));
    assert!(lights(&w).is_empty(), "faded out by day");
}

// A fire's light, which the game lights at every hour, lights the view as
// the model opens, though nothing plays.
#[test]
fn a_still_model_opens_lit_by_its_lights() {
    const FIRE: &str = "data/base/graphics/library/props/lights/fire_light_03_big_fire.cfg";
    let Some((_app, w, _config)) = open(FIRE) else {
        return;
    };
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert!(m.model.sequences.is_empty() && !m.view.night);
        assert_eq!(m.view.lights.len(), 1);
    });
}

// A light hung on a joint lights from where playback poses the joint.
#[test]
fn a_light_on_a_joint_follows_playback() {
    const FLAG: &str = "data/base/graphics/ui/infolayer3d/3d_info_rallying_point.cfg";
    let Some((app, mut w, _config)) = open(FLAG) else {
        return;
    };
    draw(&app, &mut w);
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        if !m.model.sequences.is_empty() {
            m.play(Source::Sequence(0));
        }
        m.playback.playing = true;
        m.advance(0.7);
    });
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let l = (m.model.lights.iter())
            .find(|l| l.rig.link.is_some())
            .expect("a light on a joint");
        let joint = super::playback::joint(&m.model, &m.player.poses, l.rig.link.unwrap())
            .expect("its joint is posed");
        let want = math::translation(&math::multiply(&joint, &l.transform.matrix()));
        let off = (m.view.lights.iter())
            .map(|k| math::length(math::sub(k.position, want)))
            .fold(f32::MAX, f32::min);
        assert!(off < 1e-3, "{off} m off its joint");
    });
}

// The View menu offers the island skies of the model's region; picking
// one lights the model by it (in the game's light units, the preset's sky
// behind it), Night keeps it, and Studio goes back to the editor's sun.
#[test]
fn an_island_sky_lights_the_model_and_studio_comes_back() {
    let Some((app, mut w, _config)) = open(INSTITUTION) else {
        return;
    };
    draw(&app, &mut w);
    let first = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert!(m.view.island.is_none(), "the studio by default");
        assert!(m.view.skies.len() <= super::sky::OFFERED);
        assert!(
            m.view.skies[0].starts_with("amb_roman_"),
            "{:?}",
            m.view.skies
        );
        m.view.skies[0].clone()
    });
    w.update(|ed, _, cx| ed.model_sky(Some(first.clone()), cx));
    app.run_until_parked();
    w.simulate_keystrokes("n");
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let sky = m.view.island.as_ref().expect("lit by the sky");
        assert_eq!(sky.name, first);
        assert!(m.view.night);
        assert_eq!(m.view.light_scale(), 1.);
    });
    w.update(|ed, _, cx| ed.model_sky(None, cx));
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert!(m.view.island.is_none() && m.view.light_scale() < 1.);
    });
}
