// Anno 2070's `.efc` effects: a file of emitters (`<Effect0>`, `<Effect1>`,
// …), each a few scalars and named curves, read and edited the way the
// game's loader reads them, and the game's own emitter run to bake one into
// an `.rdp` the editor plays and the later games read. docs/particle-format.md
// (`.efc`) has the fields, the emitter and the Anno5.exe functions behind it.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use crate::rdp::{Material, Rdp};
use crate::xml::{Doc, Id};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// `EffectType`: what an effect draws. Shipped: 1 (472 effects), 2 (114),
/// 3 (83), 4 (15), 6 (1), 7 (26).
pub const PARTICLE: u32 = 1;
/// A mesh (`effect_model.fx`), not drawn here.
pub const MODEL: u32 = 2;
/// One sprite that follows the curves over its life, again each cycle.
pub const BILLBOARD: u32 = 3;
pub const LINE: u32 = 4;
pub const ARROW: u32 = 6;
/// One sprite the flame shader animates with noise.
pub const FLAME: u32 = 7;

/// A whole `.efc`, kept as its element tree so an unedited file writes back
/// byte for byte and an edit changes only the elements it names. Fields the
/// editor does not model (`Texture1`, `RenderTarget`, a mesh's `Model`) stay
/// as the file has them.
#[derive(Clone, Debug)]
pub struct Efc {
    doc: Doc,
}

impl Efc {
    /// Refuses a file whose top elements are not `EffectN` with an
    /// `EffectType`: the game builds one effect per top element.
    pub fn parse(bytes: &[u8]) -> Result<Efc> {
        let doc = Doc::parse(bytes);
        let mut any = false;
        for e in doc.children(Id::ROOT) {
            let name = doc.name(e);
            let numbered = name
                .strip_prefix("Effect")
                .is_some_and(|n| n.parse::<u32>().is_ok());
            ensure!(numbered, "<{name}> is not an effect");
            ensure!(
                doc.child(e, "EffectType").is_some(),
                "<{name}> has no EffectType"
            );
            any = true;
        }
        ensure!(any, "no effects");
        Ok(Efc { doc })
    }

    pub fn effects(&self) -> Vec<Effect> {
        (self.doc.children(Id::ROOT))
            .map(|e| Effect::read(&self.doc, e))
            .collect()
    }

    /// Writes `effect` over effect `k`: only the fields and curves that
    /// differ from what the file holds, in the game's own spelling (`%f`
    /// floats, hex knot numbers). A curve's knots are written whole.
    pub fn set(&mut self, k: usize, effect: &Effect) -> Result<()> {
        let id =
            (self.doc.children(Id::ROOT).nth(k)).ok_or_else(|| anyhow::anyhow!("no effect {k}"))?;
        let old = Effect::read(&self.doc, id);
        let doc = &mut self.doc;
        let f = |v: f32| format!("{v:.6}");
        let b = |v: bool| if v { "1" } else { "0" }.to_owned();
        let mut put = |tag: &str, changed: bool, text: String| {
            if changed {
                put_leaf(doc, id, tag, &text);
            }
        };
        put("Name", old.name != effect.name, effect.name.clone());
        put(
            "EffectType",
            old.kind != effect.kind,
            effect.kind.to_string(),
        );
        put(
            "Texture0",
            old.texture != effect.texture,
            effect.texture.clone(),
        );
        put(
            "Technique",
            old.technique != effect.technique,
            effect.technique.to_string(),
        );
        put("Delay", old.delay != effect.delay, effect.delay.to_string());
        put(
            "LifeTime",
            old.life_time != effect.life_time,
            effect.life_time.to_string(),
        );
        put(
            "Duration",
            old.duration != effect.duration,
            effect.duration.to_string(),
        );
        put("Loop", old.looped != effect.looped, b(effect.looped));
        put("Bound", old.bound != effect.bound, b(effect.bound));
        put(
            "LReceiver",
            old.light_receiver != effect.light_receiver,
            b(effect.light_receiver),
        );
        for (a, tag) in ["OffsetX", "OffsetY", "OffsetZ"].iter().enumerate() {
            put(tag, old.offset[a] != effect.offset[a], f(effect.offset[a]));
        }
        put(
            "Alignment",
            old.alignment != effect.alignment,
            effect.alignment.to_string(),
        );
        put(
            "WindDependency",
            old.wind_dependency != effect.wind_dependency,
            f(effect.wind_dependency),
        );
        let atlas = [
            "AtlasTilingU",
            "AtlasTilingV",
            "AtlasOffsetU",
            "AtlasOffsetV",
        ];
        for (a, tag) in atlas.iter().enumerate() {
            put(tag, old.atlas[a] != effect.atlas[a], f(effect.atlas[a]));
        }
        for (name, curve) in &effect.curves {
            let before = old.curve(name);
            if before == Some(curve) {
                continue;
            }
            write_curve(doc, id, name, before, curve);
        }
        Ok(())
    }

