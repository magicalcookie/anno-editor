// Owns the particle effects the Model workspace edits: an `.rdp` read from
// the project or the game, changed through its typed fields and a few
// whole-run operations (speed, size, colour); a 2070 `.efc`'s emitters,
// changed number by number and baked to play; a copy of either for one
// model; and new 117 and 1800 effects baked from 2070's emitter. Edited
// effects ride in the same undo snapshots as the cfg and save beside it.
//
// An `.rdp` is a baked simulation, not emitter rules, so a shipped effect
// can be retimed, resized, recoloured and re-textured but not re-simulated.
// Effects made here keep their emitter in the project's settings and bake
// again on every change.

use super::ModelEditor;
use anno_formats::efc::{self, BILLBOARD, Curve, Efc, FLAME, Launch, PARTICLE};
use anno_formats::rdp::Rdp;
use anno_formats::xml::Id;
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What New particle effect shapes: 2070's emitter (`formats::efc`), a
/// launch and gravity it lacks, and a flipbook. The effect's `texture` is
/// the sprite.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Emitter {
    pub effect: efc::Effect,
    pub launch: Launch,
    pub flipbook: Option<Flipbook>,
}

/// A flipbook the texture holds: columns, rows and frames (as the cfg's
/// `SubImageSize`), and frames a second, 0 playing it once over each
/// particle's life.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Flipbook {
    pub grid: [u32; 3],
    pub fps: f32,
}

impl Default for Emitter {
    /// A small warm plume about a brazier's size: 2070's plume launched at
    /// 1.2 m/s within 20° of up and lifting at 0.6 m/s².
    fn default() -> Emitter {
        Emitter {
            effect: efc::Effect::plume(),
            launch: Launch {
                speed: 1.2,
                spread: 20.,
                gravity: 0.6,
            },
            flipbook: None,
        }
    }
}

impl Emitter {
    /// The effect as an `.rdp`: without a flipbook its material crops the
    /// header's texture by the effect's atlas, as the game draws a
    /// `MovieFPS` of -1; with one it plays the texture's frames.
    pub fn bake(&self) -> Result<Rdp> {
        let mut rdp = (self.effect.bake(&self.launch, 1))
            .context("only particle, billboard and flame emitters bake")?;
        if let Some(book) = self.flipbook {
            let texture = std::mem::take(&mut rdp.texture_path);
            let m = &mut rdp.materials[0];
            m.movie_tex_path = texture;
            m.movie_fps = book.fps.max(0.);
            m.crop = [0., 0., 1., 1.];
        }
        Ok(rdp)
    }
}

/// An effect made in the editor, kept in the project's settings so its
/// emitter stays editable after the editor closes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "Saved")]
pub struct Authored {
    /// The effect's `.rdp` path in the mod.
    pub path: String,
    pub emitter: Emitter,
}

// Settings as projects hold them: the emitter now, or the plume of the
// first New particle effect, which projects saved before 2070's emitter
// came in still name.
#[derive(Deserialize)]
#[serde(untagged)]
enum Saved {
    Emitter {
        path: String,
        emitter: Emitter,
    },
    Plume {
        path: String,
        emitter: Plume,
        texture: String,
    },
}

impl From<Saved> for Authored {
    fn from(saved: Saved) -> Authored {
        match saved {
            Saved::Emitter { path, emitter } => Authored { path, emitter },
            Saved::Plume {
                path,
                emitter,
                texture,
            } => Authored {
                path,
                emitter: emitter.into_emitter(texture),
            },
        }
    }
}

// The first emitter: particles born evenly over a run of 30 fps frames,
// thrown up in a cone, pulled by gravity, size and colour from birth to
// death.
#[derive(Deserialize)]
struct Plume {
    frames: u32,
    particles: u32,
    life: u32,
    speed: f32,
    spread: f32,
    gravity: f32,
    size: [f32; 2],
    color: [[f32; 4]; 2],
    spin: f32,
    additive: bool,
}

