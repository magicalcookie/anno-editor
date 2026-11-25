// Owns posing the open model at the playhead: which sequence or part
// animation plays, the animation and particle files read once, skinning
// meshes on the CPU, hanging bone-linked entries on their joints, the
// parts' own motion, visibility by sequence conditions, and particle frames
// drawn as camera-facing sprites.
//
// Skinning runs on the CPU and rebuilds each skinned piece's mesh a frame:
// an editor shows one model, a unit is a few thousand vertices, and the
// renderer's families need no skinning stage for it.
// TODO(alex): GPU skinning in the kit once the island draws units too.
// Engine addresses here are Anno117.exe build 24988830 (Steam).

use super::ModelEditor;
use crate::island::models::Piece;
use anno_formats::cloth::{self, ClothMesh, ClothState};
use anno_formats::efc::{self, Efc, Launch};
use anno_formats::model::{BoneLink, ColorRun, Model, Play, Rig, Scene, Transform};
use anno_formats::rdm::{Animation, JointPose, Rdm};
use anno_formats::rdp::{self, Rdp};
use anno_formats::vfs::Install;
use anno_formats::xml::Id;
use gpui_editor_kit::{
    gpu::{self, Batch, Instances, Mesh, MeshData},
    math::{self, Mat4, Vec3},
    texture::{Dds, Image, Texture},
};
use std::sync::Arc;

/// Particle textures load at most this wide: sprites are small on screen.
const SPRITE_TEXTURE: u32 = 256;
/// A flipbook sheet holds every frame, so it loads wider: 1800's oil fire
/// is 15 frames side by side in 3840 × 256, which at 256 across kept 16
/// texels a frame.
const FLIPBOOK_TEXTURE: u32 = 2048;

/// What plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Nothing posed: the bind pose, with motion and particles running.
    Still,
    /// `Model::sequences[k]`, the cfg's own timeline for one state.
    Sequence(usize),
    /// One part's animation alone: `parts[part].animations[animation]`.
    Animation { part: usize, animation: usize },
}

/// The playback clock: what plays, where the playhead is, how it moves.
#[derive(Clone, Copy, Debug)]
pub struct Playback {
    pub source: Source,
    pub playing: bool,
    /// Seconds on the source's clock.
    pub time: f32,
    /// The timeline's rate, over the sequence's own `Speed`.
    pub speed: f32,
    pub looping: bool,
    /// The scene's wind for particles and cloth: strength as the game's
    /// global wind (it caps particle drift at 2), and where it blows to,
    /// degrees from +X toward +Z.
    pub wind: f32,
    pub wind_direction: f32,
    /// Sequences' sounds play while this is on.
    pub sound: bool,
}

impl Playback {
    /// The wind as a vector along the ground, its length the strength the
    /// particle shader takes (at most 2).
    pub fn wind_vector(&self) -> Vec3 {
        let (s, c) = self.wind_direction.to_radians().sin_cos();
        let w = self.wind.clamp(0., 2.);
        [c * w, 0., s * w]
    }
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            source: Source::Still,
            playing: false,
            time: 0.,
            speed: 1.,
            looping: true,
            wind: 1.,
            wind_direction: 0.,
            sound: true,
        }
    }
}

/// A piece as playback draws it this frame.
#[derive(Clone)]
pub struct Posed {
    /// A skinned mesh in place of the bind pose.
    pub mesh: Option<Arc<Mesh>>,
    pub local: Mat4,
    pub shown: bool,
}

/// What posing keeps between frames.
#[derive(Default)]
pub struct Player {
    /// Animation rdms and particle effects by path, read on first use;
    /// `None` when missing or broken (said once in `problems`).
    animations: Vec<(String, Option<Arc<Animation>>)>,
    effects: Vec<(String, Option<Arc<Rdp>>)>,
    /// 2070 `.efc` effects, read and baked on first use or edit.
    efcs: Vec<(String, Option<Arc<Baked>>)>,
    /// Flipbooks with their ramps baked in, by flipbook and ramp paths.
    ramped: Vec<(String, Option<Image>)>,
    pub problems: Vec<String>,
    skins: Vec<Skin>,
    /// Model-space joint poses per part this frame; empty for a part
    /// that plays nothing.
    pub(super) poses: Vec<Vec<JointPose>>,
    /// Scratch: a part's skinning transforms, an effect's live particles.
    skinning: Vec<JointPose>,
    frame: rdp::Frame,
    /// The source's length in seconds, set when the source changes.
    pub length: f32,
    /// The cfg's `<Clothes>`, simulated while playback runs.
    cloths: Vec<ClothRun>,
    /// What played at the last pose: part, animation index, seconds into
    /// the animation, and how many timeline seconds one of those lasts
    /// (its `Scale` in a sequence). The pose tools key there.
    pub(super) playing: Vec<(usize, usize, f32, f32)>,
    /// Each entry's COLOR_TRANSFORM as it blends between its states.
    pub(super) colors: Vec<(Id, ColorRun)>,
    /// Playback seconds the colours have yet to step.
    due: f32,
    /// Seconds since the model's own effects, the particles no clip runs,
    /// last started. Like the game's, they keep this clock through a
    /// sequence's loops and switches.
    own_time: f32,
    /// Whether a sequence that restarts effects started them here. Until
    /// one does they started before the model was opened, so a one-shot
    /// one has burned out: the airship's wreck explosion doesn't fire each
    /// time a sequence starts.
    own_restarted: bool,
}

/// The damage the editor shows models at: none, as the game builds them.
const DAMAGE: f32 = 0.;

impl Player {
    /// The colour entry `element`'s COLOR_TRANSFORM gives it now; white
    /// without one.
    pub(super) fn color(&self, element: Id) -> [f32; 4] {
        (self.colors.iter())
            .find(|(e, _)| *e == element)
            .map_or([1.; 4], |(_, c)| c.color)
    }
}

/// A 2070 `.efc`, its emitters, and the run each bakes to (`None` for
/// those not drawn): what playback draws of it and the inspector shows.
pub struct Baked {
    pub efc: Arc<Efc>,
    pub effects: Vec<efc::Effect>,
    pub runs: Vec<Option<Arc<Rdp>>>,
}

