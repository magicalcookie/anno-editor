// The Model workspace's timeline pane in GPUI's test platform, on the 117 install
// (see `tests::open`).

use super::picker::Want;
use super::playback::Source;
use super::tests::{DOG, WORKER, Window, open};
use anno_formats::model::{Model, Play, Sound};
use anno_formats::xml::Doc;
use gpui::{
    Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    ScrollDelta, ScrollEvent, ScrollSource, TestApp, TouchPhase, point, px,
};

/// Where the ruler laid out last, after a frame drawn with the dog's
/// first sequence playing.
fn ruler(app: &TestApp, w: &mut Window) -> Bounds<Pixels> {
    w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        m.play(Source::Sequence(0));
        cx.notify();
    });
    app.run_until_parked();
    w.draw();
    let b = w.read(|ed, _| ed.model_ruler.get());
    assert!(
        b.size.width > px(100.),
        "the timeline is not laid out: {b:?}"
    );
    b
}

/// Window x of second `t` on the ruler `b`, with the view the editor shows.
fn x_of(w: &Window, b: Bounds<Pixels>, t: f32) -> Pixels {
    let [start, len] = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        m.timeline.span(m.clip_length())
    });
    b.origin.x + b.size.width * ((t - start) / len)
}

fn drag(w: &mut Window, from: gpui::Point<Pixels>, to: gpui::Point<Pixels>, ctrl: bool) {
    let modifiers = Modifiers {
        control: ctrl,
        ..Default::default()
    };
    w.simulate_event(MouseDownEvent {
        position: from,
        button: MouseButton::Left,
        modifiers,
        click_count: 1,
        first_mouse: false,
    });
    for k in 1..=4 {
        let x = from.x + (to.x - from.x) * (k as f32 / 4.);
        w.simulate_event(MouseMoveEvent {
            position: point(x, to.y),
            modifiers,
            pressed_button: Some(MouseButton::Left),
        });
    }
    w.simulate_event(MouseUpEvent {
        position: to,
        button: MouseButton::Left,
        modifiers,
        click_count: 1,
    });
}

// A track added to the dog's idle takes a walk clip at the playhead, which
// runs as long as the walk; deleting it and the track leaves the file as it
// was. Each is one undo step.
#[test]
fn clips_and_tracks_add_and_delete_as_one_edit_each() {
    let Some((_app, mut w, _config)) = open(DOG) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        m.play(Source::Sequence(0));
        let seq = m.model.sequences[0].element;
        let before = m.doc.write();
        let track = m.add_track(seq);
        assert_eq!(m.tracks(seq).last(), Some(&track));
        let id = m
            .doc
            .child(track, "TrackID")
            .map(|i| m.doc.text(i).to_owned());
        assert_eq!(id.as_deref(), Some("2"));
        let walk = Play::Animation {
            model: 0,
            animation: 1,
        };
        let clip = m.add_clip(track, walk, 0.504).unwrap();
        assert_eq!(m.selected, Some(clip));
        let c = (m.model.sequences[0].clips.iter())
            .find(|c| c.element == clip)
            .copied()
            .expect("the clip plays in the sequence");
        assert_eq!((c.track, c.what), (2, walk));
        assert!(
            (c.start - 0.5).abs() < 1e-6,
            "on the 10 ms grid: {}",
            c.start
        );
        assert!(c.end > c.start, "{c:?}");
        // The game reads it back the same.
        let again = Model::from_doc(&Doc::parse(&m.doc.write())).unwrap();
        assert!(
            again.sequences[0]
                .clips
                .iter()
                .any(|k| k.what == walk && k.track == 2)
        );
        m.delete_clip(clip);
        assert_eq!(m.selected, None);
        assert!(m.model.sequences[0].clips.iter().all(|c| c.element != clip));
        m.delete_track(track);
        assert_eq!(m.doc.write(), before, "the track and clip are gone again");
        for _ in 0..4 {
            assert!(m.undo(false));
        }
        assert_eq!(m.doc.write(), before, "four edits, four undos");
    });
}

// A condition clip shows the worker's tool until the sequence's end.
#[test]
fn a_condition_clip_runs_to_the_sequences_end() {
    let Some((_app, mut w, _config)) = open(WORKER) else {
        return;
    };
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        let bits = (m.model.props.iter())
            .find_map(|p| p.rig.shown_if)
            .expect("an entry a condition shows");
        m.play(Source::Sequence(0));
        let seq = m.model.sequences[0].element;
        let end = m.model.sequences[0].length();
        let track = m.add_track(seq);
        let clip = m.add_clip(track, Play::Condition(bits), 0.).unwrap();
        let c = (m.model.sequences[0].clips.iter())
            .find(|c| c.element == clip)
            .copied()
            .unwrap();
        assert_eq!(c.what, Play::Condition(bits));
        assert!((c.end - end).abs() < 1e-3, "{} against {end}", c.end);
    });
}

