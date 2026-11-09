// Owns the animation player and the Timeline pane: the clips of the open
// file and of the container templates it shows, playback (play, pause,
// loop, speed, frame and key stepping, scrubbing on the ruler), and key
// editing (select, multi-select, drag in time snapped to frames, add,
// delete, easing by name, value). Lanes group by the widget they animate.
// A playing clip writes its tracks through the binding slots of its file
// (`phx::animation`), and the canvas lays out again each frame.
use super::UiEditor;
use crate::editor::{Editor, Pane, empty_pane};
use anno_formats::phx::{
    Asset, DefaultValue, Edit, Guid, Value,
    animation::{Key, NewKey, TrackKind},
    binding::{NewSlot, Slot, builtin},
    layout::{self, Play},
};
use anyhow::{Context as _, Result, bail};
use gpui_editor_kit::{
    field::{Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme,
    widgets::{Tooltip, button},
};
use std::time::Duration;

/// Frame interval while playing: 30 fps keeps CPU-composited masks smooth
/// enough without relaying out a big tree 60 times a second.
const FRAME: Duration = Duration::from_millis(33);
const SPEEDS: [f32; 4] = [0.25, 0.5, 1., 2.];

/// One track of a clip as the pane shows it.
#[derive(Clone)]
pub struct Lane {
    /// "Glow · AlphaTint": the slot as the Bindings pane names it.
    pub label: String,
    /// What it animates: the widget (index into its file's widgets) and
    /// its name, the lane's group.
    pub widget: Option<usize>,
    pub group: String,
    /// The property: "AlphaTint", "Position.x".
    pub prop: String,
    /// Its keys, with their spans for clips of the open file.
    pub keys: Vec<Key>,
}

/// One playable clip: where it lives and what it animates.
#[derive(Clone)]
pub struct ClipRef {
    pub file: Option<Guid>,
    pub index: usize,
    pub name: String,
    /// The file it lives in, for the list.
    pub owner: String,
    pub duration: f32,
    pub fps: u32,
    pub lanes: Vec<Lane>,
    /// Spans of its stored name and length in frames.
    pub name_span: Option<usize>,
    pub frames_span: Option<usize>,
}

/// Keys being dragged in time: where the drag began (window x) and by how
/// much they move, in seconds, snapped to frames.
#[derive(Clone, Copy, Debug)]
pub struct KeyDrag {
    pub start_x: f32,
    pub dt: f32,
    pub moved: bool,
    /// Alt was held: off the frames.
    pub free: bool,
}

#[derive(Default)]
pub struct Player {
    pub clips: Vec<ClipRef>,
    pub current: Option<usize>,
    pub time: f32,
    pub playing: bool,
    pub looped: bool,
    pub speed: f32,
    /// The key the editor row works on: (lane, key) of the current clip.
    pub key: Option<(usize, usize)>,
    /// Further selected keys, moved and deleted with `key`.
    pub also: Vec<(usize, usize)>,
    /// The time the lanes show: first second and seconds across; None
    /// fits the clip.
    pub view: Option<[f32; 2]>,
    /// The lanes' time area: window x of its left edge and its width.
    pub area: [f32; 2],
    pub drag: Option<KeyDrag>,
    /// The ruler is being dragged: the playhead follows.
    pub scrubbing: bool,
    /// Only lanes of the widget selected on the canvas.
    pub selection_only: bool,
    /// Folded lane groups, by name.
    pub folded: std::collections::HashSet<String>,
    /// Narrows the clip list.
    pub filter: String,
    /// Canvas moves and resizes key the current clip at the playhead
    /// instead of changing the stored values.
    pub recording: bool,
    /// The file the clips without one (`ClipRef::file` None) were collected
    /// from: the file edited then, to know a clip again across a step into
    /// or out of a template.
    pub clips_of: Option<Guid>,
}

/// What "+ Track" animates on a widget: built-in property id (named as
/// the lane it makes names it, `builtin::name`) and the track type shipped
/// clips use for it.
pub const ANIMATABLE: [(u32, TrackKind); 7] = [
    (builtin::ALPHA, TrackKind::Float),
    (builtin::VISIBLE, TrackKind::Bool),
    (builtin::POSITION, TrackKind::Vec3),
    (builtin::SCALE, TrackKind::Vec3),
    (builtin::ROTATION, TrackKind::Float),
    (builtin::SIZE, TrackKind::Vec2),
    (builtin::PIVOT, TrackKind::Vec3),
];

impl Player {
    pub fn play(&self) -> Option<Play> {
        let c = self.clips.get(self.current?)?;
        Some(Play {
            file: c.file,
            clip: c.index,
            time: self.time,
        })
    }
}

/// The clips of `asset`, with their tracks named. A track on a container's
/// template input is named by that template (`templates`), as Data rows
/// and graph pins are (round 6 showed a track as `131547d4`).
fn clip_refs(
    asset: &Asset,
    file: Option<Guid>,
    owner: &str,
    templates: &std::collections::HashMap<Guid, std::sync::Arc<Asset>>,
) -> Vec<ClipRef> {
    let widgets = asset.widgets();
    let slots = asset.bindings(&widgets).slots;
    let template = |w: usize| templates.get(&asset.container_template(widgets.get(w)?)?.id);
    let input = |w: usize, path: &[(u32, u32)]| {
        let [(_, uid)] = path else { return None };
        let t = template(w)?;
        let p = t.properties().iter().find(|p| p.uid == *uid)?;
        Some(t.property_name(p).to_owned())
    };
    asset
        .clips()
        .into_iter()
        .enumerate()
        .map(|(index, c)| ClipRef {
            file,
            index,
            name: if c.name.is_empty() {
                format!("clip {index}")
            } else {
                c.name.clone()
            },
            owner: owner.to_owned(),
            duration: c.duration(),
            fps: c.frame_rate.max(1),
            name_span: c.name_span,
            frames_span: c.frames_span,
            lanes: c
                .tracks
                .iter()
                .map(|t| {
                    // A track inside a nested template names the widgets it
                    // walks through, as the graph's pins do.
                    let steps = match slots.get(t.target) {
                        Some(Slot::Nested { widget, inner, .. }) => {
                            template(*widget).map_or_else(Vec::new, |at| {
                                super::bindings::nested_steps(at.clone(), inner, &mut |g| {
                                    templates.get(&g).cloned()
                                })
                            })
                        }
                        _ => Vec::new(),
                    };
                    let label = super::bindings::slot_label(
                        asset, &widgets, &slots, t.target, &input, &steps,
                    );
                    let widget = super::bindings::slot_widget(&slots, t.target);
                    let (group, prop) = match label.split_once(" · ") {
                        Some((g, p)) => (g.to_owned(), p.to_owned()),
                        None => (String::new(), label.clone()),
                    };
                    Lane {
                        label,
                        widget,
                        group,
                        prop,
                        keys: t.keys.clone(),
                    }
                })
                .collect(),
        })
        .collect()
}

impl UiEditor {
    /// Finds the clips of the open file and of every container template it
    /// expands. Call after the file's structure changes.
    pub(super) fn collect_clips(&mut self) {
        let install = self.install.clone();
        let tree =
            layout::Tree::build_with(&self.asset, &mut |g| self.images.asset(&install, g), false);
        let templates: std::collections::HashMap<Guid, std::sync::Arc<Asset>> = tree
            .templates
            .iter()
            .filter_map(|t| Some((t.guid()?, t.clone())))
            .collect();
        let mut clips = clip_refs(&self.asset, None, "this file", &templates);
        let mut files: Vec<super::preview::File> = super::preview::file(&self.asset, "this file")
            .into_iter()
            .collect();
        // A template is named after the container widgets that expand it.
        let mut owners: Vec<(Guid, String)> = Vec::new();
        for &(file, k) in &tree.source {
            let asset = if file == 0 {
                &self.asset
            } else {
                &*tree.templates[file - 1]
            };
            let widgets = asset.widgets();
            let Some(w) = widgets.get(k) else { continue };
            if let Some(r) = asset.container_template(w)
                && !owners.iter().any(|(g, _)| *g == r.id)
            {
                owners.push((r.id, w.name.clone().unwrap_or_default()));
            }
        }
        let mut seen = Vec::new();
        for t in &tree.templates {
            let Some(g) = t.guid() else { continue };
            if seen.contains(&g) {
                continue;
            }
            seen.push(g);
            let owner = owners
                .iter()
                .find(|(o, n)| *o == g && !n.is_empty())
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| g.to_string()[..8].to_owned());
            clips.extend(clip_refs(t, Some(g), &owner, &templates));
            files.extend(super::preview::file(t, &owner));
        }
        self.preview.files = files;
        // Keep the current clip when it still exists, by the file it lives
        // in: "Edit X here" turned its template's clips from X's into this
        // file's, and the clip being looked at closed (round 13).
        let (was, now) = (self.player.clips_of, self.asset.guid());
        let current = self
            .player
            .current
            .and_then(|i| self.player.clips.get(i))
            .and_then(|c| {
                clips
                    .iter()
                    .position(|n| n.file.or(now) == c.file.or(was) && n.index == c.index)
            });
        self.player.clips = clips;
        self.player.clips_of = now;
        self.player.current = current;
        if current.is_none() {
            self.player.playing = false;
        }
        // Selected keys are indices: one an undo took (a Record's track,
        // an added key) goes, or a move or an easing would index past the
        // clip's lanes.
        let lanes: Vec<usize> = self
            .clip()
            .map(|c| c.lanes.iter().map(|l| l.keys.len()).collect())
            .unwrap_or_default();
        let has = |&(l, k): &(usize, usize)| lanes.get(l).is_some_and(|&n| k < n);
        if !self.player.key.as_ref().is_some_and(has) {
            self.player.key = None;
        }
        self.player.also.retain(has);
    }

    fn select_clip(&mut self, i: usize) {
        self.player.current = Some(i);
        self.player.time = 0.;
        self.player.key = None;
        self.player.also.clear();
        self.player.view = None;
        self.relayout();
    }

    /// The last click was in the timeline.
    pub fn timeline_focused(&self) -> bool {
        self.keys == super::Keys::Timeline
    }

    pub(super) fn clip(&self) -> Option<&ClipRef> {
        self.player.current.and_then(|i| self.player.clips.get(i))
    }

    /// One frame of the current clip, in seconds.
    fn frame(&self) -> f32 {
        1. / self.clip().map_or(30, |c| c.fps) as f32
    }

    fn snap(&self, t: f32) -> f32 {
        let f = self.frame();
        (t / f).round() * f
    }

    /// The time the lanes show: first second and seconds across.
    pub(super) fn time_view(&self) -> [f32; 2] {
        self.player.view.unwrap_or_else(|| {
            let d = self.current_duration().max(0.1);
            // A margin either side, so first and last keys are not cut.
            [-d * 0.02, d * 1.04]
        })
    }

    /// Window x of time `t` in the lanes, and back.
    fn x_of(&self, t: f32) -> f32 {
        let [start, len] = self.time_view();
        self.player.area[0] + (t - start) / len * self.player.area[1].max(1.)
    }

    fn t_of(&self, x: f32) -> f32 {
        let [start, len] = self.time_view();
        start + (x - self.player.area[0]) / self.player.area[1].max(1.) * len
    }

    /// Zooms the lanes by `factor` around window x `at`.
    fn zoom_time(&mut self, factor: f32, at: f32) {
        let [_, len] = self.time_view();
        let t = self.t_of(at);
        let len = (len * factor).clamp(0.05, 600.);
        let f = (at - self.player.area[0]) / self.player.area[1].max(1.);
        self.player.view = Some([t - f * len, len]);
    }

    fn pan_time(&mut self, pixels: f32) {
        let [start, len] = self.time_view();
        let dt = pixels / self.player.area[1].max(1.) * len;
        self.player.view = Some([start + dt, len]);
    }

    /// Every selected key, (lane, key) of the current clip.
    pub(super) fn selected_keys(&self) -> Vec<(usize, usize)> {
        self.player
            .key
            .into_iter()
            .chain(self.player.also.iter().copied())
            .collect()
    }

    /// Selects key `k` of `lane`; with `add` it joins (or leaves) the
    /// selection instead of replacing it.
    fn select_key(&mut self, lane: usize, k: usize, add: bool) {
        let p = &mut self.player;
        if !add {
            p.key = Some((lane, k));
            p.also.clear();
            return;
        }
        if p.key == Some((lane, k)) {
            p.key = p.also.pop();
        } else if let Some(i) = p.also.iter().position(|&x| x == (lane, k)) {
            p.also.remove(i);
        } else {
            if let Some(old) = p.key {
                p.also.push(old);
            }
            p.key = Some((lane, k));
        }
    }

    /// How far the selected keys can move in time without passing a key
    /// they are not moved with (a track keeps its keys in order) or going
    /// before zero.
    fn move_limits(&self) -> (f32, f32) {
        let Some(c) = self.clip() else {
            return (0., 0.);
        };
        let sel = self.selected_keys();
        let f = self.frame();
        let (mut lo, mut hi) = (f32::MIN, f32::MAX);
        for &(l, k) in &sel {
            let keys = &c.lanes[l].keys;
            let t = keys[k].time;
            lo = lo.max(-t);
            if let Some(p) = (0..k).rev().find(|&i| !sel.contains(&(l, i))) {
                lo = lo.max(keys[p].time + f - t);
            }
            if let Some(n) = (k + 1..keys.len()).find(|&i| !sel.contains(&(l, i))) {
                hi = hi.min(keys[n].time - f - t);
            }
        }
        (lo.min(0.), hi.max(0.))
    }

    /// The editable keys of the selection, or why there are none.
    fn own_selection(&self) -> Result<(&ClipRef, Vec<(usize, usize)>)> {
        let clip = self.clip().context("No clip selected")?;
        if clip.file.is_some() {
            bail!(
                "This clip lives in the {} template; Edit … here, above the tracks, opens it for editing",
                clip.owner
            );
        }
        let sel = self.selected_keys();
        anyhow::ensure!(!sel.is_empty(), "Select a key first");
        Ok((clip, sel))
    }

    /// Moves the selected keys by `dt` seconds (kept in order) as one undo
    /// step; with `snap` each lands on a whole frame.
    pub fn move_keys(&mut self, dt: f32, snap: bool) -> Result<String> {
        let (lo, hi) = self.move_limits();
        let dt = dt.clamp(lo, hi);
        let frame = self.frame();
        let (clip, sel) = self.own_selection()?;
        if dt.abs() < 1e-6 {
            return Ok(String::new());
        }
        let edits: Vec<Edit> = sel
            .iter()
            .filter_map(|&(l, k)| {
                let key = &clip.lanes[l].keys[k];
                let t = key.time + dt;
                Some(Edit::Value {
                    span: key.time_span?,
                    value: Value::F32(if snap { (t / frame).round() * frame } else { t }),
                })
            })
            .collect();
        let n = edits.len();
        self.apply(&edits)?;
        self.collect_clips();
        Ok(format!(
            "Moved {} by {:+.0} ms",
            crate::project::plural(n, "key", "keys"),
            dt * 1000.
        ))
    }

    /// Deletes every selected key as one undo step.
    pub fn delete_keys(&mut self) -> Result<String> {
        let (clip, mut sel) = self.own_selection()?;
        let index = clip.index;
        // Last first, so the keys still to go keep their place.
        sel.sort_unstable_by(|a, b| b.cmp(a));
        let mut asset = self.asset.clone();
        for &(l, k) in &sel {
            let span = asset
                .clips()
                .get(index)
                .and_then(|c| c.tracks.get(l))
                .and_then(|t| t.keys.get(k))
                .map(|k| k.span)
                .context("the key is gone")?;
            asset = asset.remove_key(span)?;
        }
        self.player.also.clear();
        self.replace_keys(asset, None);
        Ok(format!(
            "Deleted {}",
            crate::project::plural(sel.len(), "key", "keys")
        ))
    }

    /// Sets the easing of every selected key (the segment after it).
    pub fn set_ease(&mut self, ease: u32) -> Result<String> {
        let (clip, sel) = self.own_selection()?;
        let edits: Vec<Edit> = sel
            .iter()
            .filter_map(|&(l, k)| {
                let span = clip.lanes[l].keys[k].ease_span?;
                let value = match &self.asset.spans()[span].record {
                    anno_formats::phx::Record::Value(Value::Enum { ty, .. }) => Value::Enum {
                        ty: *ty,
                        value: ease,
                    },
                    _ => Value::U32(ease),
                };
                Some(Edit::Value { span, value })
            })
            .collect();
        let n = edits.len();
        self.apply(&edits)?;
        self.collect_clips();
        Ok(format!(
            "{} on {}",
            ease_name(ease),
            crate::project::plural(n, "key", "keys")
        ))
    }

    /// Adds an empty clip to the open file, one second at 60 fps, and
    /// selects it.
    pub fn new_clip(&mut self) -> Result<String> {
        self.ensure_editable()?;
        let clips = self.asset.clips();
        let taken: Vec<&str> = clips.iter().map(|c| c.name.as_str()).collect();
        let name = (1..)
            .map(|i| format!("Clip_{i:02}"))
            .find(|n| !taken.contains(&n.as_str()))
            .expect("a free name");
        // The file's own frame rate (30 in many), one second long.
        let fps = clips
            .iter()
            .map(|c| c.frame_rate)
            .find(|&f| f > 0)
            .unwrap_or(60);
        let next = self.asset.add_clip(&name, fps, fps)?;
        self.checkpoint();
        self.asset = next;
        self.collect_clips();
        if let Some(i) = self
            .player
            .clips
            .iter()
            .position(|c| c.file.is_none() && c.name == name)
        {
            self.select_clip(i);
        }
        Ok(format!("Added clip {name}"))
    }

    /// The current value of built-in property `id` on this file's widget
    /// `node`, as a key of `kind` holds it.
    fn current_value(&self, node: usize, id: u32, kind: TrackKind) -> DefaultValue {
        let f = &self.frames[node];
        let v3 = |v: [f32; 3]| DefaultValue::Vec(v.map(f32::to_bits).to_vec());
        match (id, kind) {
            (builtin::POSITION, _) => v3(f.position),
            (builtin::PIVOT, _) => v3(f.pivot),
            (builtin::SCALE, _) => v3(f.scale),
            (builtin::ROTATION, _) => DefaultValue::F32(f.rotation.to_bits()),
            (builtin::SIZE, _) => {
                let s = f.bound.size.unwrap_or([f.local[2], f.local[3]]);
                DefaultValue::Vec(s.map(f32::to_bits).to_vec())
            }
            (builtin::VISIBLE, _) => DefaultValue::Bool(u8::from(self.nodes[node].visible)),
            _ => DefaultValue::F32(f.bound.alpha.unwrap_or(1.).to_bits()),
        }
    }

    /// Keys `value` on built-in property `id` of this file's widget `node`
    /// at the playhead of the current clip: on its track, or on a new
    /// track for it.
    fn key_property(
        &mut self,
        node: usize,
        id: u32,
        kind: TrackKind,
        value: DefaultValue,
    ) -> Result<String> {
        let clip = self.clip().context("Pick a clip to animate")?;
        if clip.file.is_some() {
            bail!(
                "This clip lives in the {} template; open that file to animate it",
                clip.owner
            );
        }
        anyhow::ensure!(
            !self.nodes[node].foreign,
            "Only this file's widgets can be animated here"
        );
        let (index, w) = (clip.index, self.nodes[node].widget);
        let path = vec![(1, 0x8000_0000 | id)];
        let time = self.snap(self.player.time).max(0.);
        let key = NewKey {
            time,
            ease: 1,
            value,
        };
        let widgets = self.asset.widgets();
        let slots = self.asset.bindings(&widgets).slots;
        let track = self.asset.clips()[index].tracks.iter().position(|t| {
            slots.get(t.target)
                == Some(&Slot::Widget {
                    widget: Some(w),
                    path: path.clone(),
                })
        });
        let next = match track {
            Some(t) => self.asset.set_key(index, t, &key)?,
            None => self.asset.add_track(
                &widgets,
                index,
                &NewSlot::Widget {
                    widget: w,
                    path: path.clone(),
                },
                kind,
                &[key],
            )?,
        };
        // The key written becomes the selection: the one picked before is
        // an index that may now name the new key or its neighbour (round 7:
        // Delete took a key nobody picked).
        let slots = next.bindings(&next.widgets()).slots;
        let written = next.clips()[index]
            .tracks
            .iter()
            .enumerate()
            .find(|(_, t)| {
                slots.get(t.target)
                    == Some(&Slot::Widget {
                        widget: Some(w),
                        path: path.clone(),
                    })
            })
            .and_then(|(lane, t)| {
                let k = t.keys.iter().position(|k| (k.time - time).abs() < 1e-4)?;
                Some((lane, k))
            });
        self.replace_asset(next);
        self.player.key = written;
        self.player.also.clear();
        let name = builtin::name(id).unwrap_or("value");
        Ok(format!("Keyed {name} at {time:.2} s"))
    }

    /// "+ Track": animates property `which` (of [`ANIMATABLE`]) of the
    /// selected widget, keyed at the playhead with its current value.
    pub fn add_track_for(&mut self, which: usize) -> Result<String> {
        let node = self
            .canvas
            .selected
            .filter(|&n| n > 0)
            .context("Select a widget on the canvas to animate")?;
        let (id, kind) = ANIMATABLE[which];
        let value = self.current_value(node, id, kind);
        self.key_property(node, id, kind, value)
    }

    /// In Record mode, a finished canvas move or resize of `node` becomes
    /// Position and Scale keys at the playhead. False when not recording.
    pub(super) fn record_change(&mut self, node: usize, edits: &[Edit]) -> bool {
        if !self.player.recording || self.clip().is_none_or(|c| c.file.is_some()) {
            return false;
        }
        let f = self.frames[node];
        let mut done = Vec::new();
        for e in edits {
            let Edit::Value {
                span,
                value: Value::Vec3(v),
            } = e
            else {
                continue;
            };
            let id = if Some(*span) == f.position_span {
                builtin::POSITION
            } else if Some(*span) == f.scale_span {
                builtin::SCALE
            } else {
                continue;
            };
            let value = DefaultValue::Vec(v.map(f32::to_bits).to_vec());
            match self.key_property(node, id, TrackKind::Vec3, value) {
                Ok(m) => done.push(m),
                Err(e) => {
                    self.message = Some((format!("Not keyed: {e:#}"), true));
                    return true;
                }
            }
        }
        // Refused, not dropped: the widget snaps back, so say why.
        self.message = Some(if done.is_empty() {
            (
                "Not keyed: Record keys Position and Scale; turn Record off to change the stored value".into(),
                true,
            )
        } else {
            (done.join("; "), false)
        });
        true
    }

    /// Moves the playhead `n` frames.
    fn step_frames(&mut self, n: i32) {
        let t = self.snap(self.player.time) + n as f32 * self.frame();
        self.player.playing = false;
        self.seek(t);
    }

    /// Moves the playhead to the next (or previous) key of any lane.
    fn step_key(&mut self, forward: bool) {
        let Some(c) = self.clip() else { return };
        let now = self.player.time;
        let times = c.lanes.iter().flat_map(|l| l.keys.iter().map(|k| k.time));
        let next = if forward {
            times
                .filter(|&t| t > now + 1e-4)
                .fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.min(t))))
        } else {
            times
                .filter(|&t| t < now - 1e-4)
                .fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.max(t))))
        };
        if let Some(t) = next {
            self.player.playing = false;
            self.seek(t);
        }
    }

    /// The selected key, when the current clip is the open file's own.
    fn own_key(&self) -> Result<&Key> {
        let p = &self.player;
        let clip = p
            .current
            .and_then(|i| p.clips.get(i))
            .context("No clip selected")?;
        if clip.file.is_some() {
            bail!(
                "This clip lives in the {} template; Edit … here, above the tracks, opens it for editing",
                clip.owner
            );
        }
        let (lane, key) = p.key.context("Select a key first")?;
        clip.lanes
            .get(lane)
            .and_then(|l| l.keys.get(key))
            .context("The key is gone")
    }

    /// Replaces the asset after a structural key edit, as one undo step.
    fn replace_keys(&mut self, next: anno_formats::phx::Asset, select: Option<(usize, usize)>) {
        self.checkpoint();
        self.asset = next;
        self.collect_clips();
        self.player.key = select;
        self.relayout();
    }

    /// Adds a key at the playhead on the selected key's track, copying the
    /// key just before it (or the first, when the playhead is earlier) so
    /// the track stays in time order.
    pub fn add_key(&mut self) -> Result<String> {
        self.own_key()?;
        let (lane, _) = self.player.key.expect("own_key checked");
        self.add_key_at(lane, self.player.time)
    }

    /// Adds a key to `lane` at `t` seconds, copying the key before it.
    pub fn add_key_at(&mut self, lane: usize, t: f32) -> Result<String> {
        let clip = self.clip().context("No clip selected")?;
        if clip.file.is_some() {
            bail!(
                "This clip lives in the {} template; Edit … here, above the tracks, opens it for editing",
                clip.owner
            );
        }
        let keys = &clip.lanes.get(lane).context("no such track")?.keys;
        anyhow::ensure!(!keys.is_empty(), "the track has no key to copy");
        let before = keys.iter().rposition(|k| k.time <= t + 1e-4);
        if before.is_some_and(|i| (keys[i].time - t).abs() < 1e-4) {
            bail!("There is already a key at {t:.2} s");
        }
        let source = before.unwrap_or(0);
        // The track's value there, so adding a key never changes the
        // motion (a copy of the key before would flatten a fade).
        let value = anno_formats::phx::animation::sample_track(keys, t);
        let more = self.asset.duplicate_key(keys[source].span)?;
        // The copy sits right after its source. Before the first key, the
        // source moves to the playhead instead and the copy keeps its time.
        let moved = match before {
            Some(i) => i + 1,
            None => 0,
        };
        let span = more
            .clips()
            .get(clip.index)
            .and_then(|c| c.tracks.get(lane))
            .and_then(|tr| tr.keys.get(moved))
            .and_then(|k| k.time_span)
            .context("the copied key has no Time")?;
        let mut next = more
            .apply(&Edit::Value {
                span,
                value: Value::F32(t),
            })?
            .asset;
        // Kinds `set_key` cannot write yet (colour) keep the copied value.
        if let Some(Ok(sampled)) = value.map(|value| {
            next.set_key(
                clip.index,
                lane,
                &NewKey {
                    time: t,
                    ease: 0,
                    value,
                },
            )
        }) {
            next = sampled;
        }
        self.player.also.clear();
        self.replace_keys(next, Some((lane, moved)));
        Ok(format!("Added a key at {t:.2} s"))
    }

    fn seek(&mut self, t: f32) {
        let d = self.current_duration();
        self.player.time = t.clamp(0., d);
        self.relayout();
    }

    fn current_duration(&self) -> f32 {
        self.player
            .current
            .and_then(|i| self.player.clips.get(i))
            .map_or(0., |c| c.duration)
    }

    /// Advances playback by `dt` seconds; false when it stopped.
    fn advance(&mut self, dt: f32) -> bool {
        if !self.player.playing {
            return false;
        }
        let d = self.current_duration();
        let mut t = self.player.time + dt * self.player.speed.max(0.05);
        if t >= d {
            if self.player.looped && d > 0. {
                t %= d;
            } else {
                t = d;
                self.player.playing = false;
            }
        }
        self.player.time = t;
        // A zoomed view pages along with the playhead, as it leaves it.
        if let Some([start, len]) = self.player.view
            && (t < start || t > start + len)
        {
            self.player.view = Some([t - len * 0.05, len]);
        }
        self.relayout();
        self.player.playing
    }
}

