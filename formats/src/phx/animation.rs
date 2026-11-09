// Timeline animations of a widget template (`AnimationInstances` on the
// root): named clips of keyframed tracks, and sampling them at a time.
//
// A clip (AnimationInstance) has a FrameRate, a Duration in frames and
// tracks (`0x23c12f18`). A track's `0x1935a21e` is a slot of the template's
// binding table (`binding::Bindings::slots`, all 2,275 installed clips
// resolve), mostly built-in widget properties: AlphaTint, ColorTint,
// Visible, Position, Scale, Size. Keys hold a Time in seconds, an easing
// (`0x8f7aa617`, enum `0x7eead398`) and a Value.
//
// Easing values are the Phoenix easing function's cases
// (Robert Penner's set): 1 Linear, then Sine, Quad, Cubic, Quart, Quint,
// Expo, Circ, Back, Elastic, Bounce, each In/Out/InOut (2..=31), the same
// order as the EaseType enum. 0 has no case: the value holds until the next
// key (boolean keys always use it).

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};

use super::{Asset, DefaultValue, ObjectId, Record, Splice, Value, name_crc};

const CLASS_CLIP: u32 = name_crc(b"AnimationInstance");
const PROP_CLIPS: u32 = name_crc(b"AnimationInstances");
const PROP_TRACKS: u32 = name_crc(b"Tracks");
const PROP_TARGET: u32 = 0x1935_a21e;
const PROP_KEYS: u32 = name_crc(b"Track");
const PROP_TIME: u32 = name_crc(b"Time");
const PROP_EASE: u32 = 0x8f7a_a617;
const PROP_VALUE: u32 = name_crc(b"Value");
const PROP_FRAME_RATE: u32 = name_crc(b"FrameRate");
const PROP_DURATION: u32 = name_crc(b"Duration");
const PROP_NAME: u32 = super::PROP_NAME;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Key {
    pub time: f32,
    pub ease: u32,
    /// Floats as F32 / Vec bits, booleans as Bool, colours as a Vec of
    /// three floats 0..=255. Other key types (strings, events) are None.
    pub value: Option<DefaultValue>,
    /// The key object's span, and its stored Time, easing and Value spans
    /// (fixed-width values [`super::Edit::Value`] rewrites in place). 0 /
    /// None for keys not read from a file.
    pub span: usize,
    pub time_span: Option<usize>,
    pub ease_span: Option<usize>,
    pub value_span: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// Slot of the binding table it writes.
    pub target: usize,
    pub keys: Vec<Key>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// The AnimationInstance's object id: bindings address a clip's Start,
    /// Stop and Completed through it.
    pub id: Option<super::Guid>,
    pub name: String,
    pub frame_rate: u32,
    pub frames: u32,
    pub tracks: Vec<Track>,
    /// Spans of the stored name and Duration, for editing in place.
    pub name_span: Option<usize>,
    pub frames_span: Option<usize>,
}

impl Clip {
    /// Plays when a screen opens, by the names shipped clips use: `Anim_In`,
    /// `Content_In`, `FadeIn`, `Show`, `Open`. Clips that react to input or
    /// data (`ShowInteractionBG`, `ScrollbarAppear`, the governors bar's
    /// `Data_Change_in`, which swaps its second row to another party's
    /// island) do not count.
    pub fn opens(&self) -> bool {
        let n = self.name.to_ascii_lowercase();
        if n.contains("change") {
            return false;
        }
        n.ends_with("_in")
            || matches!(
                n.as_str(),
                "in" | "fadein" | "fade_in" | "show" | "open" | "appear"
            )
    }

    /// Length in seconds: the stored duration, or the last key when that
    /// is later.
    pub fn duration(&self) -> f32 {
        let stored = self.frames as f32 / self.frame_rate.max(1) as f32;
        let last = self
            .tracks
            .iter()
            .flat_map(|t| t.keys.last())
            .map(|k| k.time)
            .fold(0.0, f32::max);
        stored.max(last)
    }

    /// Every track's value at `t` seconds, by target slot.
    // TODO(alex): the segment takes the easing of the key it starts from;
    // whether Phoenix uses the start or the end key is not traced.
    pub fn sample(&self, t: f32) -> Vec<(usize, DefaultValue)> {
        self.tracks
            .iter()
            .filter_map(|tr| Some((tr.target, sample_track(&tr.keys, t)?)))
            .collect()
    }
}

