// Owns how island models move: the clip a cfg plays by default, baked once
// per cfg, and each piece's pose from it and from the joints and
// `ANIMATION_TRANSFORM` motion of every entry above the piece, which the
// renderer loops on the GPU (gpu::Pose). The Model workspace's playback
// (src/model/playback.rs) poses one model per frame on the CPU from the
// same records and formulas.
use anno_formats::{
    model::{BoneLink, Model, Motion, Play, Sequence, Transform},
    rdm::{Animation, JointPose, Rdm, Skeleton},
    vfs::Install,
};
use gpui_editor_kit::{
    gpu,
    math::{self, Mat4, Vec3},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

/// The sequence a placed object plays when nothing in the game sets one:
/// 117's Neutral Idle 1 (`action << 24 | (info + 52) << 8 | variant`), then
/// the legacy 1000 that 1800 and 117's 1800-numbered cfgs (flags, harbour
/// doors) use for theirs. Of the shipped cfgs with an animated skinned
/// part, 960 of 117's 1,275 have 100691457 and 1,559 of 1800's 1,816 have
/// 1000 (src/island/motion_tests.rs `default_sequences_in_shipped_cfgs`).
const IDLE: [u32; 2] = [100_691_457, 1000];

/// Frames a second a clip bakes at, and at most per clip: the exporter
/// keys animations at 15 a second, and the renderer blends between frames.
/// A 60 s crane loop holds 1024 frames, 17 a second.
const FPS: f32 = 30.;
const MAX_FRAMES: usize = 1024;

/// A cfg's default sequence (`IDLE`, else its first looped one playing a
/// skinned part, else that part's first animation alone) baked: per part,
/// its joints' model-space poses frame after frame, empty for parts that
/// play nothing. Frames are evenly spaced over `period` seconds of wall
/// time; the last runs back into the first.
pub struct Clip {
    pub period: f32,
    pub frames: usize,
    poses: Vec<Vec<JointPose>>,
}

impl Clip {
    /// Whether part `part` plays in the clip.
    pub fn plays(&self, part: usize) -> bool {
        self.poses.get(part).is_some_and(|p| !p.is_empty())
    }

    /// Part `part`'s pose at frame `f`, one per joint; `None` when the
    /// part plays nothing.
    fn pose(&self, part: usize, f: usize) -> Option<&[JointPose]> {
        let poses = self.poses.get(part).filter(|p| !p.is_empty())?;
        let joints = poses.len() / self.frames;
        Some(&poses[f * joints..(f + 1) * joints])
    }
}

/// Values by path, each made once by the first to ask while others asking
/// meanwhile wait for it (as models.rs `Meshes` builds meshes).
type Once<T> = Mutex<HashMap<String, Arc<OnceLock<Option<Arc<T>>>>>>;

/// Clips by cfg and the animations they read, shared by every load of an
/// island's models.
#[derive(Default)]
pub struct Clips {
    clips: Once<Clip>,
    animations: Once<Animation>,
}

impl Clips {
    /// `model`'s default clip, baked by the first load to ask.
    pub fn get(&self, install: &Install, model: &Model) -> Option<Arc<Clip>> {
        let cell = (self.clips.lock().unwrap())
            .entry(model.path.clone())
            .or_default()
            .clone();
        cell.get_or_init(|| self.bake(install, model).map(Arc::new))
            .clone()
    }

    fn animation(&self, install: &Install, path: &str) -> Option<Arc<Animation>> {
        let cell = (self.animations.lock().unwrap())
            .entry(path.to_owned())
            .or_default()
            .clone();
        cell.get_or_init(|| {
            let read = install.read(path).and_then(|b| Animation::parse(&b));
            read.map_err(|e| eprintln!("{path}: {e:#}"))
                .ok()
                .map(Arc::new)
        })
        .clone()
    }

    fn bake(&self, install: &Install, model: &Model) -> Option<Clip> {
        let _span = gpui_editor_kit::profile::span("models: bake clips");
        let (plays, length, speed) = match default_plays(model)? {
            Plays::Sequence(plays, length, speed) => (plays, length, speed),
            Plays::Alone(part) => {
                let path = model.parts[part].animations.first()?;
                let scale = scale(model, part, 0);
                let length = self.animation(install, path)?.duration * scale;
                let play = Played {
                    part,
                    animation: 0,
                    start: 0.,
                    end: length,
                    scale,
                };
                (vec![play], length, 1.)
            }
        };
        if length <= 0. {
            return None;
        }
        let period = length / speed;
        let frames = ((period * FPS).ceil() as usize).clamp(2, MAX_FRAMES);
        let mut poses: Vec<Vec<JointPose>> = vec![Vec::new(); model.parts.len()];
        let mut pose = Vec::new();
        for f in 0..frames {
            let t = f as f32 * length / frames as f32;
            for (part, out) in poses.iter_mut().enumerate() {
                let Some(skeleton) = skeleton(model, part) else {
                    continue;
                };
                if !plays.iter().any(|c| c.part == part) {
                    continue;
                }
                // The last clip live for the part, as a sequence lists them.
                // One that ends with the sequence holds its last frame.
                let live = (plays.iter().rev())
                    .find(|c| c.part == part && t >= c.start && (t < c.end || c.end >= length));
                let animation = live.and_then(|c| {
                    let path = model.parts[part].animations.get(c.animation)?;
                    Some((c, self.animation(install, path)?))
                });
                pose.clear();
                pose.extend_from_slice(&skeleton.bind_local);
                if let Some((c, a)) = animation {
                    // Clips longer than their animation wrap, as playback
                    // does.
                    let local = (t - c.start) / c.scale;
                    let local = if a.duration > 0. {
                        local % a.duration
                    } else {
                        0.
                    };
                    a.sample(local, skeleton, &mut pose);
                }
                skeleton.to_model(&mut pose);
                out.extend_from_slice(&pose);
            }
        }
        (poses.iter().any(|p| !p.is_empty())).then_some(Clip {
            period,
            frames,
            poses,
        })
    }
}

/// One animation clip of a sequence, seconds on its clock.
struct Played {
    part: usize,
    animation: usize,
    start: f32,
    end: f32,
    /// The animation's `Scale`: it plays this many times slower.
    scale: f32,
}

/// The skeleton of `model`'s part `part` when its mesh is skinned by it.
fn skeleton(model: &Model, part: usize) -> Option<&Skeleton> {
    let rdm = model.parts.get(part)?.mesh.as_ref()?;
    (rdm.joints.len() == rdm.positions.len())
        .then_some(rdm.skeleton.as_ref())
        .flatten()
}

/// What a cfg plays by default (`Clip`).
enum Plays {
    /// A sequence's animation clips, its length on its clock, and its rate.
    Sequence(Vec<Played>, f32, f32),
    /// No looped sequence plays a skinned part: the first animated part's
    /// first animation, looped alone.
    Alone(usize),
}

/// An animation's `Scale`: a sequence plays it this many times slower.
fn scale(model: &Model, part: usize, animation: usize) -> f32 {
    (model.parts[part].animation_scales.get(animation))
        .copied()
        .filter(|s| *s > 0.)
        .unwrap_or(1.)
}

/// What `model` plays by default; `None` when no skinned part animates.
fn default_plays(model: &Model) -> Option<Plays> {
    let animated =
        |part: usize| skeleton(model, part).is_some() && !model.parts[part].animations.is_empty();
    let plays = |s: &Sequence| {
        (s.clips.iter())
            .any(|c| matches!(c.what, Play::Animation { model, .. } if animated(model as usize)))
    };
    let looped = |s: &&Sequence| s.looped && plays(s);
    let sequence = (IDLE.iter())
        .find_map(|&id| model.sequences.iter().filter(looped).find(|s| s.id == id))
        .or_else(|| model.sequences.iter().find(looped));
    if let Some(s) = sequence {
        let clips: Vec<Played> = (s.clips.iter())
            .filter_map(|c| match c.what {
                Play::Animation {
                    model: part,
                    animation,
                } if animated(part as usize) => {
                    let (part, animation) = (part as usize, animation as usize);
                    Some(Played {
                        part,
                        animation,
                        start: c.start,
                        end: c.end,
                        scale: scale(model, part, animation),
                    })
                }
                _ => None,
            })
            .collect();
        let speed = if s.speed > 0. { s.speed } else { 1. };
        return Some(Plays::Sequence(clips, s.length(), speed));
    }
    (0..model.parts.len())
        .find(|&p| animated(p))
        .map(Plays::Alone)
}

/// Where an entry of a cfg stands in its parent's frame and how that moves:
/// onto the joint it hangs on, then by its own motion about its placement
/// (playback.rs `entry_change`). The island's static placement of the
/// entry is `hang.still().then(transform)`.
#[derive(Clone)]
pub struct Link {
    pub hang: Hang,
    pub transform: Transform,
    pub motion: Option<Motion>,
}

/// The joint an entry hangs on.
#[derive(Clone)]
pub enum Hang {
    None,
    /// A joint that holds its bind pose: the host part's placement times it.
    Still(Transform),
    /// Joint `joint` of part `part` as `clip` moves it, under the host
    /// part's placement `host`.
    Follows {
        clip: Arc<Clip>,
        part: usize,
        joint: usize,
        host: Transform,
        bind: Transform,
    },
}

impl Hang {
    /// For an entry of `model` hung on `link`: following the joint where
    /// `clip` moves its part, else held in the bind pose. `None` when the
    /// joint is not found, as the island then leaves the hang out.
    pub fn of(model: &Model, clip: Option<&Arc<Clip>>, link: BoneLink) -> Option<Hang> {
        let bind = model.joint_placement(link)?;
        let part = link.model as usize;
        let joint = skeleton(model, part).and_then(|s| s.find_hash(link.bone));
        Some(match (clip, joint) {
            (Some(clip), Some(joint)) if clip.pose(part, 0).is_some() => Hang::Follows {
                clip: clip.clone(),
                part,
                joint,
                host: model.parts[part].transform,
                bind,
            },
            _ => Hang::Still(bind),
        })
    }

    fn still(&self) -> Transform {
        match self {
            Hang::None => Transform::IDENTITY,
            Hang::Still(t) | Hang::Follows { bind: t, .. } => *t,
        }
    }

    /// `t` seconds into its clip's loop, at the nearest frame, when it
    /// follows one.
    fn at(&self, t: f32) -> Transform {
        match self {
            Hang::Follows {
                clip,
                part,
                joint,
                host,
                ..
            } => {
                let f = (t / clip.period * clip.frames as f32).round() as usize % clip.frames;
                let p = clip.pose(*part, f).map(|p| p[*joint]);
                p.map_or(self.still(), |p| {
                    host.then(&Transform {
                        position: p.translation,
                        rotation: p.rotation,
                        scale: [1.; 3],
                    })
                })
            }
            _ => self.still(),
        }
    }
}

impl Link {
    pub fn still(&self) -> Transform {
        self.hang.still().then(&self.transform)
    }

    /// `t` seconds into the loops of its hang and its motion.
    fn at(&self, t: f32) -> Transform {
        let Some(motion) = self.motion else {
            return self.hang.at(t).then(&self.transform);
        };
        let (offset, rotation) = motion.at(t);
        let shift = Transform {
            position: offset,
            ..Transform::IDENTITY
        };
        let turn = Transform {
            rotation,
            ..Transform::IDENTITY
        };
        self.hang
            .at(t)
            .then(&shift)
            .then(&self.transform)
            .then(&turn)
    }

    fn clip(&self) -> Option<&Arc<Clip>> {
        match &self.hang {
            Hang::Follows { clip, .. } => Some(clip),
            _ => None,
        }
    }
}

/// A part's skinning: its rdm, the clip that plays it and its index there.
pub struct Skinned<'a> {
    pub rdm: &'a Rdm,
    pub clip: &'a Arc<Clip>,
    pub part: usize,
}