impl Editor {
    /// Drives playback while a clip plays.
    fn start_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ui_ticking {
            return;
        }
        self.ui_ticking = true;
        cx.spawn(async move |this, cx| {
            let mut last = std::time::Instant::now();
            loop {
                cx.background_executor().timer(FRAME).await;
                let now = std::time::Instant::now();
                let dt = (now - last).as_secs_f32();
                last = now;
                let more = this
                    .update(cx, |this, cx| {
                        let more = this.ui.as_mut().is_some_and(|u| u.advance(dt));
                        if more {
                            // A frame moves the canvas and the playhead only:
                            // redrawing every pane (graph, inspector, file
                            // list) each frame cost 10 ms on a big screen.
                            this.notify_pane(Pane::UiCanvas, cx);
                            this.notify_pane(Pane::UiTimeline, cx);
                        } else {
                            // Stopped: the rest catches up with the frame
                            // it stopped on (values, Evaluated).
                            this.ui_ticking = false;
                            cx.notify();
                        }
                        more
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
        })
        .detach();
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = &mut self.ui else { return };
        if ui.player.current.is_none() {
            if ui.player.clips.is_empty() {
                return;
            }
            ui.select_clip(0);
        }
        if ui.player.speed == 0. {
            ui.player.speed = 1.;
        }
        // Playing from the end starts over.
        if !ui.player.playing && ui.player.time >= ui.current_duration() - 1e-4 {
            ui.player.time = 0.;
        }
        ui.player.playing = !ui.player.playing;
        if ui.player.playing {
            self.start_ticking(cx);
        }
    }

    /// Timeline keys: Space plays or pauses; after a click in the
    /// timeline, `,` and `.` step a frame (Shift: to the previous or next
    /// key), Delete removes the selected keys and Home/End jump.
    pub fn timeline_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(ui) = &mut self.ui else {
            return false;
        };
        if event.keystroke.key == "space" && !ui.player.clips.is_empty() {
            self.toggle_play(cx);
            return true;
        }
        ui.timeline_key_down(event)
    }
}

impl UiEditor {
    /// Keys after a click in the timeline (Space is the editor's: it plays
    /// wherever the keys are). Delete, arrows and chords stop here even
    /// when they do nothing: the canvas behind would act on the selected
    /// widget (round 7: Delete with no key selected deleted it). Only the
    /// editor's own shortcuts go on (`editor_shortcut`).
    pub(super) fn timeline_key_down(&mut self, event: &KeyDownEvent) -> bool {
        if self.keys != super::Keys::Timeline {
            return false;
        }
        let m = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        if super::editor_shortcut(&event.keystroke) {
            return false;
        }
        // Every other chord stops here (round 8: Ctrl+Delete went on to
        // the tree and deleted the selected widget).
        if m.control || m.platform || m.alt {
            return true;
        }
        match key {
            "escape" => self.hand_back = true,
            "delete" | "backspace" if self.player.clips.is_empty() => {
                self.message = Some(("This file has no clips yet: + Clip adds one".into(), false));
            }
            "delete" | "backspace" if self.selected_keys().is_empty() => {
                self.message = Some(("Select a key first".into(), false));
            }
            "delete" | "backspace" => {
                let r = self.delete_keys();
                self.message = Some(match r {
                    Ok(m) => (m, false),
                    Err(e) => (format!("Not deleted: {e:#}"), true),
                });
            }
            // Arrows step frames: on the canvas they move the widget. With
            // Shift, and < and > (what Linux reports for Shift , and .), to
            // the previous or next key.
            _ if self.player.clips.is_empty() => {}
            "left" if m.shift => self.step_key(false),
            "right" if m.shift => self.step_key(true),
            "left" => self.step_frames(-1),
            "right" => self.step_frames(1),
            "<" => self.step_key(false),
            ">" => self.step_key(true),
            "," if m.shift => self.step_key(false),
            "." if m.shift => self.step_key(true),
            "," => self.step_frames(-1),
            "." => self.step_frames(1),
            "home" => self.seek(0.),
            "end" => {
                let d = self.current_duration();
                self.seek(d);
            }
            "k" => {
                let n = self.select_keys_at_playhead(m.shift);
                self.message = Some(match n {
                    0 => ("No key at the playhead".into(), false),
                    n => (
                        format!("Selected {}", crate::project::plural(n, "key", "keys")),
                        false,
                    ),
                });
            }
            // Every other plain key stops here too (round 9: F framed the
            // canvas behind); Space plays before this.
            _ => {}
        }
        true
    }
}

/// + Clip: an empty one-second clip in the file being edited.
fn clip_button(cx: &mut Context<Editor>) -> AnyElement {
    button("+ Clip", false)
        .tooltip(Tooltip::text(
            "New clip",
            "",
            "Add an empty one-second clip to this file",
        ))
        .on_click(cx.listener(|this, _, _, cx| {
            if let Some(ui) = &mut this.ui {
                let r = ui.new_clip();
                ui.message = Some(match r {
                    Ok(m) => (m, false),
                    Err(e) => (format!("Not added: {e:#}"), true),
                });
            }
            cx.notify();
        }))
        .into_any_element()
}

impl Editor {
    pub fn render_ui_timeline(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(ui) = &self.ui else {
            return crate::editor::pane_note("Open a screen to see its animations.");
        };
        let p = &ui.player;
        if p.clips.is_empty() {
            const NONE: &str = "This file and its templates have no timeline animations";
            if !ui.editable() {
                return empty_pane(NONE);
            }
            // The first clip starts here: + Clip lives in the transport
            // bar, which needs a clip (round 8: templates without
            // animations could never get one).
            return gpui_editor_kit::widgets::empty_state(NONE)
                .child(div().pt_2().child(clip_button(cx)))
                .into_any_element();
        }
        let entity = cx.entity();
        let duration = ui.current_duration();
        let [start, len] = ui.time_view();
        let time = p.time;
        let sel = ui.selected_keys();
        let drag_dt = p.drag.map_or(0., |d| d.dt);
        let clip = ui.clip().cloned();
        let fps = clip.as_ref().map_or(30, |c| c.fps);

        // The clip list: this file's first, then each template's, with a
        // filter for screens that carry hundreds.
        let needle = p.filter.to_lowercase();
        let mut list = div().flex().flex_col().gap(px(1.));
        let mut owner = None;
        for (i, c) in p.clips.iter().enumerate() {
            if !needle.is_empty()
                && !c.name.to_lowercase().contains(&needle)
                && !c.owner.to_lowercase().contains(&needle)
            {
                continue;
            }
            if owner != Some(&c.owner) {
                owner = Some(&c.owner);
                list = list.child(
                    div()
                        .pt_1()
                        .px_2()
                        .text_size(px(10.))
                        .text_color(theme::get().muted)
                        .truncate()
                        .child(if c.file.is_none() {
                            "This file".to_owned()
                        } else {
                            format!("{} · template", c.owner)
                        }),
                );
            }
            let on = p.current == Some(i);
            list = list.child(
                div()
                    .id(("timeline-clip", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(22.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .when(on, |d| d.bg(theme::get().selected))
                    .when(!on, |d| d.hover(|s| s.bg(theme::get().hover)))
                    .child(div().flex_1().min_w_0().truncate().child(c.name.clone()))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .text_color(theme::get().muted)
                            .child(format!("{:.2} s", c.duration)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.keys_to(super::Keys::Timeline);
                        if let Some(ui) = &mut this.ui {
                            ui.select_clip(i);
                        }
                        cx.notify();
                    })),
            );
        }
        let filter_edit = ui
            .editing
            .as_ref()
            .filter(|e| e.field == CLIP_FILTER)
            .map(|e| e.display());
        let filter = p.filter.clone();
        let clip_column = div()
            .w(px(200.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .border_r_1()
            .border_color(theme::get().border)
            .pr_1()
            .child(
                text_field("ui-clip-filter", &p.filter, filter_edit, "Filter clips").on_click(
                    cx.listener(move |this, _, window, cx| {
                        let typing = |ui: &UiEditor| {
                            ui.editing.as_ref().is_some_and(|e| e.field == CLIP_FILTER)
                        };
                        if this.ui.as_ref().is_some_and(typing) {
                            return;
                        }
                        this.focus.focus(window, cx);
                        this.keys_to(super::Keys::Timeline);
                        if let Some(ui) = &mut this.ui {
                            ui.editing =
                                Some(TextEdit::new(CLIP_FILTER, filter.clone(), Filter::Text));
                        }
                        cx.notify();
                    }),
                ),
            )
            .child(
                gpui_editor_kit::widgets::vscroll(
                    "ui-timeline-clips",
                    &self.scrolls.get("ui-timeline-clips"),
                    list,
                )
                .flex_1()
                .min_h_0(),
            );

        // Transport: start, previous key, play, next key, end, loop,
        // speed, the time in seconds and frames, and the view.
        let speed = if p.speed == 0. { 1. } else { p.speed };
        let tbutton =
            |label: &'static str, tip: &'static str, key: &'static str, f: fn(&mut UiEditor)| {
                button(label, false)
                    .tooltip(Tooltip::text(tip, key, ""))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.keys_to(super::Keys::Timeline);
                        if let Some(ui) = &mut this.ui {
                            f(ui);
                        }
                        cx.notify();
                    }))
            };
        let transport = div()
            .flex()
            .items_center()
            .gap_1()
            .text_size(px(11.))
            .child(tbutton("⏮", "To the start", "Home", |ui| ui.seek(0.)))
            .child(tbutton("◆◀", "Previous key", "Shift ,", |ui| {
                ui.step_key(false)
            }))
            .child(tbutton("◀", "Previous frame", ",", |ui| {
                ui.step_frames(-1)
            }))
            .child(
                button(if p.playing { "⏸" } else { "▶" }, p.playing)
                    .tooltip(Tooltip::text("Play or pause", "Space", ""))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_play(cx);
                        cx.notify();
                    })),
            )
            .child(tbutton("▶|", "Next frame", ".", |ui| ui.step_frames(1)))
            .child(tbutton("▶◆", "Next key", "Shift .", |ui| {
                ui.step_key(true)
            }))
            .child(tbutton("⏭", "To the end", "End", |ui| {
                let d = ui.current_duration();
                ui.seek(d);
            }))
            .child(
                button("Loop", p.looped).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.player.looped = !ui.player.looped;
                    }
                    cx.notify();
                })),
            )
            .child(
                button(format!("{speed}×"), false)
                    .tooltip(Tooltip::text("Speed", "", "Playback speed"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            let s = if ui.player.speed == 0. {
                                1.
                            } else {
                                ui.player.speed
                            };
                            let i = SPEEDS.iter().position(|&x| x >= s - 1e-3).unwrap_or(2);
                            ui.player.speed = SPEEDS[(i + 1) % SPEEDS.len()];
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .px_2()
                    .text_color(theme::get().text)
                    .child(match &clip {
                        Some(c) => format!(
                            "{} · {:.2} s · frame {} / {} · {} fps",
                            c.name,
                            time,
                            (time * fps as f32).round() as i32,
                            (duration * fps as f32).round() as i32,
                            fps
                        ),
                        None => "Pick a clip".to_owned(),
                    }),
            )
            .child(div().flex_1())
            .children(self.authoring_bar(window, cx))
            .child(
                button("Selection only", p.selection_only)
                    .tooltip(Tooltip::text(
                        "Selection only",
                        "",
                        "Show only the tracks of the widget selected on the canvas",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.player.selection_only = !ui.player.selection_only;
                        }
                        cx.notify();
                    })),
            )
            .child(
                button("Fit", false)
                    .tooltip(Tooltip::text(
                        "Fit the clip",
                        "",
                        "Ctrl wheel zooms, Shift wheel pans",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.player.view = None;
                        }
                        cx.notify();
                    })),
            )
            // Stop is a transport button: the clip stays open, back at
            // its start. Close is what shows the file as stored again.
            .child(
                button("Stop", false)
                    .tooltip(Tooltip::text(
                        "Stop",
                        "",
                        "Stop playing and go back to the start; the clip stays open",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.player.playing = false;
                            ui.seek(0.);
                        }
                        cx.notify();
                    })),
            )
            .child(
                button("Close", false)
                    .tooltip(Tooltip::text(
                        "Close",
                        "",
                        "Close the clip: the file shows as stored again",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.player.playing = false;
                            ui.player.current = None;
                            ui.player.time = 0.;
                            ui.relayout();
                        }
                        cx.notify();
                    })),
            );

        // The ruler: labelled ticks at a step that leaves room for the
        // labels, frames when zoomed in far enough. Dragging it scrubs.
        let width = p.area[1].max(1.);
        let pps = width / len;
        let step = [
            1. / fps as f32,
            2. / fps as f32,
            5. / fps as f32,
            0.1,
            0.25,
            0.5,
            1.,
            2.,
            5.,
            10.,
            30.,
        ]
        .into_iter()
        .find(|s| s * pps >= 64.)
        .unwrap_or(60.);
        let mut ruler_labels = div().absolute().size_full();
        let first = (start / step).floor() as i64;
        for i in first..=((start + len) / step).ceil() as i64 {
            let t = i as f32 * step;
            if t < -1e-4 {
                continue;
            }
            let x = (t - start) * pps;
            ruler_labels = ruler_labels.child(
                div()
                    .absolute()
                    .left(px(x + 3.))
                    .top(px(1.))
                    .text_size(px(9.))
                    .text_color(theme::get().muted)
                    .child(if step < 0.1 {
                        format!("{}f", (t * fps as f32).round() as i64)
                    } else {
                        format!("{t:.2}s")
                    }),
            );
        }
        let bounds_entity = entity.clone();
        let ruler = div()
            .id("ui-timeline-ruler")
            .relative()
            .flex_1()
            .h(px(RULER_H))
            .bg(theme::get().input)
            .cursor(CursorStyle::ResizeLeftRight)
            .child(
                canvas(
                    move |bounds, _, cx| {
                        bounds_entity.update(cx, |this, _| {
                            if let Some(ui) = &mut this.ui {
                                ui.player.area =
                                    [f32::from(bounds.origin.x), f32::from(bounds.size.width)];
                            }
                        });
                    },
                    move |bounds, _, window, _| {
                        let pps = f32::from(bounds.size.width) / len;
                        let x0 = f32::from(bounds.origin.x);
                        let y1 = bounds.origin.y + bounds.size.height;
                        // Minor ticks a fifth of a step apart, or every frame.
                        let minor = if step <= 5. / fps as f32 {
                            1. / fps as f32
                        } else {
                            step / 5.
                        };
                        if minor * pps >= 4. {
                            let first = (start / minor).floor() as i64;
                            for i in first..=((start + len) / minor).ceil() as i64 {
                                let t = i as f32 * minor;
                                let x = px(x0 + (t - start) * pps);
                                let major = ((t / step).round() * step - t).abs() < minor * 0.25;
                                let h = if major { 10. } else { 4. };
                                window.paint_quad(fill(
                                    Bounds::new(point(x, y1 - px(h)), size(px(1.), px(h))),
                                    theme::get().muted,
                                ));
                            }
                        }
                        // The clip's end, and the playhead.
                        let end = px(x0 + (duration - start) * pps);
                        window.paint_quad(fill(
                            Bounds::new(
                                point(end, bounds.origin.y),
                                size(px(1.), bounds.size.height),
                            ),
                            theme::get().border,
                        ));
                        let x = px(x0 + (time - start) * pps);
                        gpui_editor_kit::paint::polygon(
                            &[
                                point(x - px(5.), bounds.origin.y),
                                point(x + px(5.), bounds.origin.y),
                                point(x + px(5.), bounds.origin.y + px(8.)),
                                point(x, bounds.origin.y + px(13.)),
                                point(x - px(5.), bounds.origin.y + px(8.)),
                            ],
                            crate::colors::get().layout.link,
                            window,
                        );
                        window.paint_quad(fill(
                            Bounds::new(
                                point(x, bounds.origin.y),
                                size(px(1.), bounds.size.height),
                            ),
                            crate::colors::get().layout.link,
                        ));
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(ruler_labels)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    this.keys_to(super::Keys::Timeline);
                    if let Some(ui) = &mut this.ui {
                        ui.player.playing = false;
                        ui.player.scrubbing = true;
                        let t = ui.snap(ui.t_of(f32::from(e.position.x)));
                        ui.seek(t);
                    }
                    cx.notify();
                }),
            );

        // Lanes, grouped by the widget they animate; a group's header row
        // shows all its keys. Selection only keeps the canvas selection's.
        let only = p.selection_only.then(|| {
            ui.canvas.selected.and_then(|n| {
                let node = ui.nodes.get(n)?;
                let file = if node.foreign {
                    Some(node.owners.first()?.0)
                } else {
                    None
                };
                Some((file, node.widget))
            })
        });
        let mut rows = div().flex().flex_col();
        if let Some(c) = &clip {
            let mut groups: Vec<(String, Option<usize>, Vec<usize>)> = Vec::new();
            for (li, lane) in c.lanes.iter().enumerate() {
                if let Some(want) = &only
                    && *want != Some((c.file, lane.widget.unwrap_or(usize::MAX)))
                {
                    continue;
                }
                match groups.iter_mut().find(|g| g.0 == lane.group) {
                    Some(g) => g.2.push(li),
                    None => groups.push((lane.group.clone(), lane.widget, vec![li])),
                }
            }
            if groups.is_empty() {
                rows = rows.child(
                    div()
                        .p_2()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        // Says what to do next, not only what is missing.
                        .child(if only.is_some() {
                            "This clip does not animate the selected widget: + Track keys one of its properties"
                        } else {
                            "This clip has no tracks yet: select a widget, then + Track (or turn on Record and move it)"
                        }),
                );
            }
            let file = c.file;
            for (gi, (group, widget, lanes)) in groups.into_iter().enumerate() {
                let folded = p.folded.contains(&group);
                // Every key of the group, for its summary row.
                let summary: Vec<(f32, u32, bool)> = lanes
                    .iter()
                    .flat_map(|&l| {
                        let sel = &sel;
                        c.lanes[l].keys.iter().enumerate().map(move |(k, key)| {
                            let on = sel.contains(&(l, k));
                            (key.time + if on { drag_dt } else { 0. }, 0, on)
                        })
                    })
                    .collect();
                let fold_group = group.clone();
                let header = div()
                    .id(("ui-tl-group", gi))
                    .w(px(HEAD_W))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .hover(|s| s.bg(theme::get().hover))
                    // The triangle folds the group; the rest of the row
                    // selects what it animates.
                    .child(
                        div()
                            .id(("ui-tl-fold", gi))
                            .w(px(14.))
                            .text_color(theme::get().muted)
                            .hover(|s| s.text_color(theme::get().text))
                            .child(if folded { "▸" } else { "▾" })
                            .on_click({
                                let fold_group = fold_group.clone();
                                cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.keys_to(super::Keys::Timeline);
                                    if let Some(ui) = &mut this.ui
                                        && !ui.player.folded.remove(&fold_group)
                                    {
                                        ui.player.folded.insert(fold_group.clone());
                                    }
                                    cx.notify();
                                })
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(if group.is_empty() {
                                "(clip)".to_owned()
                            } else {
                                group.clone()
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme::get().muted)
                            .child(lanes.len().to_string()),
                    )
                    .tooltip(Tooltip::text(
                        group.clone(),
                        "",
                        "Click to select and frame it on the canvas; the triangle folds its tracks",
                    ))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.keys_to(super::Keys::Timeline);
                        if let Some(ui) = &mut this.ui
                            && let Some(w) = widget
                        {
                            let m = ui.select_animated(file, w);
                            ui.message = Some((m, false));
                        }
                        cx.notify();
                    }));
                rows = rows.child(
                    div()
                        .flex()
                        .h(px(ROW_H))
                        .border_b_1()
                        .border_color(theme::get().border)
                        .bg(theme::get().input)
                        .child(header)
                        .child(lane_canvas(summary, start, len, time, duration, true)),
                );
                if folded {
                    continue;
                }
                for li in lanes {
                    let lane = &c.lanes[li];
                    let keys: Vec<(f32, u32, bool)> = lane
                        .keys
                        .iter()
                        .enumerate()
                        .map(|(k, key)| {
                            let on = sel.contains(&(li, k));
                            (key.time + if on { drag_dt } else { 0. }, key.ease, on)
                        })
                        .collect();
                    let has_sel = sel.iter().any(|s| s.0 == li);
                    rows = rows.child(
                        div()
                            .flex()
                            .h(px(ROW_H))
                            .border_b_1()
                            .border_color(theme::get().border)
                            .child(
                                div()
                                    .w(px(HEAD_W))
                                    .flex_none()
                                    .pl(px(20.))
                                    .pr_1()
                                    .flex()
                                    .items_center()
                                    .text_size(px(10.))
                                    .text_color(if has_sel {
                                        theme::get().text
                                    } else {
                                        theme::get().muted
                                    })
                                    .truncate()
                                    .child(lane.prop.clone()),
                            )
                            .child(
                                div()
                                    .id(("ui-tl-lane", li))
                                    .flex_1()
                                    .h_full()
                                    .child(lane_canvas(keys, start, len, time, duration, false))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                            this.focus.focus(window, cx);
                                            this.keys_to(super::Keys::Timeline);
                                            if let Some(ui) = &mut this.ui {
                                                ui.lane_press(li, e);
                                            }
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    );
                }
            }
        } else {
            rows = rows.child(
                div()
                    .p_2()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("Pick a clip on the left to see its tracks."),
            );
        }
        let key_bar = self.key_bar(window, cx);
        div()
            .id("ui-timeline")
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(theme::get().panel)
            .child(transport)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap_2()
                    .child(clip_column)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .child(
                                        div()
                                            .w(px(HEAD_W))
                                            .flex_none()
                                            .text_size(px(10.))
                                            .text_color(theme::get().muted)
                                            .px_1()
                                            .child(format!("{:.2} s across", len)),
                                    )
                                    .child(ruler),
                            )
                            .child(
                                gpui_editor_kit::widgets::vscroll(
                                    "ui-timeline-lanes",
                                    &self.scrolls.get("ui-timeline-lanes"),
                                    rows,
                                )
                                .flex_1()
                                .min_h_0(),
                            )
                            .child(key_bar),
                    ),
            )
            .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
                let Some(ui) = &mut this.ui else { return };
                let m = e.modifiers;
                if !(m.control || m.platform || m.shift) {
                    return;
                }
                let d = match e.delta {
                    ScrollDelta::Pixels(p) => {
                        f32::from(if m.shift && p.y == px(0.) { p.x } else { p.y })
                    }
                    ScrollDelta::Lines(p) => (if m.shift && p.y == 0. { p.x } else { p.y }) * 32.,
                };
                if m.shift {
                    ui.pan_time(-d);
                } else {
                    ui.zoom_time((-d * 0.004).exp(), f32::from(e.position.x));
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                let Some(ui) = &mut this.ui else { return };
                let x = f32::from(e.position.x);
                if e.pressed_button != Some(MouseButton::Left) {
                    // A press that ended outside the pane.
                    if ui.player.scrubbing || ui.player.drag.is_some() {
                        ui.timeline_release();
                        cx.notify();
                    }
                    return;
                }
                if ui.player.scrubbing {
                    let t = ui.snap(ui.t_of(x));
                    ui.seek(t);
                    cx.notify();
                } else if let Some(d) = ui.player.drag {
                    let raw = (x - d.start_x) / ui.player.area[1].max(1.) * ui.time_view()[1];
                    let (lo, hi) = ui.move_limits();
                    let dt = if e.modifiers.alt { raw } else { ui.snap(raw) }.clamp(lo, hi);
                    let moved = d.moved || (x - d.start_x).abs() >= 3.;
                    ui.player.drag = Some(KeyDrag {
                        dt: if moved { dt } else { 0. },
                        moved,
                        free: e.modifiers.alt,
                        ..d
                    });
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.timeline_release();
                    }
                    cx.notify();
                }),
            )
            .into_any_element()
    }
}