impl Plume {
    fn into_emitter(self, texture: String) -> Emitter {
        let ms = |frames: u32| (frames.max(1) as f32 * 1000. / 30.).round() as u32;
        let mut e = efc::Effect::plume();
        e.texture = texture;
        e.duration = ms(self.frames);
        e.life_time = ms(self.life);
        e.technique = u32::from(self.additive);
        let every = e.duration as f32 / self.particles.max(1) as f32;
        *e.curve_mut("PEP", Curve::default) = Curve::constant(0., 100., every.max(1.));
        let [c0, c1] = self.color;
        let curves = [
            (
                "Scale",
                Curve::ramp(0., 1., [self.size[0] / 2.; 4], [self.size[1] / 2.; 4]),
            ),
            ("Color", Curve::ramp(0., 1., c0, c1)),
            ("Alpha", Curve::ramp(0., 1., [c0[3]; 4], [c1[3]; 4])),
            (
                "Rotation",
                Curve::ramp(
                    -2.,
                    2.,
                    [0.; 4],
                    [self.spin * e.life_time as f32 / 1000.; 4],
                ),
            ),
        ];
        for (name, curve) in curves {
            *e.curve_mut(name, Curve::default) = curve;
        }
        e.curve_mut("EmitArea", Curve::default).def = 0.;
        Emitter {
            effect: e,
            launch: Launch {
                speed: self.speed,
                spread: self.spread,
                gravity: self.gravity,
            },
            flipbook: None,
        }
    }
}

/// One number of an emitter the inspector edits, as it shows it: sizes
/// across in metres, turns in degrees, rates a second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Knob {
    Life,
    Duration,
    Delay,
    /// Particles a second at the start or end of the emitter's duration.
    Rate(bool),
    /// The birth ring's radius at the start or end of the duration.
    Ring(bool),
    /// Metres from the birth point at the end of a life, by axis: 2070's
    /// `Velocity` curves.
    Path(usize),
    PathSpread,
    Size(bool),
    SizeSpread,
    Turn(bool),
    TurnSpread,
    /// Red, green, blue (0..3) or alpha (3) at the start or end of a life.
    Color(bool, usize),
    Speed,
    Cone,
    Gravity,
    /// Flipbook columns, rows, frames (0..3) and frames a second (3).
    Book(usize),
}

impl Knob {
    pub const ALL: [Knob; 31] = [
        Knob::Life,
        Knob::Duration,
        Knob::Delay,
        Knob::Rate(false),
        Knob::Rate(true),
        Knob::Ring(false),
        Knob::Ring(true),
        Knob::Path(1),
        Knob::Path(0),
        Knob::Path(2),
        Knob::PathSpread,
        Knob::Speed,
        Knob::Cone,
        Knob::Gravity,
        Knob::Size(false),
        Knob::Size(true),
        Knob::SizeSpread,
        Knob::Turn(false),
        Knob::Turn(true),
        Knob::TurnSpread,
        Knob::Color(false, 0),
        Knob::Color(false, 1),
        Knob::Color(false, 2),
        Knob::Color(false, 3),
        Knob::Color(true, 0),
        Knob::Color(true, 1),
        Knob::Color(true, 2),
        Knob::Color(true, 3),
        Knob::Book(0),
        Knob::Book(1),
        Knob::Book(3),
    ];

