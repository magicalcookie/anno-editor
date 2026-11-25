// Owns the cfg's lights as they light the model in the Model viewport: each
// `<Lights>` entry read as the game reads it, its colour on the lights' own
// clock (ColorGraph flicker, DAYNIGHT_TRANSFORM by the view's day or night,
// and the COLOR_TRANSFORM colour playback gives every entry) and the kit
// light it becomes.
//
// 117's lights are as its executable builds them (build 26043928: the
// entry's reader at 0x14232EC20, its colour and reach at 0x14232D2F0 and
// 0x14232DEF0, the cluster upload at 0x1425825E0) and its shaders light
// them (functions_lighting_base.fxh). 1800 and 2070 light by 1800's
// shaders; how their executables turn an entry into a light is not traced,
// so their colour and reach are estimates.

use super::ModelEditor;
use anno_core::game::Game;
use anno_formats::model::Model;
use anno_formats::xml::{Doc, Id};
use gpui_editor_kit::{
    gpu::{Falloff, LocalLight, Spot},
    math::{self, Mat4, Vec3},
};
use std::f32::consts::{FRAC_PI_2, PI, TAU};

/// 117's light manager ends a light's reach where `Diffuse.a` over the
/// squared distance falls to this (its constant at +300, 0.008).
const CUTOFF: f32 = 0.008;
/// 117 uploads point and spot colours three times over (0x1425825E0).
const UPLOAD: f32 = 3.;

/// A light's `Type`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Point,
    /// `Theta` and `Phi` are its inner and outer half angles: 117 keeps
    /// their cosines and tests them against the angle off the aim.
    Spot {
        cos_inner: f32,
        cos_outer: f32,
    },
    Quad,
    Disc,
    Sphere,
    Line,
}

/// One `<Lights>` entry as the game reads it.
#[derive(Clone, Debug)]
struct Lamp {
    element: Id,
    kind: Kind,
    /// `Diffuse.rgb × Diffuse.a`, as 117 reads it, gamma encoded.
    color: [f32; 3],
    /// `Diffuse.a`: 117 scales the light and its reach by it.
    intensity: f32,
    /// 1800's reach; 117's is the light's fog volume and lights nothing.
    range: f32,
    falloff: f32,
    /// `Width` and `Height`, an area light's half extents.
    size: [f32; 2],
    /// `ColorGraph`: times 0 to 1 over `GraphDuration` seconds, each key's
    /// colour times its alpha.
    graph: Vec<(f32, [f32; 3])>,
    duration: f32,
    /// `AnimateOffset` and `AnimateSpeed`.
    wobble: Vec3,
    wobble_speed: f32,
    day_night: Option<DayNight>,
}

/// `DAYNIGHT_TRANSFORM`: lit during its time slice, fading in and out over
/// `FadeTime` seconds.
#[derive(Clone, Copy, Debug)]
struct DayNight {
    slice: u8,
    fade: f32,
}

impl DayNight {
    /// Whether the slice holds noon (day) or midnight (night). The game's
    /// own editor names them (0x142325770): 0 Night Begin to Night End, 1
    /// Sundown to Sunrise and 2 Day End to Day Begin hold the night; 3 Day
    /// Begin to Day End, 4 Sunrise to Sundown and 5 Night End to Day End
    /// the day.
    fn lit(self, night: bool) -> bool {
        (self.slice <= 2) == night
    }
}

/// What a lamp carries between frames: DAYNIGHT_TRANSFORM's share lit, 0
/// to 1.
#[derive(Clone, Copy, Debug)]
struct Run {
    element: Id,
    lit: f32,
}

/// The cfg's lights and their clock.
#[derive(Debug)]
pub struct Lights {
    game: Game,
    lamps: Vec<Lamp>,
    runs: Vec<Run>,
    /// Seconds the lights' clock has run: their graphs and wobble.
    clock: f32,
    /// Playback seconds not yet stepped.
    due: f32,
}

