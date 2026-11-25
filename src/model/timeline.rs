// Owns the Model workspace's timeline pane: what plays (still, the cfg's
// sequences, each part's animations), the transport and wind, a ruler that
// zooms and scrolls with the playhead and the picked joint's keys on it, a
// sequence's tracks as lanes of clips and editing them (add, delete, move,
// trim, new tracks), and the playback clock's ticks.

use super::{ModelEditor, inspector::hint, playback::Source};
use crate::editor::Editor;
use anno_formats::model::{Model, Play};
use anno_formats::xml::Id;
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    icon::Icon,
    theme,
    widgets::{self, Tone, Tooltip},
};

/// Dragged clip edges land on this grid (seconds); Ctrl drags free, to the
/// millisecond the cfg keeps.
const SNAP: f32 = 0.01;
/// A trim stops this short of the other edge, so a clip never turns
/// inside out.
const SHORTEST: f32 = 0.01;
/// The lanes' name column, the ruler's and a lane's height, in px.
const LABEL_W: f32 = 160.;
const RULER_H: f32 = 22.;
const LANE_H: f32 = 22.;
/// Grab width of a clip's edge, in px, inside the bar.
const EDGE: f32 = 5.;
/// Items per group in the Add clip menu: a unit part has forty
/// animations, and the menu scrolls past that.
const MENU_GROUP: usize = 60;
/// The transport's glyphs, on the kit's 24 px icon grid.
const TO_START: Icon = Icon(&[
    &[[6., 5.], [6., 19.]],
    &[[18., 5.], [9., 12.], [18., 19.], [18., 5.]],
]);
const TO_END: Icon = Icon(&[
    &[[18., 5.], [18., 19.]],
    &[[6., 5.], [15., 12.], [6., 19.], [6., 5.]],
]);
const PLAY: Icon = Icon(&[&[[7., 4.], [19., 12.], [7., 20.], [7., 4.]]]);
const PAUSE: Icon = Icon(&[&[[8., 5.], [8., 19.]], &[[16., 5.], [16., 19.]]]);
/// The transport's speeds, in the order its button steps through them.
const SPEEDS: [f32; 4] = [0.25, 0.5, 1., 2.];

/// The timeline's own state beside the playback clock: the seconds the
/// lanes show, the track whose Add menu is open, what a press holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pane {
    /// First second and seconds across; `None` fits the source.
    view: Option<[f32; 2]>,
    /// The `Track` whose Add menu is open, and where it opened.
    adding: Option<(Id, Point<Pixels>)>,
    grab: Option<Grab>,
}

/// What a press on the ruler, the lanes or the overview holds until release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grab {
    Playhead,
    /// The overview's window, panned.
    Overview,
}

/// All of a source `length` long, with a margin either side so the first
/// and last clip edges are not flush with the pane.
fn fit(length: f32) -> [f32; 2] {
    let d = if length > 0. { length } else { 1. };
    [-d * 0.02, d * 1.04]
}

impl Pane {
    /// First second and seconds across the lanes.
    pub(super) fn span(&self, length: f32) -> [f32; 2] {
        self.view.unwrap_or_else(|| fit(length))
    }

    /// Zooms by `factor` keeping second `t` at `share` of the width. Out
    /// past the whole source, the view fits it again.
    fn zoom(&mut self, length: f32, factor: f32, t: f32, share: f32) {
        let all = fit(length);
        let [_, len] = self.span(length);
        let len = (len * factor).clamp(0.05, all[1]);
        if len >= all[1] {
            self.view = None;
            return;
        }
        self.view = Some([t - share * len, len]);
        self.pan(length, 0.);
    }

    /// Scrolls a zoomed view by `dt` seconds, kept inside the source.
    fn pan(&mut self, length: f32, dt: f32) {
        let Some([start, len]) = self.view else {
            return;
        };
        let all = fit(length);
        let start = (start + dt).clamp(all[0], all[0] + all[1] - len);
        self.view = Some([start, len]);
    }

    /// Fits the source again, as when another one plays.
    pub fn reset(&mut self) {
        self.view = None;
        self.adding = None;
    }
}

/// Window x to seconds over the lanes, from where the ruler last laid out.
#[derive(Clone, Copy, Debug)]
struct Scale {
    start: f32,
    len: f32,
    x0: f32,
    width: f32,
}

impl Scale {
    fn new(m: &ModelEditor, area: Bounds<Pixels>) -> Scale {
        let [start, len] = m.timeline.span(m.clip_length());
        Scale {
            start,
            len,
            x0: f32::from(area.origin.x),
            width: f32::from(area.size.width).max(1.),
        }
    }

    /// Second `t` as a share of the lanes' width from their left edge.
    fn share(&self, t: f32) -> f32 {
        (t - self.start) / self.len
    }

    fn time(&self, x: Pixels) -> f32 {
        self.start + (f32::from(x) - self.x0) / self.width * self.len
    }
}

fn snap(t: f32, free: bool) -> f32 {
    match free {
        true => (t * 1000.).round() / 1000.,
        false => (t / SNAP).round() * SNAP,
    }
}

fn ms(t: f32) -> String {
    format!("{}", (t * 1000.).round() as i64)
}

/// Which part of a clip a drag holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Start,
    End,
    Both,
}

/// A clip being dragged along its lane or by an edge, as it was when the
/// drag began.
#[derive(Clone, Copy, Debug)]
pub struct ClipDrag {
    clip: Id,
    from: Pixels,
    start: f32,
    end: f32,
    edge: Edge,
    recorded: bool,
}

/// An animation's file name without its folder and `.rdm`.
fn file_stem(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.trim_end_matches(".rdm")
}