    pub fn label(self) -> &'static str {
        match self {
            Knob::Life => "Life ms",
            Knob::Duration => "Emit for ms",
            Knob::Delay => "Delay ms",
            Knob::Rate(false) => "Rate start /s",
            Knob::Rate(true) => "Rate end /s",
            Knob::Ring(false) => "Ring start m",
            Knob::Ring(true) => "Ring end m",
            Knob::Path(0) => "Drift X m",
            Knob::Path(1) => "Rise m",
            Knob::Path(_) => "Drift Z m",
            Knob::PathSpread => "Path spread",
            Knob::Size(false) => "Size start m",
            Knob::Size(true) => "Size end m",
            Knob::SizeSpread => "Size spread",
            Knob::Turn(false) => "Turn start °",
            Knob::Turn(true) => "Turn end °",
            Knob::TurnSpread => "Turn spread",
            Knob::Color(end, c) => match (end, c) {
                (false, 0) => "Red start",
                (false, 1) => "Green start",
                (false, 2) => "Blue start",
                (false, _) => "Alpha start",
                (true, 0) => "Red end",
                (true, 1) => "Green end",
                (true, 2) => "Blue end",
                (true, _) => "Alpha end",
            },
            Knob::Speed => "Speed m/s",
            Knob::Cone => "Cone °",
            Knob::Gravity => "Gravity m/s²",
            Knob::Book(0) => "Flipbook columns",
            Knob::Book(1) => "Flipbook rows",
            Knob::Book(2) => "Flipbook frames",
            Knob::Book(_) => "Flipbook fps",
        }
    }

    pub fn step(self) -> f32 {
        match self {
            Knob::Life | Knob::Duration | Knob::Delay => 100.,
            Knob::Rate(_) => 5.,
            Knob::Turn(_) | Knob::Cone => 15.,
            Knob::Book(_) => 1.,
            Knob::Speed | Knob::Gravity | Knob::Path(_) => 0.1,
            _ => 0.05,
        }
    }

    /// Whether an effect of `kind` reads it: the launch and flipbook only
    /// effects made for 117 and 1800 (`authored`), which 2070 cannot hold.
    pub fn applies(self, kind: u32, authored: bool) -> bool {
        match self {
            Knob::Speed | Knob::Cone | Knob::Gravity | Knob::Book(_) => authored,
            Knob::Life | Knob::Delay | Knob::Size(_) | Knob::SizeSpread | Knob::Color(..) => {
                matches!(kind, PARTICLE | BILLBOARD | FLAME)
            }
            Knob::Path(_) | Knob::PathSpread | Knob::Turn(_) => {
                matches!(kind, PARTICLE | BILLBOARD)
            }
            Knob::Duration | Knob::Rate(_) | Knob::Ring(_) | Knob::TurnSpread => kind == PARTICLE,
        }
    }

    pub fn get(self, e: &Emitter) -> f32 {
        let fx = &e.effect;
        let curve = |name: &str| {
            fx.curve(name)
                .cloned()
                .unwrap_or_else(|| default_curve(name))
        };
        let book = e.flipbook.unwrap_or(Flipbook {
            grid: [1, 1, 1],
            fps: 0.,
        });
        match self {
            Knob::Life => fx.life_time as f32,
            Knob::Duration => fx.duration as f32,
            Knob::Delay => fx.delay as f32,
            Knob::Rate(end) => 1000. / curve("PEP").edge(end, 0).trunc().max(1.),
            Knob::Ring(end) => curve("EmitArea").edge(end, 0),
            Knob::Path(a) => curve(PATH[a]).edge(true, 0),
            Knob::PathSpread => curve(PATH[1]).varianz,
            Knob::Size(end) => curve("Scale").edge(end, 0) * 2.,
            Knob::SizeSpread => curve("Scale").varianz,
            Knob::Turn(end) => curve(turn(fx.kind)).edge(end, 0).to_degrees(),
            Knob::TurnSpread => curve(turn(fx.kind)).varianz,
            Knob::Color(end, 3) => curve("Alpha").edge(end, 0),
            Knob::Color(end, c) => curve("Color").edge(end, c),
            Knob::Speed => e.launch.speed,
            Knob::Cone => e.launch.spread,
            Knob::Gravity => e.launch.gravity,
            Knob::Book(3) => book.fps,
            Knob::Book(i) => book.grid[i] as f32,
        }
    }

    pub fn set(self, e: &mut Emitter, v: f32) {
        let kind = e.effect.kind;
        let fx = &mut e.effect;
        match self {
            Knob::Life => fx.life_time = v.max(1.) as u32,
            Knob::Duration => fx.duration = v.max(0.) as u32,
            Knob::Delay => fx.delay = v.max(0.) as u32,
            Knob::Rate(end) => curve(fx, "PEP").set_edge(end, 0, 1000. / v.max(0.1)),
            Knob::Ring(end) => curve(fx, "EmitArea").set_edge(end, 0, v.max(0.)),
            Knob::Path(a) => curve(fx, PATH[a]).set_edge(true, 0, v),
            Knob::PathSpread => {
                for name in PATH {
                    curve(fx, name).varianz = v.clamp(0., 1.);
                }
            }
            Knob::Size(end) => curve(fx, "Scale").set_edge(end, 0, v.max(0.) / 2.),
            Knob::SizeSpread => curve(fx, "Scale").varianz = v.clamp(0., 1.),
            Knob::Turn(end) => curve(fx, turn(kind)).set_edge(end, 0, v.to_radians()),
            Knob::TurnSpread => curve(fx, turn(kind)).varianz = v.clamp(0., 1.),
            Knob::Color(end, 3) => curve(fx, "Alpha").set_edge(end, 0, v.clamp(0., 1.)),
            Knob::Color(end, c) => curve(fx, "Color").set_edge(end, c, v.clamp(0., 1.)),
            Knob::Speed => e.launch.speed = v,
            Knob::Cone => e.launch.spread = v.clamp(0., 180.),
            Knob::Gravity => e.launch.gravity = v,
            Knob::Book(i) => {
                let mut book = e.flipbook.unwrap_or(Flipbook {
                    grid: [1, 1, 1],
                    fps: 0.,
                });
                match i {
                    3 => book.fps = v.max(0.),
                    _ => book.grid[i] = v.clamp(1., 64.) as u32,
                }
                // It plays every cell of its grid; one cell is no flipbook.
                book.grid[2] = book.grid[0] * book.grid[1];
                e.flipbook = (book.grid[2] > 1).then_some(book);
            }
        }
    }
}