    pub fn write(&self) -> Vec<u8> {
        self.doc.write()
    }
}

/// Sets `<tag>` under `id`, adding it last when the effect has none.
fn put_leaf(doc: &mut Doc, id: Id, tag: &str, text: &str) {
    match doc.child(id, tag) {
        Some(c) => doc.set_text(c, text),
        None => {
            doc.insert(id, None, &format!("<{tag}>{text}</{tag}>"));
        }
    }
}

// A curve's four scalars, then its `Size` knots right after `Size`, as the
// game's serializer (`sub_14089D150`) lays them out.
fn write_curve(doc: &mut Doc, id: Id, name: &str, before: Option<&Curve>, c: &Curve) {
    let f = |v: f32| format!("{v:.6}");
    let scalars = [
        ("ValueMin", before.map(|b| b.min), c.min),
        ("ValueMax", before.map(|b| b.max), c.max),
        ("ValueDef", before.map(|b| b.def), c.def),
        ("Varianz", before.map(|b| b.varianz), c.varianz),
    ];
    for (suffix, old, new) in scalars {
        if old != Some(new) {
            put_leaf(doc, id, &format!("{name}{suffix}"), &f(new));
        }
    }
    if before.is_some_and(|b| b.knots == c.knots) {
        return;
    }
    let knot = |c: Id| {
        let n = doc.name(c);
        n.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix("Knot"))
            .is_some_and(|rest| {
                ["Pos_", "X_", "Y_", "Z_", "W_"]
                    .iter()
                    .any(|p| rest.starts_with(p))
            })
    };
    let old: Vec<Id> = doc.children(id).filter(|&c| knot(c)).collect();
    for k in old {
        doc.remove(k);
    }
    put_leaf(doc, id, &format!("{name}Size"), &c.knots.len().to_string());
    let size = doc.child(id, &format!("{name}Size")).expect("just put");
    let after = doc.children(id).skip_while(|&s| s != size).nth(1);
    for (i, k) in c.knots.iter().enumerate() {
        let fields = [
            ("Pos", k.pos),
            ("X", k.value[0]),
            ("Y", k.value[1]),
            ("Z", k.value[2]),
            ("W", k.value[3]),
        ];
        for (field, v) in fields {
            let tag = format!("{name}Knot{field}_{i:x}");
            doc.insert(id, after, &format!("<{tag}>{}</{tag}>", f(v)));
        }
    }
}

/// One emitter of an `.efc`: what the editor reads and edits of it. The
/// curves keep the file's names and order; which a kind reads is in
/// [`Effect::bake`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub name: String,
    /// `EffectType`, see [`PARTICLE`] and the others.
    pub kind: u32,
    /// `Texture0`: the sprite, as 2070 spells it (`….png`, shipped as
    /// `…_0.dds`).
    pub texture: String,
    /// 0 blends, 1 adds, 2 refracts, 3 writes depth, 4 ignores depth, 5
    /// draws into the water's dynamic waves (`effect_particle.fx` METHODs).
    pub technique: u32,
    /// Milliseconds: wait before emitting, emit for, and each particle's
    /// life.
    pub delay: u32,
    pub duration: u32,
    pub life_time: u32,
    /// The emitter starts again after `delay` once `duration` has run.
    pub looped: bool,
    /// Particles follow the emitter; off, each stays where it was born.
    pub bound: bool,
    /// `LReceiver`: lit by the sun, else unlit.
    pub light_receiver: bool,
    /// Metres from the cfg entry's origin to the emitter.
    pub offset: [f32; 3],
    /// 0 faces the camera, 2 also turns each sprite along the emitter's X
    /// axis as the camera sees it; 1 ships on six effects.
    pub alignment: u32,
    /// How far the scene's wind pushes a particle, by its height squared.
    pub wind_dependency: f32,
    /// `AtlasTilingU`, `…V`, `AtlasOffsetU`, `…V`: the texture coordinates
    /// at the quad's (+1, +1) and (-1, -1) corners, so the sprite's rect in
    /// its atlas.
    pub atlas: [f32; 4],
    pub curves: Vec<(String, Curve)>,
}

