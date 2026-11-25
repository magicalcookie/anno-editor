// Owns the Model workspace's sound: the game's Wwise library, loaded once
// per install off the UI thread (formats::audio), decoded media kept by id,
// and an output stream that mixes the sounds a sequence's clips start as
// the playhead crosses them, each panned and attenuated by where its clip
// plays against the viewport camera. Looped clips run until playback
// stops. What Looped and the joint fields mean: docs/audio-format.md.

use super::ModelEditor;
use super::playback::{self, Source};
use crate::editor::Editor;
use anno_formats::audio::{self, Library, Media};
use anno_formats::model::{Play, Sound as Clip};
use anno_formats::xml::Id;
use anyhow::{Context as _, Result};
use gpui_editor_kit::gpui::prelude::*;
use gpui_editor_kit::math::{self, Vec3};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Sounds playing at once at most; a dense sequence starting more drops
/// the oldest. 117's busiest shipped sequence starts 6 together, and a
/// looped clip holds its voice until playback stops.
const VOICES: usize = 24;
/// Under the game's own mix: clips decode near full scale.
const VOLUME: f32 = 0.7;
/// The language the game's voice banks are read in.
const LANGUAGE: &str = "en_us";

/// One decoded sound playing.
struct Voice {
    pcm: Arc<[f32]>,
    channels: usize,
    /// Source frames per output frame.
    step: f64,
    at: f64,
    /// The sound clip that started it, by element; `None` for the
    /// picker's Listen, which no playback state stops.
    clip: Option<u32>,
    /// Starts over at its end: a Looped clip's, or a looping event's.
    looped: bool,
    /// Left and right gain: where its clip plays, heard from the camera.
    gain: [f32; 2],
}

impl Voice {
    fn frames(&self) -> usize {
        self.pcm.len() / self.channels.max(1)
    }
}

/// What the output stream's callback mixes, shared with the UI thread.
#[derive(Default)]
struct Mixer {
    voices: Vec<Voice>,
}

impl Mixer {
    /// Mixes the voices into `out` (`channels` interleaved), resampled by
    /// nearest frame: a voice's left and right channels (its only one when
    /// mono) to the first two outputs at its gains, a mono output gets
    /// their mean. Looped voices wrap, the others drop when finished.
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        out.fill(0.);
        for v in &mut self.voices {
            let frames = v.frames();
            if frames == 0 {
                continue;
            }
            for frame in out.chunks_mut(channels) {
                if v.at as usize >= frames {
                    if !v.looped {
                        break;
                    }
                    v.at %= frames as f64;
                }
                let k = v.at as usize * v.channels;
                let (l, r) = (v.pcm[k], v.pcm[k + (v.channels > 1) as usize]);
                let [gl, gr] = v.gain;
                match frame {
                    [mono] => *mono += (l * gl + r * gr) * 0.5 * VOLUME,
                    [left, right, ..] => {
                        *left += l * gl * VOLUME;
                        *right += r * gr * VOLUME;
                    }
                    [] => {}
                }
                v.at += v.step;
            }
        }
        self.voices
            .retain(|v| v.looped || (v.at as usize) < v.frames());
        for o in out.iter_mut() {
            *o = o.clamp(-1., 1.);
        }
    }

    /// Adds a voice, dropping the oldest when all are in use.
    fn push(&mut self, v: Voice) {
        if self.voices.len() >= VOICES {
            self.voices.remove(0);
        }
        self.voices.push(v);
    }
}

/// One decoded sound: interleaved samples at its own rate.
#[derive(Clone)]
struct Pcm {
    samples: Arc<[f32]>,
    channels: usize,
    rate: u32,
}

/// The sound of the Model workspace: kept by the editor across models.
#[derive(Default)]
pub struct Sound {
    library: Option<Arc<Library>>,
    loading: bool,
    /// Decoded media, by id. `None` when it does not decode (one shipped
    /// file is broken).
    pcm: HashMap<u32, Option<Pcm>>,
    mixer: Arc<Mutex<Mixer>>,
    /// The output stream and its rate, opened on the first sound.
    #[cfg_attr(test, allow(dead_code))]
    stream: Option<(cpal::Stream, u32)>,
    /// Why there is no sound, said once.
    failed: Option<String>,
    roll: u32,
}