impl Baked {
    fn new(efc: Arc<Efc>) -> Baked {
        let effects = efc.effects();
        // Each emitter rolls its own: seeds 1, 2, … as MSVC's srand would.
        let runs = (effects.iter().enumerate())
            .map(|(k, e)| e.bake(&Launch::default(), k as u32 + 1).map(Arc::new))
            .collect();
        Baked { efc, effects, runs }
    }
}

/// A `<Clothes>` entry running: its built mesh and simulation, and what
/// each frame draws with (the rdm's UVs, triangles both ways round, the
/// cloth's textures).
struct ClothRun {
    cloth: cloth::Cloth,
    mesh: ClothMesh,
    state: ClothState,
    data: MeshData,
}

/// Cloth textures load at most this wide.
const CLOTH_TEXTURE: u32 = 1024;

/// A skinned piece: its mesh rebuilt each frame from its part's rdm.
struct Skin {
    piece: usize,
    part: usize,
    /// The rdm vertex behind each of the mesh's vertices.
    order: Vec<u32>,
    /// Materials, maps and the bind pose; positions and normals are
    /// rewritten each frame.
    data: MeshData,
}

impl Player {
    /// Finds the skinned pieces again after a rebuild. A piece of a part
    /// with joints gets its group's vertices in first-use order, as the
    /// island's loader numbers them, with the built mesh's materials.
    pub fn bind(&mut self, pieces: &[Piece], model: &Model) {
        self.skins.clear();
        for (i, piece) in pieces.iter().enumerate() {
            let Some(part) = model
                .parts
                .iter()
                .position(|p| Some(p.element) == piece.element)
            else {
                continue;
            };
            let Some(rdm) = &model.parts[part].mesh else {
                continue;
            };
            if rdm.skeleton.is_none() || rdm.joints.len() != rdm.positions.len() {
                continue;
            }
            let Some(group) = rdm.groups.get(piece.group as usize) else {
                continue;
            };
            let range = group.start as usize..(group.start + group.count) as usize;
            let Some(indices) = rdm.indices.get(range) else {
                continue;
            };
            let mut order = Vec::new();
            let mut remap = std::collections::HashMap::new();
            let mut data = piece.mesh.data().clone();
            data.indices = indices
                .iter()
                .map(|&v| {
                    *remap.entry(v).or_insert_with(|| {
                        order.push(v);
                        order.len() as u32 - 1
                    })
                })
                .collect();
            let tint = piece
                .mesh
                .geometry()
                .colors
                .first()
                .copied()
                .unwrap_or(0xffffff);
            data.positions = order.iter().map(|&v| rdm.positions[v as usize]).collect();
            data.normals = (order.iter())
                .map(|&v| rdm.normals.get(v as usize).copied().unwrap_or([0., 1., 0.]))
                .collect();
            data.uvs = match data.texture.is_some() {
                true => order
                    .iter()
                    .map(|&v| rdm.uvs.get(v as usize).copied().unwrap_or_default())
                    .collect(),
                false => Vec::new(),
            };
            data.colors = vec![tint; order.len()];
            self.skins.push(Skin {
                piece: i,
                part,
                order,
                data,
            });
        }
    }

    pub(super) fn animation(&mut self, install: &Install, path: &str) -> Option<Arc<Animation>> {
        if let Some((_, a)) = self.animations.iter().find(|(p, _)| p == path) {
            return a.clone();
        }
        let read = install.read(path).and_then(|b| Animation::parse(&b));
        let a = read
            .map_err(|e| self.problems.push(format!("{path}: {e:#}")))
            .ok()
            .map(Arc::new);
        self.animations.push((path.to_owned(), a.clone()));
        a
    }

    /// Starts every `<Clothes>` entry of `doc` at rest (formats::cloth).
    pub(super) fn bind_cloth(
        &mut self,
        doc: &anno_formats::xml::Doc,
        install: &Install,
        cache: &crate::island::models::Cache,
    ) {
        self.cloths.clear();
        let colliders = cloth::colliders(doc);
        for c in cloth::clothes(doc) {
            let rdm = match install.read(&c.mesh).and_then(|b| Rdm::parse(&b)) {
                Ok(rdm) => rdm,
                Err(e) => {
                    self.problems.push(format!("{}: {e:#}", c.mesh));
                    continue;
                }
            };
            let mesh = ClothMesh::build(&rdm);
            let state = ClothState::new(&mesh, &c, &colliders);
            let texture = |t: &Option<String>| {
                (t.as_deref()).and_then(|t| cache.texture(install, t, CLOTH_TEXTURE))
            };
            let [r, g, b] = c.material.color.map(|v| (v.clamp(0., 1.) * 255.) as u32);
            let texture_map = texture(&c.material.diffuse);
            // Cloth draws double-sided: each triangle both ways round.
            let mut indices = mesh.triangles.clone();
            indices.extend(mesh.triangles.chunks(3).flat_map(|t| [t[0], t[2], t[1]]));
            let data = MeshData {
                uvs: match texture_map.is_some() {
                    true => rdm.uvs.clone(),
                    false => Vec::new(),
                },
                colors: vec![r << 16 | g << 8 | b; rdm.positions.len()],
                normal_map: texture_map.as_ref().and(texture(&c.material.normal)),
                texture: texture_map,
                // Albedo alpha carries gloss on cloth as on other models.
                ignore_alpha: true,
                indices,
                material: gpu::Material::Lit,
                ..Default::default()
            };
            self.cloths.push(ClothRun {
                cloth: c,
                mesh,
                state,
                data,
            });
        }
    }