/// The pose of a part placed through `chain` (outermost first, the part's
/// own link last): its skeleton playing `skin`'s clip, and the joints and
/// motion of every link. `None` when nothing in it moves.
///
/// Rigid links make one matrix a frame, the part's static placement's
/// inverse times where the links place it then. A loop follows the clip a
/// skin or a hang plays, else the longest motion's period; motions of
/// another period jump once a loop. Skins and hangs start each object at a
/// phase of its own, motion at the clock's zero, as the game starts it
/// when the object appears.
pub fn part_pose(
    chain: &[&Link],
    skin: Option<Skinned>,
    mesh: [Vec3; 2],
) -> Option<Arc<gpu::Pose>> {
    let clip = skin
        .as_ref()
        .map(|s| s.clip)
        .or_else(|| chain.iter().rev().find_map(|l| l.clip()));
    let motion = (chain.iter())
        .filter_map(|l| l.motion?.period())
        .fold(None, |p: Option<f32>, m| Some(p.map_or(m, |p| p.max(m))));
    let (period, frames) = match (clip, motion) {
        (Some(c), _) => (c.period, c.frames),
        (None, Some(p)) => (p, ((p * FPS).ceil() as usize).clamp(8, MAX_FRAMES)),
        (None, None) => return None,
    };
    let place = |t: Option<f32>| {
        (chain.iter()).fold(Transform::IDENTITY, |at, l| {
            at.then(&match t {
                Some(t) => l.at(t),
                None => l.still(),
            })
        })
    };
    let inverse = math::inverse(&place(None).matrix())?;
    let rigid: Vec<Mat4> = (0..frames)
        .map(|f| {
            let t = f as f32 * period / frames as f32;
            math::multiply(&inverse, &place(Some(t)).matrix())
        })
        .collect();
    let rows = |m: &Mat4| -> [[f32; 4]; 3] {
        std::array::from_fn(|r| [m[r], m[4 + r], m[8 + r], m[12 + r]])
    };
    let corners = |m: &Mat4, b: [Vec3; 2]| math::corners(b).map(|c| math::transform(m, c));
    let (frames_out, joints, bounds) = match skin {
        Some(s) => {
            let skeleton = s.rdm.skeleton.as_ref()?;
            let joints = skeleton.len();
            // Each joint's bind-pose box, over the vertices it weighs on.
            let mut boxes = vec![[[f32::MAX; 3], [f32::MIN; 3]]; joints];
            for ((p, js), ws) in s
                .rdm
                .positions
                .iter()
                .zip(&s.rdm.joints)
                .zip(&s.rdm.weights)
            {
                for (&j, _) in js.iter().zip(ws).filter(|(_, w)| **w > 0) {
                    if let Some(b) = boxes.get_mut(j as usize) {
                        *b = math::union(*b, [*p, *p]);
                    }
                }
            }
            let mut out = Vec::with_capacity(frames * joints);
            let mut seen = Vec::new();
            for (f, d) in rigid.iter().enumerate() {
                let pose = s.clip.pose(s.part, f)?;
                for (j, (p, ib)) in pose.iter().zip(&skeleton.inverse_bind).enumerate() {
                    let m = math::multiply(d, &flat(p.mul(ib).matrix()));
                    if boxes[j][0][0] <= boxes[j][1][0] {
                        seen.extend(corners(&m, boxes[j]));
                    }
                    out.push(rows(&m));
                }
            }
            (out, joints as u32, math::bounds(seen))
        }
        None => {
            let seen = rigid.iter().flat_map(|m| corners(m, mesh));
            (rigid.iter().map(rows).collect(), 1, math::bounds(seen))
        }
    };
    Some(Arc::new(gpu::Pose {
        frames: frames_out,
        joints,
        period,
        phased: clip.is_some(),
        origin: math::transform(&inverse, [0.; 3]),
        bounds,
    }))
}

fn flat(m: [[f32; 4]; 4]) -> Mat4 {
    std::array::from_fn(|i| m[i / 4][i % 4])
}

/// A skinned rdm's vertex `i`: its four joints, numbered as `base`'s
/// skeleton numbers them (a shipped detail level may order its own
/// apart), then their weights (gpu::MeshData::skin).
pub fn skin(rdm: &Rdm, base: &Skeleton, i: usize) -> [u8; 8] {
    let (j, w) = (rdm.joints[i], rdm.weights[i]);
    let joint = |k: u8| match &rdm.skeleton {
        Some(own) => (own.hashes.get(k as usize))
            .and_then(|&h| base.find_hash(h))
            .map_or(0, |b| b.min(255) as u8),
        None => k,
    };
    [
        joint(j[0]),
        joint(j[1]),
        joint(j[2]),
        joint(j[3]),
        w[0],
        w[1],
        w[2],
        w[3],
    ]
}