/// A value over a 0..1 time (a particle's age, or the emitter's time in
/// its `duration`), kept as the game keeps it: knots at 0..1 of
/// `min..max`, or `def` without knots, and a random spread `varianz`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Curve {
    pub min: f32,
    pub max: f32,
    pub def: f32,
    pub varianz: f32,
    pub knots: Vec<Knot>,
}

/// One point of a [`Curve`]: where (0..1), and four values at 0..1 of the
/// curve's range. Scalar curves use the first; `Color` the first three.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Knot {
    pub pos: f32,
    pub value: [f32; 4],
}

impl Curve {
    /// A curve that holds `value` everywhere, within `min..max`.
    pub fn constant(min: f32, max: f32, value: f32) -> Curve {
        Curve {
            min,
            max,
            def: value,
            varianz: 0.,
            knots: Vec::new(),
        }
    }

    /// A curve from `from` at 0 to `to` at 1 (all four values), within
    /// `min..max` widened to hold both.
    pub fn ramp(min: f32, max: f32, from: [f32; 4], to: [f32; 4]) -> Curve {
        let mut c = Curve::constant(min, max, from[0]);
        for k in 0..4 {
            c.set_edge(false, k, from[k]);
            c.set_edge(true, k, to[k]);
        }
        c
    }

    fn range(&self) -> f32 {
        self.max - self.min
    }

    /// The value at `t` as the game samples it (`sub_14089E3D0`): `def`
    /// without knots, the first knot before it, the last after, and
    /// linear between, scanning knots in file order.
    pub fn at(&self, t: f32) -> f32 {
        let k = &self.knots;
        let scale = |x: f32| x * self.range() + self.min;
        match k.len() {
            0 => return self.def,
            1 => return scale(k[0].value[0]),
            _ => {}
        }
        let last = k[k.len() - 1];
        if t >= last.pos {
            return scale(last.value[0]);
        }
        let hi = k.iter().position(|n| t <= n.pos).unwrap_or(k.len() - 1);
        let lo = hi.saturating_sub(1);
        let span = k[hi].pos - k[lo].pos;
        if span == 0. {
            return scale(k[hi].value[0]);
        }
        let u = (t - k[lo].pos) / span;
        scale((1. - u) * k[lo].value[0] + u * k[hi].value[0])
    }

    /// All four values at `t` as the colour map's bake reads `Color`
    /// (`sub_14089E230`): the nearest knots below and at or above `t` in any
    /// order, blended. As the game writes it, only the upper knot is scaled
    /// by the range; every shipped colour curve spans 0..1, where that
    /// changes nothing.
    pub fn values_at(&self, t: f32) -> [f32; 4] {
        let k = &self.knots;
        match k.len() {
            0 => return [self.def; 4],
            1 => return k[0].value.map(|v| v * self.range() + self.min),
            _ => {}
        }
        let first = k
            .iter()
            .fold(k[0], |m, n| if n.pos < m.pos { *n } else { m });
        let mut lo = first;
        let mut hi: Option<Knot> = None;
        for n in k {
            if n.pos >= t {
                if hi.is_none_or(|h| n.pos < h.pos) {
                    hi = Some(*n);
                }
            } else if n.pos > lo.pos {
                lo = *n;
            }
        }
        let hi = hi.unwrap_or(lo);
        let span = hi.pos - lo.pos;
        let u = match span == 0. {
            true => 0.,
            false => ((t - lo.pos) / span).clamp(0., 1.),
        };
        std::array::from_fn(|c| u * hi.value[c] * self.range() + (1. - u) * lo.value[c] + self.min)
    }

    /// The factor a particle's random `r` (0..1) puts on the curve:
    /// `(r - 0.5) · 2v + (1 - v)`, so 1 - 2v..1 (`sub_14089FA60`). One `r`
    /// serves all of a particle's curves.
    pub fn spread(&self, r: f32) -> f32 {
        match self.varianz != 0. {
            true => (r - 0.5) * (2. * self.varianz) + (1. - self.varianz),
            false => 1.,
        }
    }

    /// Value `k` of the curve at its start (0) or end (1).
    pub fn edge(&self, end: bool, k: usize) -> f32 {
        let t = if end { 1. } else { 0. };
        match k {
            0 => self.at(t),
            _ => self.values_at(t)[k],
        }
    }