    /// An edit of the effect at `path` replaces the file's.
    pub(super) fn set_effect(&mut self, path: &str, rdp: Arc<Rdp>) {
        match self.effects.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot.1 = Some(rdp),
            None => self.effects.push((path.to_owned(), Some(rdp))),
        }
    }

    /// An edit of the `.efc` at `path` replaces the file's, baked again.
    pub(super) fn set_efc(&mut self, path: &str, efc: Arc<Efc>) {
        let baked = Some(Arc::new(Baked::new(efc)));
        match self.efcs.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot.1 = baked,
            None => self.efcs.push((path.to_owned(), baked)),
        }
    }

    /// After undo: the edited effects and animations as they now are; the
    /// rest read their files again.
    pub(super) fn reset_edits(
        &mut self,
        effects: &[(String, Arc<Rdp>)],
        efcs: &[(String, Arc<Efc>)],
        animations: &[(String, Arc<Animation>)],
    ) {
        self.effects.clear();
        for (path, rdp) in effects {
            self.set_effect(path, rdp.clone());
        }
        self.efcs.clear();
        for (path, efc) in efcs {
            self.set_efc(path, efc.clone());
        }
        self.animations.clear();
        for (path, a) in animations {
            self.set_animation(path, a.clone());
        }
    }

    /// An edit of the animation at `path` replaces the file's.
    pub(super) fn set_animation(&mut self, path: &str, a: Arc<Animation>) {
        match self.animations.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot.1 = Some(a),
            None => self.animations.push((path.to_owned(), Some(a))),
        }
    }

    /// A material's flipbook as the game colours it: the texture's red
    /// picks the albedo ramp's colour, its green adds the emissive ramp's,
    /// its alpha stays. The kit's shaders take no ramps, so the editor
    /// bakes them into a copy once per flipbook. `None` without ramps or
    /// when a file does not read; the flipbook then draws as stored.
    fn ramped(&mut self, install: &Install, m: &rdp::Material) -> Option<Image> {
        if m.movie_tex_path.is_empty() || m.albedo_ramp_path.is_empty() {
            return None;
        }
        let key = format!(
            "{}|{}|{}",
            m.movie_tex_path, m.albedo_ramp_path, m.emissive_ramp_path
        );
        if let Some((_, image)) = self.ramped.iter().find(|(k, _)| *k == key) {
            return image.clone();
        }
        let read = |source: &str, side: u32| -> Option<Texture> {
            let path = anno_formats::model::texture_path(source);
            let bytes = install.read(&path).ok()?;
            let dds = Dds::parse(&bytes).ok()?;
            // The first level at most `side` wide: a preview sprite is small.
            let level = (0..dds.mips)
                .find(|&l| (dds.width >> l) <= side)
                .unwrap_or(0);
            dds.decode(&bytes, level).ok()
        };
        let baked = (|| {
            let book = read(&m.movie_tex_path, 1024)?;
            let albedo = read(&m.albedo_ramp_path, 256)?;
            let emissive = (!m.emissive_ramp_path.is_empty())
                .then(|| read(&m.emissive_ramp_path, 256))
                .flatten();
            // A ramp's first row, at a 0..1 coordinate.
            let at = |ramp: &Texture, u: u8| {
                let x = (usize::from(u) * (ramp.width as usize - 1)) / 255;
                let i = x * 4;
                [ramp.rgba[i], ramp.rgba[i + 1], ramp.rgba[i + 2]]
            };
            let mut rgba = book.rgba.clone();
            for px in rgba.as_chunks_mut::<4>().0 {
                let base = at(&albedo, px[0]);
                let glow = emissive.as_ref().map_or([0; 3], |e| at(e, px[1]));
                for c in 0..3 {
                    px[c] = base[c].saturating_add(glow[c]);
                }
            }
            Some(Image::from(
                Texture::from_rgba(book.width, book.height, rgba).ok()?,
            ))
        })();
        self.ramped.push((key, baked.clone()));
        baked
    }

    /// The `.efc` at `path`, read and baked on first use.
    pub(super) fn baked(&mut self, install: &Install, path: &str) -> Option<Arc<Baked>> {
        if let Some((_, b)) = self.efcs.iter().find(|(p, _)| p == path) {
            return b.clone();
        }
        let read = install.read(path).and_then(|b| Efc::parse(&b));
        let b = read
            .map_err(|e| self.problems.push(format!("{path}: {e:#}")))
            .ok()
            .map(|efc| Arc::new(Baked::new(Arc::new(efc))));
        self.efcs.push((path.to_owned(), b.clone()));
        b
    }

    pub(super) fn effect(&mut self, install: &Install, path: &str) -> Option<Arc<Rdp>> {
        if let Some((_, e)) = self.effects.iter().find(|(p, _)| p == path) {
            return e.clone();
        }
        let read = install.read(path).and_then(|b| Rdp::parse(&b));
        let e = read
            .map_err(|e| self.problems.push(format!("{path}: {e:#}")))
            .ok()
            .map(Arc::new);
        self.effects.push((path.to_owned(), e.clone()));
        e
    }
}

fn flat(m: [[f32; 4]; 4]) -> Mat4 {
    std::array::from_fn(|i| m[i / 4][i % 4])
}

fn rotate(q: [f32; 4], v: Vec3) -> Vec3 {
    JointPose {
        rotation: q,
        translation: [0.; 3],
    }
    .apply(v)
}

/// Seconds into an animation of `duration` that a clip shows `local`
/// seconds (of the animation's own clock) after it starts: repeating while
/// it has plays left, `loops` of them (0: forever), then holding the end.
/// The game's per-frame step (`sub_142456800`), in seconds for its ms.
pub(super) fn clip_time(local: f32, duration: f32, loops: u32) -> f32 {
    if duration <= 0. {
        return 0.;
    }
    match loops == 0 || local / duration < loops as f32 {
        true => local.max(0.) % duration,
        false => duration,
    }
}

/// What alignment and springs read: the view's camera as the game's camera
/// rotation (+Z into the screen; the Model view is mirrored to the game's
/// left-handed space, so right × up points there), the timeline's wind, the
/// time of day, and the view's ground at 0.
fn scene(camera: &gpui_editor_kit::camera::Camera, wind: Vec3) -> Scene {
    let (right, down) = (camera.right(), camera.down());
    let up = down.map(|v| -v);
    let ahead = math::cross(right, up);
    let basis: Mat4 = [
        right[0], right[1], right[2], 0., up[0], up[1], up[2], 0., ahead[0], ahead[1], ahead[2],
        0., 0., 0., 0., 1.,
    ];
    // The game reads the PC's local time; the editor has no time zone, so
    // its clock hands show UTC. No shipped 117 cfg has one.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Scene {
        camera: Transform::from_matrix(&basis).rotation,
        wind,
        clock: [
            (now / 3600 % 24) as u32,
            (now / 60 % 60) as u32,
            (now % 60) as u32,
        ],
        ground: 0.,
    }
}