/// What a clip plays, in words: the animation's file, the particle entry's
/// name, the entries a condition shows, the sound asset's name.
fn clip_label(m: &ModelEditor, what: Play) -> String {
    match what {
        Play::Animation { model, animation } => {
            let part = m.model.parts.get(model as usize);
            (part.and_then(|p| p.animations.get(animation as usize)))
                .map(|p| file_stem(p).to_owned())
                .unwrap_or_else(|| format!("animation {animation} of part {model}"))
        }
        Play::Particle(p) => match m.entries("Particles").get(p as usize) {
            Some(&e) => m.label(e),
            None => format!("particle {p}"),
        },
        Play::Condition(c) => {
            let shown = shown_by(m, c);
            match shown.first() {
                Some(first) if shown.len() > 1 => format!("shows {first} +{}", shown.len() - 1),
                Some(first) => format!("shows {first}"),
                None => format!("condition {c:#x}"),
            }
        }
        Play::Sound(s) => match m.sounds.as_ref().and_then(|l| l.name(s.guid)) {
            Some(name) => name.to_owned(),
            None => format!("sound {}", s.guid),
        },
        Play::Other(t) => format!("type {t}"),
    }
}

/// The names of entries a condition's bits show.
fn shown_by(m: &ModelEditor, bits: u32) -> Vec<String> {
    let m2 = &m.model;
    (m2.parts.iter().map(|p| (p.element, p.rig)))
        .chain(m2.props.iter().map(|p| (p.element, p.rig)))
        .chain(m2.lights.iter().map(|p| (p.element, p.rig)))
        .chain(m2.particles.iter().map(|p| (p.element, p.rig)))
        .filter(|(_, r)| r.shown_if.is_some_and(|need| need & bits != 0))
        .map(|(e, _)| m.label(e))
        .collect()
}

/// The kind stripe on a clip bar.
fn kind_color(what: Play) -> Rgba {
    let t = theme::get();
    match what {
        Play::Animation { .. } => t.accent,
        Play::Particle(_) => t.highlight,
        Play::Condition(_) => t.success,
        Play::Sound(_) => t.syntax.keyword,
        Play::Other(_) => t.muted,
    }
}

/// What a new clip plays, for the Add menu: label and the clip.
fn clip_choices(m: &mut ModelEditor) -> Vec<(String, Vec<(String, Play)>)> {
    let mut groups = Vec::new();
    for (k, part) in m.model.parts.iter().enumerate() {
        let items: Vec<(String, Play)> = (part.animations.iter().enumerate())
            .take(MENU_GROUP)
            .map(|(a, path)| {
                let play = Play::Animation {
                    model: k as u32,
                    animation: a as u32,
                };
                (file_stem(path).to_owned(), play)
            })
            .collect();
        if !items.is_empty() {
            groups.push((format!("Animations of {}", part.name), items));
        }
    }
    let particles: Vec<(String, Play)> = (m.entries("Particles").into_iter().enumerate())
        .take(MENU_GROUP)
        .map(|(k, e)| (m.label(e), Play::Particle(k as u32)))
        .collect();
    if !particles.is_empty() {
        groups.push(("Particles".to_owned(), particles));
    }
    // The bits entries test, each once, named by what they show.
    let mut bits: Vec<u32> = (m.model.parts.iter().map(|p| p.rig.shown_if))
        .chain(m.model.props.iter().map(|p| p.rig.shown_if))
        .chain(m.model.lights.iter().map(|p| p.rig.shown_if))
        .chain(m.model.particles.iter().map(|p| p.rig.shown_if))
        .flatten()
        .collect();
    bits.sort_unstable();
    bits.dedup();
    let conditions: Vec<(String, Play)> = (bits.into_iter().take(MENU_GROUP))
        .map(|b| {
            let play = Play::Condition(b);
            (clip_label(m, play), play)
        })
        .collect();
    if !conditions.is_empty() {
        groups.push(("Conditions".to_owned(), conditions));
    }
    groups
}

impl ModelEditor {
    /// The playing sequence's `Config`, when a sequence plays.
    fn sequence_element(&self) -> Option<Id> {
        match self.playback.source {
            Source::Sequence(k) => self.model.sequences.get(k).map(|s| s.element),
            _ => None,
        }
    }

    /// The `Track`s of sequence `seq`, in file order.
    pub fn tracks(&self, seq: Id) -> Vec<Id> {
        (self.doc.children(seq))
            .filter(|&c| self.doc.name(c) == "Track")
            .collect()
    }

    /// After an edit of a sequence's tracks: reads the sequences again
    /// (the meshes stay) and keeps the playhead where it was.
    pub(super) fn sequences_edited(&mut self) {
        match Model::load_doc(&self.install, &self.path, Some(&self.doc)) {
            Ok(model) => self.model.sequences = model.sequences,
            Err(e) => self.message = Some((format!("Model not read: {e:#}"), true)),
        }
        let time = self.playback.time;
        self.play(self.playback.source);
        self.playback.time = time;
        self.pose();
    }