impl Lights {
    pub fn new(game: Game) -> Lights {
        Lights {
            game,
            lamps: Vec::new(),
            runs: Vec::new(),
            clock: 0.,
            due: 0.,
        }
    }

    /// Reads the model's lights from the doc again. A light still there
    /// keeps where its colour and fade had got to.
    pub fn read(&mut self, doc: &Doc, model: &Model) {
        self.lamps = (model.lights.iter())
            .map(|l| Lamp::read(doc, l.element))
            .collect();
        let old = std::mem::take(&mut self.runs);
        self.runs = (self.lamps.iter())
            .map(|lamp| {
                let kept = old.iter().find(|r| r.element == lamp.element);
                kept.copied().unwrap_or(Run {
                    element: lamp.element,
                    lit: 1.,
                })
            })
            .collect();
    }

    /// Lets `dt` seconds of playback pass on the next [`Lights::settle`].
    pub fn advance(&mut self, dt: f32) {
        self.clock += dt;
        self.due += dt;
    }

    /// Moves each light's DAYNIGHT_TRANSFORM toward `night` over the
    /// seconds advanced since; `snap` arrives at once, as a paused view
    /// does.
    pub fn settle(&mut self, night: bool, snap: bool) {
        let dt = std::mem::take(&mut self.due);
        for (lamp, run) in self.lamps.iter().zip(&mut self.runs) {
            if let Some(d) = lamp.day_night {
                let to = if d.lit(night) { 1. } else { 0. };
                run.lit = match snap || d.fade <= 0. {
                    true => to,
                    false if to > run.lit => (run.lit + dt / d.fade).min(to),
                    false => (run.lit - dt / d.fade).max(to),
                };
            }
        }
    }

    /// The lights in the kit's terms, in the game's light units times
    /// `scale`: light `k` of the model's placed by `place(k)` (its matrix
    /// in model space, which a DAMAGE_TRANSFORM scales, and its
    /// COLOR_TRANSFORM colour; `None` while hidden), at the clock.
    pub fn lit(
        &self,
        place: impl Fn(usize) -> Option<(Mat4, [f32; 4])>,
        scale: f32,
    ) -> Vec<LocalLight> {
        let mut out = Vec::new();
        for (k, (lamp, run)) in self.lamps.iter().zip(&self.runs).enumerate() {
            let Some((m, tint)) = place(k) else {
                continue;
            };
            let color = lamp.color_at(self.game, tint, run.lit, self.clock);
            if color.iter().all(|c| *c <= 0.) {
                continue;
            }
            let reach = lamp.reach(self.game, largest_scale(&m));
            if reach <= 0. {
                continue;
            }
            // An area light lights as a point at its middle, as bright as
            // the point light the game gives the same reach: an estimate.
            let area = match self.game {
                Game::Anno117 => lamp.area(),
                _ => 1.,
            };
            let at = math::add(math::translation(&m), lamp.wobble_at(self.clock));
            out.push(LocalLight {
                position: at,
                reach,
                color: color.map(|c| c * area * scale),
                falloff: match self.game {
                    Game::Anno117 => Falloff::InverseSquare,
                    _ => Falloff::Power(lamp.falloff),
                },
                spot: match lamp.kind {
                    // Down its own -Y, as its marker shows.
                    Kind::Spot {
                        cos_inner,
                        cos_outer,
                    } => Some(Spot {
                        aim: [-m[4], -m[5], -m[6]],
                        cos_inner,
                        cos_outer,
                    }),
                    _ => None,
                },
            });
        }
        out
    }

    /// How far `element`'s light reaches in metres, placed by `m`.
    pub fn reach(&self, element: Id, m: &Mat4) -> Option<f32> {
        let lamp = self.lamps.iter().find(|l| l.element == element)?;
        Some(lamp.reach(self.game, largest_scale(m)))
    }
}

