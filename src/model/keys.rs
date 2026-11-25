// Owns editing a model's bone animation: which animation and time the
// picked joint plays at the playhead, the joint's pose there in its
// parent's space, keying a pose (from the gizmo or typed numbers) into the
// animation, removing a key, a copy of the animation for one model, and a
// new animation in the bind pose. Edited and new animations ride in the
// cfg's undo snapshots and save beside it.

use super::{ModelEditor, playback::Source};
use anno_formats::model::Transform;
use anno_formats::rdm::{Animation, JointPose, KEY_EPSILON};
use anyhow::{Context as _, Result};
use gpui_editor_kit::math::{self, Mat4};
use std::sync::Arc;

/// What the pose tools key into: the picked joint, the animation its part
/// plays now, and where in it the playhead is.
#[derive(Clone, Debug, PartialEq)]
pub struct Keying {
    /// Index into `Model::parts`.
    pub part: usize,
    pub joint: usize,
    /// Index into the part's `<Animations>`, and that animation's path.
    pub animation: usize,
    pub path: String,
    /// Seconds into the animation.
    pub time: f32,
    /// Seconds on the timeline's clock where the animation starts, and
    /// timeline seconds per second of it.
    pub offset: f32,
    pub scale: f32,
}

fn pose_matrix(p: &JointPose) -> Mat4 {
    let m = p.matrix();
    std::array::from_fn(|i| m[i / 4][i % 4])
}

fn pose_of(m: &Mat4) -> JointPose {
    let t = Transform::from_matrix(m);
    JointPose {
        rotation: t.rotation,
        translation: t.position,
    }
}

impl ModelEditor {
    /// The picked joint and the animation its part plays at the playhead.
    /// `None` without a picked joint, or while its part plays nothing.
    pub fn keying(&self) -> Option<Keying> {
        let (element, joint) = self.joint?;
        let part = self.model.parts.iter().position(|p| p.element == element)?;
        let &(_, animation, time, scale) = self.player.playing.iter().find(|(p, ..)| *p == part)?;
        let path = self.model.parts[part].animations.get(animation)?.clone();
        Some(Keying {
            part,
            joint: joint as usize,
            animation,
            path,
            time,
            offset: self.playback.time - time * scale,
            scale,
        })
    }

    /// The animation at `path` as the editor has it: this session's edit,
    /// else the file.
    pub fn animation(&mut self, path: &str) -> Option<Arc<Animation>> {
        if let Some((_, a)) = self.animations.iter().find(|(p, _)| p == path) {
            return Some(a.clone());
        }
        self.player.animation(&self.install, path)
    }

    /// The picked joint in model space at the playhead: where the gizmo
    /// sits while keying.
    pub fn joint_matrix(&self) -> Option<Mat4> {
        let k = self.keying()?;
        let pose = self.player.poses.get(k.part)?.get(k.joint)?;
        let host = self.model.parts[k.part].transform.matrix();
        Some(math::multiply(&host, &pose_matrix(pose)))
    }

    /// The picked joint's pose at the playhead, in its parent's space: what
    /// a key holds.
    pub fn joint_local(&self) -> Option<JointPose> {
        let k = self.keying()?;
        let poses = self.player.poses.get(k.part)?;
        let skeleton = self.model.parts[k.part].mesh.as_ref()?.skeleton.as_ref()?;
        let model = *poses.get(k.joint)?;
        Some(match poses.get(skeleton.parents[k.joint] as usize) {
            Some(parent) => parent.inverse().mul(&model),
            None => model,
        })
    }

    /// Keys the picked joint at the playhead to `world`, its matrix in
    /// model space (the gizmo's result): turned into the parent's space.
    pub fn key_joint_at(&mut self, world: &Mat4, record: bool) -> Result<()> {
        let k = self.keying().context("pick a joint of a part that plays")?;
        let host = self.model.parts[k.part].transform.matrix();
        let inverse = math::inverse(&host).context("the part's placement does not invert")?;
        let model = pose_of(&math::multiply(&inverse, world));
        let skeleton = (self.model.parts[k.part].mesh.as_ref())
            .and_then(|r| r.skeleton.as_ref())
            .context("the part has no skeleton")?;
        let parent = (self.player.poses.get(k.part))
            .and_then(|p| p.get(skeleton.parents[k.joint] as usize))
            .copied();
        let local = match parent {
            Some(parent) => parent.inverse().mul(&model),
            None => model,
        };
        self.key_joint(local, record)
    }

    /// Keys the picked joint's pose in its parent's space at the playhead:
    /// on its track, which is added when the animation has none for it.
    pub fn key_joint(&mut self, local: JointPose, record: bool) -> Result<()> {
        let k = self.keying().context("pick a joint of a part that plays")?;
        let mut a = Animation::clone(
            &*self
                .animation(&k.path)
                .context("the animation does not read")?,
        );
        let skeleton = (self.model.parts[k.part].mesh.as_ref())
            .and_then(|r| r.skeleton.as_ref())
            .context("the part has no skeleton")?;
        let track = match a.track_for(skeleton, k.joint) {
            Some(t) => t,
            None => a.add_track(skeleton, k.joint),
        };
        a.set_key(track, k.time, local);
        self.store_animation(&k.path, a, record)
    }

    /// The key of the picked joint at the playhead, when there is one.
    pub fn joint_key(&mut self) -> Option<(usize, usize)> {
        let k = self.keying()?;
        let a = self.animation(&k.path)?;
        let skeleton = self.model.parts[k.part].mesh.as_ref()?.skeleton.as_ref()?;
        let track = a.track_for(skeleton, k.joint)?;
        let (times, ..) = a.keys(track);
        let key = times
            .iter()
            .position(|t| (t - k.time).abs() <= KEY_EPSILON)?;
        Some((track, key))
    }