impl Editor {
    /// The transport's authoring controls: add a clip, the current clip's
    /// name and length, a track for the selected widget, and Record.
    fn authoring_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(ui) = &self.ui else {
            return Vec::new();
        };
        let mut out = vec![clip_button(cx)];
        // A template's clip is edited in that template: the way in, as the
        // Data section offers it.
        if let Some(c) = ui.clip()
            && let Some(g) = c.file
        {
            let at = ui
                .nodes
                .iter()
                .flat_map(|n| n.owners.iter())
                .find(|o| o.0 == g)
                .map(|o| o.1);
            if let Some(key) = at {
                out.push(
                    button(format!("Edit {} here", ui.template_name(g)), false)
                        .tooltip(Tooltip::text(
                            "Edit here",
                            "",
                            "This clip lives in a template: edit that template in place to change it",
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                let r = ui.enter_template(g, key);
                                ui.message = Some(match r {
                                    Ok(m) => (m, false),
                                    Err(e) => (format!("{e:#}"), true),
                                });
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            return out;
        }
        let Some(clip) = ui.clip() else {
            return out;
        };
        let editing = ui.editing.as_ref().map(|e| (e.field, e.display()));
        let field = |id: &'static str, span: Option<usize>, width: f32, cx: &mut Context<Self>| {
            let span = span?;
            let text = ui.edit_text(span)?;
            let shown = editing
                .as_ref()
                .filter(|e| e.0 == span)
                .map(|e| e.1.clone());
            Some(
                text_field((id, span), &text, shown, "")
                    .w(px(width))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_span_edit(span, super::Keys::Timeline, Filter::Text, window, cx)
                    }))
                    .into_any_element(),
            )
        };
        out.extend(field("ui-clip-name", clip.name_span, 110., cx));
        if let Some(f) = field("ui-clip-frames", clip.frames_span, 44., cx) {
            out.push(f);
            out.push(
                div()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child("frames")
                    .into_any_element(),
            );
        }
        let can_track = ui
            .canvas
            .selected
            .is_some_and(|n| n > 0 && !ui.nodes[n].foreign);
        if can_track {
            let options: Vec<(Option<usize>, SharedString)> =
                std::iter::once((None, "+ Track".into()))
                    .chain(ANIMATABLE.iter().enumerate().map(|(i, a)| {
                        (
                            Some(i),
                            SharedString::from(builtin::name(a.0).unwrap_or("?")),
                        )
                    }))
                    .collect();
            out.push(
                gpui_editor_kit::widgets::dropdown(
                    "ui-add-track",
                    options,
                    &None,
                    window,
                    cx,
                    |this, which, _, cx| {
                        if let (Some(ui), Some(which)) = (&mut this.ui, which) {
                            let r = ui.add_track_for(which);
                            ui.message = Some(match r {
                                Ok(m) => (m, false),
                                Err(e) => (format!("Not added: {e:#}"), true),
                            });
                        }
                        cx.notify();
                    },
                )
                .w(px(96.))
                .into_any_element(),
            );
        } else {
            // There but greyed, with why: a control that vanishes reads as
            // a feature that does not exist.
            let why = match ui.canvas.selected.filter(|&n| n > 0) {
                None => "Select a widget of this file to animate it",
                Some(_) => {
                    "That widget is part of a template: edit the template here to animate it"
                }
            };
            out.push(
                div()
                    .id("ui-add-track-off")
                    .px_2()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .text_color(theme::get().muted)
                    .child("+ Track")
                    .tooltip(Tooltip::text("+ Track", "", why))
                    .into_any_element(),
            );
        }
        let rec = ui.player.recording;
        out.push(
            div()
                .id("ui-record")
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .h(px(22.))
                .rounded(px(3.))
                .cursor_pointer()
                .when(rec, |d| d.bg(crate::colors::get().recording))
                .when(!rec, |d| d.hover(|s| s.bg(theme::get().hover)))
                .child(div().size(px(8.)).rounded(px(4.)).bg(if rec { crate::colors::get().recording_dot } else { theme::get().muted }))
                .child("Record")
                .tooltip(Tooltip::text(
                    "Record",
                    "",
                    "Moving or resizing a widget on the canvas keys its Position or Scale at the playhead instead of changing the stored value",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(ui) = &mut this.ui {
                        ui.player.recording = !ui.player.recording;
                    }
                    cx.notify();
                }))
                .into_any_element(),
        );
        out
    }
}