    /// Sets value `k` at the start or end: the first or last knot, one added
    /// at 0 or 1 when the knots stop short, two made from `def` when there
    /// are none. `min..max` widens to hold it, the other knots keeping their
    /// values.
    pub fn set_edge(&mut self, end: bool, k: usize, v: f32) {
        if self.knots.is_empty() {
            let x = self.normalize(self.def);
            let knot = |pos| Knot { pos, value: [x; 4] };
            self.knots = vec![knot(0.), knot(1.)];
        }
        let (lo, hi) = (self.min.min(v), self.max.max(v));
        if (lo, hi) != (self.min, self.max) {
            let (old_min, old_range) = (self.min, self.range());
            (self.min, self.max) = (lo, hi);
            let range = self.range();
            for n in &mut self.knots {
                n.value = n.value.map(|x| match range != 0. {
                    true => (x * old_range + old_min - lo) / range,
                    false => 0.,
                });
            }
        }
        let i = match end {
            true if self.knots.last().is_some_and(|n| n.pos >= 1.) => self.knots.len() - 1,
            true => {
                let mut n = *self.knots.last().expect("knots made above");
                n.pos = 1.;
                self.knots.push(n);
                self.knots.len() - 1
            }
            false if self.knots[0].pos <= 0. => 0,
            false => {
                let mut n = self.knots[0];
                n.pos = 0.;
                self.knots.insert(0, n);
                0
            }
        };
        self.knots[i].value[k] = self.normalize(v);
    }

    fn normalize(&self, v: f32) -> f32 {
        match self.range() != 0. {
            true => (v - self.min) / self.range(),
            false => 0.,
        }
    }
}

impl Default for Curve {
    /// What a curve the file leaves out reads as: 0 within 0..1.
    fn default() -> Curve {
        Curve::constant(0., 1., 0.)
    }
}

impl Effect {
    fn read(doc: &Doc, id: Id) -> Effect {
        let leaf = |name: &str| doc.child(id, name).map(|c| doc.text(c).trim());
        let num = |name: &str| leaf(name).and_then(|t| t.parse::<f32>().ok());
        let int = |name: &str| {
            leaf(name)
                .and_then(|t| t.parse::<i64>().ok())
                .map_or(0, |v| v.max(0) as u32)
        };
        let mut curves: Vec<(String, Curve)> = Vec::new();
        for c in doc.children(id) {
            if let Some(name) = doc.name(c).strip_suffix("ValueMin")
                && !curves.iter().any(|(n, _)| n == name)
            {
                curves.push((name.to_owned(), read_curve(doc, id, name)));
            }
        }
        Effect {
            name: leaf("Name").unwrap_or("").to_owned(),
            kind: int("EffectType"),
            texture: leaf("Texture0").unwrap_or("").to_owned(),
            technique: int("Technique"),
            delay: int("Delay"),
            duration: int("Duration"),
            life_time: int("LifeTime"),
            looped: int("Loop") != 0,
            bound: int("Bound") != 0,
            light_receiver: int("LReceiver") != 0,
            offset: ["OffsetX", "OffsetY", "OffsetZ"].map(|n| num(n).unwrap_or(0.)),
            alignment: int("Alignment"),
            wind_dependency: num("WindDependency").unwrap_or(0.),
            atlas: [
                "AtlasTilingU",
                "AtlasTilingV",
                "AtlasOffsetU",
                "AtlasOffsetV",
            ]
            .map(|n| num(n).unwrap_or(0.)),
            curves,
        }
    }

    /// The curve `name`, when the effect has one.
    pub fn curve(&self, name: &str) -> Option<&Curve> {
        self.curves.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }

    /// The curve `name`, added as `default` when missing.
    pub fn curve_mut(&mut self, name: &str, default: impl FnOnce() -> Curve) -> &mut Curve {
        match self.curves.iter().position(|(n, _)| n == name) {
            Some(i) => &mut self.curves[i].1,
            None => {
                self.curves.push((name.to_owned(), default()));
                &mut self.curves.last_mut().expect("just pushed").1
            }
        }
    }

    /// Draws sprites the editor plays: particles, billboards and flames.
    pub fn drawn(&self) -> bool {
        matches!(self.kind, PARTICLE | BILLBOARD | FLAME)
    }