impl ModelEditor {
    /// Plays `source` from its start.
    pub fn play(&mut self, source: Source) {
        self.playback.source = source;
        self.playback.time = 0.;
        if let Source::Sequence(k) = source
            && self
                .model
                .sequences
                .get(k)
                .is_some_and(|s| s.restart_effects)
        {
            (self.player.own_time, self.player.own_restarted) = (0., true);
        }
        self.player.length = match source {
            Source::Still => 0.,
            Source::Sequence(k) => self.model.sequences.get(k).map_or(0., |s| s.length()),
            Source::Animation { part, animation } => {
                let path = (self.model.parts.get(part)).and_then(|p| p.animations.get(animation));
                let path = path.cloned();
                (path.and_then(|p| self.player.animation(&self.install, &p)))
                    .map_or(0., |a| a.duration)
            }
        };
        self.pose();
    }

    /// The source's length in seconds; 0 when nothing plays.
    pub fn clip_length(&self) -> f32 {
        self.player.length
    }

    /// Moves the playhead by `dt` seconds of wall time and poses the model
    /// there. A sequence's clock runs at its own `Speed`.
    pub fn advance(&mut self, dt: f32) {
        let rate = match self.playback.source {
            Source::Sequence(k) => self.model.sequences.get(k).map_or(1., |s| s.speed),
            _ => 1.,
        };
        let from = self.playback.time;
        self.playback.time += dt * self.playback.speed * rate;
        // Sound clips the playhead crossed, before `pose` wraps it.
        let due = self.sounds_between(from, self.playback.time);
        self.sounds_due.extend(due);
        // Cloth steps on the frame's own time, as the game's does.
        let wind = self.playback.wind_vector();
        for run in &mut self.player.cloths {
            run.state.step(&run.mesh, dt * self.playback.speed, wind);
        }
        self.lights.advance(dt * self.playback.speed);
        self.player.due += dt * self.playback.speed;
        self.player.own_time += dt * self.playback.speed;
        self.pose();
    }