    /// Adds a clip to `track` that plays `what` from `at` seconds, as one
    /// edit, and selects it. It runs as long as its animation, else to the
    /// sequence's end (a second when that is behind it); a sound for a
    /// millisecond, as the game's: it only starts there.
    pub fn add_clip(&mut self, track: Id, what: Play, at: f32) -> Result<Id> {
        let start = snap(at.max(0.), false);
        let seq_end = (self.model.sequences.iter())
            .find(|s| Some(s.element) == self.doc.parent(track))
            .map_or(0., |s| s.length());
        let to_end = if seq_end > start + SHORTEST {
            seq_end
        } else {
            start + 1.
        };
        // The cfg's `Type`: 0 animation, 1 particle, 2 condition, 3 sound.
        let (kind, end, fields) = match what {
            Play::Animation { model, animation } => {
                let path = (self.model.parts.get(model as usize))
                    .and_then(|p| p.animations.get(animation as usize))
                    .cloned()
                    .context("no such animation")?;
                let install = self.install.clone();
                let anim = (self.player.animation(&install, &path))
                    .context("the animation does not read")?;
                let fields =
                    format!("<ModelID>{model}</ModelID>\n<AnimationID>{animation}</AnimationID>\n");
                (0, start + anim.duration.max(SHORTEST), fields)
            }
            Play::Particle(p) => (1, to_end, format!("<ParticleID>{p}</ParticleID>\n")),
            Play::Condition(c) => (2, to_end, format!("<Condition>{c}</Condition>\n")),
            // The picker's: on the first mesh's first joint, as 2,947 of
            // 117's 2,977 sound clips, and not looped, as 2,713.
            Play::Sound(s) => (
                3,
                start + 0.001,
                format!(
                    "<GUID>{}</GUID>\n<Looped>{}</Looped>\n<ModelID>{}</ModelID>\n<SkeletonID>0</SkeletonID>\n<BoneID>{}</BoneID>\n",
                    s.guid, s.looped as u8, s.model, s.bone
                ),
            ),
            Play::Other(t) => anyhow::bail!("clips of type {t} are not added here"),
        };
        self.checkpoint();
        let xml = format!(
            "<TrackElement><Type>{kind}</Type>\n<StartTime>{}</StartTime>\n<EndTime>{}</EndTime>\n{fields}</TrackElement>",
            ms(start),
            ms(end)
        );
        let clip = self.doc.insert(track, None, &xml);
        self.sequences_edited();
        self.select(Some(clip));
        Ok(clip)
    }

    /// Takes a clip out of its track as one edit. The track stays, empty,
    /// to take another.
    pub fn delete_clip(&mut self, clip: Id) {
        self.checkpoint();
        self.doc.remove(clip);
        if self.selected == Some(clip) {
            self.selected = None;
        }
        self.sequences_edited();
    }

    /// A new, empty track at the end of sequence `seq`, numbered after its
    /// highest `TrackID`.
    pub fn add_track(&mut self, seq: Id) -> Id {
        let next = (self.tracks(seq).iter())
            .filter_map(|&t| self.doc.child(t, "TrackID"))
            .filter_map(|i| self.doc.text(i).trim().parse::<u32>().ok())
            .max()
            .map_or(1, |n| n + 1);
        self.checkpoint();
        let track = (self.doc).insert(
            seq,
            None,
            &format!("<Track><TrackID>{next}</TrackID>\n</Track>"),
        );
        self.sequences_edited();
        track
    }

    /// Removes a track with what it plays, as one edit.
    pub fn delete_track(&mut self, track: Id) {
        self.checkpoint();
        let clips: Vec<Id> = self.doc.children(track).collect();
        self.doc.remove(track);
        if self.selected.is_some_and(|s| clips.contains(&s)) {
            self.selected = None;
        }
        self.sequences_edited();
    }
}

/// One lane: its `Track`, its id, and what its clips play in a few words.
struct Lane {
    track: Id,
    id: String,
    summary: String,
}

fn lanes(m: &ModelEditor) -> Vec<Lane> {
    let Some(seq) = m.sequence_element() else {
        return Vec::new();
    };
    let s = (m.model.sequences.iter()).find(|s| s.element == seq);
    (m.tracks(seq).into_iter())
        .map(|track| {
            let id = (m.doc.child(track, "TrackID"))
                .map(|i| m.doc.text(i).trim().to_owned())
                .unwrap_or_default();
            let clips: Vec<Play> = (s.iter().flat_map(|s| &s.clips))
                .filter(|c| m.doc.parent(c.element) == Some(track))
                .map(|c| c.what)
                .collect();
            let summary = match clips.as_slice() {
                [] => "empty".to_owned(),
                [one] => clip_label(m, *one),
                [first, rest @ ..] => format!("{} +{}", clip_label(m, *first), rest.len()),
            };
            Lane { track, id, summary }
        })
        .collect()
}

/// A row of the source list.
enum SourceRow {
    Heading(String),
    Item {
        source: Source,
        label: String,
        length: Option<f32>,
    },
}

fn source_rows(m: &mut ModelEditor) -> Vec<SourceRow> {
    let mut rows = vec![SourceRow::Item {
        source: Source::Still,
        label: "Still".to_owned(),
        length: None,
    }];
    if !m.model.sequences.is_empty() {
        rows.push(SourceRow::Heading("Sequences".to_owned()));
    }
    for (k, s) in m.model.sequences.iter().enumerate() {
        rows.push(SourceRow::Item {
            source: Source::Sequence(k),
            label: m.names.name(s.id),
            length: Some(s.length()),
        });
    }
    let install = m.install.clone();
    for part in 0..m.model.parts.len() {
        let paths = m.model.parts[part].animations.clone();
        if paths.is_empty() {
            continue;
        }
        rows.push(SourceRow::Heading(m.model.parts[part].name.clone()));
        for (animation, path) in paths.iter().enumerate() {
            // Read once and kept by the player, which plays them from there.
            let length = m.player.animation(&install, path).map(|a| a.duration);
            rows.push(SourceRow::Item {
                source: Source::Animation { part, animation },
                label: file_stem(path).to_owned(),
                length,
            });
        }
    }
    rows
}