// Dragging the end edge of the dog's idle clip trims it onto the 10 ms
// grid and leaves its start; with Ctrl it lands to the millisecond. One
// undo takes each drag back.
#[test]
fn dragging_an_edge_trims_the_clip() {
    let Some((app, mut w, _config)) = open(DOG) else {
        return;
    };
    let b = ruler(&app, &mut w);
    let (clip, start, end, before) = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let c = m.model.sequences[0].clips[0];
        (c.element, c.start, c.end, m.doc.write())
    });
    // The first lane sits under the ruler row.
    let y = b.origin.y + b.size.height + px(2. + 11.);
    let edge = x_of(&w, b, end) - px(2.);
    drag(&mut w, point(edge, y), point(edge - px(60.), y), false);
    let trimmed = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let c = (m.model.sequences[0].clips.iter())
            .find(|c| c.element == clip)
            .copied()
            .unwrap();
        assert_eq!(c.start, start, "the start stays");
        assert!(c.end < end - 0.05, "{} not trimmed from {end}", c.end);
        let ms = (c.end * 1000.).round() as i64;
        assert_eq!(ms % 10, 0, "{ms} ms is off the grid");
        c.end
    });
    w.simulate_keystrokes("ctrl-z");
    w.read(|ed, _| assert_eq!(ed.model.as_ref().unwrap().doc.write(), before));
    // Ctrl: the same drag, to the millisecond, 0.5 px a step.
    let edge = x_of(&w, b, end) - px(2.);
    drag(&mut w, point(edge, y), point(edge - px(60.5), y), true);
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let c = m.model.sequences[0].clips[0];
        assert!((c.end - trimmed).abs() < 0.02 && c.end < end - 0.05);
    });
}

// A press on a lane puts the playhead under the pointer; Ctrl wheel over
// the lanes zooms in around it, and the fit takes it back.
#[test]
fn lanes_scrub_and_zoom() {
    let Some((app, mut w, _config)) = open(DOG) else {
        return;
    };
    // An empty second track, under the idle's lane.
    w.update(|ed, _, _| {
        let m = ed.model.as_mut().unwrap();
        m.play(Source::Sequence(0));
        let seq = m.model.sequences[0].element;
        m.add_track(seq);
    });
    let b = ruler(&app, &mut w);
    let y = b.origin.y + b.size.height + px(2. + 22. + 2. + 11.);
    let at = x_of(&w, b, 1.25);
    // A drag along it: the playhead follows to where it lets go.
    drag(&mut w, point(at - px(40.), y), point(at, y), true);
    let selected = w.read(|ed, _| ed.model.as_ref().unwrap().selected);
    assert_eq!(selected, None, "the press took a clip");
    let t = w.read(|ed, _| ed.model.as_ref().unwrap().playback.time);
    assert!(
        (t - 1.25).abs() < 0.02,
        "the playhead is at {t}, the ruler at {b:?}"
    );
    let full = w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        m.timeline.span(m.clip_length())
    });
    w.simulate_event(ScrollEvent {
        position: point(at, y),
        delta: ScrollDelta::Pixels(point(px(0.), px(200.))),
        modifiers: Modifiers {
            control: true,
            ..Default::default()
        },
        touch_phase: TouchPhase::Moved,
        source: ScrollSource::Pointer,
    });
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        let [start, len] = m.timeline.span(m.clip_length());
        assert!(len < full[1] * 0.75, "not zoomed: {len} of {}", full[1]);
        // The second under the pointer stays under it.
        let share = (1.25 - full[0]) / full[1];
        assert!((start + share * len - 1.25).abs() < 0.05, "{start} {len}");
    });
    w.update(|ed, _, _| ed.model.as_mut().unwrap().timeline.reset());
    w.read(|ed, _| {
        let m = ed.model.as_ref().unwrap();
        assert_eq!(m.timeline.span(m.clip_length()), full);
    });
}

// Sound… in the Add clip menu opens the picker on the sounds the model
// plays; typing searches the game's by name, and the pick lands as a clip
// the game reads: a millisecond long, started as the playhead crosses it.
#[test]
fn a_sound_clip_adds_from_the_picker() {
    let Some((app, mut w, _config)) = open(DOG) else {
        return;
    };
    let track = w.update(|ed, _, cx| {
        let m = ed.model.as_mut().unwrap();
        let sounds = m.sounds.clone().expect("the game's sounds loaded");
        m.play(Source::Sequence(0));
        let mut own: Vec<String> = (m.model.sequences.iter().flat_map(|s| &s.clips))
            .filter_map(|c| match c.what {
                Play::Sound(s) => sounds.name(s.guid).map(str::to_owned),
                _ => None,
            })
            .collect();
        own.sort();
        own.dedup();
        let track = m.add_track(m.model.sequences[0].element);
        ed.open_model_picker(Want::Sound { track, at: 0.25 }, cx);
        assert_eq!(ed.model_picker.as_ref().unwrap().hits, own);
        track
    });
    w.simulate_keystrokes("s w e e p i n g");
    app.run_until_parked();
    let at = w.read(|ed, _| {
        let hits = &ed.model_picker.as_ref().unwrap().hits;
        assert!(hits.iter().all(|h| h.contains("sweeping")), "{hits:?}");
        (hits.iter().position(|h| h == "evt_fba_resident_sweeping")).expect("the broom")
    });
    for _ in 0..at {
        w.simulate_keystrokes("down");
    }
    w.simulate_keystrokes("enter");
    app.run_until_parked();
    w.read(|ed, _| {
        assert!(ed.model_picker.is_none());
        let m = ed.model.as_ref().unwrap();
        let c = (m.model.sequences[0].clips.iter())
            .find(|c| m.doc.parent(c.element) == Some(track))
            .copied()
            .expect("the clip is in the track");
        assert_eq!(c.what, Play::Sound(Sound::of(51235)));
        assert!(
            (c.start - 0.25).abs() < 1e-6 && (c.end - 0.251).abs() < 1e-6,
            "{c:?}"
        );
        let again = Model::from_doc(&Doc::parse(&m.doc.write())).unwrap();
        assert!(
            again.sequences[0]
                .clips
                .iter()
                .any(|k| k.what == Play::Sound(Sound::of(51235)))
        );
        assert!(
            m.sounds_between(0.2, 0.3)
                .contains(&(c.element, Sound::of(51235)))
        );
    });
}