    /// Poses the model at the playhead and hands the view this frame.
    pub fn pose(&mut self) {
        let length = self.player.length;
        let p = &mut self.playback;
        if length > 0. && p.time > length {
            match p.looping {
                true => p.time %= length,
                false => (p.time, p.playing) = (length, false),
            }
        }
        let t = p.time;
        let m = &self.model;
        // What plays: animations by part (with the plays a clip has before
        // it holds), which particles run, which condition bits are up.
        // An effect runs as (particle, seconds into it, whether a one-shot
        // one has burned out). Still runs every effect on the playhead.
        let mut plays: Vec<(usize, usize, f32, f32, u32)> = Vec::new();
        let mut effects: Option<Vec<(usize, f32, bool)>> = None;
        let mut conditions: Option<u32> = None;
        match p.source {
            Source::Still => {}
            Source::Animation { part, animation } => {
                plays.push((part, animation, t, 1., u32::from(!p.looping)));
                effects = Some(Vec::new());
            }
            Source::Sequence(k) => {
                let Some(s) = m.sequences.get(k) else {
                    return;
                };
                let (mut on, mut bits) = (Vec::new(), 0);
                for c in &s.clips {
                    // A clip that ends with the sequence holds its last frame.
                    let live = t >= c.start && (t < c.end || c.end >= length);
                    if !live {
                        continue;
                    }
                    match c.what {
                        // A clip plays its animation `Scale` times slower,
                        // `LoopCount` times; 0 is forever in a looped
                        // sequence and once in another (`sub_1422B2500`).
                        Play::Animation { model, animation } => {
                            let part = m.parts.get(model as usize);
                            let scale = (part
                                .and_then(|p| p.animation_scales.get(animation as usize)))
                            .copied()
                            .unwrap_or(1.);
                            let loops = (part
                                .and_then(|p| p.animation_loops.get(animation as usize)))
                            .copied()
                            .unwrap_or(0);
                            let loops = match loops {
                                0 => u32::from(!s.looped),
                                n => n,
                            };
                            plays.push((
                                model as usize,
                                animation as usize,
                                (t - c.start) / scale,
                                scale,
                                loops,
                            ))
                        }
                        Play::Particle(k) => on.push((k as usize, t - c.start, false)),
                        Play::Condition(b) => bits |= b,
                        Play::Sound(_) | Play::Other(_) => {}
                    }
                }
                effects = Some(on);
                conditions = Some(bits);
            }
        }
        // An effect a sequence starts runs only while its clip does. The
        // model's own, which no clip runs, run on their own clock unless the
        // sequence disables them: a production building smokes only while
        // it works, and a one-shot effect doesn't fire on every loop.
        let disabled = matches!(p.source,
            Source::Sequence(k) if m.sequences.get(k).is_some_and(|s| s.disable_effects));
        if let Some(on) = &mut effects
            && !disabled
        {
            let named = |k: usize| {
                (m.sequences.iter().flat_map(|s| &s.clips))
                    .any(|c| c.what == Play::Particle(k as u32))
            };
            let (own, burned) = (self.player.own_time, !self.player.own_restarted);
            on.extend(
                (0..m.particles.len())
                    .filter(|&k| !named(k))
                    .map(|k| (k, own, burned)),
            );
        }
        let rig = &mut self.player;
        // Colours step on playback's time; paused or scrubbed they stand
        // where the playhead's conditions take them.
        let (raised, snap) = (conditions.unwrap_or(0), !p.playing);
        let dt = std::mem::take(&mut rig.due);
        let rigs = (m.parts.iter().map(|p| (p.element, p.rig)))
            .chain(m.props.iter().map(|p| (p.element, p.rig)))
            .chain(m.lights.iter().map(|p| (p.element, p.rig)))
            .chain(m.particles.iter().map(|p| (p.element, p.rig)));
        let colors = rigs
            .filter_map(|(element, r)| Some((element, r.color?)))
            .map(|(element, c)| {
                let kept = rig.colors.iter().find(|(e, _)| *e == element);
                let mut run = kept.map_or_else(|| ColorRun::new(&c, raised), |(_, run)| *run);
                run.step(&c, raised, dt, snap);
                (element, run)
            })
            .collect();
        rig.colors = colors;
        rig.poses.resize_with(m.parts.len(), Vec::new);
        rig.poses.iter_mut().for_each(Vec::clear);
        rig.playing.clear();
        for (part, animation, local, scale, loops) in plays {
            let Some(skeleton) = (m.parts.get(part))
                .and_then(|p| p.mesh.as_ref())
                .and_then(|r| r.skeleton.as_ref())
            else {
                continue;
            };
            let Some(path) = m.parts[part].animations.get(animation) else {
                continue;
            };
            let Some(anim) = rig.animation(&self.install, path) else {
                continue;
            };
            let local = clip_time(local, anim.duration, loops);
            rig.playing.push((part, animation, local, scale));
            let pose = &mut rig.poses[part];
            pose.extend_from_slice(&skeleton.bind_local);
            anim.sample(local, skeleton, pose);
            skeleton.to_model(pose);
        }
        let mut posed: Vec<Posed> = (self.pieces.iter())
            .map(|p| Posed {
                mesh: None,
                local: p.local,
                shown: true,
            })
            .collect();
        for skin in &rig.skins {
            let pose = &rig.poses[skin.part];
            let Some(rdm) = m.parts[skin.part].mesh.as_ref() else {
                continue;
            };
            let Some(skeleton) = rdm.skeleton.as_ref() else {
                continue;
            };
            if pose.len() != skeleton.len() {
                continue;
            }
            rig.skinning.clear();
            rig.skinning.extend(
                pose.iter()
                    .zip(&skeleton.inverse_bind)
                    .map(|(m, ib)| m.mul(ib)),
            );
            let mut data = skin.data.clone();
            for (k, &v) in skin.order.iter().enumerate() {
                let v = v as usize;
                let (joints, weights) = (rdm.joints[v], rdm.weights[v]);
                let total: f32 = weights.iter().map(|&w| f32::from(w)).sum();
                if total <= 0. {
                    continue;
                }
                let (mut at, mut normal) = ([0.; 3], [0.; 3]);
                for (&j, &w) in joints.iter().zip(&weights).filter(|(_, w)| **w > 0) {
                    let Some(s) = rig.skinning.get(j as usize) else {
                        continue;
                    };
                    let w = f32::from(w) / total;
                    let (p, n) = (
                        s.apply(rdm.positions[v]),
                        rotate(s.rotation, data.normals[k]),
                    );
                    for a in 0..3 {
                        at[a] += p[a] * w;
                        normal[a] += n[a] * w;
                    }
                }
                data.positions[k] = at;
                data.normals[k] = math::normalize(normal).unwrap_or([0., 1., 0.]);
            }
            if let Ok(mesh) = Mesh::new(data) {
                posed[skin.piece].mesh = Some(Arc::new(mesh));
            }
        }
        // Entries on joints, entries that move, entries a condition shows.
        let entries = (m.parts.iter().map(|p| (p.element, p.transform, p.rig)))
            .chain(m.props.iter().map(|p| (p.element, p.transform, p.rig)))
            .chain(m.lights.iter().map(|p| (p.element, p.transform, p.rig)))
            .chain(m.particles.iter().map(|p| (p.element, p.transform, p.rig)));
        let mut placed: Vec<(Id, Mat4)> = Vec::new();
        let around = scene(&self.view.camera, self.playback.wind_vector());
        for (element, at, r) in entries {
            // A DAMAGE_TRANSFORM scales the entry about its origin; a
            // COLOR_TRANSFORM's alpha fades it, which a mesh shows only as
            // drawn or not.
            let damage = r.damage.map_or(1., |d| d.scale_at(DAMAGE));
            if damage <= 0. || rig.color(element)[3] <= 0. {
                hide(&self.pieces, &mut posed, element, true);
            }
            let change = entry_change(m, &rig.poses, &at, &r, t, &around);
            let change = match damage != 1. {
                true => {
                    let p = at.matrix();
                    let inverse = math::inverse(&p).unwrap_or(math::IDENTITY);
                    let sized = Transform {
                        scale: [damage; 3],
                        ..Transform::IDENTITY
                    };
                    let s = math::multiply(&p, &math::multiply(&sized.matrix(), &inverse));
                    Some(change.map_or(s, |c| math::multiply(&c, &s)))
                }
                false => change,
            };
            let Some(change) = change else {
                if let (Some(bits), Some(need)) = (conditions, r.shown_if) {
                    hide(&self.pieces, &mut posed, element, bits & need == 0);
                }
                continue;
            };
            for (piece, pose) in self.pieces.iter().zip(&mut posed) {
                if piece.element == Some(element) {
                    pose.local = math::multiply(&change, &pose.local);
                }
            }
            if let (Some(bits), Some(need)) = (conditions, r.shown_if) {
                hide(&self.pieces, &mut posed, element, bits & need == 0);
            }
            placed.push((element, change));
        }
        // Particle frames as quads, one mesh per material: each its own
        // texture, blending and facing.
        let camera = self.view.camera;
        let (right, down) = (camera.right(), camera.down());
        let wind = self.playback.wind_vector();
        let mut sprites = Vec::new();
        for (k, particle) in m.particles.iter().enumerate() {
            let run = match &effects {
                None => Some((t, false)),
                Some(on) => on.iter().find(|e| e.0 == k).map(|&(_, l, b)| (l, b)),
            };
            let Some((local, burned)) = run else {
                continue;
            };
            if let (Some(bits), Some(need)) = (conditions, particle.rig.shown_if)
                && bits & need == 0
            {
                continue;
            }
            // Undamaged or clear, it draws nothing.
            let color = rig.color(particle.element);
            let damage = (particle.rig.damage).map_or(1., |d| d.scale_at(DAMAGE));
            if color[3] <= 0. || damage <= 0. {
                continue;
            }
            // A 2070 `.efc` plays each of its emitters' baked runs.
            let runs: Vec<Arc<Rdp>> = match particle.file.ends_with(".efc") {
                true => (rig.baked(&self.install, &particle.file))
                    .map(|b| b.runs.iter().flatten().cloned().collect())
                    .unwrap_or_default(),
                false => rig
                    .effect(&self.install, &particle.file)
                    .into_iter()
                    .collect(),
            };
            for effect in runs {
                if burned && !effect.looped {
                    continue;
                }
                // Effect time runs `TimeScale` times slower than the clock.
                let time = local / particle.time_scale;
                effect.frame(time, &mut rig.frame);
                let change = (placed.iter().find(|(e, _)| *e == particle.element))
                    .map_or(math::IDENTITY, |(_, c)| *c);
                let look = Look {
                    at: math::multiply(&change, &particle.transform.matrix()),
                    tint: std::array::from_fn(|k| particle.tint[k] * color[k]),
                    sub_images: particle.sub_images,
                    time,
                    right,
                    down,
                    // docs/particle-format.md, Playback step 4.
                    wind: wind.map(|w| {
                        w * effect.frame_time * particle.time_scale * particle.wind_impact * 0.0002
                    }),
                };
                for (k, material) in effect.materials.iter().enumerate() {
                    // Refraction bends what is behind, ripple particles draw
                    // into the water simulation: neither adds colour of its own.
                    if material.refractive || material.wave_particle || material.churn_particle {
                        continue;
                    }
                    let (path, side) = match material.movie_tex_path.is_empty() {
                        true => (effect.texture_path.as_str(), SPRITE_TEXTURE),
                        false => (material.movie_tex_path.as_str(), FLIPBOOK_TEXTURE),
                    };
                    let texture = rig.ramped(&self.install, material).or_else(|| {
                        (!path.is_empty())
                            .then(|| self.cache.texture(&self.install, path, side))
                            .flatten()
                    });
                    let Some(mesh) = sprites_mesh(&effect, &rig.frame, k as u8, &look, texture)
                    else {
                        continue;
                    };
                    sprites.push(Batch::new(
                        Arc::new(mesh),
                        Arc::new(Instances {
                            matrices: vec![math::IDENTITY],
                            ids: vec![Some(u64::from(particle.element.raw()))],
                        }),
                    ));
                }
            }
        }
        let still = matches!(self.playback.source, Source::Still)
            && sprites.is_empty()
            && placed.is_empty();
        self.view.posed = if still { Vec::new() } else { posed };
        // Cloth as it last stepped, on its joint when it hangs on one.
        for run in &mut rig.cloths {
            let at = match run.cloth.link.and_then(|l| joint(m, &rig.poses, l)) {
                Some(j) => {
                    Transform::from_matrix(&math::multiply(&j, &run.cloth.transform.matrix()))
                }
                None => run.cloth.transform,
            };
            run.state.placement = at;
            let mut data = run.data.clone();
            data.positions = run.state.positions().to_vec();
            data.normals = run.state.normals().to_vec();
            if let Ok(mesh) = Mesh::new(data) {
                sprites.push(Batch::new(
                    Arc::new(mesh),
                    Arc::new(Instances {
                        matrices: vec![at.matrix()],
                        ids: vec![Some(u64::from(run.cloth.element.raw()))],
                    }),
                ));
            }
        }
        self.view.sprites = sprites;
        self.view.overlay = self.skeleton_lines();
        self.light_up(conditions, &placed);
        match self.playback.playing {
            true => self.view.present_frame(self.selected),
            false => self.view.present(self.selected),
        }
    }