/// The longest of a matrix's axes: what 117 scales a light's reach by.
fn largest_scale(m: &Mat4) -> f32 {
    (0..3)
        .map(|c| math::length([m[c * 4], m[c * 4 + 1], m[c * 4 + 2]]))
        .fold(0., f32::max)
}

impl Lamp {
    fn read(doc: &Doc, entry: Id) -> Lamp {
        let num = |at: Id, name: &str| {
            (doc.child(at, name)).and_then(|c| doc.text(c).trim().parse::<f32>().ok())
        };
        let f = |name: &str| num(entry, name);
        let diffuse = ["Diffuse.r", "Diffuse.g", "Diffuse.b", "Diffuse.a"].map(f);
        let intensity = diffuse[3].unwrap_or(1.);
        // Every shipped light names both; 0.5 and 0.8 are what most carry.
        let (theta, phi) = (f("Theta").unwrap_or(0.5), f("Phi").unwrap_or(0.8));
        let kind = match f("Type").unwrap_or(0.) as u32 {
            2 => Kind::Spot {
                cos_inner: theta.cos(),
                cos_outer: phi.cos(),
            },
            5 => Kind::Quad,
            6 => Kind::Disc,
            7 => Kind::Sphere,
            8 => Kind::Line,
            _ => Kind::Point,
        };
        let graph = (doc.child(entry, "ColorGraph").into_iter())
            .flat_map(|g| doc.children(g))
            .filter_map(|key| {
                let rgba = ["c.r", "c.g", "c.b", "c.a"].map(|n| num(key, n).unwrap_or(1.));
                Some((num(key, "t")?, [0, 1, 2].map(|c| rgba[c] * rgba[3])))
            })
            .collect();
        let wobble = doc.child(entry, "AnimateOffset").map_or([0.; 3], |o| {
            ["x", "y", "z"].map(|n| num(o, n).unwrap_or(0.))
        });
        let mut lamp = Lamp {
            element: entry,
            kind,
            color: [0, 1, 2].map(|c| diffuse[c].unwrap_or(1.) * intensity),
            intensity,
            range: f("Range").unwrap_or(1.),
            // 117 holds it at 2 at most.
            falloff: f("Falloff").unwrap_or(2.).min(2.),
            size: [f("Width").unwrap_or(1.), f("Height").unwrap_or(1.)],
            graph,
            duration: f("GraphDuration").unwrap_or(1.),
            wobble,
            wobble_speed: f("AnimateSpeed").unwrap_or(1.),
            day_night: None,
        };
        let transformers = (doc.child(entry, "Transformer").into_iter())
            .flat_map(|t| doc.children(t))
            .filter(|&c| doc.name(c) == "Config");
        for t in transformers {
            if doc
                .child(t, "ConfigType")
                .is_some_and(|c| doc.text(c) == "DAYNIGHT_TRANSFORM")
            {
                lamp.day_night = Some(DayNight {
                    // 1, Sundown to Sunrise, where it names none.
                    slice: num(t, "TimeSlice").map_or(1, |v| v as u8),
                    fade: num(t, "FadeTime").unwrap_or(0.25),
                });
            }
        }
        lamp
    }

    /// The light's colour in the game's linear units, 117's tripled as it
    /// uploads it: `pow(c, 2.2) × Diffuse.a × A` (0x14232D2F0), `c` the
    /// graph's colour at the clock, else `Diffuse` times the
    /// COLOR_TRANSFORM's colour `tint`, and `A` that colour's alpha times
    /// the day or night fade `lit`.
    fn color_at(&self, game: Game, tint: [f32; 4], lit: f32, clock: f32) -> [f32; 3] {
        let alpha = tint[3] * lit;
        if alpha <= 0. {
            return [0.; 3];
        }
        let base: [f32; 3] = match game {
            Game::Anno117 => std::array::from_fn(|c| tint[c] * self.color[c]),
            // An estimate: Diffuse the colour, its alpha the strength.
            _ => std::array::from_fn(|c| tint[c] * self.color[c] / self.intensity.max(1e-6)),
        };
        let c = self.graph_at(clock).unwrap_or(base);
        let upload = if game == Game::Anno117 { UPLOAD } else { 1. };
        c.map(|v| v.max(0.).powf(2.2) * self.intensity * alpha * upload)
    }