    /// A small warm plume for an effect made in the editor: 20 particles a
    /// second on a 10 cm ring, each living a second, turning, growing and
    /// fading to grey. A [`Launch`] gives it its rise.
    pub fn plume() -> Effect {
        let c = |min: f32, max: f32, v: f32| Curve::constant(min, max, v);
        Effect {
            name: "Plume".to_owned(),
            kind: PARTICLE,
            texture: String::new(),
            technique: 0,
            delay: 0,
            duration: 1000,
            life_time: 1000,
            looped: true,
            bound: true,
            light_receiver: false,
            offset: [0.; 3],
            alignment: 0,
            wind_dependency: 0.,
            // The whole texture, upright: see `Effect::material`.
            atlas: [1., 0., 0., 1.],
            curves: vec![
                ("PEP".to_owned(), c(0., 100., 50.)),
                ("EmitArea".to_owned(), c(0., 1., 0.1)),
                ("VelocityX".to_owned(), c(-1., 1., 0.)),
                ("VelocityY".to_owned(), c(-1., 1., 0.)),
                ("VelocityZ".to_owned(), c(-1., 1., 0.)),
                (
                    "Rotation".to_owned(),
                    Curve::ramp(-2., 2., [0.; 4], [1.; 4]),
                ),
                (
                    "Scale".to_owned(),
                    Curve::ramp(0., 1., [0.075; 4], [0.25; 4]),
                ),
                (
                    "Color".to_owned(),
                    Curve::ramp(0., 1., [1., 0.75, 0.35, 1.], [0.4, 0.35, 0.3, 1.]),
                ),
                ("Alpha".to_owned(), Curve::ramp(0., 1., [0.9; 4], [0.; 4])),
            ],
        }
    }
}

// The loader's read of one curve (`sub_14089D150`): `Size` knots, each
// field absent from the file left at `(min + def) / range` clamped to
// 0..1, at position 0. Knot numbers are hex (`_a` is the eleventh).
fn read_curve(doc: &Doc, id: Id, name: &str) -> Curve {
    let leaf = |suffix: &str| {
        doc.child(id, &format!("{name}{suffix}"))
            .map(|c| doc.text(c).trim())
    };
    let num = |suffix: &str| leaf(suffix).and_then(|t| t.parse::<f32>().ok());
    let mut c = Curve {
        min: num("ValueMin").unwrap_or(0.),
        max: num("ValueMax").unwrap_or(1.),
        def: num("ValueDef").unwrap_or(0.),
        varianz: num("Varianz").unwrap_or(0.),
        knots: Vec::new(),
    };
    // Bounded: no shipped curve has more than 14 knots.
    let size = leaf("Size")
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(0, 256);
    // `fminf` keeps 1 over NaN, so a 0 range fills with 1.
    let fill = (c.min + c.def) / c.range();
    let fill = match fill.min(1.) >= 0. {
        true if fill < 1. => fill,
        true => 1.,
        false => 0.,
    };
    for i in 0..size {
        let field = |f: &str, default: f32| num(&format!("Knot{f}_{i:x}")).unwrap_or(default);
        c.knots.push(Knot {
            pos: field("Pos", 0.),
            value: [
                field("X", fill),
                field("Y", fill),
                field("Z", fill),
                field("W", fill),
            ],
        });
    }
    c
}

/// What the bake adds to the game's emitter for effects made for 117 and
/// 1800: each particle launched at `speed` m/s within `spread` degrees of
/// straight up, and pulled by `gravity` m/s² along Y (negative falls).
/// 2070's own effects bake without; its emitter has no velocity of its own,
/// and it reads `Gravity` curves but never applies them (all 586 shipped
/// ones are 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Launch {
    pub speed: f32,
    pub spread: f32,
    pub gravity: f32,
}

/// The bake's frame: a 30th of a second, as most shipped `.rdp` run.
const FRAME_MS: f32 = 1000. / 30.;
/// Bounds one bake: the busiest shipped emitter keeps about 600 alive.
const MOST_PARTICLES: usize = 20_000;
const MOST_TICKS: usize = 4_000_000;

/// MSVC's `rand()`, which the game rolls: the same sequence from the same
/// seed, so a bake is repeatable.
struct Rand(u32);

impl Rand {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(214_013).wrapping_add(2_531_011);
        ((self.0 >> 16) & 0x7fff) as f32 / 32767.
    }
}

/// A particle as the emitter made it: when its age was 0 (ms into the
/// run), where, its random `r`, its launch velocity, and the emitter's
/// turn then.
struct Born {
    at: f32,
    origin: [f32; 3],
    r: f32,
    launch: [f32; 3],
    turn: [f32; 3],
}