    /// The selected part's bones as lines, posed: what the timeline plays
    /// is easier to read on the skeleton than on a dense mesh.
    fn skeleton_lines(&self) -> Vec<[Vec3; 2]> {
        // A picked joint's part, else the selected part.
        let shown = self.joint.map(|(e, _)| e).or(self.selected);
        let Some(part) = shown.and_then(|s| self.model.parts.iter().position(|p| p.element == s))
        else {
            return Vec::new();
        };
        let p = &self.model.parts[part];
        let Some(skeleton) = p.mesh.as_ref().and_then(|r| r.skeleton.as_ref()) else {
            return Vec::new();
        };
        let pose = match self.player.poses.get(part) {
            Some(pose) if pose.len() == skeleton.len() => &pose[..],
            _ => &skeleton.bind[..],
        };
        let at = p.transform.matrix();
        let place = |j: usize| math::transform(&at, pose[j].translation);
        let mut lines: Vec<[Vec3; 2]> = (0..skeleton.len())
            .filter(|&j| (skeleton.parents[j] as usize) < skeleton.len())
            .map(|j| [place(skeleton.parents[j] as usize), place(j)])
            .collect();
        // The joint picked in the hierarchy: a small cross on it.
        if let Some((_, j)) = self.joint.filter(|(e, _)| *e == p.element)
            && (j as usize) < pose.len()
        {
            let c = place(j as usize);
            let r = (self.view.extent.size * 0.02).max(0.01);
            for a in 0..3 {
                let (mut lo, mut hi) = (c, c);
                lo[a] -= r;
                hi[a] += r;
                lines.push([lo, hi]);
            }
        }
        lines
    }

    /// What the timeline says under its ruler.
    pub fn timeline_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.model.sequences.is_empty()
            && self.model.parts.iter().all(|p| p.animations.is_empty())
        {
            notes.push("No sequences or animations: Play runs particles and motion".to_owned());
        }
        notes.extend(self.player.problems.iter().take(4).cloned());
        notes
    }
}

// How an entry's pieces move from where the cfg places them: onto the joint
// it hangs on, then by its own motion (`Motion::place`), worked out on the
// placement in model space so a billboard faces the camera on a joint too.
// `None` when they stay.
fn entry_change(
    m: &Model,
    poses: &[Vec<JointPose>],
    at: &Transform,
    rig: &Rig,
    t: f32,
    around: &Scene,
) -> Option<Mat4> {
    let link = rig.link.and_then(|l| joint(m, poses, l));
    let Some(motion) = rig.motion else {
        return link;
    };
    let base = match link {
        Some(l) => math::multiply(&l, &at.matrix()),
        None => at.matrix(),
    };
    let moved = motion.place(t, &Transform::from_matrix(&base), around);
    let inverse = math::inverse(&base)?;
    let change = math::multiply(&moved.matrix(), &inverse);
    Some(match link {
        Some(l) => math::multiply(&change, &l),
        None => change,
    })
}

impl ModelEditor {
    /// Whether some entry turns to the camera, so the view poses again as
    /// it orbits.
    pub fn faces_camera(&self) -> bool {
        use anno_formats::model::Alignment;
        let m = &self.model;
        (m.parts.iter().map(|p| p.rig))
            .chain(m.props.iter().map(|p| p.rig))
            .chain(m.lights.iter().map(|p| p.rig))
            .chain(m.particles.iter().map(|p| p.rig))
            .filter_map(|r| r.motion)
            .any(|mo| matches!(mo.alignment, Alignment::Camera | Alignment::CameraYaw))
    }
}

/// Where joint `link.bone` of `Models[link.model]` is, in model space: the
/// host part's placement times the joint's pose (playing, else bind).
pub(super) fn joint(m: &Model, poses: &[Vec<JointPose>], link: BoneLink) -> Option<Mat4> {
    let host = m.parts.get(link.model as usize)?;
    let skeleton = host.mesh.as_ref()?.skeleton.as_ref()?;
    joint_at(
        m,
        poses,
        link.model as usize,
        skeleton.find_hash(link.bone)?,
    )
}