    /// The graph's colour `clock` seconds in (0x14232DEF0): it loops over
    /// `GraphDuration`, blending the keys around the time; `None` without
    /// a graph or past its last key.
    fn graph_at(&self, clock: f32) -> Option<[f32; 3]> {
        if self.graph.len() < 2 || self.duration <= 0. {
            return None;
        }
        let t = clock.rem_euclid(self.duration) / self.duration;
        let k = (self.graph.windows(2)).position(|w| t >= w[0].0 && w[1].0 > t)?;
        let ((t0, a), (t1, b)) = (self.graph[k], self.graph[k + 1]);
        let f = (t - t0) / (t1 - t0);
        Some(std::array::from_fn(|c| a[c] + (b[c] - a[c]) * f))
    }

    /// How far it lights at a scale of `size`. 117: until `Diffuse.a`
    /// times its area over the squared distance falls to [`CUTOFF`]
    /// (0x14232D2F0). 1800 and 2070: `Range`, scaled, an estimate.
    fn reach(&self, game: Game, size: f32) -> f32 {
        match game {
            Game::Anno117 => size * (self.area() * self.intensity.max(0.) / CUTOFF).sqrt(),
            _ => size * self.range,
        }
    }

    /// What 117 sizes the reach by, at unit scale: 1 for a point or spot,
    /// a quad's or disc's `4 × Width × Height`, a sphere's `2π × Width²`
    /// and a line's `4π × Width × Height`.
    fn area(&self) -> f32 {
        let [w, h] = self.size.map(f32::abs);
        match self.kind {
            Kind::Point | Kind::Spot { .. } => 1.,
            Kind::Quad | Kind::Disc => 4. * w * h,
            Kind::Sphere => TAU * w * w,
            Kind::Line => 2. * TAU * w * h,
        }
    }

    /// `AnimateOffset` swung by its phase, `clock × AnimateSpeed` radians,
    /// a quarter turn apart per axis (0x14232E200).
    fn wobble_at(&self, clock: f32) -> Vec3 {
        if self.wobble == [0.; 3] {
            return [0.; 3];
        }
        let phase = (clock * self.wobble_speed).rem_euclid(TAU);
        let [x, y, z] = self.wobble;
        [
            x * phase.sin(),
            y * (phase + FRAC_PI_2).sin(),
            z * (phase + PI).sin(),
        ]
    }
}

impl ModelEditor {
    /// Hands the view this frame's lights: each where `placed` (the
    /// entries playback moved this frame) puts it, hidden while it waits
    /// on a condition the sequence does not raise.
    pub(super) fn light_up(&mut self, raised: Option<u32>, placed: &[(Id, Mat4)]) {
        let snap = !self.playback.playing;
        self.lights.settle(self.view.night, snap);
        let (model, player) = (&self.model, &self.player);
        // The lamps are read in the model's order (`Lights::read`).
        let place = |k: usize| {
            let l = model.lights.get(k)?;
            if let (Some(bits), Some(need)) = (raised, l.rig.shown_if)
                && bits & need == 0
            {
                return None;
            }
            let m = l.transform.matrix();
            let m = match placed.iter().find(|(e, _)| *e == l.element) {
                Some((_, change)) => math::multiply(change, &m),
                None => m,
            };
            Some((m, player.color(l.element)))
        };
        let lit = self.lights.lit(place, self.view.light_scale());
        self.view.lights = std::sync::Arc::new(lit);
    }
}

#[cfg(test)]
mod tests {
    use super::Lights;
    use anno_core::game::Game;
    use anno_formats::model::Model;
    use anno_formats::xml::Doc;
    use gpui_editor_kit::gpu::{Falloff, LocalLight};