/// A sound clip starting: its element, its fields, whether its sound
/// loops, and its gains.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Cue {
    pub(super) clip: Id,
    pub(super) sound: Clip,
    pub(super) looped: bool,
    pub(super) gain: [f32; 2],
}

impl Sound {
    pub fn library(&self) -> Option<Arc<Library>> {
        self.library.clone()
    }

    /// Why sound clips can't be added yet, when they can't.
    pub fn missing(&self) -> Option<&'static str> {
        match (&self.library, self.loading) {
            (Some(_), _) => None,
            (None, true) => Some("The game's sounds are still loading"),
            // 2070 plays no sound from its cfgs: its sounds bind to assets.
            (None, false) => {
                Some("The game's sounds aren't read (117's and 1800's are; 2070's cfgs play none)")
            }
        }
    }

    /// Opens the default output device, float samples at its own rate.
    #[cfg(not(test))]
    fn open(&mut self) -> Result<u32> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        if let Some((_, rate)) = &self.stream {
            return Ok(*rate);
        }
        let device = (cpal::default_host().default_output_device()).context("no sound device")?;
        let supported = device.default_output_config()?;
        anyhow::ensure!(
            supported.sample_format() == cpal::SampleFormat::F32,
            "the sound device takes {:?} samples, not floats",
            supported.sample_format()
        );
        let config: cpal::StreamConfig = supported.into();
        let (rate, channels) = (config.sample_rate.0, config.channels as usize);
        let mixer = self.mixer.clone();
        let stream = device.build_output_stream(
            &config,
            move |out: &mut [f32], _| {
                // Never wait on the UI thread here: a held lock is a
                // silent buffer, not a glitch in the editor.
                match mixer.try_lock() {
                    Ok(mut m) => m.fill(out, channels),
                    Err(_) => out.fill(0.),
                }
            },
            |e| eprintln!("sound: {e}"),
            None,
        )?;
        stream.play()?;
        self.stream = Some((stream, rate));
        Ok(rate)
    }

    /// Tests mix into no device, whether the machine has one or not: the
    /// voices stay in the mixer for them to read.
    #[cfg(test)]
    fn open(&mut self) -> Result<u32> {
        Ok(48_000)
    }

    /// The voices playing: the clip that started each, whether it loops,
    /// its gains.
    #[cfg(test)]
    pub(super) fn voices(&self) -> Vec<(Option<u32>, bool, [f32; 2])> {
        (self.mixer().voices.iter())
            .map(|v| (v.clip, v.looped, v.gain))
            .collect()
    }

    /// A pick below `n`: which variation of a random sound plays.
    fn roll(&mut self, n: usize) -> usize {
        // xorshift: enough for variety, no dependency.
        let mut x = self.roll.wrapping_add(0x9e37_79b9);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.roll = x;
        x as usize % n.max(1)
    }

    fn mixer(&self) -> std::sync::MutexGuard<'_, Mixer> {
        self.mixer
            .lock()
            .expect("the stream never panics holding it")
    }

    /// Plays asset `guid`, for clip `cue` (or the picker when `None`). A
    /// looped clip whose sound still runs is not started again: a looped
    /// sequence wrapping past it keeps the one it started.
    fn play(
        &mut self,
        install: &anno_formats::vfs::Install,
        guid: u32,
        cue: Option<Cue>,
    ) -> Result<()> {
        let library = self
            .library
            .clone()
            .context("the game's sounds are still loading")?;
        let looped = cue.is_some_and(|c| c.looped);
        let clip = cue.map(|c| c.clip.raw());
        if looped && clip.is_some() && self.mixer().voices.iter().any(|v| v.clip == clip) {
            return Ok(());
        }
        let rate = self.open()?;
        let mut picks: Vec<Media> = Vec::new();
        library.pick(guid, |n| self.roll(n), &mut picks);
        for media in picks {
            let decoded = self.pcm.entry(media.id).or_insert_with(|| {
                let bytes = library.read(install, media).ok()?;
                let mut pcm = Vec::new();
                let format = audio::decode(&bytes, &mut pcm).ok()?;
                // Ambisonic ambiences: channel 0 is the omni mix.
                let channels = if format.ambisonic {
                    1
                } else {
                    format.channels as usize
                };
                let pcm: Vec<f32> = match format.ambisonic {
                    true => pcm.chunks(format.channels as usize).map(|f| f[0]).collect(),
                    false => pcm,
                };
                Some(Pcm {
                    samples: Arc::from(pcm),
                    channels,
                    rate: format.sample_rate,
                })
            });
            let Some(pcm) = decoded.clone() else {
                continue;
            };
            self.mixer().push(Voice {
                step: f64::from(pcm.rate) / f64::from(rate),
                pcm: pcm.samples,
                channels: pcm.channels,
                at: 0.,
                clip,
                looped,
                gain: cue.map_or([1.; 2], |c| c.gain),
            });
        }
        Ok(())
    }

    /// Stops every sound a clip started; the picker's keeps playing.
    fn stop_clips(&mut self) {
        self.mixer().voices.retain(|v| v.clip.is_none());
    }

    /// Sets the gains of what each of `cues`' clips plays.
    fn place(&mut self, cues: &[Cue]) {
        let mut m = self.mixer();
        for v in &mut m.voices {
            if let Some(c) = cues.iter().find(|c| Some(c.clip.raw()) == v.clip) {
                v.gain = c.gain;
            }
        }
    }
}