/// [`joint`] by index: joint `j` of `parts[part]`, as a sound clip names it.
pub(super) fn joint_at(m: &Model, poses: &[Vec<JointPose>], part: usize, j: usize) -> Option<Mat4> {
    let host = m.parts.get(part)?;
    let skeleton = host.mesh.as_ref()?.skeleton.as_ref()?;
    let pose = (poses.get(part))
        .and_then(|p| p.get(j))
        .or(skeleton.bind.get(j))?;
    Some(math::multiply(
        &host.transform.matrix(),
        &flat(pose.matrix()),
    ))
}

fn hide(pieces: &[Piece], posed: &mut [Posed], element: Id, hidden: bool) {
    for (piece, pose) in pieces.iter().zip(posed) {
        if piece.element == Some(element) {
            pose.shown &= !hidden;
        }
    }
}

/// How a particle entry draws its effect this frame: where the emitter
/// is, the entry's tint and flipbook grid, the effect's time, the camera's
/// axes, and how far wind pushes a particle per frame of its life squared.
struct Look {
    at: Mat4,
    tint: [f32; 4],
    sub_images: Option<[u32; 3]>,
    time: f32,
    right: Vec3,
    down: Vec3,
    wind: Vec3,
}

/// A flipbook's columns, rows and frames: the cfg's `SubImageSize`, else
/// square frames in one row across the sheet (docs/particle-format.md).
/// 968 of 117's particle entries and the 1800 airship's name no grid; read
/// as one frame, a 15-frame strip squeezed into every quad as streaks.
fn flipbook(sub_images: Option<[u32; 3]>, sheet: Option<(u32, u32)>) -> Option<[u32; 3]> {
    sub_images.or_else(|| {
        let (w, h) = sheet?;
        let n = ((w as f32 / h.max(1) as f32).round() as u32).max(1);
        Some([n, 1, n])
    })
}

/// The atlas rect a particle shows: its flipbook frame (of `grid`) when its
/// material plays one (`MovieFPS` > 0 loops at that rate with a phase of
/// 0.27 per particle; 0 plays once over its life), else the material's crop.
fn particle_uv(
    m: Option<&rdp::Material>,
    grid: Option<[u32; 3]>,
    time: f32,
    id: u32,
    age: f32,
    life: f32,
) -> [f32; 4] {
    let Some(m) = m else {
        return [0., 0., 1., 1.];
    };
    let (Some([cols, rows, count]), false) = (grid, m.movie_tex_path.is_empty()) else {
        return m.crop;
    };
    if m.movie_fps < 0. || count == 0 {
        return m.crop;
    }
    let k = match m.movie_fps > 0. {
        true => (time * m.movie_fps + id as f32 * 0.27) as u32 % count,
        false => ((age / life.max(1.)) * count as f32).min(count as f32 - 1.) as u32,
    };
    let (w, h) = (1. / cols as f32, 1. / rows as f32);
    [(k % cols) as f32 * w, (k / cols) as f32 * h, w, h]
}

/// The quads of an effect's live particles of material `k`, in model
/// space, as the game's particle vertex shader places them
/// (docs/particle-format.md, Drawing one particle): the corner (±0.5, 0,
/// ±0.5) turned by the particle's rotation, moved by the material's centre
/// offset and scaled per axis, then through the emitter's matrix (facing 0),
/// onto the view's right and up (1) or the view's right and world up (2),
/// at the particle's position, drifted by wind. Colour is the particle's
/// times the entry's tint, its alpha the vertex's transparency.
fn sprites_mesh(
    effect: &Rdp,
    frame: &rdp::Frame,
    k: u8,
    look: &Look,
    texture: Option<Image>,
) -> Option<Mesh> {
    let m = effect.materials.get(k as usize)?;
    let grid = flipbook(look.sub_images, texture.as_ref().map(|t| t.size()));
    let mut data = MeshData {
        material: gpu::Material::Blend(gpu::Blend {
            opacity: 1.,
            additive: m.additive,
            unlit: !effect.lighting,
            ..Default::default()
        }),
        casts_shadow: false,
        texture,
        ..Default::default()
    };
    let at = &look.at;
    let linear =
        |v: Vec3| std::array::from_fn(|r| at[r] * v[0] + at[4 + r] * v[1] + at[8 + r] * v[2]);
    // Facing quads keep the emitter's size, not its turn.
    let size = (0..3).map(|c| math::length([at[c * 4], at[c * 4 + 1], at[c * 4 + 2]]));
    let size = size.fold(0f32, f32::max).max(1e-6);
    let up = look.down.map(|v| -v);
    let facing = math::normalize(math::cross(look.right, up)).unwrap_or([0., 0., 1.]);
    let [ox, oy] = m.center_offset;
    for i in (0..frame.len()).filter(|&i| frame.material[i] == k) {
        let q = frame.rotation[i];
        let n = q.iter().map(|c| c * c).sum::<f32>().sqrt().max(1e-6);
        let q = q.map(|c| c / n);
        let [sx, sy, sz] = frame.scale[i];
        let corner = |cx: f32, cz: f32| -> Vec3 {
            let r = rotate(q, [cx, 0., cz]);
            [(r[0] + ox) * sx, (r[1] + oy) * sy, r[2] * sz]
        };
        let (age, life) = (frame.age[i], frame.life[i].max(1.));
        let drift = (age / life).powi(2) * life;
        let center = math::transform(at, frame.position[i]);
        let center: Vec3 = std::array::from_fn(|c| center[c] + look.wind[c] * drift);
        let place = |v: Vec3| -> Vec3 {
            let offset: Vec3 = match m.view_aligned {
                0 => linear(v),
                1 => std::array::from_fn(|c| (look.right[c] * v[0] + up[c] * v[2]) * size),
                _ => {
                    std::array::from_fn(|c| (look.right[c] * v[0] + [0., 1., 0.][c] * v[2]) * size)
                }
            };
            std::array::from_fn(|c| center[c] + offset[c])
        };
        let normal = match m.view_aligned {
            0 => math::normalize(linear(rotate(q, [0., 1., 0.]))).unwrap_or(facing),
            _ => facing,
        };
        let c = frame.color[i];
        let [r, g, b, a] = std::array::from_fn(|k| (c[k] * look.tint[k]).clamp(0., 1.));
        let byte = |v: f32| (v * 255.) as u32;
        let color = byte(1. - a) << 24 | byte(r) << 16 | byte(g) << 8 | byte(b);
        let crop = particle_uv(Some(m), grid, look.time, frame.particle[i], age, life);
        let base = data.positions.len() as u32;
        for (cx, cz, u, v) in [
            (-0.5, 0.5, 0., 0.),
            (0.5, 0.5, 1., 0.),
            (0.5, -0.5, 1., 1.),
            (-0.5, -0.5, 0., 1.),
        ] {
            data.positions.push(place(corner(cx, cz)));
            data.uvs
                .push([crop[0] + u * crop[2], crop[1] + v * crop[3]]);
            data.colors.push(color);
            data.normals.push(normal);
        }
        data.indices
            .extend([0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2].map(|k| base + k));
    }
    if data.indices.is_empty() {
        return None;
    }
    if data.texture.is_none() {
        data.uvs.clear();
    }
    Mesh::new(data).ok()
}