    /// A cfg of one light: `fields` inside its entry, `transformers` in
    /// its `<Transformer>`.
    fn cfg(fields: &str, transformers: &str) -> (Doc, Model) {
        let xml = format!(
            "<Config><ConfigType>MAIN</ConfigType><Lights><Config><ConfigType>LIGHT</ConfigType>\
             <Transformer>{transformers}</Transformer>{fields}</Config></Lights></Config>"
        );
        let doc = Doc::parse(xml.as_bytes());
        let model = Model::from_doc(&doc).unwrap();
        (doc, model)
    }

    fn lit(lights: &Lights, model: &Model, scale: f32) -> Vec<LocalLight> {
        lights.lit(
            |k| Some((model.lights[k].transform.matrix(), [1.; 4])),
            scale,
        )
    }

    fn lights(game: Game, doc: &Doc, model: &Model) -> Lights {
        let mut l = Lights::new(game);
        l.read(doc, model);
        l.settle(false, true);
        l
    }

    const WHITE: &str = "<Diffuse.r>2.845939</Diffuse.r><Diffuse.g>2.845939</Diffuse.g>\
        <Diffuse.b>2.845939</Diffuse.b><Diffuse.a>0.35</Diffuse.a><Range>1</Range>";

    // Most of 117's lights: a white light of strength 0.35. It reaches
    // where 0.35 over the squared distance falls to 0.008, half as far at
    // half the size, and its colour is pow(0.996, 2.2) · 0.35, tripled.
    #[test]
    fn a_117_light_reaches_as_far_as_its_strength_carries() {
        let (doc, model) = cfg(&format!("<Type>0</Type>{WHITE}"), "");
        let l = lights(Game::Anno117, &doc, &model);
        let out = lit(&l, &model, 1.);
        assert_eq!(out.len(), 1);
        assert!(
            (out[0].reach - (0.35f32 / 0.008).sqrt()).abs() < 1e-3,
            "{out:?}"
        );
        assert!((out[0].color[0] - 0.996f32.powf(2.2) * 0.35 * 3.).abs() < 1e-3);
        assert_eq!(out[0].falloff, Falloff::InverseSquare);
        let small = model.lights[0].transform.matrix().map(|v| v * 0.5);
        let reach = l.reach(model.lights[0].element, &small).unwrap();
        assert!((reach - out[0].reach * 0.5).abs() < 1e-3);
    }

    // An 1800 light reaches its Range and dims by its Falloff power.
    #[test]
    fn an_1800_light_reaches_its_range() {
        let fields = "<Type>0</Type><Diffuse.r>1</Diffuse.r><Diffuse.g>0.5</Diffuse.g>\
            <Diffuse.b>0.25</Diffuse.b><Diffuse.a>2</Diffuse.a><Range>3</Range><Falloff>1.5</Falloff>";
        let (doc, model) = cfg(fields, "");
        let out = lit(&lights(Game::Anno1800, &doc, &model), &model, 1.);
        assert!((out[0].reach - 3.).abs() < 1e-5);
        assert_eq!(out[0].falloff, Falloff::Power(1.5));
        assert!((out[0].color[0] - 2.).abs() < 1e-4 && out[0].color[1] < 0.5);
    }