/// Position, half size, turn (radians) and colour of one particle tick.
type Tick = ([f32; 3], f32, f32, [f32; 4]);

impl Effect {
    /// The game's run of this effect, baked as an `.rdp` of one material at
    /// 30 frames a second: a looped effect as whole emitter cycles long
    /// enough for a particle's life, a one-shot one until its last particle
    /// dies. `None` for kinds the editor does not draw. `seed` picks the
    /// random rolls; the same seed bakes the same effect.
    ///
    /// Particles (`sub_14089FA60`): one born every `PEP` milliseconds,
    /// `PEP` read at the emitter's time in its `duration`, on a horizontal
    /// ring of radius `EmitArea`, and placed at its age by the `Velocity`
    /// curves, which hold metres from where it was born, not speeds. Size,
    /// turn (`Rotation`, radians), colour and alpha follow its age. The
    /// emitter turns by `EmitterRot` (yaw Y, pitch X, roll Z); a particle
    /// that is not bound keeps the turn it was born with. Billboards
    /// (`sub_1408A4890`) are one such particle a cycle, at `Offset`, turning
    /// by `RotationZ`. Flames are one sprite at `Offset` that only grows and
    /// fades; the flame shader's noise is not run.
    pub fn bake(&self, launch: &Launch, seed: u32) -> Option<Rdp> {
        if !self.drawn() {
            return None;
        }
        let life = self.life_time.max(1) as f32;
        let mut rand = Rand(seed);
        let (cycle, mut born) = self.births(launch, &mut rand);
        let run = match self.looped {
            true => cycle * (life / cycle).ceil().max(1.),
            false => born.iter().map(|b| b.at).fold(0., f32::max) + life + FRAME_MS,
        };
        if self.looped {
            born = self.cycles(born, cycle, run, launch, &mut rand);
        }
        let frames = (run / FRAME_MS).ceil().clamp(2., 256.) as u32;
        let ft = run / frames as f32;
        let mut rdp = Rdp::default();
        rdp.frame_count = frames;
        rdp.frame_time = ft;
        rdp.looped = self.looped;
        rdp.lighting = self.light_receiver;
        rdp.texture_path = self.texture.clone();
        rdp.materials = vec![self.material()];
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        let mut top = 0f32;
        for b in &born {
            let start = (b.at / ft).ceil();
            let ticks = (((b.at + life) / ft).ceil() - start).clamp(1., frames as f32) as u32;
            if rdp.positions.len() + ticks as usize > MOST_TICKS {
                break;
            }
            let first = start as u32 % frames;
            rdp.starts.push(first as u16);
            rdp.ends.push(((first + ticks - 1) % frames) as u16);
            rdp.particle_materials.push(0);
            for j in 0..ticks {
                let t = (start + j as f32) * ft;
                let age = ((t - b.at) / life).clamp(0., 1.);
                let (at, size, angle, color) = self.tick(b, age, (t - b.at) / 1000., launch, t);
                for a in 0..3 {
                    lo[a] = lo[a].min(at[a]);
                    hi[a] = hi[a].max(at[a]);
                }
                top = top.max(size);
                rdp.positions.push(at);
                // 2070's quad spans ±Scale; an `.rdp` quad ±scale / 2.
                rdp.scales.push([size * 2.; 3]);
                // The game turns a sprite's corners clockwise by the angle
                // (`vs_particle_transform`); an `.rdp` turns them about the
                // quad's normal the other way.
                rdp.rotations.push([0., -angle, 0.]);
                rdp.colors.push(color);
            }
        }
        if rdp.starts.is_empty() {
            (lo, hi) = ([0.; 3], [0.; 3]);
        }
        rdp.center = std::array::from_fn(|a| (lo[a] + hi[a]) / 2.);
        rdp.extents = std::array::from_fn(|a| ((hi[a] - lo[a]) / 2.).max(1e-3));
        rdp.max_scale = (top * 2.).max(1e-3);
        Some(rdp)
    }

    /// Blending and the sprite's rect. Every kind faces the camera; the
    /// turn along the emitter (`Alignment` 2) depends on the view and is not
    /// baked.
    fn material(&self) -> Material {
        let [tu, tv, ou, ov] = self.atlas;
        let mut m = Material::default();
        m.view_aligned = 1;
        m.additive = self.technique == 1;
        m.refractive = self.technique == 2;
        m.wave_particle = self.technique == 5;
        // `cTextureAtlasCoordinates` puts (OffsetU, OffsetV) at the quad's
        // lower left and (TilingU, TilingV) at its upper right; an `.rdp`
        // crop starts at the upper left.
        m.crop = [ou, tv, tu - ou, ov - tv];
        m
    }