fn floats(v: &DefaultValue) -> Option<Vec<f32>> {
    match v {
        DefaultValue::F32(b) => Some(vec![f32::from_bits(*b)]),
        DefaultValue::Vec(x) => Some(x.iter().map(|b| f32::from_bits(*b)).collect()),
        _ => None,
    }
}

/// A track's value at `t` seconds: held before the first key and after the
/// last, eased between (hold where a key has no easing).
pub fn sample_track(keys: &[Key], t: f32) -> Option<DefaultValue> {
    let first = keys.first()?;
    if t <= first.time {
        return first.value.clone();
    }
    let i = keys.iter().rposition(|k| k.time <= t)?;
    let (a, Some(b)) = (&keys[i], keys.get(i + 1)) else {
        return keys[i].value.clone();
    };
    let (Some(va), Some(vb)) = (&a.value, &b.value) else {
        return a.value.clone();
    };
    if a.ease == 0 || b.time <= a.time {
        return Some(va.clone());
    }
    let (Some(fa), Some(fb)) = (floats(va), floats(vb)) else {
        return Some(va.clone());
    };
    if fa.len() != fb.len() {
        return Some(va.clone());
    }
    let u = ease(a.ease, (t - a.time) / (b.time - a.time));
    let mixed: Vec<f32> = fa.iter().zip(&fb).map(|(x, y)| x + (y - x) * u).collect();
    Some(match va {
        DefaultValue::F32(_) => DefaultValue::F32(mixed[0].to_bits()),
        _ => DefaultValue::Vec(mixed.iter().map(|x| x.to_bits()).collect()),
    })
}

/// Phoenix easing `kind` at `t` in 0..1, as a fraction of the change.
pub fn ease(kind: u32, t: f32) -> f32 {
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    let t = t.clamp(0.0, 1.0);
    let in_out = |f: &dyn Fn(f32) -> f32| {
        if t < 0.5 {
            f(t * 2.0) * 0.5
        } else {
            1.0 - f((1.0 - t) * 2.0) * 0.5
        }
    };
    let bounce_out = |t: f32| {
        if t < 1.0 / 2.75 {
            7.5625 * t * t
        } else if t < 2.0 / 2.75 {
            let t = t - 1.5 / 2.75;
            7.5625 * t * t + 0.75
        } else if t < 2.5 / 2.75 {
            let t = t - 2.25 / 2.75;
            7.5625 * t * t + 0.9375
        } else {
            let t = t - 2.625 / 2.75;
            7.5625 * t * t + 0.984375
        }
    };
    let pow_in = |p: i32| move |t: f32| t.powi(p);
    let expo_in = |t: f32| {
        if t == 0.0 {
            0.0
        } else {
            2f32.powf(10.0 * (t - 1.0))
        }
    };
    let circ_in = |t: f32| 1.0 - (1.0 - t * t).max(0.0).sqrt();
    let back_in = |t: f32| t * t * (2.70158 * t - 1.70158);
    let elastic_in = |t: f32, p: f32| {
        if t == 0.0 || t == 1.0 {
            return t;
        }
        let s = p / 4.0;
        let t = t - 1.0;
        -(2f32.powf(10.0 * t) * ((t - s) * TAU / p).sin())
    };
    match kind {
        1 => t,
        2 => 1.0 - (t * FRAC_PI_2).cos(),
        3 => (t * FRAC_PI_2).sin(),
        4 => -0.5 * ((PI * t).cos() - 1.0),
        5..=16 => {
            let p = 2 + (kind as i32 - 5) / 3;
            let f = pow_in(p);
            match (kind - 5) % 3 {
                0 => f(t),
                1 => 1.0 - f(1.0 - t),
                _ => in_out(&f),
            }
        }
        17 => expo_in(t),
        18 => 1.0 - expo_in(1.0 - t),
        19 => in_out(&expo_in),
        20 => circ_in(t),
        21 => 1.0 - circ_in(1.0 - t),
        22 => in_out(&circ_in),
        23 => back_in(t),
        24 => 1.0 - back_in(1.0 - t),
        25 => in_out(&|t| t * t * (3.5949094 * t - 2.5949094)),
        26 => elastic_in(t, 0.3),
        27 => 1.0 - elastic_in(1.0 - t, 0.3),
        28 => in_out(&|t| elastic_in(t, 0.45)),
        29 => 1.0 - bounce_out(1.0 - t),
        30 => bounce_out(t),
        31 => in_out(&|t| 1.0 - bounce_out(1.0 - t)),
        _ => t,
    }
}