type PressListener = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// A press on a clip or one of its edges: selects it and starts dragging
/// that part of it.
fn clip_grab(
    clip: Id,
    start: f32,
    end: f32,
    edge: Edge,
    cx: &mut Context<Editor>,
) -> PressListener {
    // Boxed: the listener's type holds `cx`'s lifetime, and three of them
    // go on one bar.
    Box::new(cx.listener(move |this, e: &MouseDownEvent, _, cx| {
        cx.stop_propagation();
        if let Some(m) = &mut this.model {
            m.select(Some(clip));
            m.timeline.adding = None;
        }
        this.model_clip_drag = Some(ClipDrag {
            clip,
            from: e.position.x,
            start,
            end,
            edge,
            recorded: false,
        });
        this.model_edited(cx);
    }))
}

impl Editor {
    /// The Timeline: what plays (still, a sequence, one part's animation),
    /// transport, a ruler to scrub and zoom, and a sequence's tracks as
    /// lanes of clips. A clip drags along its lane, or by an edge to trim
    /// it; a click selects it for the inspector, which edits every field
    /// of it.
    pub fn render_model_timeline(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(m) = &mut self.model else {
            return crate::editor::pane_note(
                "The open model's sequences and animations play here.",
            );
        };
        let keys = m.joint_key_times();
        let sources = source_rows(m);
        let choices = match m.timeline.adding {
            Some(_) => clip_choices(m),
            None => Vec::new(),
        };
        let sound_missing = self.model_sound.missing();
        let m = &*m;
        let p = m.playback;
        let length = m.clip_length();
        let bounds = self.model_ruler.get();
        let scale = Scale::new(m, bounds);

        let picker = Self::model_source_list(m, sources, cx);
        let transport = Self::model_transport(m, window, cx);

        // Labelled ticks at a step that leaves room for the labels, a fifth
        // of it between them.
        // Before the first layout the width is unknown: a guess, and the
        // canvas asks for the frame again once it knows.
        let width = if bounds.size.width > px(1.) {
            scale.width
        } else {
            600.
        };
        let pps = width / scale.len;
        let step = [
            0.01f32, 0.02, 0.05, 0.1, 0.25, 0.5, 1., 2., 5., 10., 30., 60.,
        ]
        .into_iter()
        .find(|s| s * pps >= 56.)
        .unwrap_or(120.);
        let mut labels = div().absolute().inset_0();
        let first = (scale.start / step).floor() as i64;
        for i in first..=((scale.start + scale.len) / step).ceil() as i64 {
            let t = i as f32 * step;
            if t < -1e-4 {
                continue;
            }
            // As many decimals as the step has: 0.25 s steps read 1.25,
            // not 1.2.
            let tenths = ((step * 10.).round() - step * 10.).abs() < 1e-4;
            let text = match step {
                s if s >= 1. => format!("{t:.0} s"),
                _ if tenths => format!("{t:.1} s"),
                _ => format!("{t:.2} s"),
            };
            labels = labels.child(
                div()
                    .absolute()
                    .left(relative(scale.share(t)))
                    .ml(px(3.))
                    .top(px(1.))
                    .text_size(px(9.))
                    .text_color(theme::get().muted)
                    .child(text),
            );
        }
        let cell = self.model_ruler.clone();
        let entity = cx.entity().downgrade();
        let (start, len, time) = (scale.start, scale.len, p.time);
        let ticks = canvas(
            move |b, _, cx| {
                // The tick step and the clips' x follow the width: a new
                // width redraws the pane with it.
                let resized = cell.get().size.width != b.size.width;
                cell.set(b);
                if resized {
                    let _ = entity.update(cx, |_, cx| cx.notify());
                }
            },
            move |b, _, window, _| {
                let pps = f32::from(b.size.width) / len;
                let x0 = f32::from(b.origin.x);
                let bottom = b.origin.y + b.size.height;
                let minor = step / 5.;
                if minor * pps >= 4. {
                    let first = (start / minor).floor() as i64;
                    for i in first..=((start + len) / minor).ceil() as i64 {
                        let t = i as f32 * minor;
                        if t < -1e-4 {
                            continue;
                        }
                        let major = ((t / step).round() * step - t).abs() < minor * 0.25;
                        let h = if major { 9. } else { 4. };
                        window.paint_quad(fill(
                            Bounds::new(
                                point(px(x0 + (t - start) * pps), bottom - px(h)),
                                size(px(1.), px(h)),
                            ),
                            theme::get().muted,
                        ));
                    }
                }
                // The playhead's head: a pointed tab over its line.
                let x = px(x0 + (time - start) * pps);
                if x >= b.origin.x - px(6.) && x <= b.origin.x + b.size.width + px(6.) {
                    let y = b.origin.y;
                    gpui_editor_kit::paint::polygon(
                        &[
                            point(x - px(5.), y),
                            point(x + px(5.), y),
                            point(x + px(5.), y + px(7.)),
                            point(x, y + px(12.)),
                            point(x - px(5.), y + px(7.)),
                        ],
                        theme::get().accent,
                        window,
                    );
                }
            },
        )
        .absolute()
        .inset_0();
        let playhead = |at: f32| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(at))
                .w(px(1.))
                .bg(theme::get().accent)
        };
        let in_view = |t: f32| (0. ..=1.).contains(&scale.share(t));
        let ruler = div()
            .id("model-ruler")
            .relative()
            .flex_1()
            .min_w_0()
            .h(px(RULER_H))
            .overflow_hidden()
            .bg(theme::get().card)
            .border_b_1()
            .border_color(theme::get().border)
            .rounded_t(px(2.))
            .cursor(CursorStyle::ResizeLeftRight)
            .child(ticks)
            .child(labels)
            // The picked joint's keys.
            .children(keys.iter().filter(|&&t| in_view(t)).map(|&t| {
                div()
                    .absolute()
                    .bottom(px(2.))
                    .left(relative(scale.share(t)))
                    .ml(px(-3.))
                    .size(px(6.))
                    .rounded(px(1.))
                    .bg(theme::get().highlight)
            }))
            .when(in_view(p.time), |d| d.child(playhead(scale.share(p.time))))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.model_grab(Grab::Playhead, e.position, cx)
                }),
            );
        let zoomed = m.timeline.view.is_some();
        let seq = m.sequence_element();
        let ruler_row = div()
            .flex()
            .items_center()
            .child(
                div()
                    .w(px(LABEL_W))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pr_2()
                    .child(hint(match zoomed {
                        true => format!("Tracks · {:.0}%", len / fit(length)[1] * 100.),
                        false => "Tracks".to_owned(),
                    }))
                    .child(div().flex_1())
                    // In the header, not a row of its own: a pane two
                    // hundred pixels tall has room for few lanes.
                    .when_some(seq, |d, seq| {
                        d.child(
                            widgets::icon_action("model-add-track", Icon::PLUS)
                                .tooltip(Tooltip::text(
                                    "Add a track",
                                    "",
                                    "An empty lane; its + adds a clip",
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(m) = &mut this.model {
                                        m.add_track(seq);
                                    }
                                    this.model_edited(cx);
                                })),
                        )
                    })
                    .when(zoomed, |d| {
                        d.child(
                            widgets::icon_action("model-timeline-fit", Icon::FIT)
                                .tooltip(Tooltip::text("Show all of it", "", ""))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(m) = &mut this.model {
                                        m.timeline.view = None;
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(ruler);

        let mut lanes_box = div().flex().flex_col().gap(px(2.)).w_full();
        let shown = (m.model.sequences.iter()).find(|s| Some(s.element) == seq);
        for (k, lane) in lanes(m).into_iter().enumerate() {
            let track = lane.track;
            let empty = lane.summary == "empty";
            let label = div()
                .id(("model-lane-label", k))
                .w(px(LABEL_W))
                .h(px(LANE_H))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .pr_1()
                .child(
                    div()
                        .flex_shrink_0()
                        .min_w(px(16.))
                        .px_1()
                        .rounded(px(2.))
                        .bg(theme::get().card)
                        .text_size(px(10.))
                        .text_color(theme::get().muted)
                        .child(lane.id.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(if empty {
                            theme::get().disabled
                        } else {
                            theme::get().text
                        })
                        .child(lane.summary.clone()),
                )
                .child(
                    // No tooltip: it stayed up over the menu the press opens,
                    // which says what can be added.
                    widgets::icon_action(("model-lane-add", k), Icon::PLUS).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            if let Some(m) = &mut this.model {
                                m.timeline.adding = Some((track, e.position));
                            }
                            cx.notify();
                        }),
                    ),
                )
                .when(empty, |d| {
                    d.child(
                        widgets::icon_action(("model-lane-remove", k), Icon::CLOSE)
                            .tooltip(Tooltip::text("Remove this empty track", "", ""))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(m) = &mut this.model {
                                    m.delete_track(track);
                                }
                                this.model_edited(cx);
                            })),
                    )
                });
            let mut area = div()
                .id(("model-lane", k))
                .relative()
                .flex_1()
                .min_w_0()
                .h(px(LANE_H))
                .overflow_hidden()
                .bg(theme::get().well)
                .rounded(px(2.))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, e: &MouseDownEvent, _, cx| {
                        this.model_grab(Grab::Playhead, e.position, cx)
                    }),
                );
            for clip in (shown.iter().flat_map(|s| &s.clips))
                .filter(|c| m.doc.parent(c.element) == Some(track))
            {
                let (from, to) = (
                    scale.share(clip.start),
                    scale.share(clip.end.max(clip.start)),
                );
                if to < 0. || from > 1. {
                    continue;
                }
                let element = clip.element;
                let selected = m.selected == Some(element);
                let (start, end) = (clip.start, clip.end);
                let name = clip_label(m, clip.what);
                let tip = format!("{:.2} s – {:.2} s · track {}", start, end, lane.id);

                let handle = |id: &'static str, left: bool| {
                    div()
                        .id((id, element.raw() as usize))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .w(px(EDGE))
                        .when(left, |d| d.left_0())
                        .when(!left, |d| d.right_0())
                        .cursor(CursorStyle::ResizeLeftRight)
                        .hover(|d| d.bg(theme::get().accent.alpha(0.5)))
                };
                area = area.child(
                    div()
                        .id(("model-clip", element.raw() as usize))
                        .absolute()
                        .top(px(2.))
                        .bottom(px(2.))
                        .left(relative(from))
                        .w(relative((to - from).max(0.004)))
                        .min_w(px(3.))
                        // A clip that starts left of the view keeps its
                        // name in sight.
                        .pl(px(6. + (-from).max(0.) * scale.width))
                        .pr(px(EDGE))
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .rounded(px(2.))
                        .text_size(px(10.))
                        .bg(if selected {
                            theme::get().selected
                        } else {
                            theme::get().track
                        })
                        .border_1()
                        .border_color(if selected {
                            theme::get().accent
                        } else {
                            theme::get().track
                        })
                        .text_color(if selected {
                            theme::get().on_accent
                        } else {
                            theme::get().text
                        })
                        .cursor_grab()
                        .tooltip(Tooltip::text(name.clone(), "", tip))
                        // What it plays, as a stripe down its left edge.
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .bottom_0()
                                .w(px(3.))
                                .bg(kind_color(clip.what)),
                        )
                        .child(div().truncate().child(name))
                        .child(handle("model-clip-start", true).on_mouse_down(
                            MouseButton::Left,
                            clip_grab(element, start, end, Edge::Start, cx),
                        ))
                        .child(handle("model-clip-end", false).on_mouse_down(
                            MouseButton::Left,
                            clip_grab(element, start, end, Edge::End, cx),
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            clip_grab(element, start, end, Edge::Both, cx),
                        ),
                );
            }
            if in_view(p.time) {
                area = area.child(playhead(scale.share(p.time)));
            }
            lanes_box = lanes_box.child(div().flex().items_center().child(label).child(area));
        }
        // The visible window over the whole source, while zoomed: drag it
        // to scroll.
        let overview = m.timeline.view.map(|[s, l]| {
            let all = fit(length);
            let (a, w) = ((s - all[0]) / all[1], l / all[1]);
            div()
                .flex()
                .child(div().w(px(LABEL_W)).flex_shrink_0())
                .child(
                    div()
                        .id("model-overview")
                        .relative()
                        .flex_1()
                        .h(px(8.))
                        .rounded(px(4.))
                        .bg(theme::get().well)
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(relative(a))
                                .w(relative(w))
                                .rounded(px(4.))
                                .bg(theme::get().scrollbar),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, e: &MouseDownEvent, _, cx| {
                                this.model_grab(Grab::Overview, e.position, cx)
                            }),
                        ),
                )
        });
        let footer = seq.map(|_| Self::model_clip_bar(m, cx));
        let menu = m.timeline.adding.map(|(track, at)| {
            let mut body = div()
                .id("model-add-clip")
                .occlude()
                .w(px(260.))
                .max_h(px(380.))
                .overflow_y_scroll()
                .bg(theme::get().popup)
                .rounded(px(6.))
                .border_1()
                .border_color(theme::get().floating_border)
                .shadow_lg()
                .p_1()
                .flex()
                .flex_col()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.timeline.adding = None;
                    }
                    cx.notify();
                }));
            if choices.is_empty() {
                body = body.child(widgets::menu_item_off(
                    "Nothing to add",
                    "The cfg has no animations, particles or conditions",
                ));
            }
            for (g, (heading, items)) in choices.into_iter().enumerate() {
                body = body.child(match g {
                    0 => widgets::menu_header(heading).into_any_element(),
                    _ => widgets::menu_section(heading).into_any_element(),
                });
                for (k, (label, what)) in items.into_iter().enumerate() {
                    body = body.child(
                        widgets::menu_item(label, "")
                            .id(("model-add-clip-item", g * 1000 + k))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(m) = &mut this.model else { return };
                                m.timeline.adding = None;
                                let at = m.playback.time;
                                if let Err(e) = m.add_clip(track, what, at) {
                                    m.message = Some((format!("No clip added: {e:#}"), true));
                                }
                                this.model_edited(cx);
                            })),
                    );
                }
            }
            // From the game's sounds, through the picker: thousands of them.
            body = body.child(widgets::menu_section("Sounds"));
            body = body.child(match sound_missing {
                None => widgets::menu_item("Sound…", "")
                    .id("model-add-sound")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let Some(m) = &mut this.model else { return };
                        m.timeline.adding = None;
                        let at = m.playback.time;
                        this.open_model_picker(super::picker::Want::Sound { track, at }, cx);
                    }))
                    .into_any_element(),
                Some(why) => widgets::menu_item_off("Sound…", why).into_any_element(),
            });
            deferred(
                anchored()
                    .position(at)
                    .snap_to_window_with_margin(px(8.))
                    .child(body),
            )
            .with_priority(1)
        });
        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .p_2()
            .gap_2()
            .text_size(px(12.))
            .child(transport)
            .child(
                div()
                    .id("model-lanes")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    // As tall as its lanes, shrinking to scroll them in a
                    // short pane: the row under them stays in sight.
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(ruler_row)
                    .child(lanes_box)
                    .children(overview)
                    .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| this.model_wheel(e, cx))),
            )
            .children(footer)
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                this.model_timeline_move(e, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.model_timeline_release(cx)),
            );
        for line in m.timeline_notes() {
            body = body.child(hint(line));
        }
        div()
            .size_full()
            .flex()
            .bg(theme::get().panel)
            .text_size(px(12.))
            .child(picker)
            .child(body)
            .children(menu)
            .into_any_element()
    }

    /// The source list: Still, then the sequences and each part's
    /// animations under headings, lengths in a column; what plays marked.
    fn model_source_list(
        m: &ModelEditor,
        rows: Vec<SourceRow>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let now = m.playback.source;
        div()
            .id("model-sources")
            .flex()
            .flex_col()
            .w(px(230.))
            .flex_shrink_0()
            .py_1()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme::get().border)
            .children(rows.into_iter().enumerate().map(|(i, row)| {
                match row {
                    // Rows keep their height: in a list that scrolls, an
                    // overflow-hidden row would give it up first.
                    SourceRow::Heading(text) => div()
                        .id(("model-source", i))
                        .flex_shrink_0()
                        .px_2()
                        .pt_2()
                        .pb(px(2.))
                        .truncate()
                        .text_size(px(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::get().muted)
                        .child(text),
                    SourceRow::Item {
                        source,
                        label,
                        length,
                    } => {
                        let on = source == now;
                        div()
                            .id(("model-source", i))
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(20.))
                            .flex_shrink_0()
                            .pl(px(if source == Source::Still { 8. } else { 16. }))
                            .pr_2()
                            .cursor_pointer()
                            .when(on, |d| {
                                d.bg(theme::get().selected).child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .top_0()
                                        .bottom_0()
                                        .w(px(2.))
                                        .bg(theme::get().accent),
                                )
                            })
                            .when(!on, |d| d.hover(|d| d.bg(theme::get().hover)))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .w(px(48.))
                                    .text_right()
                                    .text_size(px(11.))
                                    .text_color(theme::get().muted)
                                    .child(
                                        length.map_or_else(String::new, |l| format!("{l:.2} s")),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(m) = &mut this.model {
                                    m.timeline.reset();
                                    m.play(source);
                                }
                                cx.notify();
                            }))
                    }
                }
            }))
    }

    /// The transport, as the UI timeline's: to the start, play, to the end,
    /// loop and speed, the clock; then the scene around the model, its
    /// sound and wind, at the row's end.
    fn model_transport(m: &ModelEditor, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let p = m.playback;
        let length = m.clip_length();
        // Drawn icons: the media glyphs (⏮ ⏭) came out of the emoji font
        // as blue tiles.
        let seek = |id: &'static str, icon: Icon, tip: &'static str, end: bool| {
            widgets::icon_button(id, icon, false)
                .size(px(28.))
                .tooltip(Tooltip::text(tip, "", ""))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.playback.time = if end { m.clip_length() } else { 0. };
                        m.pose();
                    }
                    cx.notify();
                }))
        };
        let start = seek("model-to-start", TO_START, "To the start", false);
        let end = seek("model-to-end", TO_END, "To the end", true);
        let play = widgets::icon_button(
            "model-play",
            if p.playing { PAUSE } else { PLAY },
            p.playing,
        )
        .size(px(28.))
        .tooltip(Tooltip::text("Play or pause", "Space", ""))
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(m) = &mut this.model {
                // Played from the end, a clip that stops there starts
                // over, as the UI timeline's does.
                let length = m.clip_length();
                let p = &mut m.playback;
                if !p.playing && !p.looping && p.time >= length {
                    p.time = 0.;
                }
                p.playing = !p.playing;
            }
            this.model_tick(cx);
            cx.notify();
        }));
        let looping = widgets::button("Loop", p.looping)
            .tooltip(Tooltip::text(
                "Loop",
                "",
                "Play the clip over and over; off, it stops at its end",
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.playback.looping = !m.playback.looping;
                }
                cx.notify();
            }));
        let speed = widgets::button(format!("{}×", p.speed), false)
            .tooltip(Tooltip::text(
                "Speed",
                "",
                "0.25×, 0.5×, 1× or 2×; a click steps to the next",
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    let s = m.playback.speed;
                    let i = SPEEDS.iter().position(|&x| x >= s - 1e-3).unwrap_or(2);
                    m.playback.speed = SPEEDS[(i + 1) % SPEEDS.len()];
                }
                cx.notify();
            }));
        let sound = widgets::button("Sound", p.sound)
            .tooltip(Tooltip::text(
                "Sound",
                "",
                "Play the sounds the clips start, from the game's banks",
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.playback.sound = !m.playback.sound;
                }
                cx.notify();
            }));
        // The scene's wind, for particles that drift, cloth and flags.
        let wind = widgets::field_dropdown(
            "model-wind",
            "Wind",
            [
                (0f32, "Calm"),
                (0.5, "Breeze"),
                (1., "Strong"),
                (2., "Storm"),
            ]
            .map(|(w, l)| (w, l.into()))
            .to_vec(),
            &p.wind,
            window,
            cx,
            |this: &mut Editor, wind, _, cx| {
                if let Some(m) = &mut this.model {
                    m.playback.wind = wind;
                    m.pose();
                }
                cx.notify();
            },
        )
        .w(px(124.));
        let toward = widgets::field_dropdown(
            "model-wind-direction",
            "Toward",
            [(0f32, "+X"), (90., "+Z"), (180., "−X"), (270., "−Z")]
                .map(|(d, l)| (d, l.into()))
                .to_vec(),
            &p.wind_direction,
            window,
            cx,
            |this: &mut Editor, direction, _, cx| {
                if let Some(m) = &mut this.model {
                    m.playback.wind_direction = direction;
                    m.pose();
                }
                cx.notify();
            },
        )
        .w(px(112.));
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .text_size(px(11.))
            .child(start)
            .child(play)
            .child(end)
            .child(looping)
            .child(speed)
            .child(
                div()
                    .px_2()
                    .min_w(px(104.))
                    .child(format!("{:.2} / {:.2} s", p.time, length)),
            )
            .when_some(
                match p.source {
                    Source::Animation { part, animation } => Some((part, animation)),
                    _ => None,
                },
                |d, (part, animation)| {
                    d.child(
                        widgets::button("New sequence from it", false)
                            .tooltip(Tooltip::text(
                                "New sequence",
                                "",
                                "A looped sequence that plays this animation once through; set its id in the inspector",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(m) = &mut this.model
                                    && let Err(e) = m.new_sequence(part, animation)
                                {
                                    m.message = Some((format!("No sequence made: {e:#}"), true));
                                }
                                this.model_edited(cx);
                            })),
                    )
                },
            )
            .child(div().flex_1())
            .child(sound)
            .child(wind)
            .child(toward)
    }

    /// The row under the lanes: the selected clip, when and what it
    /// plays, with Delete; then how the lanes take the mouse.
    fn model_clip_bar(m: &ModelEditor, cx: &mut Context<Self>) -> Div {
        let clip = (m.model.sequences.iter().flat_map(|s| &s.clips))
            .find(|c| Some(c.element) == m.selected)
            .copied();
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(26.))
            .flex_shrink_0()
            .when_some(clip, |d, c| {
                let text = format!("{} · {:.2} – {:.2} s", clip_label(m, c.what), c.start, c.end);
                d.child(div().flex_shrink_0().text_size(px(11.)).child(text))
                    .child(
                        widgets::action("Delete clip", Tone::Danger)
                            .flex_shrink_0()
                            .tooltip(Tooltip::text("Delete the selected clip", "Delete", ""))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(m) = &mut this.model {
                                    m.delete_clip(c.element);
                                }
                                this.model_edited(cx);
                            })),
                    )
            })
            // Cut at the end, never across: a row sliced in half read as
            // a glitch.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child(
                        "+ adds a clip at the playhead · drag a clip, or its edge to trim · Ctrl drags off the 10 ms grid · Ctrl wheel zooms, Shift wheel scrolls",
                    ),
            )
    }

    /// A press on the ruler, a lane or the overview: the playhead or the
    /// view follows the pointer until release.
    fn model_grab(&mut self, grab: Grab, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        m.timeline.grab = Some(grab);
        m.timeline.adding = None;
        self.model_follow(at, false, cx);
    }

    fn model_follow(&mut self, at: Point<Pixels>, free: bool, cx: &mut Context<Self>) {
        let bounds = self.model_ruler.get();
        let Some(m) = &mut self.model else { return };
        if bounds.size.width <= px(0.) {
            return;
        }
        let length = m.clip_length();
        let scale = Scale::new(m, bounds);
        match m.timeline.grab {
            Some(Grab::Playhead) => {
                let t = scale.time(at.x).clamp(0., length);
                m.playback.time = if free { t } else { snap(t, true) };
                m.pose();
            }
            Some(Grab::Overview) => {
                // The window's centre goes under the pointer.
                let all = fit(length);
                let share = (f32::from(at.x) - scale.x0) / scale.width;
                let centre = all[0] + share * all[1];
                let [start, len] = m.timeline.span(length);
                m.timeline.pan(length, centre - len / 2. - start);
            }
            None => return,
        }
        cx.notify();
    }

    fn model_timeline_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if e.pressed_button != Some(MouseButton::Left) {
            // A press that ended outside the pane.
            if self
                .model
                .as_ref()
                .is_some_and(|m| m.timeline.grab.is_some())
                || self.model_clip_drag.is_some()
            {
                self.model_timeline_release(cx);
            }
            return;
        }
        if self.model_clip_drag.is_some() {
            return self.model_clip_move(e, cx);
        }
        let free = e.modifiers.control || e.modifiers.platform;
        self.model_follow(e.position, free, cx);
    }

    fn model_timeline_release(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = &mut self.model {
            m.timeline.grab = None;
        }
        if self.model_clip_drag.take().is_some() {
            self.model_edited(cx);
        }
    }

    /// Ctrl wheel zooms around the pointer; Shift wheel, or a sideways
    /// one, scrolls the zoomed lanes. A plain wheel scrolls the lanes up
    /// and down.
    fn model_wheel(&mut self, e: &ScrollEvent, cx: &mut Context<Self>) {
        let bounds = self.model_ruler.get();
        let Some(m) = &mut self.model else { return };
        let mods = e.modifiers;
        let (dx, dy) = match e.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(p) => (p.x * 32., p.y * 32.),
        };
        let length = m.clip_length();
        let scale = Scale::new(m, bounds);
        if mods.control || mods.platform {
            let share = (f32::from(e.position.x) - scale.x0) / scale.width;
            let t = scale.time(e.position.x);
            m.timeline
                .zoom(length, (-dy * 0.004).exp(), t, share.clamp(0., 1.));
        } else if mods.shift || dx != 0. {
            let d = if mods.shift && dx == 0. { dy } else { dx };
            m.timeline.pan(length, -d * scale.len / scale.width);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// A clip dragged along its lane (start and end together) or by an
    /// edge (that one alone), by the ruler's seconds per pixel, onto the
    /// 10 ms grid unless Ctrl is held. The doc is recorded once, at the
    /// first move.
    fn model_clip_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let bounds = self.model_ruler.get();
        let (Some(drag), Some(m)) = (&mut self.model_clip_drag, &mut self.model) else {
            return;
        };
        if bounds.size.width <= px(0.) {
            return;
        }
        let scale = Scale::new(m, bounds);
        let free = e.modifiers.control || e.modifiers.platform;
        let shift = f32::from(e.position.x - drag.from) / scale.width * scale.len;
        let (start, end) = match drag.edge {
            Edge::Both => {
                let start = snap((drag.start + shift).max(0.), free);
                (start, start + (drag.end - drag.start))
            }
            Edge::Start => {
                let start = snap(drag.start + shift, free);
                (start.clamp(0., drag.end - SHORTEST), drag.end)
            }
            Edge::End => (
                drag.start,
                snap(drag.end + shift, free).max(drag.start + SHORTEST),
            ),
        };
        let (Some(a), Some(b)) = (
            m.doc.child(drag.clip, "StartTime"),
            m.doc.child(drag.clip, "EndTime"),
        ) else {
            return;
        };
        let (start, end) = (ms(start), ms(end));
        if m.doc.text(a) == start && m.doc.text(b) == end {
            return;
        }
        m.retime(a, b, &start, &end, !drag.recorded);
        drag.recorded = true;
        cx.notify();
    }

    /// Advances playback a frame while it plays, plays the sounds it
    /// crossed, keeps a zoomed view on the playhead, and asks for the next.
    pub(crate) fn model_tick(&mut self, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else {
            self.model_sound_frame((Source::Still, 0.), Vec::new());
            return;
        };
        let before = (m.playback.source, m.playback.time);
        if !m.playback.playing {
            // The tick after a pause: what clips started stops.
            self.model_sound_frame(before, Vec::new());
            return;
        }
        let frame = std::time::Duration::from_millis(33);
        m.advance(frame.as_secs_f32());
        // A zoomed view pages along as the playhead leaves it.
        let length = m.clip_length();
        if let Some([start, len]) = m.timeline.view
            && !(start..start + len).contains(&m.playback.time)
        {
            m.timeline.pan(length, m.playback.time - len * 0.05 - start);
        }
        // Taken every frame, so sounds crossed while muted do not all
        // start at once when it is turned on.
        let due = std::mem::take(&mut m.sounds_due);
        self.model_sound_frame(before, due);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(frame).await;
            this.update(cx, |this, cx| this.model_tick(cx)).ok();
        })
        .detach();
    }
}