    fn curve_or(&self, name: &str) -> Curve {
        self.curve(name).cloned().unwrap_or_default()
    }

    /// The length of one emitter cycle in ms, and its particles (all of a
    /// one-shot run), times from the cycle's start.
    fn births(&self, launch: &Launch, rand: &mut Rand) -> (f32, Vec<Born>) {
        let duration = self.duration as f32;
        let delay = self.delay as f32;
        let mut born = Vec::new();
        if self.kind != PARTICLE {
            // One sprite a cycle, which lasts its life and the delay before.
            born.push(Born {
                at: delay,
                origin: self.offset,
                r: rand.next(),
                launch: [0.; 3],
                turn: [0.; 3],
            });
            return (self.cycle(), born);
        }
        let pep = self.curve_or("PEP");
        // `(int)PEP`, at least 1 ms (the game's detail factor taken as 1).
        let every = |t: f32| match pep.at(t) {
            p if p >= 1. => p.trunc(),
            _ => 1.,
        };
        let cycle = self.cycle();
        if self.looped && self.delay == 0 {
            // No delay keeps the emitter's clock running across cycles:
            // particles come evenly through the seam.
            let mut s = 0.;
            while s < cycle && born.len() < MOST_PARTICLES {
                let t = self.time_in(s);
                born.push(self.born(s, t, launch, rand));
                s += every(t);
            }
            return (cycle, born);
        }
        // Each cycle starts counting after its delay: the first particle a
        // full interval in, the last the one whose interval began within
        // `duration`.
        let mut s = every(0.);
        let mut previous = 0.;
        while previous <= duration && born.len() < MOST_PARTICLES {
            let t = self.time_in(s);
            born.push(self.born(delay + s, t, launch, rand));
            previous = s;
            s += every(t);
        }
        (cycle, born)
    }

    fn born(&self, at: f32, t: f32, launch: &Launch, rand: &mut Rand) -> Born {
        let r = rand.next();
        let dx = rand.next() * 2. - 1.;
        let dz = rand.next() * 2. - 1.;
        let area = self.curve_or("EmitArea");
        let radius = area.at(t) * area.spread(r);
        let len = (dx * dx + dz * dz).sqrt();
        let dir = match len > 0. {
            true => [dx / len, 0., dz / len],
            false => [0.; 3],
        };
        let mut velocity = [0.; 3];
        if launch.speed != 0. {
            // Even over the cap's area: the tilt's cosine from 1 down to the
            // spread's.
            let lowest = launch.spread.clamp(0., 180.).to_radians().cos();
            let up = 1. - rand.next() * (1. - lowest);
            let side = (1. - up * up).max(0.).sqrt();
            let turn = std::f32::consts::TAU * rand.next();
            velocity = [
                side * turn.cos() * launch.speed,
                up * launch.speed,
                side * turn.sin() * launch.speed,
            ];
        }
        Born {
            at,
            origin: std::array::from_fn(|a| dir[a] * radius + self.offset[a]),
            r,
            launch: velocity,
            turn: self.turn(t),
        }
    }

    /// A looped run of `run` ms: the cycle's particles over and over, each
    /// cycle with its own rolls, wrapped into the run.
    fn cycles(
        &self,
        first: Vec<Born>,
        cycle: f32,
        run: f32,
        launch: &Launch,
        rand: &mut Rand,
    ) -> Vec<Born> {
        let n = (run / cycle).round().max(1.) as usize;
        let pattern: Vec<f32> = first.iter().map(|b| b.at).collect();
        let roll = first.first().map_or(0., |b| b.r);
        let mut out = first;
        for c in 1..n {
            for &at in &pattern {
                if out.len() >= MOST_PARTICLES {
                    break;
                }
                let mut b = match self.kind {
                    PARTICLE => self.born(at, self.time_in(at - self.delay as f32), launch, rand),
                    // A billboard keeps its one roll for good.
                    _ => Born {
                        at,
                        origin: self.offset,
                        r: roll,
                        launch: [0.; 3],
                        turn: [0.; 3],
                    },
                };
                b.at += c as f32 * cycle;
                out.push(b);
            }
        }
        for b in &mut out {
            b.at = b.at.rem_euclid(run);
        }
        out
    }