/// Left and right gain of a sound `pan` across the view (-1 its left edge,
/// 1 its right) and `distance` from the listener, heard to `reach` (the
/// event's MaxAttenuation; 0 for a sound without position, which stays
/// centred and whole). Balance, not equal power, so a centred sound keeps
/// the level it always had. The game's attenuation curves are in its
/// banks' type 14 objects, which are not read: the square of the share of
/// the reach left is an estimate.
fn gain(pan: f32, distance: f32, reach: f32) -> [f32; 2] {
    if reach <= 0. {
        return [1.; 2];
    }
    let level = (1. - distance / reach).clamp(0., 1.).powi(2);
    [level * (1. - pan).min(1.), level * (1. + pan).min(1.)]
}

impl ModelEditor {
    /// The sound clips of the playing sequence and their elements.
    fn sound_clips(&self) -> Vec<(f32, Id, Clip)> {
        let Source::Sequence(k) = self.playback.source else {
            return Vec::new();
        };
        let Some(s) = self.model.sequences.get(k) else {
            return Vec::new();
        };
        (s.clips.iter())
            .filter_map(|c| match c.what {
                Play::Sound(sound) => Some((c.start, c.element, sound)),
                _ => None,
            })
            .collect()
    }

    /// The sound clips of the playing sequence that start in `[from, to)`
    /// on its clock, wrapping past its end when it loops.
    pub(super) fn sounds_between(&self, from: f32, to: f32) -> Vec<(Id, Clip)> {
        let length = self.clip_length();
        let wraps = self.playback.looping && length > 0. && to >= length;
        (self.sound_clips().into_iter())
            .filter(|&(start, ..)| (start >= from && start < to) || (wraps && start < to - length))
            .map(|(_, clip, sound)| (clip, sound))
            .collect()
    }

    /// Whether clip `s`'s sound runs until playback stops: the clip is
    /// Looped, or its event loops by itself (Wwise loops it either way).
    fn loops(&self, s: Clip) -> bool {
        s.looped
            || (self.sounds.as_ref()).is_some_and(|l| l.event(s.guid).is_some_and(|e| e.looping))
    }

    /// The looping clips that started before `t`: playback starting there
    /// hears them already running, as it would have from the start.
    pub(super) fn looped_before(&self, t: f32) -> Vec<(Id, Clip)> {
        (self.sound_clips().into_iter())
            .filter(|&(start, _, sound)| start < t && self.loops(sound))
            .map(|(_, clip, sound)| (clip, sound))
            .collect()
    }

    /// Where sound clip `s` plays now, model space: joint `bone` of mesh
    /// `model` as posed; that mesh's origin when its skeleton has no such
    /// joint or it has none; the model's origin when there is no such mesh.
    pub(super) fn sound_at(&self, s: Clip) -> Vec3 {
        let (model, bone) = (s.model as usize, s.bone as usize);
        let Some(part) = self.model.parts.get(model) else {
            return [0.; 3];
        };
        match playback::joint_at(&self.model, &self.player.poses, model, bone) {
            Some(at) => math::transform(&at, [0.; 3]),
            None => part.transform.position,
        }
    }