/// The effect's curve `name`, added as shipped effects write it when
/// missing.
fn curve<'a>(fx: &'a mut efc::Effect, name: &str) -> &'a mut Curve {
    fx.curve_mut(name, || default_curve(name))
}

const PATH: [&str; 3] = ["VelocityX", "VelocityY", "VelocityZ"];

/// The curve a kind turns its sprites by.
fn turn(kind: u32) -> &'static str {
    match kind {
        BILLBOARD => "RotationZ",
        _ => "Rotation",
    }
}

/// A curve an effect lacks, as most shipped effects write it: the path
/// ±1 m, size 0.5, turns ±2 rad, an emission every 100 ms, white.
fn default_curve(name: &str) -> Curve {
    match name {
        "VelocityX" | "VelocityY" | "VelocityZ" => Curve::constant(-1., 1., 0.),
        "Scale" => Curve::constant(0., 1., 0.5),
        "Rotation" | "RotationZ" => Curve::constant(-2., 2., 0.),
        "PEP" => Curve::constant(0., 100., 100.),
        "Color" | "Alpha" => Curve::constant(0., 1., 1.),
        _ => Curve::default(),
    }
}

/// A whole-run change the inspector offers for any `.rdp`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    /// Milliseconds a frame lasts: the run's speed.
    FrameTime(f32),
    Looped(bool),
    /// Every quad's size times this.
    Size(f32),
    /// Every colour times this, alpha included.
    Color([f32; 4]),
    /// Material `k` draws additively.
    Additive(usize, bool),
    /// Material `k` faces the camera (1), stays upright (2) or keeps its
    /// own rotation (0).
    Facing(usize, u8),
    /// Material `k`'s colour boost byte.
    Boost(usize, u8),
}

impl Change {
    fn apply(self, rdp: &mut Rdp) {
        match self {
            Change::FrameTime(ms) => rdp.frame_time = ms.max(1.),
            Change::Looped(on) => rdp.looped = on,
            Change::Size(by) => {
                for s in &mut rdp.scales {
                    *s = s.map(|v| v * by);
                }
                // Scales quantize to MaxScale / 255: it grows with them.
                let top = rdp.scales.iter().flatten().fold(0f32, |m, &v| m.max(v));
                rdp.max_scale = rdp.max_scale.max(top);
            }
            Change::Color(by) => {
                for c in &mut rdp.colors {
                    *c = std::array::from_fn(|k| (c[k] * by[k]).clamp(0., 1.));
                }
            }
            Change::Additive(k, on) => {
                if let Some(m) = rdp.materials.get_mut(k) {
                    m.additive = on;
                }
            }
            Change::Facing(k, v) => {
                if let Some(m) = rdp.materials.get_mut(k) {
                    m.view_aligned = v.min(2);
                }
            }
            Change::Boost(k, v) => {
                if let Some(m) = rdp.materials.get_mut(k) {
                    m.color_boost = v;
                }
            }
        }
    }
}

impl ModelEditor {
    /// The effect at `path` as the editor has it: this session's edit, else
    /// the file read once.
    pub fn effect(&mut self, path: &str) -> Option<Arc<Rdp>> {
        if let Some((_, e)) = self.effects.iter().find(|(p, _)| p == path) {
            return Some(e.clone());
        }
        self.player.effect(&self.install, path)
    }