const HEAD_W: f32 = 200.;
const RULER_H: f32 = 24.;
const ROW_H: f32 = 20.;
/// The clip filter's field key among the inspector's edits.
pub(super) const CLIP_FILTER: usize = usize::MAX - 1;

/// One lane's time row: segments between keys (a bar where the value
/// eases, a thin line where it holds), keys as diamonds (selected ones
/// lit), the clip's end and the playhead. `summary` rows draw small dots.
fn lane_canvas(
    keys: Vec<(f32, u32, bool)>,
    start: f32,
    len: f32,
    time: f32,
    duration: f32,
    summary: bool,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let pps = f32::from(bounds.size.width) / len;
            let x0 = f32::from(bounds.origin.x);
            let mid = bounds.origin.y + bounds.size.height / 2.;
            let at = |t: f32| px(x0 + (t - start) * pps);
            let right = bounds.origin.x + bounds.size.width;
            let visible = |x: Pixels| x >= bounds.origin.x - px(8.) && x <= right + px(8.);
            // Past the clip's end is shaded.
            let end = at(duration).max(bounds.origin.x);
            if end < right {
                window.paint_quad(fill(
                    Bounds::new(
                        point(end, bounds.origin.y),
                        size(right - end, bounds.size.height),
                    ),
                    theme::get().shadow.alpha(0.19),
                ));
            }
            if !summary {
                let mut sorted = keys.clone();
                sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
                for w in sorted.windows(2) {
                    let (a, b) = (at(w[0].0).max(bounds.origin.x), at(w[1].0).min(right));
                    if b <= a {
                        continue;
                    }
                    let (h, color) = if w[0].1 == 0 {
                        (px(1.), theme::get().muted.alpha(0.5))
                    } else {
                        (px(6.), theme::get().accent.alpha(0.25))
                    };
                    window.paint_quad(fill(
                        Bounds::new(point(a, mid - h / 2.), size(b - a, h)),
                        color,
                    ));
                }
            }
            for &(t, _, on) in &keys {
                let x = at(t);
                if !visible(x) {
                    continue;
                }
                let r = px(if summary { 3. } else { 5. });
                let color = if on {
                    theme::get().text
                } else {
                    theme::get().highlight
                };
                gpui_editor_kit::paint::polygon(
                    &[
                        point(x, mid - r),
                        point(x + r, mid),
                        point(x, mid + r),
                        point(x - r, mid),
                    ],
                    color,
                    window,
                );
            }
            let x = at(time);
            if visible(x) {
                window.paint_quad(fill(
                    Bounds::new(point(x, bounds.origin.y), size(px(1.), bounds.size.height)),
                    crate::colors::get().layout.link,
                ));
            }
        },
    )
    .size_full()
}