impl Asset {
    /// The template's timeline clips, in file order.
    /// Each clip's object id, in [`Asset::clips`] order, without decoding
    /// its tracks.
    pub fn clip_ids(&self) -> Vec<Option<super::Guid>> {
        let Some((root, _)) = self.objects().next() else {
            return Vec::new();
        };
        self.values_of(root, PROP_CLIPS)
            .into_iter()
            .filter_map(|c| match &self.spans[c].record {
                Record::Object(h) if h.class == CLASS_CLIP => Some(match h.id {
                    ObjectId::Id(g) => Some(g),
                    _ => None,
                }),
                _ => None,
            })
            .collect()
    }

    pub fn clips(&self) -> Vec<Clip> {
        let Some((root, _)) = self.objects().next() else {
            return Vec::new();
        };
        let u32_of = |o: usize, crc: u32| {
            self.values_of(o, crc)
                .first()
                .and_then(|&s| match self.spans[s].record {
                    Record::Value(Value::U32(v) | Value::Enum { value: v, .. }) => Some(v),
                    _ => None,
                })
        };
        self.values_of(root, PROP_CLIPS)
            .into_iter()
            .filter(
                |&c| matches!(&self.spans[c].record, Record::Object(h) if h.class == CLASS_CLIP),
            )
            .map(|c| Clip {
                id: match &self.spans[c].record {
                    Record::Object(h) => match h.id {
                        ObjectId::Id(g) => Some(g),
                        _ => None,
                    },
                    _ => None,
                },
                name: self
                    .values_of(c, PROP_NAME)
                    .first()
                    .and_then(|&s| match &self.spans[s].record {
                        Record::Str(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                frame_rate: u32_of(c, PROP_FRAME_RATE).unwrap_or(60),
                frames: u32_of(c, PROP_DURATION).unwrap_or(0),
                name_span: self.values_of(c, PROP_NAME).first().copied(),
                frames_span: self.values_of(c, PROP_DURATION).first().copied(),
                tracks: self
                    .values_of(c, PROP_TRACKS)
                    .into_iter()
                    .filter_map(|tr| {
                        let target = u32_of(tr, PROP_TARGET)? as usize;
                        let keys = self
                            .values_of(tr, PROP_KEYS)
                            .into_iter()
                            .map(|k| self.key(k))
                            .collect();
                        Some(Track { target, keys })
                    })
                    .collect(),
            })
            .collect()
    }

    fn key(&self, k: usize) -> Key {
        let mut key = Key {
            span: k,
            ..Key::default()
        };
        for p in self.object_props(k) {
            match (p.crc, p.value) {
                (PROP_TIME, Value::F32(t)) => {
                    key.time = t;
                    key.time_span = Some(p.span);
                }
                (PROP_EASE, Value::Enum { value, .. }) => {
                    key.ease = value;
                    key.ease_span = Some(p.span);
                }
                (PROP_VALUE, v) => {
                    key.value_span = Some(p.span);
                    key.value = match v {
                        Value::F32(x) => Some(DefaultValue::F32(x.to_bits())),
                        Value::Vec2(x) => Some(DefaultValue::Vec(x.map(f32::to_bits).to_vec())),
                        Value::Vec3(x) => Some(DefaultValue::Vec(x.map(f32::to_bits).to_vec())),
                        Value::Vec4(x) => Some(DefaultValue::Vec(x.map(f32::to_bits).to_vec())),
                        Value::Bool(b) => Some(DefaultValue::Bool(u8::from(b))),
                        Value::U32(x) | Value::Enum { value: x, .. } => Some(DefaultValue::U32(x)),
                        Value::S32(x) => Some(DefaultValue::S32(x)),
                        Value::Color(c) => Some(DefaultValue::Vec(
                            c.map(|x| f32::from(x).to_bits()).to_vec(),
                        )),
                        _ => None,
                    }
                }
                _ => {}
            }
        }
        key
    }

    /// The count span of the key array a key sits in.
    fn key_count(&self, key: usize) -> Result<usize> {
        let f = self.spans[key].field.context("key outside any array")?;
        (f.object..key)
            .rev()
            .find(|&i| {
                self.spans[i].field == Some(f) && matches!(self.spans[i].record, Record::Count(_))
            })
            .context("key array count not found")
    }

    /// Removes the key object at `key` from its track.
    pub fn remove_key(&self, key: usize) -> Result<Asset> {
        let count = self.key_count(key)?;
        let splices = vec![Splice {
            range: self.subtree_bytes(key),
            bytes: Vec::new(),
        }];
        self.finish(splices, HashMap::from([(count, -1)]), &[])
    }

    /// Adds a copy of the key at `key` right after it (same time, easing
    /// and value; the caller moves it). Keys carry no object id, so the
    /// bytes copy as they are.
    pub fn duplicate_key(&self, key: usize) -> Result<Asset> {
        let Record::Object(h) = &self.spans[key].record else {
            bail!("span {key} is not a key object");
        };
        ensure!(
            h.id == ObjectId::None,
            "keys with an object id cannot be copied"
        );
        let count = self.key_count(key)?;
        let range = self.subtree_bytes(key);
        let splices = vec![Splice {
            range: range.end..range.end,
            bytes: self.bytes[range].to_vec(),
        }];
        self.finish(splices, HashMap::from([(count, 1)]), &[])
    }
}

// ---------------------------------------------------------------------------
// Authoring: new clips and tracks.
//
//   AnimationInstance  02 02 <id16> b03e911f 06 7c  FrameRate Duration(frames)
//                      [track] NameCrc Name
//   track              02 00 <class> 03 60  target slot, [key]
//   key                02 00 <class> 04 70  Time ease Value

/// The value type a new track animates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Float,
    Vec2,
    Vec3,
    Vec4,
    Bool,
}

impl TrackKind {
    const ALL: [TrackKind; 5] = [
        TrackKind::Float,
        TrackKind::Vec2,
        TrackKind::Vec3,
        TrackKind::Vec4,
        TrackKind::Bool,
    ];

    /// Track and key classes. The float track is `Float32TrackInstance`
    /// while its key is `FloatKey`.
    const fn classes(self) -> (u32, u32) {
        match self {
            TrackKind::Float => (name_crc(b"Float32TrackInstance"), name_crc(b"FloatKey")),
            TrackKind::Vec2 => (name_crc(b"Vector2TrackInstance"), name_crc(b"Vector2Key")),
            TrackKind::Vec3 => (name_crc(b"Vector3TrackInstance"), name_crc(b"Vector3Key")),
            TrackKind::Vec4 => (name_crc(b"Vector4TrackInstance"), name_crc(b"Vector4Key")),
            TrackKind::Bool => (name_crc(b"BooleanTrackInstance"), name_crc(b"BooleanKey")),
        }
    }

    fn encode(self, v: &DefaultValue, out: &mut Vec<u8>) -> Result<()> {
        let floats = |n: usize| -> Result<Vec<u32>> {
            Ok(match v {
                DefaultValue::F32(b) if n == 1 => vec![*b],
                DefaultValue::Vec(x) if x.len() >= n => x[..n].to_vec(),
                _ => bail!("a {self:?} key needs {n} numbers, not {v:?}"),
            })
        };
        match self {
            TrackKind::Bool => out.push(match v {
                DefaultValue::Bool(b) => u8::from(*b != 0),
                _ => bail!("a yes/no key needs on or off"),
            }),
            k => {
                let n = match k {
                    TrackKind::Float => 1,
                    TrackKind::Vec2 => 2,
                    TrackKind::Vec3 => 3,
                    _ => 4,
                };
                for b in floats(n)? {
                    out.extend_from_slice(&b.to_le_bytes());
                }
            }
        }
        Ok(())
    }
}

/// A key to write: seconds, easing (see [`ease`]) and value.
#[derive(Clone, Debug, PartialEq)]
pub struct NewKey {
    pub time: f32,
    pub ease: u32,
    pub value: DefaultValue,
}

fn key_bytes(kind: TrackKind, k: &NewKey, out: &mut Vec<u8>) -> Result<()> {
    let (_, key) = kind.classes();
    out.extend_from_slice(&[0x02, 0x00]);
    out.extend_from_slice(&key.to_le_bytes());
    out.extend_from_slice(&[0x04, 0x70]);
    out.extend_from_slice(&k.time.to_bits().to_le_bytes());
    out.extend_from_slice(&k.ease.to_le_bytes());
    kind.encode(&k.value, out)
}

impl Asset {
    /// Adds an empty clip `name` at the end of the root's clips: `fps`
    /// frames a second, `frames` long.
    pub fn add_clip(&self, name: &str, fps: u32, frames: u32) -> Result<Asset> {
        let (root, _) = self.objects().next().context("empty file")?;
        let clips = self
            .array(root, PROP_CLIPS)
            .context("this file stores no AnimationInstances")?;
        let mut c = vec![0x02, 0x02];
        c.extend_from_slice(&super::Guid::random().0);
        c.extend_from_slice(&CLASS_CLIP.to_le_bytes());
        c.extend_from_slice(&[0x06, 0x7c]);
        c.extend_from_slice(&fps.max(1).to_le_bytes());
        c.extend_from_slice(&frames.to_le_bytes());
        c.push(0);
        c.extend_from_slice(&super::crc32(name.as_bytes()).to_le_bytes());
        super::write_str_payload(&mut c, name);
        let mut splices = Vec::new();
        let mut counts = HashMap::new();
        super::binding::append(self, &mut splices, &mut counts, &clips, vec![c]);
        self.finish(splices, counts, &[CLASS_CLIP])
    }

    /// Adds a track to clip `clip` (index into [`Asset::clips`]) writing
    /// `target` (a widget property; its slot is added when the file has
    /// none), with `keys` in time order.
    pub fn add_track(
        &self,
        widgets: &[super::Widget],
        clip: usize,
        target: &super::binding::NewSlot,
        kind: TrackKind,
        keys: &[NewKey],
    ) -> Result<Asset> {
        ensure!(
            matches!(target, super::binding::NewSlot::Widget { .. }),
            "a track animates a widget property"
        );
        ensure!(
            keys.windows(2).all(|w| w[0].time <= w[1].time),
            "keys must be in time order"
        );
        let (root, _) = self.objects().next().context("empty file")?;
        let &object = self
            .values_of(root, PROP_CLIPS)
            .iter()
            .filter(
                |&&c| matches!(&self.spans[c].record, Record::Object(h) if h.class == CLASS_CLIP),
            )
            .nth(clip)
            .context("no such clip")?;
        let tracks = self
            .array(object, PROP_TRACKS)
            .context("the clip stores no track list")?;
        let mut plan = super::binding::SlotPlan::new(self, widgets)?;
        let at = plan.resolve(target)?;
        let slot = plan.number(&at);
        let (track_class, key_class) = kind.classes();
        let mut t = vec![0x02, 0x00];
        t.extend_from_slice(&track_class.to_le_bytes());
        t.extend_from_slice(&[0x03, 0x60]);
        t.extend_from_slice(&slot.to_le_bytes());
        super::write_varint(&mut t, keys.len() as u32);
        for k in keys {
            key_bytes(kind, k, &mut t)?;
        }
        let (mut splices, mut counts) = plan.finish();
        super::binding::append(self, &mut splices, &mut counts, &tracks, vec![t]);
        let mut classes = super::binding::SLOT_CLASSES.to_vec();
        classes.extend([track_class, key_class]);
        self.finish(splices, counts, &classes)
    }

    /// Adds a key to the track at (`clip`, `track`) in time order, or
    /// replaces the value of a key already at that time.
    pub fn set_key(&self, clip: usize, track: usize, key: &NewKey) -> Result<Asset> {
        let clips = self.clips();
        let tr = clips
            .get(clip)
            .and_then(|c| c.tracks.get(track))
            .context("no such track")?;
        if let Some(k) = tr.keys.iter().find(|k| (k.time - key.time).abs() < 1e-4) {
            let span = k.value_span.context("the key stores no value")?;
            let value = match (&self.spans[span].record, &key.value) {
                (Record::Value(Value::F32(_)), DefaultValue::F32(b)) => {
                    Value::F32(f32::from_bits(*b))
                }
                (Record::Value(Value::Bool(_)), DefaultValue::Bool(b)) => Value::Bool(*b != 0),
                (Record::Value(Value::Vec2(_)), DefaultValue::Vec(v)) if v.len() >= 2 => {
                    Value::Vec2([f32::from_bits(v[0]), f32::from_bits(v[1])])
                }
                (Record::Value(Value::Vec3(_)), DefaultValue::Vec(v)) if v.len() >= 3 => {
                    Value::Vec3([
                        f32::from_bits(v[0]),
                        f32::from_bits(v[1]),
                        f32::from_bits(v[2]),
                    ])
                }
                (Record::Value(Value::Vec4(_)), DefaultValue::Vec(v)) if v.len() >= 4 => {
                    Value::Vec4([0, 1, 2, 3].map(|i| f32::from_bits(v[i])))
                }
                _ => bail!("the key holds another kind of value"),
            };
            return Ok(self.apply(&super::Edit::Value { span, value })?.asset);
        }
        let kind = match tr.keys.first().map(|k| &self.spans[k.span].record) {
            Some(Record::Object(h)) => {
                match TrackKind::ALL
                    .into_iter()
                    .find(|k| k.classes().1 == h.class)
                {
                    Some(k) => k,
                    None => bail!("keys of class {:08x} cannot be written yet", h.class),
                }
            }
            _ => bail!("the track has no key to go by"),
        };
        let mut bytes = Vec::new();
        key_bytes(kind, key, &mut bytes)?;
        // After the last key not later than it.
        let at = match tr.keys.iter().rposition(|k| k.time <= key.time) {
            Some(i) => self.subtree_bytes(tr.keys[i].span).end,
            None => self.spans[tr.keys[0].span].range.start,
        };
        let count = self.key_count(tr.keys[0].span)?;
        self.finish(
            vec![Splice {
                range: at..at,
                bytes,
            }],
            HashMap::from([(count, 1)]),
            &[],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Clip, Key, ease, sample_track};
    use crate::phx::DefaultValue;

    #[test]
    fn easing_curves_start_at_0_and_end_at_1() {
        for kind in 1..=31 {
            assert!(ease(kind, 0.0).abs() < 1e-4, "{kind} at 0");
            assert!((ease(kind, 1.0) - 1.0).abs() < 1e-4, "{kind} at 1");
        }
        // Quad_In is slow first, Quad_Out fast first; InOut meets at half.
        assert!(ease(5, 0.5) < 0.5 && ease(6, 0.5) > 0.5);
        assert!((ease(7, 0.5) - 0.5).abs() < 1e-4);
        // Back_In dips below zero first.
        assert!(ease(23, 0.2) < 0.0);
    }

    fn key(time: f32, ease: u32, v: f32) -> Key {
        Key {
            time,
            ease,
            value: Some(DefaultValue::F32(v.to_bits())),
            ..Key::default()
        }
    }

    fn at(keys: &[Key], t: f32) -> f32 {
        match sample_track(keys, t) {
            Some(DefaultValue::F32(b)) => f32::from_bits(b),
            other => panic!("{other:?}"),
        }
    }

    // The quest tracker's ScrollbarAppear: alpha 0 → 1 linearly in 0.233 s.
    #[test]
    fn tracks_interpolate_hold_and_clamp() {
        let fade = [key(0.0, 1, 0.0), key(0.233333, 1, 1.0)];
        assert_eq!(at(&fade, -1.0), 0.0);
        assert!((at(&fade, 0.1166665) - 0.5).abs() < 1e-4);
        assert_eq!(at(&fade, 5.0), 1.0);
        let hold = [key(0.0, 0, 3.0), key(1.0, 0, 7.0)];
        assert_eq!(at(&hold, 0.99), 3.0);
        assert_eq!(at(&hold, 1.0), 7.0);
    }

    #[test]
    fn opening_clips_by_name() {
        let clip = |name: &str| Clip {
            id: None,
            name: name.into(),
            frame_rate: 60,
            frames: 0,
            tracks: Vec::new(),
            name_span: None,
            frames_span: None,
        };
        assert!(clip("Anim_In").opens());
        assert!(clip("FadeIn").opens());
        // Swaps the governors bar's second row to another party's island.
        assert!(!clip("Data_Change_in").opens());
        assert!(!clip("ScrollbarAppear").opens());
    }
}