    // A ColorGraph loops over GraphDuration: halfway between two keys the
    // light is halfway between their colours times their alphas.
    #[test]
    fn the_color_graph_plays_on_the_lights_clock() {
        let graph = "<GraphDuration>4</GraphDuration><ColorGraph>\
            <i><t>0</t><c.r>1</c.r><c.g>1</c.g><c.b>1</c.b><c.a>0.5</c.a></i>\
            <i><t>0.5</t><c.r>1</c.r><c.g>1</c.g><c.b>1</c.b><c.a>1</c.a></i>\
            <i><t>1</t><c.r>1</c.r><c.g>1</c.g><c.b>1</c.b><c.a>0.5</c.a></i></ColorGraph>";
        let (doc, model) = cfg(&format!("<Type>0</Type>{WHITE}{graph}"), "");
        let mut l = lights(Game::Anno117, &doc, &model);
        let at = |l: &Lights| lit(l, &model, 1.)[0].color[0];
        let start = at(&l);
        assert!(
            (start - 0.5f32.powf(2.2) * 0.35 * 3.).abs() < 1e-4,
            "{start}"
        );
        l.advance(1.);
        let quarter = at(&l);
        assert!(
            (quarter - 0.75f32.powf(2.2) * 0.35 * 3.).abs() < 1e-4,
            "{quarter}"
        );
        l.advance(3.);
        assert!((at(&l) - start).abs() < 1e-4, "loops");
    }

    // A DAYNIGHT_TRANSFORM of slice 2 (Day End to Day Begin) lights only
    // by night, fading in over its FadeTime while playback runs; slice 4
    // (Sunrise to Sundown) only by day.
    #[test]
    fn day_and_night_lights_light_at_their_hour() {
        let night = "<Config><ConfigType>DAYNIGHT_TRANSFORM</ConfigType><FadeTime>2</FadeTime>\
            <TimeSlice>2</TimeSlice></Config>";
        let (doc, model) = cfg(&format!("<Type>0</Type>{WHITE}"), night);
        let mut l = lights(Game::Anno117, &doc, &model);
        assert!(lit(&l, &model, 1.).is_empty(), "dark by day");
        l.advance(1.);
        l.settle(true, false);
        let half = lit(&l, &model, 1.)[0].color[0];
        l.settle(true, true);
        let full = lit(&l, &model, 1.)[0].color[0];
        assert!((half - full * 0.5).abs() < 1e-4, "{half} of {full}");
        let day = night.replace("<TimeSlice>2", "<TimeSlice>4");
        let (doc, model) = cfg(&format!("<Type>0</Type>{WHITE}"), &day);
        let mut l = lights(Game::Anno117, &doc, &model);
        assert_eq!(lit(&l, &model, 1.).len(), 1, "lit by day");
        l.settle(true, true);
        assert!(lit(&l, &model, 1.).is_empty(), "dark by night");
    }

    // The colour playback's COLOR_TRANSFORM gives the entry tints the
    // light and its alpha dims it: 117 takes it into the light's colour
    // before the gamma, its alpha after.
    #[test]
    fn the_entrys_colour_tints_the_light() {
        let (doc, model) = cfg(&format!("<Type>0</Type>{WHITE}"), "");
        let l = lights(Game::Anno117, &doc, &model);
        let at = model.lights[0].transform.matrix();
        let tinted = l.lit(|_| Some((at, [1., 0.5, 1., 0.5])), 1.);
        let white = 0.996f32.powf(2.2) * 0.35 * 3.;
        assert!((tinted[0].color[0] - white * 0.5).abs() < 1e-4);
        assert!((tinted[0].color[1] - 0.498f32.powf(2.2) * 0.35 * 3. * 0.5).abs() < 1e-3);
        assert!(
            l.lit(|_| Some((at, [1., 1., 1., 0.])), 1.).is_empty(),
            "clear"
        );
    }

    // A spot shines down its own -Y with Theta and Phi as half angles.
    #[test]
    fn spots_take_half_angles() {
        let spot = format!("<Type>2</Type>{WHITE}<Theta>0.2</Theta><Phi>0.35</Phi>");
        let (doc, model) = cfg(&spot, "");
        let out = lit(&lights(Game::Anno117, &doc, &model), &model, 1.);
        let s = out[0].spot.expect("a spot");
        assert_eq!(s.aim.map(|v| v + 0.), [0., -1., 0.]);
        assert!((s.cos_inner - 0.2f32.cos()).abs() < 1e-6);
        assert!((s.cos_outer - 0.35f32.cos()).abs() < 1e-6);
    }
}