/// Pixels from a key's diamond a click still takes it: the diamond is 7 px
/// wide, and a miss seeks and drops the selection.
const KEY_REACH: f32 = 12.;

impl UiEditor {
    /// Selects every key at the playhead (within half a frame), across the
    /// clip's tracks; `add` keeps the selection. How many it found.
    fn select_keys_at_playhead(&mut self, add: bool) -> usize {
        let Some(c) = self.clip() else { return 0 };
        let half = 0.5 / c.fps.max(1) as f32;
        let t = self.player.time;
        let hits: Vec<(usize, usize)> = c
            .lanes
            .iter()
            .enumerate()
            .flat_map(|(l, lane)| {
                lane.keys
                    .iter()
                    .enumerate()
                    .filter(move |(_, k)| (k.time - t).abs() <= half)
                    .map(move |(k, _)| (l, k))
            })
            .collect();
        for (i, &(l, k)) in hits.iter().enumerate() {
            self.select_key(l, k, add || i > 0);
        }
        hits.len()
    }

    /// A press on lane `li`'s time row: on a key it selects it (Shift adds)
    /// and starts dragging the selection; elsewhere it moves the playhead
    /// there, and a double click adds a key there.
    fn lane_press(&mut self, li: usize, e: &MouseDownEvent) {
        self.take_keys(super::Keys::Timeline);
        self.player.playing = false;
        let x = f32::from(e.position.x);
        let Some(c) = self.clip() else { return };
        let hit = c.lanes.get(li).and_then(|l| {
            l.keys
                .iter()
                .enumerate()
                .map(|(k, key)| (k, (self.x_of(key.time) - x).abs()))
                .filter(|(_, d)| *d <= KEY_REACH)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(k, _)| k)
        });
        match hit {
            Some(k) => {
                let selected = self.selected_keys().contains(&(li, k));
                if e.modifiers.shift || !selected {
                    self.select_key(li, k, e.modifiers.shift);
                } else {
                    // The one pressed leads; the rest stay selected.
                    let p = &mut self.player;
                    if let Some(i) = p.also.iter().position(|&s| s == (li, k)) {
                        p.also.remove(i);
                        if let Some(old) = p.key.replace((li, k)) {
                            p.also.push(old);
                        }
                    }
                }
                self.player.drag = Some(KeyDrag {
                    start_x: x,
                    dt: 0.,
                    moved: false,
                    free: false,
                });
            }
            None if e.click_count >= 2 => {
                let t = self.snap(self.t_of(x)).max(0.);
                let r = self.add_key_at(li, t);
                self.message = Some(match r {
                    Ok(m) => (m, false),
                    Err(e) => (format!("Not added: {e:#}"), true),
                });
            }
            None => {
                if !e.modifiers.shift {
                    self.player.key = None;
                    self.player.also.clear();
                }
                let t = self.snap(self.t_of(x));
                self.seek(t);
            }
        }
    }

    /// The left button came up over the timeline: a key drag moves the
    /// keys; a key click without one puts the playhead on it.
    fn timeline_release(&mut self) {
        self.player.scrubbing = false;
        let Some(d) = self.player.drag.take() else {
            return;
        };
        if d.moved && d.dt.abs() > 1e-6 {
            let r = self.move_keys(d.dt, !d.free);
            self.message = Some(match r {
                Ok(m) => (m, false),
                Err(e) => (format!("Not moved: {e:#}"), true),
            });
        } else if let Some((l, k)) = self.player.key
            && let Some(t) = self
                .clip()
                .and_then(|c| Some(c.lanes.get(l)?.keys.get(k)?.time))
        {
            self.seek(t);
        }
    }

    /// Selects on the canvas the widget `w` of `file` (None: the open file)
    /// that a clip animates.
    /// Selects and frames the widget a lane group animates; says which,
    /// or that it is not on the canvas.
    pub(super) fn select_animated(&mut self, file: Option<Guid>, w: usize) -> String {
        let n = (0..self.nodes.len()).find(|&n| {
            let node = &self.nodes[n];
            node.widget == w
                && match file {
                    None => !node.foreign,
                    Some(g) => node.owners.first().is_some_and(|o| o.0 == g),
                }
        });
        let Some(n) = n else {
            return "That widget is not on the canvas".into();
        };
        self.select_widget(n);
        self.frame_rect(self.extent(n));
        format!("Selected {}", self.nodes[n].name)
    }
}