    /// Clip `clip` playing `s`, with its gains heard from the camera now.
    pub(super) fn cue(&self, clip: Id, s: Clip) -> Cue {
        let reach = (self.sounds.as_ref())
            .and_then(|l| l.event(s.guid))
            .map_or(0., |e| e.max_attenuation);
        let (pan, distance) = self.view.hear(self.sound_at(s));
        Cue {
            clip,
            sound: s,
            looped: self.loops(s),
            gain: gain(pan, distance, reach),
        }
    }

    /// The fields of sound clip `clip`, when it is one.
    pub fn sound_clip(&self, clip: Id) -> Option<Clip> {
        (self.model.sequences.iter().flat_map(|s| &s.clips))
            .find(|c| c.element == clip)
            .and_then(|c| match c.what {
                Play::Sound(s) => Some(s),
                _ => None,
            })
    }

    /// Sets sound clip `clip`'s Looped and joint as one edit. `SkeletonID`
    /// goes to 0, as beside all but 3 of the shipped clips that name a
    /// joint.
    pub fn set_sound(&mut self, clip: Id, s: Clip) {
        if self.sound_clip(clip).is_none_or(|old| old == s) {
            return;
        }
        self.checkpoint();
        for (name, value) in [
            ("Looped", u32::from(s.looped)),
            ("ModelID", s.model),
            ("SkeletonID", 0),
            ("BoneID", s.bone),
        ] {
            self.doc.set_child_text(clip, name, &value.to_string());
        }
        self.sequences_edited();
    }

    /// Where sound clip `s` plays, in words.
    pub fn sound_place(&self, s: Clip) -> String {
        let Some(part) = self.model.parts.get(s.model as usize) else {
            return format!("At the model's origin: there is no mesh {}", s.model);
        };
        let skeleton = part.mesh.as_ref().and_then(|r| r.skeleton.as_ref());
        match skeleton.filter(|k| (s.bone as usize) < k.len()) {
            Some(k) => format!("At {} › {}", part.name, k.name(s.bone as usize)),
            None => format!("At {}'s origin: it has no joint {}", part.name, s.bone),
        }
    }

    /// The joint picked in the hierarchy as a sound clip names it: mesh
    /// and joint by index.
    pub fn picked_sound_joint(&self) -> Option<(u32, u32)> {
        let (part, j) = self.joint?;
        let k = self.model.parts.iter().position(|p| p.element == part)?;
        Some((k as u32, u32::from(j)))
    }

    /// Every sound clip of the playing sequence, cued from where it is now.
    fn cues(&self) -> Vec<Cue> {
        (self.sound_clips().into_iter())
            .map(|(_, clip, s)| self.cue(clip, s))
            .collect()
    }
}