    /// Applies `change` to the effect at `path`, one undo step.
    pub fn change_effect(&mut self, path: &str, change: Change) -> Result<()> {
        let mut rdp = Rdp::clone(&*self.effect(path).context("the effect does not read")?);
        change.apply(&mut rdp);
        self.store_effect(path, rdp)
    }

    /// Sets material `k`'s texture (its flipbook, or the atlas it crops).
    pub fn set_effect_texture(&mut self, path: &str, k: usize, texture: &str) -> Result<()> {
        let mut rdp = Rdp::clone(&*self.effect(path).context("the effect does not read")?);
        let m = rdp.materials.get_mut(k).context("no such material")?;
        m.movie_tex_path = texture.trim().to_owned();
        self.store_effect(path, rdp)
    }

    fn store_effect(&mut self, path: &str, rdp: Rdp) -> Result<()> {
        // Refused here, not at save: a change the file cannot hold.
        rdp.encode().context("the effect would not write")?;
        self.checkpoint();
        self.put_effect(path, rdp);
        self.pose();
        Ok(())
    }

    fn put_effect(&mut self, path: &str, rdp: Rdp) {
        let rdp = Arc::new(rdp);
        match self.effects.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot.1 = rdp.clone(),
            None => self.effects.push((path.to_owned(), rdp.clone())),
        }
        self.player.set_effect(path, rdp);
    }

    /// The 2070 `.efc` at `path` as the editor has it: this session's edit,
    /// else the file read once.
    pub fn efc(&mut self, path: &str) -> Option<Arc<Efc>> {
        if let Some((_, e)) = self.efcs.iter().find(|(p, _)| p == path) {
            return Some(e.clone());
        }
        self.player
            .baked(&self.install, path)
            .map(|b| b.efc.clone())
    }

    /// An `.efc`'s emitters and the runs they bake to (`None` for those
    /// the editor does not draw), read once.
    pub fn efc_emitters(&mut self, path: &str) -> Option<Arc<super::playback::Baked>> {
        self.player.baked(&self.install, path)
    }

    /// Writes `effect` over emitter `k` of the `.efc` at `path` and plays
    /// it baked again, one undo step.
    pub fn set_efc_effect(&mut self, path: &str, k: usize, effect: &efc::Effect) -> Result<()> {
        let mut efc = Efc::clone(&*self.efc(path).context("the effect does not read")?);
        efc.set(k, effect)?;
        self.checkpoint();
        let efc = Arc::new(efc);
        match self.efcs.iter_mut().find(|(p, _)| p == path) {
            Some(slot) => slot.1 = efc.clone(),
            None => self.efcs.push((path.to_owned(), efc.clone())),
        }
        self.player.set_efc(path, efc);
        self.pose();
        Ok(())
    }

    /// Points the particle entry at a copy of its effect beside the cfg, so
    /// edits change it in this model only, not everywhere the game uses it.
    pub fn copy_effect(&mut self, entry: Id) -> Result<String> {
        let file = (self.doc.child(entry, "FileName")).context("the entry names no effect")?;
        let from = anno_formats::rda::normalize_path(self.doc.text(file)).into_owned();
        let to = self.fresh_effect_path(from.rsplit('/').next().unwrap_or(&from));
        if from.ends_with(".efc") {
            let efc = self.efc(&from).context("the effect does not read")?;
            self.checkpoint();
            self.efcs.push((to.clone(), efc.clone()));
            self.player.set_efc(&to, efc);
        } else {
            let rdp = self.effect(&from).context("the effect does not read")?;
            self.checkpoint();
            self.effects.push((to.clone(), rdp.clone()));
            self.player.set_effect(&to, rdp);
        }
        self.doc.set_text(file, &to.replace('/', "\\"));
        self.rebuild();
        Ok(to)
    }

    /// A new effect baked from `emitter`, saved beside the cfg and placed
    /// at the model's origin. Its emitter stays editable.
    pub fn new_effect(&mut self, emitter: Emitter) -> Result<Id> {
        let rdp = emitter.bake()?;
        rdp.encode().context("the effect would not write")?;
        let path = self.fresh_effect_path("effect.rdp");
        // Playback knows the file before the entry naming it exists; `add`
        // records the undo step for the entry, the effect and its emitter.
        let rdp = Arc::new(rdp);
        self.player.set_effect(&path, rdp.clone());
        self.add("Particles", &path)?;
        self.effects.push((path.clone(), rdp));
        let entry = self.selected.context("the new entry is selected")?;
        self.set_flipbook(entry, emitter.flipbook);
        self.emitters.retain(|a| a.path != path);
        self.emitters.push(Authored { path, emitter });
        self.rebuild();
        Ok(entry)
    }

    /// Bakes an effect made in the editor again with a changed emitter,
    /// one undo step; entries playing it take its flipbook grid.
    pub fn rebake(&mut self, path: &str, emitter: Emitter) -> Result<()> {
        ensure!(
            self.emitters.iter().any(|a| a.path == path),
            "only effects made in the editor have an emitter"
        );
        let rdp = emitter.bake()?;
        rdp.encode().context("the effect would not write")?;
        self.checkpoint();
        let entries: Vec<Id> = (self.model.particles.iter())
            .filter(|p| p.file == path)
            .map(|p| p.element)
            .collect();
        for entry in entries {
            self.set_flipbook(entry, emitter.flipbook);
        }
        let slot = (self.emitters.iter_mut().find(|a| a.path == path)).expect("checked above");
        slot.emitter = emitter;
        self.put_effect(path, rdp);
        self.rebuild();
        Ok(())
    }

    // The entry's `SubImageSize`: the grid its material's flipbook plays,
    // removed without one.
    fn set_flipbook(&mut self, entry: Id, book: Option<Flipbook>) {
        let old = self.doc.child(entry, "SubImageSize");
        match (book, old) {
            (None, Some(old)) => self.doc.remove(old),
            (None, None) => {}
            (Some(b), _) => {
                if let Some(old) = old {
                    self.doc.remove(old);
                }
                let [x, y, z] = b.grid;
                let xml = format!("<SubImageSize><x>{x}</x><y>{y}</y><z>{z}</z></SubImageSize>");
                self.doc.insert(entry, None, &xml);
            }
        }
    }

    /// The emitter of an effect made in the editor.
    pub fn emitter(&self, path: &str) -> Option<Emitter> {
        (self.emitters.iter().find(|a| a.path == path)).map(|a| a.emitter.clone())
    }

    /// `<cfg folder>/<ext>/<stem>_<n>.<ext>`, the first not in the game, the
    /// project or this session.
    fn fresh_effect_path(&self, file: &str) -> String {
        let dir = self.path.rsplit_once('/').map_or("", |(d, _)| d);
        let (stem, ext) = file.rsplit_once('.').unwrap_or((file, "rdp"));
        let taken = |p: &str| {
            self.install.exists(p)
                || self.effects.iter().any(|(e, _)| e == p)
                || self.efcs.iter().any(|(e, _)| e == p)
        };
        (1..)
            .map(|n| format!("{dir}/{ext}/{stem}_{n:02}.{ext}"))
            .find(|p| !taken(p))
            .expect("some number is free")
    }

    /// Writes every edited effect into the project beside the cfg, and the
    /// emitters of those made in the editor into its settings.
    pub(super) fn stage_effects(&mut self, project: &mut crate::project::Project) -> Result<()> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for (path, rdp) in &self.effects {
            files.push((
                path.clone(),
                rdp.encode().with_context(|| format!("writing {path}"))?,
            ));
        }
        for (path, efc) in &self.efcs {
            files.push((path.clone(), efc.write()));
        }
        for (path, bytes) in files {
            ensure!(!bytes.is_empty(), "{path} came out empty");
            if !self.files_before.iter().any(|(p, _)| *p == path) {
                let before = project.read_file(&path).map(|b| b.into_owned());
                self.files_before.push((path.clone(), before));
            }
            project.stage(&path, bytes);
        }
        // Edits undone since a save: the project gets back what it held.
        for (path, before) in &self.files_before {
            let effect = path.ends_with(".rdp") || path.ends_with(".efc");
            let live = self.effects.iter().any(|(p, _)| p == path)
                || self.efcs.iter().any(|(p, _)| p == path);
            if !effect || live {
                continue;
            }
            match before {
                Some(bytes) => project.stage(path, bytes.clone()),
                None => {
                    project.remove_file(path);
                    project.effects.retain(|e| &e.path != path);
                }
            }
        }
        for a in &self.emitters {
            if !self.effects.iter().any(|(p, _)| *p == a.path) {
                continue;
            }
            match project.effects.iter_mut().find(|e| e.path == a.path) {
                Some(kept) => *kept = a.clone(),
                None => project.effects.push(a.clone()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Authored, Emitter, Knob, Rdp};
    use anno_formats::efc::{BILLBOARD, PARTICLE};

    // A baked plume writes, reads back with its run, and plays: particles
    // live mid-run and rise against gravity's pull.
    #[test]
    fn an_emitter_bakes_an_effect_the_game_reads() {
        let mut e = Emitter::default();
        e.effect.texture = "data/maps/spark.psd".into();
        let rdp = e.bake().unwrap();
        let again = Rdp::parse(&rdp.encode().unwrap()).unwrap();
        assert_eq!(again.starts.len(), rdp.starts.len());
        assert!(again.looped);
        assert_eq!(again.texture_path, "data/maps/spark.psd");
        assert_eq!(again.materials[0].movie_fps, -1., "no flipbook: crops");
        let mut frame = Default::default();
        again.frame(0.5, &mut frame);
        assert!(!frame.is_empty());
        let top = frame.position.iter().map(|p| p[1]).fold(f32::MIN, f32::max);
        assert!(top > 0.5, "nothing rose: {top}");
    }

    // The inspector's numbers read back what they set, in the units shown;
    // a flipbook grid makes the material play the texture's frames.
    #[test]
    fn knobs_set_what_they_read() {
        let mut e = Emitter::default();
        e.effect.texture = "data/maps/fire_atlas.psd".into();
        for (knob, v) in [
            (Knob::Rate(true), 40.),
            (Knob::Size(false), 0.6),
            (Knob::Turn(true), 90.),
            (Knob::Color(true, 1), 0.2),
            (Knob::Path(1), 2.5),
            (Knob::Gravity, -3.),
            (Knob::Life, 1500.),
        ] {
            knob.set(&mut e, v);
            assert!(
                (knob.get(&e) - v).abs() < 1e-3,
                "{knob:?}: {}",
                knob.get(&e)
            );
        }
        assert!(Knob::Rate(false).applies(PARTICLE, false));
        assert!(!Knob::Rate(false).applies(BILLBOARD, false));
        assert!(!Knob::Speed.applies(PARTICLE, false), "2070 cannot hold it");
        Knob::Book(0).set(&mut e, 4.);
        Knob::Book(1).set(&mut e, 4.);
        assert_eq!(e.flipbook.unwrap().grid, [4, 4, 16]);
        let rdp = e.bake().unwrap();
        assert_eq!(rdp.materials[0].movie_fps, 0.);
        assert!(rdp.texture_path.is_empty());
        assert_eq!(rdp.materials[0].movie_tex_path, "data/maps/fire_atlas.psd");
    }

    // A project saved with the first plume still opens, its emitter brought
    // over: the same rate, life and launch.
    #[test]
    fn a_saved_plume_opens_as_an_emitter() {
        let json = r#"{"path":"data/x/rdp/effect_01.rdp","texture":"data/maps/spark.psd",
            "emitter":{"frames":60,"particles":40,"life":30,"speed":1.2,"spread":20.0,
            "gravity":0.6,"size":[0.15,0.5],"color":[[1.0,0.75,0.35,0.9],[0.4,0.35,0.3,0.0]],
            "spin":1.0,"additive":true}}"#;
        let a: Authored = serde_json::from_str(json).unwrap();
        let e = &a.emitter;
        assert_eq!((e.effect.duration, e.effect.life_time), (2000, 1000));
        assert_eq!(e.effect.technique, 1);
        assert_eq!(e.effect.texture, "data/maps/spark.psd");
        assert!((Knob::Rate(false).get(e) - 20.).abs() < 0.5);
        assert!((Knob::Size(true).get(e) - 0.5).abs() < 1e-4);
        assert_eq!(e.launch.speed, 1.2);
        let again: Authored = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(again, a);
    }
}