/// Phoenix easing names by value (see `phx::animation`): 0 holds the value
/// until the next key.
fn ease_name(kind: u32) -> &'static str {
    const NAMES: [&str; 32] = [
        "Hold",
        "Linear",
        "Sine In",
        "Sine Out",
        "Sine InOut",
        "Quad In",
        "Quad Out",
        "Quad InOut",
        "Cubic In",
        "Cubic Out",
        "Cubic InOut",
        "Quart In",
        "Quart Out",
        "Quart InOut",
        "Quint In",
        "Quint Out",
        "Quint InOut",
        "Expo In",
        "Expo Out",
        "Expo InOut",
        "Circ In",
        "Circ Out",
        "Circ InOut",
        "Back In",
        "Back Out",
        "Back InOut",
        "Elastic In",
        "Elastic Out",
        "Elastic InOut",
        "Bounce In",
        "Bounce Out",
        "Bounce InOut",
    ];
    NAMES.get(kind as usize).copied().unwrap_or("?")
}

impl Editor {
    /// The selected keys: the leading key's time and value as fields
    /// (editable for the open file's own clips), the easing of all of them
    /// by name, add at the playhead and delete.
    fn key_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .pt_1()
            .min_h(px(30.))
            .border_t_1()
            .border_color(theme::get().border)
            .text_size(px(11.));
        let Some(ui) = &self.ui else { return row };
        let p = &ui.player;
        let Some(clip) = ui.clip() else {
            return row;
        };
        let Some((lane, index)) = p.key else {
            return row.child(
                div()
                    .text_color(theme::get().muted)
                    .child("Click a key to edit it · Shift click adds · drag moves (Alt: off the frames) · double click a track adds a key"),
            );
        };
        let Some((label, key)) = clip
            .lanes
            .get(lane)
            .and_then(|l| Some((l.label.clone(), l.keys.get(index)?.clone())))
        else {
            return row;
        };
        let own = clip.file.is_none();
        let count = 1 + p.also.len();
        let editing = ui.editing.as_ref().map(|e| (e.field, e.display()));
        let field = |name: &'static str,
                     span: Option<usize>,
                     shown_as: String,
                     width: f32,
                     cx: &mut Context<Self>|
         -> AnyElement {
            // A template clip's spans are its file's, not this one's: its
            // keys show what they hold, read only.
            let text = span
                .filter(|_| own)
                .and_then(|s| ui.edit_text(s))
                .unwrap_or(shown_as);
            let el = div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_color(theme::get().muted).child(name));
            match span.filter(|_| own) {
                Some(span) => {
                    let shown = editing
                        .as_ref()
                        .filter(|e| e.0 == span)
                        .map(|e| e.1.clone());
                    el.child(
                        text_field(("ui-key-field", span), &text, shown, "")
                            .w(px(width))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_span_edit(
                                    span,
                                    super::Keys::Timeline,
                                    Filter::Text,
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .into_any_element()
                }
                None => el.child(text).into_any_element(),
            }
        };
        let mut row = row
            .child(if count > 1 {
                format!("{count} keys · leading: {label}")
            } else {
                format!("Key {} of {label}", index + 1)
            })
            .child(field(
                "Time",
                key.time_span,
                format!("{:.3}", key.time),
                64.,
                cx,
            ))
            // The ruler counts frames; the stored time is seconds.
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child(format!("s · frame {:.0}", key.time * clip.fps as f32)),
            )
            .child(field(
                "Value",
                key.value_span,
                key.value
                    .as_ref()
                    .map_or_else(|| "—".to_owned(), super::bindings::short),
                130.,
                cx,
            ));
        let ease = key.ease;
        if own {
            let options: Vec<(u32, SharedString)> = (0..32)
                .map(|k| (k, SharedString::from(ease_name(k))))
                .collect();
            row = row
                .child(div().text_color(theme::get().muted).child("Easing"))
                .child(
                    gpui_editor_kit::widgets::dropdown(
                        "ui-key-ease",
                        options,
                        &ease,
                        window,
                        cx,
                        |this, k, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                let r = ui.set_ease(k);
                                ui.message = Some(match r {
                                    Ok(m) => (m, false),
                                    Err(e) => (format!("Not applied: {e:#}"), true),
                                });
                            }
                            cx.notify();
                        },
                    )
                    .w(px(120.)),
                )
                .child(ease_curve(ease))
                .child(
                    button("Add at playhead", false)
                        .tooltip(Tooltip::text(
                            "Add key",
                            "",
                            "Copy the key before the playhead to the playhead, on this track",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                ui.message = Some(match ui.add_key() {
                                    Ok(m) => (m, false),
                                    Err(e) => (format!("Not added: {e:#}"), true),
                                });
                            }
                            cx.notify();
                        })),
                )
                .child(
                    button(
                        if count > 1 {
                            "Delete keys"
                        } else {
                            "Delete key"
                        },
                        false,
                    )
                    .tooltip(Tooltip::text("Delete", "Delete", ""))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(ui) = &mut this.ui {
                            ui.message = Some(match ui.delete_keys() {
                                Ok(m) => (m, false),
                                Err(e) => (format!("Not deleted: {e:#}"), true),
                            });
                        }
                        cx.notify();
                    })),
                );
        } else {
            row = row
                .child(
                    div()
                        .text_color(theme::get().muted)
                        .child(format!("Easing {}", ease_name(ease))),
                )
                .child(ease_curve(ease))
                .child(
                    div()
                        .text_color(theme::get().muted)
                        .child(format!("In the {} template: open it to edit", clip.owner)),
                );
        }
        row
    }
}