    /// Key times of the picked joint's track, seconds on the timeline's
    /// clock: the ruler marks them.
    pub fn joint_key_times(&mut self) -> Vec<f32> {
        let Some(k) = self.keying() else {
            return Vec::new();
        };
        let Some(a) = self.animation(&k.path) else {
            return Vec::new();
        };
        let Some(skeleton) =
            (self.model.parts[k.part].mesh.as_ref()).and_then(|r| r.skeleton.as_ref())
        else {
            return Vec::new();
        };
        (a.track_for(skeleton, k.joint))
            .map(|track| {
                a.keys(track)
                    .0
                    .iter()
                    .map(|t| k.offset + t * k.scale)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Removes the picked joint's key at the playhead.
    pub fn remove_joint_key(&mut self) -> Result<()> {
        let k = self.keying().context("pick a joint of a part that plays")?;
        let (track, key) = self.joint_key().context("no key here")?;
        let mut a = Animation::clone(
            &*self
                .animation(&k.path)
                .context("the animation does not read")?,
        );
        a.remove_key(track, key);
        self.store_animation(&k.path, a, true)
    }

    fn store_animation(&mut self, path: &str, a: Animation, record: bool) -> Result<()> {
        // Refused here, not at save: an edit the file cannot hold.
        a.encode().context("the animation would not write")?;
        if record {
            self.checkpoint();
        }
        let a = Arc::new(a);
        match self.animations.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot.1 = a.clone(),
            None => self.animations.push((path.to_owned(), a.clone())),
        }
        self.player.set_animation(path, a);
        self.pose();
        Ok(())
    }

    /// Points part `part`'s animation `animation` at a copy beside the cfg,
    /// so key edits change it in this model only, not in every model the
    /// game plays it on.
    pub fn copy_animation(&mut self, part: usize, animation: usize) -> Result<String> {
        let entry = self.model.parts.get(part).context("no such part")?.element;
        let list = self
            .doc
            .child(entry, "Animations")
            .context("the part has no animations")?;
        let config = (self.doc.children(list))
            .filter(|&c| self.doc.name(c) == "Config")
            .nth(animation)
            .context("no such animation")?;
        let file = self
            .doc
            .child(config, "FileName")
            .context("the animation names no file")?;
        let from = self.model.parts[part].animations[animation].clone();
        let a = self
            .animation(&from)
            .context("the animation does not read")?;
        let stem = from
            .rsplit('/')
            .next()
            .unwrap_or(&from)
            .trim_end_matches(".rdm");
        let to = self.free_animation_path(stem);
        self.checkpoint();
        self.doc.set_text(file, &to);
        self.animations.push((to.clone(), a.clone()));
        self.player.set_animation(&to, a);
        self.rebuild();
        Ok(to)
    }

    /// `anim/<stem>_NN.rdm` beside the cfg, numbered past every file the
    /// game, the mod or this session already has.
    fn free_animation_path(&self, stem: &str) -> String {
        let dir = self.path.rsplit_once('/').map_or("", |(d, _)| d);
        (1..)
            .map(|n| format!("{dir}/anim/{stem}_{n:02}.rdm"))
            .find(|p| !self.install.exists(p) && !self.animations.iter().any(|(e, _)| e == p))
            .expect("some number is free")
    }

    /// A new animation of `seconds` for part `part`, holding its skeleton's
    /// bind pose ([`Animation::bind_pose`]): saved beside the cfg, added
    /// last to the part's `<Animations>` as one edit, and played alone so
    /// the pose tools key into it at once. Returns its path.
    pub fn new_animation(&mut self, part: usize, seconds: f32) -> Result<String> {
        anyhow::ensure!(seconds > 0., "an animation needs a length");
        let p = self.model.parts.get(part).context("no such part")?;
        let skeleton = (p.mesh.as_ref())
            .and_then(|r| r.skeleton.as_ref())
            .context("the mesh has no skeleton")?;
        let a = Animation::bind_pose(skeleton, seconds);
        a.encode().context("the animation would not write")?;
        let (entry, index) = (p.element, p.animations.len());
        let cfg = self.path.rsplit('/').next().unwrap_or(&self.path);
        let stem = format!("{}_anim", cfg.split('.').next().unwrap_or(cfg));
        let path = self.free_animation_path(&stem);
        self.checkpoint();
        self.push_animation(entry, &path);
        let a = Arc::new(a);
        self.animations.push((path.clone(), a.clone()));
        self.player.set_animation(&path, a);
        self.rebuild();
        self.play(Source::Animation {
            part,
            animation: index,
        });
        Ok(path)
    }

    /// Writes every edited animation into the project beside the cfg; an
    /// animation whose edit was undone since a save gets back what the
    /// project held.
    pub(super) fn stage_animations(&mut self, project: &mut crate::project::Project) -> Result<()> {
        for (path, a) in &self.animations {
            let bytes = a.encode().with_context(|| format!("writing {path}"))?;
            if !self.files_before.iter().any(|(p, _)| p == path) {
                let before = project.read_file(path).map(|b| b.into_owned());
                self.files_before.push((path.clone(), before));
            }
            project.stage(path, bytes);
        }
        for (path, before) in &self.files_before {
            if !path.ends_with(".rdm") || self.animations.iter().any(|(p, _)| p == path) {
                continue;
            }
            match before {
                Some(bytes) => project.stage(path, bytes.clone()),
                None => {
                    project.remove_file(path);
                }
            }
        }
        Ok(())
    }
}