#[cfg(test)]
mod tests {
    use super::{Look, clip_time, flipbook, particle_uv, sprites_mesh};
    use anno_formats::rdp::Material;
    use gpui_editor_kit::gpu;

    // A plume's quads under a camera tilted down: facing the camera they
    // lean with it, upright they stand on world up, keeping their own
    // turn (a spin about Y) they lie flat. Additive materials add, and a
    // particle's alpha is its vertices' transparency.
    #[test]
    fn particle_quads_face_and_blend_as_their_material_says() {
        let mut rdp = super::super::effect::Emitter::default().bake().unwrap();
        let mut frame = Default::default();
        rdp.frame(1., &mut frame);
        let tilt = std::f32::consts::FRAC_1_SQRT_2;
        let look = Look {
            at: gpui_editor_kit::math::IDENTITY,
            wind: [0.; 3],
            tint: [1.; 4],
            sub_images: None,
            time: 1.,
            right: [1., 0., 0.],
            down: [0., -tilt, -tilt],
        };
        // Each quad's four corners, as offsets from their middle.
        let spans = |facing: u8, rdp: &mut anno_formats::rdp::Rdp| {
            rdp.materials[0].view_aligned = facing;
            let mesh = sprites_mesh(rdp, &frame, 0, &look, None).unwrap();
            let d = mesh.data().clone();
            let quads: Vec<[[f32; 3]; 4]> = (d.positions.chunks(4))
                .map(|q| {
                    let mid: [f32; 3] =
                        std::array::from_fn(|a| q.iter().map(|p| p[a]).sum::<f32>() / 4.);
                    std::array::from_fn(|k| std::array::from_fn(|a| q[k][a] - mid[a]))
                })
                .collect();
            (quads, d)
        };
        let (quads, _) = spans(1, &mut rdp);
        assert!(
            quads.iter().any(|q| q.iter().any(|c| c[2].abs() > 1e-3)),
            "facing leans"
        );
        let (quads, _) = spans(2, &mut rdp);
        assert!(
            quads.iter().all(|q| q.iter().all(|c| c[2].abs() < 1e-4)),
            "upright stands"
        );
        let (quads, _) = spans(0, &mut rdp);
        assert!(
            quads.iter().all(|q| q.iter().all(|c| c[1].abs() < 1e-4)),
            "own turn lies flat"
        );
        rdp.materials[0].additive = true;
        let (_, d) = spans(1, &mut rdp);
        assert!(matches!(d.material, gpu::Material::Blend(b) if b.additive));
        let clear = |i: usize| 1. - (d.colors[i * 4] >> 24) as f32 / 255.;
        for (i, c) in frame.color.iter().enumerate().take(d.colors.len() / 4) {
            assert!(
                (clear(i) - c[3].clamp(0., 1.)).abs() < 0.01,
                "alpha {} vs {}",
                clear(i),
                c[3]
            );
        }
    }

    // A 3,666 ms lie-down on a 15,500 ms clip: forever looped, it repeats;
    // given its plays (or once, in an unlooped sequence), it holds its last
    // pose after them.
    #[test]
    fn clips_repeat_while_they_have_plays_then_hold() {
        let d = 3.666;
        assert!((clip_time(4.0, d, 0) - 0.334).abs() < 1e-4);
        assert!((clip_time(15.0, d, 0) - (15.0 % d)).abs() < 1e-4);
        assert_eq!(clip_time(4.0, d, 1), d);
        assert!((clip_time(4.0, d, 2) - 0.334).abs() < 1e-4);
        assert_eq!(clip_time(8.0, d, 2), d);
        assert_eq!(clip_time(1.0, 0., 0), 0.);
    }

    // An 8 × 8 flipbook: played once over a life it starts on the first
    // frame and ends on the last; at a rate it loops; without one the
    // material's crop shows. A sheet the cfg gives no grid is square frames
    // in a row: the oil fire's 15 in 3840 × 256, its first a fifteenth wide.
    #[test]
    fn particles_show_their_flipbook_frame() {
        let grid = Some([8, 8, 64]);
        let mut m = Material::default();
        m.movie_tex_path = "maps/fire.psd".into();
        m.movie_fps = 0.;
        let cell = 1. / 8.;
        assert_eq!(
            particle_uv(Some(&m), grid, 0., 0, 0., 30.),
            [0., 0., cell, cell]
        );
        assert_eq!(
            particle_uv(Some(&m), grid, 0., 0, 29.9, 30.),
            [7. * cell, 7. * cell, cell, cell]
        );
        m.movie_fps = 16.;
        assert_eq!(
            particle_uv(Some(&m), grid, 4., 0, 0., 30.),
            [0., 0., cell, cell],
            "64 frames loop at 4 s"
        );
        let strip = flipbook(None, Some((3840, 256)));
        assert_eq!(strip, Some([15, 1, 15]));
        assert_eq!(
            flipbook(grid, Some((3840, 256))),
            grid,
            "the cfg's grid wins"
        );
        m.movie_fps = 0.;
        assert_eq!(
            particle_uv(Some(&m), strip, 0., 0, 0., 30.),
            [0., 0., 1. / 15., 1.]
        );
        m.movie_fps = -1.;
        m.crop = [0.5, 0., 0.5, 0.5];
        assert_eq!(particle_uv(Some(&m), grid, 1., 0, 0., 30.), m.crop);
    }
}