/// A small plot of easing `kind` over 0..1.
fn ease_curve(kind: u32) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            window.paint_quad(fill(bounds, theme::get().input));
            let pts: Vec<Point<Pixels>> = (0..=32)
                .map(|i| {
                    let t = i as f32 / 32.;
                    let v = if kind == 0 {
                        if t < 1. { 0. } else { 1. }
                    } else {
                        anno_formats::phx::animation::ease(kind, t)
                    };
                    bounds.origin + point(px(2. + t * (w - 4.)), px(h - 3. - v * (h - 6.)))
                })
                .collect();
            gpui_editor_kit::paint::line(&pts, 1.5, theme::get().accent, window);
        },
    )
    .w(px(40.))
    .h(px(22.))
    .rounded(px(2.))
}

#[cfg(test)]
mod tests {
    use super::{ANIMATABLE, UiEditor};
    use crate::ui::test_support::{
        ICON_ROW, QUEST_TRACKER, TECH_TREE, bindings, key, movable, open, reopen,
    };

    // Needs the game: a new clip, a Position track keyed at 0 s, and a
    // canvas move recorded at 0.5 s make the widget slide: halfway through
    // it is halfway there. Undo of the move takes only that key back.
    #[test]
    fn recording_canvas_moves_keys_a_new_clip() {
        let Some(mut ui) = open(TECH_TREE) else {
            return;
        };
        let node = (1..ui.nodes.len())
            .find(|&i| movable(&ui, i) && ui.nodes[i].visible && ui.nodes[i].image.is_some())
            .unwrap();
        let (widget, name) = (ui.nodes[node].widget, ui.nodes[node].name.clone());
        let at = |ui: &UiEditor| {
            ui.nodes
                .iter()
                .find(|n| !n.foreign && n.widget == widget && n.name == name)
                .unwrap()
                .rect
        };
        let start = at(&ui);
        ui.new_clip().unwrap();
        ui.select_widget(node);
        let position = ANIMATABLE
            .iter()
            .position(|a| a.0 == anno_formats::phx::binding::builtin::POSITION)
            .unwrap();
        ui.add_track_for(position).unwrap();
        assert_eq!(ui.clip().unwrap().lanes.len(), 1);
        ui.player.recording = true;
        ui.seek(0.5);
        let before = at(&ui);
        let mut after = before;
        after[0] += 30.;
        ui.canvas_change(crate::ui::canvas::Change {
            widget,
            before,
            after,
        });
        assert_eq!(ui.clip().unwrap().lanes[0].keys.len(), 2);
        ui.seek(0.25);
        let mid = at(&ui);
        assert!((mid[0] - start[0] - 15.).abs() < 1., "{start:?} {mid:?}");
        ui.seek(0.);
        assert!((at(&ui)[0] - start[0]).abs() < 0.5);
        assert!(ui.undo(false));
        let clip = ui
            .player
            .clips
            .iter()
            .find(|c| c.file.is_none() && c.name == "Clip_01")
            .unwrap();
        assert_eq!(clip.lanes[0].keys.len(), 1);
    }

    // Needs the game: a template clip's keys live in the template's file;
    // reading one through the screen (which has fewer spans) is refused,
    // not a panic (clicking such a key crashed the editor). The clip opens
    // for editing from the timeline: "Edit X here" enters its template in
    // place, which makes the clip this file's and keeps it open, and
    // stepping back out keeps it too (round 13: the timeline went back to
    // "Pick a clip").
    #[test]
    fn a_template_clip_opens_for_editing_and_stays_open() {
        let Some(mut ui) = open("questtrackerscene") else {
            return;
        };
        let spans = ui.asset.spans().len();
        let past = ui
            .player
            .clips
            .iter()
            .filter(|c| c.file.is_some())
            .flat_map(|c| c.lanes.iter().flat_map(|l| l.keys.iter()))
            .flat_map(|k| [k.time_span, k.value_span])
            .flatten()
            .find(|&s| s >= spans)
            .expect("a template key span past the screen's");
        assert_eq!(ui.edit_text(past), None);
        let i = ui
            .player
            .clips
            .iter()
            .position(|c| c.file.is_some() && c.name == "ScrollbarAppear")
            .expect("a template's clip");
        ui.player.current = Some(i);
        let g = ui.player.clips[i].file.unwrap();
        let key = ui
            .nodes
            .iter()
            .flat_map(|n| n.owners.iter())
            .find(|o| o.0 == g)
            .map(|o| o.1)
            .expect("an instance of its template");
        ui.enter_template(g, key).unwrap();
        let c = ui.clip().expect("still open, editable here");
        assert_eq!((c.name.as_str(), c.file), ("ScrollbarAppear", None));
        ui.leave_template();
        let c = ui.clip().expect("still open after");
        assert_eq!((c.name.as_str(), c.file), ("ScrollbarAppear", Some(g)));
    }

    // Needs the game: a click on a lane group selects the widget it
    // animates, by name.
    #[test]
    fn a_lane_group_selects_its_widget() {
        let Some(mut ui) = open(QUEST_TRACKER) else {
            return;
        };
        let (file, w) = ui
            .player
            .clips
            .iter()
            .filter(|c| c.file.is_none())
            .find_map(|c| c.lanes.iter().find_map(|l| l.widget).map(|w| (c.file, w)))
            .expect("an own clip animating a widget");
        ui.canvas.selected = None;
        let said = ui.select_animated(file, w);
        let n = ui.canvas.selected.expect("selected");
        assert_eq!(ui.nodes[n].widget, w);
        assert_eq!(said, format!("Selected {}", ui.nodes[n].name));
    }

    // Needs the game: a key added between two keys of a fade takes the
    // value the fade has there, so the motion does not change.
    #[test]
    fn added_keys_keep_the_motion() {
        use anno_formats::phx::{DefaultValue, animation::sample_track};
        let Some(mut ui) = open(QUEST_TRACKER) else {
            return;
        };
        // Two eased float keys with different values.
        let (clip, lane) = ui
            .player
            .clips
            .iter()
            .enumerate()
            .filter(|(_, c)| c.file.is_none())
            .find_map(|(i, c)| {
                let l = c.lanes.iter().position(|l| {
                    l.keys.len() >= 2
                        && l.keys[0].ease != 0
                        && matches!(l.keys[0].value, Some(DefaultValue::F32(_)))
                        && l.keys[0].value != l.keys[1].value
                })?;
                Some((i, l))
            })
            .expect("an own clip with an eased float fade");
        ui.select_clip(clip);
        let keys = ui.player.clips[clip].lanes[lane].keys.clone();
        let t = (keys[0].time + keys[1].time) / 2.;
        let want = sample_track(&keys, t);
        ui.add_key_at(lane, t).unwrap();
        let now = &ui.player.clips[clip].lanes[lane].keys;
        assert_eq!(now.len(), keys.len() + 1);
        let added = now.iter().find(|k| (k.time - t).abs() < 1e-4).unwrap();
        assert_eq!(added.value, want);
        assert_ne!(added.value, keys[0].value, "a copy of the key before");
    }