    /// One emitter cycle in ms: its delay and its duration (a particle
    /// emitter's) or its life (a single sprite's).
    fn cycle(&self) -> f32 {
        match self.kind {
            PARTICLE => (self.delay + self.duration).max(1) as f32,
            _ => (self.delay + self.life_time.max(1)) as f32,
        }
    }

    /// The emitter's 0..1 time `s` ms after its delay: past the end of its
    /// duration it reads the end, and a duration of 0 reads 1 throughout
    /// (`minss` keeps 1 over NaN).
    fn time_in(&self, s: f32) -> f32 {
        let duration = self.duration as f32;
        match duration > 0. {
            true => (s.max(0.) / duration).min(1.),
            false => 1.,
        }
    }

    /// [`Effect::time_in`] at `at` ms into a looped run of cycles.
    fn cycle_time(&self, at: f32, cycle: f32) -> f32 {
        self.time_in(at.rem_euclid(cycle) - self.delay as f32)
    }

    /// The emitter's yaw, pitch and roll at its time `t` (`sub_14089F7E0`):
    /// `EmitterRotY`, `X`, `Z` in radians, each times `1 - varianz` when it
    /// has one.
    fn turn(&self, t: f32) -> [f32; 3] {
        let read = |name: &str| {
            self.curve(name).map_or(0., |c| match c.varianz != 0. {
                true => c.at(t) * (1. - c.varianz),
                false => c.at(t),
            })
        };
        [
            read("EmitterRotY"),
            read("EmitterRotX"),
            read("EmitterRotZ"),
        ]
    }

    /// Particle `b` at `age` (0..1), `seconds` after its birth and `at` ms
    /// into the run.
    fn tick(&self, b: &Born, age: f32, seconds: f32, launch: &Launch, at: f32) -> Tick {
        let scalar = |name: &str, spread: bool| {
            self.curve(name).map_or(0., |c| match spread {
                true => c.at(age) * c.spread(b.r),
                false => c.at(age),
            })
        };
        let path = ["VelocityX", "VelocityY", "VelocityZ"];
        let (path, angle) = match self.kind {
            PARTICLE => (path.map(|n| scalar(n, true)), scalar("Rotation", true)),
            BILLBOARD => (path.map(|n| scalar(n, true)), scalar("RotationZ", false)),
            _ => ([0.; 3], 0.),
        };
        // A negative size mirrors the quad, which looks the same.
        let size = scalar("Scale", true).abs();
        let fall = 0.5 * launch.gravity * seconds * seconds;
        let local: [f32; 3] = std::array::from_fn(|a| {
            let lift = if a == 1 { fall } else { 0. };
            b.origin[a] + path[a] + b.launch[a] * seconds + lift
        });
        // A bound particle turns with the emitter, else it keeps its birth
        // turn; a billboard's turn follows its age.
        let turn = match (self.kind, self.bound) {
            (BILLBOARD, _) => self.turn(age),
            (_, true) => self.turn(self.cycle_time(at, self.cycle())),
            (_, false) => b.turn,
        };
        let rgb = match self.curve("Color") {
            Some(c) => c.values_at(age),
            None => [1.; 4],
        };
        let alpha = self.curve("Alpha").map_or(1., |c| c.at(age));
        let color = [rgb[0], rgb[1], rgb[2], alpha].map(|v| v.clamp(0., 1.));
        (yaw_pitch_roll(local, turn), size, angle, color)
    }
}

/// `v` turned as D3DX's `RotationYawPitchRoll` turns a row vector: roll
/// about Z, then pitch about X, then yaw about Y.
fn yaw_pitch_roll(v: [f32; 3], [yaw, pitch, roll]: [f32; 3]) -> [f32; 3] {
    if yaw == 0. && pitch == 0. && roll == 0. {
        return v;
    }
    let (s, c) = roll.sin_cos();
    let v = [v[0] * c - v[1] * s, v[0] * s + v[1] * c, v[2]];
    let (s, c) = pitch.sin_cos();
    let v = [v[0], v[1] * c - v[2] * s, v[1] * s + v[2] * c];
    let (s, c) = yaw.sin_cos();
    [v[0] * c + v[2] * s, v[1], -v[0] * s + v[2] * c]
}

#[cfg(test)]
#[path = "efc_tests.rs"]
mod tests;