impl Editor {
    /// Starts loading the game's sounds, once per install.
    pub(crate) fn load_model_sounds(&mut self, cx: &mut Context<Self>) {
        let Some(install) = self.install.clone() else {
            return;
        };
        let sound = &mut self.model_sound;
        if sound.library.is_some() || sound.loading || sound.failed.is_some() {
            return;
        }
        sound.loading = true;
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { Library::load(&install, LANGUAGE) })
                .await;
            this.update(cx, |this, cx| {
                let sound = &mut this.model_sound;
                sound.loading = false;
                match loaded {
                    Ok(library) => sound.library = Some(Arc::new(library)),
                    // 2070's sounds are not read: no sound.
                    Err(e) => sound.failed = Some(format!("{e:#}")),
                }
                // A model that opened first names its sound clips now.
                if let Some(m) = &mut this.model {
                    m.sounds = this.model_sound.library.clone();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Plays sound asset `guid` alone, as the picker's Listen does.
    pub(crate) fn listen_model_sound(&mut self, guid: u32) {
        let Some(install) = self.install.clone() else {
            return;
        };
        if let Err(e) = self.model_sound.play(&install, guid, None) {
            self.model_sound_failed(e);
        }
    }

    /// A frame of playback's sound, after the tick advanced the playhead
    /// from `before` (source and time) and crossed the clips `crossed`.
    /// Stops what clips started when the playhead had moved since the last
    /// frame other than by playing (a rewind, a scrub, another source) or
    /// playback stopped or went quiet; starts the crossed clips, and the
    /// looping ones behind `before` when sound starts afresh; places every
    /// clip's sound from where it is now.
    pub(crate) fn model_sound_frame(&mut self, before: (Source, f32), crossed: Vec<(Id, Clip)>) {
        let Some(m) = &mut self.model else {
            self.model_sound.stop_clips();
            return;
        };
        let p = m.playback;
        let moved = m.sound_clock.is_some_and(|clock| clock != before);
        if !p.playing || !p.sound || moved {
            m.sound_clock = None;
            self.model_sound.stop_clips();
        }
        if !p.playing || !p.sound {
            return;
        }
        let mut due = crossed;
        if m.sound_clock.is_none() && before.0 == p.source {
            due.extend(m.looped_before(before.1));
        }
        m.sound_clock = Some((p.source, p.time));
        let due: Vec<Cue> = due.into_iter().map(|(c, s)| m.cue(c, s)).collect();
        let placed = m.cues();
        let Some(install) = self.install.clone() else {
            return;
        };
        for c in due {
            if let Err(e) = self.model_sound.play(&install, c.sound.guid, Some(c)) {
                self.model_sound_failed(e);
                return;
            }
        }
        self.model_sound.place(&placed);
    }

    /// A sound that cannot play is said once in the status bar.
    fn model_sound_failed(&mut self, e: anyhow::Error) {
        if self.model_sound.failed.is_none() && self.model_sound.library.is_some() {
            self.message(format!("No sound: {e:#}"), true);
            self.model_sound.failed = Some(format!("{e:#}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mixer, Voice, gain};
    use std::sync::Arc;

    fn voice(pcm: Vec<f32>, step: f64, looped: bool, gain: [f32; 2]) -> Voice {
        Voice {
            pcm: Arc::from(pcm),
            channels: 1,
            step,
            at: 0.,
            clip: Some(1),
            looped,
            gain,
        }
    }

    // Two voices add, a voice at half the output rate holds each frame
    // twice, and finished voices leave.
    #[test]
    fn the_mixer_adds_resamples_and_drops() {
        let mut m = Mixer::default();
        m.voices.push(voice(vec![0.5, 0.5], 0.5, false, [1.; 2]));
        m.voices.push(voice(vec![0.25], 1., false, [1.; 2]));
        let mut out = [0.; 8];
        m.fill(&mut out, 2);
        let v = super::VOLUME;
        assert_eq!(out[0], (0.5 + 0.25) * v);
        assert_eq!(out[2], 0.5 * v);
        assert_eq!(out[6], 0.5 * v);
        m.fill(&mut out, 2);
        assert!(m.voices.is_empty() && out.iter().all(|&s| s == 0.));
    }

    // A looped voice starts over at its end and stays; its gains weigh
    // the left and right outputs.
    #[test]
    fn a_looped_voice_wraps_at_its_gains() {
        let mut m = Mixer::default();
        m.voices.push(voice(vec![0.5, 0.25], 1., true, [1., 0.5]));
        let mut out = [0.; 10];
        m.fill(&mut out, 2);
        let v = super::VOLUME;
        let left: Vec<f32> = out.iter().step_by(2).map(|s| s / v).collect();
        let right: Vec<f32> = out.iter().skip(1).step_by(2).map(|s| s / v).collect();
        assert_eq!(left, [0.5, 0.25, 0.5, 0.25, 0.5]);
        assert_eq!(right, [0.25, 0.125, 0.25, 0.125, 0.25]);
        assert_eq!(m.voices.len(), 1);
    }

    // Centred and close is whole; off to the right the left side fades;
    // past the reach nothing; a sound without a reach is never placed.
    #[test]
    fn gains_pan_and_fall_off() {
        assert_eq!(gain(0., 0., 10.), [1., 1.]);
        assert_eq!(gain(0.5, 0., 10.), [0.5, 1.]);
        assert_eq!(gain(-1., 0., 10.), [1., 0.]);
        assert_eq!(gain(0., 5., 10.), [0.25, 0.25]);
        assert_eq!(gain(0., 12., 10.), [0., 0.]);
        assert_eq!(gain(1., 50., 0.), [1., 1.]);
    }
}