    // Needs the game: in a template with its own clips, a key added at the
    // playhead lands between its neighbours with the playhead's time, undo
    // takes it back, and delete removes it. A key added, selected, and
    // taken away by undo is no longer selected; moving or easing what is
    // selected then works on what the clip has (round 5: the selection
    // indexed past it). Several keys move together by whole frames and
    // stop short of a key they are not moved with, easing applies to all
    // of them, and deleting them is one undo step.
    #[test]
    fn keys_add_move_ease_and_delete() {
        let Some(mut ui) = open(QUEST_TRACKER) else {
            return;
        };
        let (clip, lane) = ui
            .player
            .clips
            .iter()
            .enumerate()
            .find_map(|(i, c)| {
                let l = c.lanes.iter().position(|l| l.keys.len() >= 2)?;
                (c.file.is_none()).then_some((i, l))
            })
            .expect("an own clip with a two-key track");
        ui.select_clip(clip);
        let keys = ui.player.clips[clip].lanes[lane].keys.clone();
        let t = (keys[0].time + keys[1].time) / 2.;
        ui.player.key = Some((lane, 0));
        ui.player.time = t;
        ui.add_key().unwrap();
        let now = &ui.player.clips[clip].lanes[lane].keys;
        assert_eq!(now.len(), keys.len() + 1);
        assert!((now[1].time - t).abs() < 1e-5);
        assert_eq!(ui.player.key, Some((lane, 1)));
        assert!(ui.undo(false));
        assert_eq!(ui.player.clips[clip].lanes[lane].keys.len(), keys.len());
        assert!(ui.undo(true));
        ui.player.key = Some((lane, 1));
        ui.player.also.clear();
        ui.delete_keys().unwrap();
        assert_eq!(ui.player.clips[clip].lanes[lane].keys, keys);

        let n = keys.len();
        ui.add_key_at(lane, keys[n - 1].time + 0.5).unwrap();
        assert_eq!(ui.player.clips[clip].lanes[lane].keys.len(), n + 1);
        ui.select_key(lane, n, false);
        assert!(ui.undo(false));
        assert_eq!(ui.player.clips[clip].lanes[lane].keys.len(), n);
        assert!(ui.selected_keys().is_empty(), "{:?}", ui.selected_keys());
        // Neither panics.
        let _ = ui.move_keys(0.1, true);
        let _ = ui.set_ease(3);
        // A key added before the selected one: after its undo the selection
        // does not slide onto the key after it (round 6 then deleted that).
        let keys = &ui.player.clips[clip].lanes[lane].keys;
        let (first, second) = (keys[0].time, keys[1].time);
        ui.select_key(lane, 1, false);
        ui.add_key_at(lane, (first + second) / 2.).unwrap();
        assert!(ui.undo(false));
        assert!(ui.selected_keys().is_empty(), "{:?}", ui.selected_keys());
        let _ = ui.delete_keys();
        assert_eq!(
            ui.player.clips[clip].lanes[lane].keys.len(),
            n,
            "nothing deleted"
        );

        let before = ui.player.clips[clip].lanes[lane].keys.clone();
        let frame = 1. / ui.player.clips[clip].fps as f32;
        // The last key alone, two frames later, lands on a frame.
        let last = before.len() - 1;
        ui.select_key(lane, last, false);
        ui.move_keys(2. * frame + 0.001, true).unwrap();
        let t = ui.player.clips[clip].lanes[lane].keys[last].time;
        assert!(((t / frame) - (t / frame).round()).abs() < 1e-3, "{t}");
        assert!(t > before[last].time);
        // Moving it before the key ahead of it stops a frame after that key.
        ui.move_keys(-100., true).unwrap();
        let keys = &ui.player.clips[clip].lanes[lane].keys;
        assert!((keys[last].time - (keys[last - 1].time + frame)).abs() < 1e-4);
        // Both keys: ease, then delete in one step.
        ui.select_key(lane, 0, false);
        ui.select_key(lane, 1, true);
        assert_eq!(ui.selected_keys().len(), 2);
        ui.set_ease(5).unwrap();
        let keys = &ui.player.clips[clip].lanes[lane].keys;
        assert!(keys[0].ease == 5 && keys[1].ease == 5);
        let n = keys.len();
        ui.delete_keys().unwrap();
        assert_eq!(ui.player.clips[clip].lanes[lane].keys.len(), n - 2);
        assert!(ui.undo(false));
        assert_eq!(ui.player.clips[clip].lanes[lane].keys.len(), n);
    }

    // Needs the game: after a click in the timeline, Delete with no key
    // selected, widget shortcuts and arrows never reach the widget selected
    // on the canvas (round 7: Delete went on to delete it).
    #[test]
    fn timeline_keys_never_reach_the_canvas() {
        let Some(mut ui) = open(QUEST_TRACKER) else {
            return;
        };
        let clip = (0..ui.player.clips.len())
            .find(|&i| ui.player.clips[i].file.is_none())
            .unwrap();
        ui.select_clip(clip);
        let node = (1..ui.nodes.len()).find(|&n| !ui.nodes[n].foreign).unwrap();
        ui.select_widget(node);
        ui.keys = crate::ui::Keys::Timeline;
        let (widgets, bytes) = (ui.asset.widgets().len(), ui.asset.encode());
        for k in [
            "delete",
            "backspace",
            "ctrl-delete",
            "ctrl-backspace",
            "shift-delete",
            "alt-delete",
            "ctrl-d",
            "ctrl-x",
            "ctrl-v",
            "alt-up",
            "up",
        ] {
            assert!(ui.timeline_key_down(&key(k)), "{k} stops in the timeline");
            assert_eq!(ui.asset.widgets().len(), widgets, "{k}");
        }
        assert_eq!(ui.asset.encode(), bytes, "the file as it was");
        assert_eq!(ui.canvas.selected, Some(node));
        assert!(
            ui.message
                .as_ref()
                .is_some_and(|m| m.0 == "Select a key first")
        );
        // The editor's own shortcuts go on (round 8: Ctrl+Delete did too,
        // and the tree deleted the widget).
        for k in ["ctrl-z", "ctrl-y", "ctrl-s", "f5"] {
            assert!(!ui.timeline_key_down(&key(k)), "{k} is the editor's");
        }
        // Arrows step frames here; Esc hands the keys back.
        let t = ui.player.time;
        assert!(ui.timeline_key_down(&key("right")));
        assert!(ui.player.time > t);
        // Keys the timeline has no use for stop here too (round 9: F framed
        // the canvas behind).
        let view = ui.canvas.view.zoom;
        assert!(ui.timeline_key_down(&key("f")));
        assert_eq!(ui.canvas.view.zoom, view);
        // Shift arrows, and < and > as Linux reports Shift , and ., go to
        // the previous and next key.
        let mut keys: Vec<f32> = (ui.clip().unwrap().lanes.iter())
            .flat_map(|l| l.keys.iter().map(|k| k.time))
            .collect();
        keys.sort_by(f32::total_cmp);
        ui.seek(0.);
        let first_after = keys.iter().copied().find(|&k| k > 1e-4).unwrap();
        assert!(ui.timeline_key_down(&key(">")));
        assert!(
            (ui.player.time - first_after).abs() < 1e-4,
            "> goes to the next key"
        );
        assert!(ui.timeline_key_down(&key("shift-left")));
        assert!(ui.player.time < first_after, "Shift Left goes back a key");
        assert!(ui.timeline_key_down(&key("escape")));
        assert!(std::mem::take(&mut ui.hand_back), "Esc asks for the canvas");
        // Docked, the editor gives it the keys (editor/tests.rs covers a
        // hidden canvas).
        ui.take_keys(crate::ui::Keys::Canvas);
        assert!(!ui.timeline_key_down(&key("delete")), "the canvas's again");
    }

    // Needs the game: a file without clips: the timeline pane still takes
    // Delete once clicked (round 8 deleted the Icon from the empty pane).
    // It gets its first clip (round 8: the empty timeline offered no +
    // Clip, so templates without animations could never get one), the
    // clip reads back, and undo takes it away.
    #[test]
    fn a_file_without_clips_gets_its_first() {
        let Some(mut ui) = open(ICON_ROW) else {
            return;
        };
        assert!(ui.player.clips.is_empty() && ui.asset.clips().is_empty());
        let node = (1..ui.nodes.len()).find(|&n| !ui.nodes[n].foreign).unwrap();
        ui.select_widget(node);
        ui.take_keys(crate::ui::Keys::Timeline);
        let widgets = ui.asset.widgets().len();
        assert!(ui.timeline_key_down(&key("delete")));
        assert_eq!(ui.asset.widgets().len(), widgets);
        let view = ui.canvas.view.zoom;
        for k in ["f", "home", "up"] {
            assert!(
                ui.timeline_key_down(&key(k)),
                "{k} stops in the empty timeline"
            );
        }
        assert_eq!(ui.canvas.view.zoom, view);

        let said = ui.new_clip().unwrap();
        assert_eq!(said, "Added clip Clip_01");
        let clips = ui.asset.clips();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "Clip_01");
        let reread = reopen(&ui.asset);
        assert_eq!(reread.clips().len(), 1, "it reads back");
        let c = ui.clip().expect("the new clip is picked");
        assert_eq!((c.name.as_str(), c.file), ("Clip_01", None));
        assert!(ui.undo(false));
        assert!(ui.player.clips.is_empty());
    }

    // Needs the game: keying a property (+ Track, Record) selects the key
    // it wrote. Round 7: the selection stayed an index, which then named
    // the new key or another one, and Delete took a key nobody picked.
    #[test]
    fn keying_selects_the_key_it_wrote() {
        use anno_formats::phx::binding::Slot;
        let Some(mut ui) = open(QUEST_TRACKER) else {
            return;
        };
        // An own clip's track of a built-in the timeline can key, with two
        // keys at least two frames apart.
        let slots = bindings(&ui.asset).slots;
        let clips = ui.asset.clips();
        let (clip, lane, w, which) = ui
            .player
            .clips
            .iter()
            .enumerate()
            .filter(|(_, c)| c.file.is_none())
            .find_map(|(i, c)| {
                let frame = 1. / c.fps as f32;
                clips[c.index]
                    .tracks
                    .iter()
                    .enumerate()
                    .find_map(|(t, tr)| {
                        let Some(Slot::Widget {
                            widget: Some(w),
                            path,
                        }) = slots.get(tr.target)
                        else {
                            return None;
                        };
                        let &[(1, p)] = &path[..] else { return None };
                        let which = ANIMATABLE.iter().position(|a| 0x8000_0000 | a.0 == p)?;
                        let keys = &c.lanes[t].keys;
                        (keys.len() >= 2 && keys[1].time - keys[0].time > 2.5 * frame)
                            .then_some((i, t, *w, which))
                    })
            })
            .expect("an own clip with a keyable two-key track");
        let node = (1..ui.nodes.len())
            .find(|&n| ui.nodes[n].widget == w && !ui.nodes[n].foreign)
            .unwrap();
        ui.select_clip(clip);
        let keys = ui.player.clips[clip].lanes[lane].keys.clone();
        ui.player.key = Some((lane, 0));
        ui.player.time = ui.snap((keys[0].time + keys[1].time) / 2.);
        let (id, kind) = ANIMATABLE[which];
        let value = ui.current_value(node, id, kind);
        ui.key_property(node, id, kind, value).unwrap();
        let now = &ui.player.clips[clip].lanes[lane].keys;
        assert_eq!(now.len(), keys.len() + 1);
        assert_eq!(ui.player.key, Some((lane, 1)), "the new key");
        assert!((now[1].time - ui.player.time).abs() < 1e-4);
        assert!(ui.player.also.is_empty());
    }
}
