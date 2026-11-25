// Owns what an ambient preset makes of an island's light that every game
// shares: where the sun and the moon stand, the sky baked for a moment
// (`Baked`), the eye adaptation over it, the `Sky` each game's presets
// are read into by its own crate, the `SkyPass` a game draws its sky
// and haze with, and the glow models give off at an hour.
use gpui_editor_kit::gpu::{Curve, Pass, SkyMap};
use std::sync::Arc;

/// Where the sun and the moon stand, unit vectors toward them (y up).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunMoon {
    pub sun: [f32; 3],
    pub moon: [f32; 3],
}

impl SunMoon {
    /// At `hours` (0-24) for an ambient preset's `MaxSunElevation` and
    /// `MinSunElevation` (radians). The sun rises at 6:00, stands highest
    /// at noon, toward the south-west the editor always lit from, sets at
    /// 18:00, and never sinks below the minimum, so nights stay twilit;
    /// the moon runs the same path twelve hours behind.
    /// An approximation from the preset's two elevations, for presets lit
    /// by fixed colors; 117's skies run the engine's own path
    /// (`anno117::sky::DayPath`).
    pub fn at(hours: f32, max_elevation: f32, min_elevation: f32) -> Self {
        let phase = (hours - 6.) / 24. * std::f32::consts::TAU;
        let noon = 0.74f32.atan2(-0.67);
        let azimuth = noon + (hours - 12.) / 12. * std::f32::consts::PI;
        let toward = |elevation: f32, azimuth: f32| {
            [
                elevation.cos() * azimuth.cos(),
                elevation.sin(),
                elevation.cos() * azimuth.sin(),
            ]
        };
        let rise = phase.sin() * max_elevation;
        Self {
            sun: toward(rise.max(min_elevation), azimuth),
            moon: toward((-rise).max(min_elevation), azimuth + std::f32::consts::PI),
        }
    }

    /// How far into the night, 0 to 1: the moon takes over as the sun goes
    /// below the horizon (cDayNight.w in Anno's shaders).
    pub fn night(&self) -> f64 {
        linear_step(0., 0.2, -f64::from(self.sun[1]))
    }

    /// How far into the day, 0 to 1 (cDayNight.z): the sun's light comes up
    /// over its first tenth above the horizon.
    pub fn day(&self) -> f64 {
        linear_step(0., 0.1, f64::from(self.sun[1]))
    }

    /// The day ratio (cDayNight.x) for skies that bring none of their own:
    /// across the sun's crossing of the horizon. An estimate; both games'
    /// own skies give the engine's ([`Sky::day_ratio`]).
    pub fn day_ratio(&self) -> f32 {
        linear_step(-0.1, 0.1, f64::from(self.sun[1])) as f32
    }
}

/// What a bake gives the renderer: the sky's radiance map, the scene's one
/// light (toward the sun by day, the moon at night) and its color on the
/// ground ([`gpui_editor_kit::gpu::Light::sun_color`]), and the radiance of
/// the sun's disc ([`gpui_editor_kit::gpu::Sky::sun_disc`]), black at night.
#[derive(Clone, Debug)]
pub struct Baked {
    pub map: std::sync::Arc<SkyMap>,
    pub toward: [f32; 3],
    pub light: [f32; 3],
    pub disc: [f32; 3],
    /// The scene's diffuse light when it is not the map's own
    /// ([`gpui_editor_kit::gpu::Sky::ambient`]): 1800's diffuse cubes.
    pub ambient: Option<[[[f32; 4]; 4]; 3]>,
}

impl Baked {
    /// The sky's mean radiance around the horizon, for the flat colors that
    /// still want one (the sky's gradient fallback).
    pub fn horizon(&self) -> [f32; 3] {
        let n = 32;
        let sum = (0..n).fold([0f32; 3], |acc, i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            let c = self.map.sample([a.cos(), 0.02, a.sin()]);
            std::array::from_fn(|k| acc[k] + c[k])
        });
        sum.map(|c| c / n as f32)
    }

    pub fn zenith(&self) -> [f32; 3] {
        self.map.sample([0., 1., 0.])
    }
}

/// A game's sky as one ambient preset describes it, read by the game's crate.
pub trait Sky: Send + Sync + std::fmt::Debug {
    /// Toward the sun and the moon at `hours` (0-24).
    fn sun_moon(&self, hours: f32) -> SunMoon;
    /// The sky, its light and the sun's disc at `hours`.
    fn bake(&self, hours: f32) -> Baked;
    /// Whether the bake also moves with the renderer's clock
    /// (`gpu::clock`): clouds drifting across it, so it is baked again as
    /// the clock runs.
    fn drifts(&self) -> bool {
        false
    }
    /// The preset's own tone curve, which the scene then shows as exposed
    /// with the sky's light as is (1800's); `None` for the game's
    /// `Module::tone_curve` over its non-raytraced ambient (117's).
    fn curve(&self) -> Option<Curve> {
        None
    }
    /// How much it is day at `hours`, 0 at night to 1 (cDayNight.x): night
    /// masks light as it falls. By the sun's height unless the game's
    /// engine says otherwise.
    fn day_ratio(&self, hours: f32) -> f32 {
        self.sun_moon(hours).day_ratio()
    }
    /// The game's own type behind this sky, for its `SkyPass`.
    fn as_any(&self) -> &dyn std::any::Any;
    /// The shade its clouds cast on the sun's light at `hours`; none for
    /// skies without.
    fn cloud_shadows(&self, _hours: f32) -> Option<gpui_editor_kit::gpu::CloudShadows> {
        None
    }
    /// Writes the numbers the game's scene lighting reads of this preset
    /// into the frame's host words (`gpu::Glow::host`), over what the model
    /// family put there: 2070's scattering constants. Most skies write none.
    fn host_words(&self, _words: &mut [f32; gpui_editor_kit::gpu::HOST_WORDS]) {}
}

/// A way of seeing an island other than from the air, which the island
/// view switches to and back (2070's from below the surface): the family
/// set's view mode (`gpu::Scene::view_mode`), the pictures its lighting
/// reads, and the passes it draws in place of the sky's and the water's.
#[derive(Clone)]
pub struct View {
    /// What the View settings call it.
    pub name: &'static str,
    /// `gpu::Scene::view_mode`: the island's family set's
    /// `Lighting::modes[mode - 1]`.
    pub mode: u32,
    pub host_texture: Option<gpui_editor_kit::gpu::HostTexture>,
    pub passes: Vec<Arc<dyn Pass>>,
}

impl std::fmt::Debug for View {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("View")
            .field("name", &self.name)
            .field("mode", &self.mode)
            .field("passes", &self.passes.len())
            .finish()
    }
}

/// What a game's sky and water passes may leave out or draw cheaper: the
/// editor's Graphics pane. The default draws all of it, as the game does.
/// What the kit draws (shadows, occlusion, cloud shadows) is the kit's
/// `gpu::Detail` and the scene's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detail {
    /// 117's volumetric clouds; off, the sky shows clear.
    pub clouds: bool,
    /// 117's haze over the island; the sky keeps its own air.
    pub haze: bool,
    /// Water reflecting the island in screen space; off, the sky alone.
    pub reflections: bool,
    /// Every wave band; off, the finest ripples are left out.
    pub fine_waves: bool,
}

impl Default for Detail {
    fn default() -> Self {
        Self {
            clouds: true,
            haze: true,
            reflections: true,
            fine_waves: true,
        }
    }
}

/// A game's drawing of its sky over one island view (`Module::sky`): the
/// backdrop as the camera sees it, and the haze over what stands in it.
/// The scene's light still comes from `Sky::bake`, as in game, where the
/// sky is lit from a cube rendered at the ground and drawn per pixel from
/// the camera.
pub trait SkyPass: Send + Sync {
    /// What the next frames draw: `sky`'s preset at `hours` (0-24).
    /// `None`, or a sky of another game's crate, draws nothing and leaves
    /// the kit's backdrop.
    fn show(&self, sky: Option<&Arc<dyn Sky>>, hours: f32);
    /// What it may leave out from the next frame on.
    fn detail(&self, _detail: Detail) {}
    /// The scene passes that draw it.
    fn passes(&self) -> Vec<Arc<dyn Pass>>;
    /// Whether what is shown hazes the distance itself, so the scene's own
    /// fog stays off.
    fn hazes(&self) -> bool;
    /// The cube surfaces reflect under what is shown (`gpu::Scene::
    /// environment`); `None` reflects the kit's sky map.
    fn environment(&self) -> Option<gpui_editor_kit::gpu::Environment> {
        None
    }
    /// Whether what it draws has come up and, for a view at rest, stopped
    /// changing: what it computes off the frame is in and anything it
    /// builds up over frames has settled.
    fn settled(&self) -> bool {
        true
    }
}

fn linear_step(e0: f64, e1: f64, x: f64) -> f64 {
    ((x - e0) / (e1 - e0)).clamp(0., 1.)
}

/// A game's sea as one ambient preset describes it, read by the game's
/// crate (117's ocean) and drawn by that game's `water::Water` pass, which
/// reads it back through `as_any`. Presets without one draw the editor's
/// flat sea.
pub trait Sea: Send + Sync + std::fmt::Debug {
    fn as_any(&self) -> &dyn std::any::Any;
}

/// How an ambient preset lights the island, in the game's linear units:
/// `LightColor` is the sun's radiance, `GroundColorAmbient` the light from
/// below, `Exposure` scales before tone mapping, `SunAO` is how much
/// occlusion maps shade the sun, `AOIntensity` how dark screen-space
/// occlusion gets, `AmbientLightFactor` scales the sky's light, and
/// `MaxSunElevation` (radians) is how high the sun stands. Presets with a
/// `sky` light and draw by it; for the rest `sky` (its light) and
/// `zenith`/`horizon` (how it looks) are fixed stand-ins, the horizon a
/// pale haze as in game.
#[derive(Clone, Debug)]
pub struct Ambient {
    pub sun: [f32; 3],
    pub sky_light: [f32; 3],
    pub zenith: [f32; 3],
    pub horizon: [f32; 3],
    pub ground: [f32; 3],
    pub exposure: f32,
    pub sun_occlusion: f32,
    pub occlusion: f32,
    pub ambient: f32,
    pub elevation: f32,
    /// `MinSunElevation` (radians): how far the sun sinks at night.
    pub min_elevation: f32,
    /// The preset's own sky model, which then replaces `sun`, `sky_light`,
    /// `zenith` and `horizon`: 117's physical atmosphere, 1800's time of
    /// day curves and cube maps.
    pub sky: Option<Arc<dyn Sky>>,
    /// The preset's sea, when the game draws one of its own.
    pub sea: Option<Arc<dyn Sea>>,
    /// `GlowBrightness` and `NightGlowBrightness`: how bright models' own
    /// light shines, and what their night masks add to it.
    pub glow: f32,
    pub night_glow: f32,
}

impl Default for Ambient {
    /// `amb_roman_01` and `amb_celtic_01`, which differ only in `SunAO`:
    /// what any game's preset falls back to for what it leaves out.
    fn default() -> Self {
        Self {
            sun: [4.197, 3.597, 3.198],
            sky_light: [1.0, 1.12, 1.3],
            zenith: [0.32, 0.58, 1.15],
            horizon: [1.05, 1.2, 1.4],
            ground: [0.2; 3],
            exposure: 0.55,
            sun_occlusion: 0.16,
            occlusion: 0.8,
            ambient: 1.,
            elevation: 0.65,
            min_elevation: -0.2,
            sky: None,
            sea: None,
            glow: 1.5,
            night_glow: 1.5,
        }
    }
}

/// Equal when the light is and the sky and sea are the same ones read: the
/// editor re-bakes the sky when its ambient changes, and a model read once
/// is shared, never edited.
impl PartialEq for Ambient {
    fn eq(&self, other: &Self) -> bool {
        fn same<T: ?Sized>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
        }
        self.sun == other.sun
            && self.sky_light == other.sky_light
            && self.zenith == other.zenith
            && self.horizon == other.horizon
            && self.ground == other.ground
            && self.exposure == other.exposure
            && self.sun_occlusion == other.sun_occlusion
            && self.occlusion == other.occlusion
            && self.ambient == other.ambient
            && self.elevation == other.elevation
            && self.min_elevation == other.min_elevation
            && self.glow == other.glow
            && self.night_glow == other.night_glow
            && same(&self.sky, &other.sky)
            && same(&self.sea, &other.sea)
    }
}

/// Orders an island's ambient presets for the picker: its own region's
/// (`own`, a name prefix) first, then `amb_default_*`, then other `amb_*`,
/// then the rest, each alphabetically, without repeats.
pub fn order_presets(names: &mut Vec<String>, own: &str) {
    let rank = |name: &str| {
        if name.starts_with(own) {
            0
        } else if name.starts_with("amb_default_") {
            1
        } else if name.starts_with("amb_") {
            2
        } else {
            3
        }
    };
    names.sort_by(|a, b| (rank(a), a).cmp(&(rank(b), b)));
    names.dedup();
}

/// What the game's models take of the scene beside its light: the colour
/// their dye masks blend toward and the light they give off, steady and by
/// night. The renderer's model family reads it (anno-render `a7m_models`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelGlow {
    /// What dye masks blend the albedo toward, linear.
    pub tint: [f32; 3],
    /// Scales all the light models give off.
    pub strength: f32,
    /// Night masks light where they rise past this, over a quarter of
    /// their range, the brightest texels first: 1 lights none, 0 every
    /// texel above 0.
    pub night: f32,
    /// What a fully lit night mask adds to the steady one.
    pub night_strength: f32,
    /// Where a night mask lights, the light turns toward its own grey
    /// times this, keeping `night_keep` of its colour (1 keeps it all).
    pub night_color: [f32; 3],
    pub night_keep: f32,
    /// Night masks in this many steps, floored, as kept in so few bits; 0
    /// keeps them smooth.
    pub night_steps: f32,
}

impl Default for ModelGlow {
    /// Undyed, at full strength, lighting no night mask.
    fn default() -> Self {
        Self {
            tint: [1.; 3],
            strength: 1.,
            night: 1.,
            night_strength: 1.,
            night_color: [1.; 3],
            night_keep: 1.,
            night_steps: 0.,
        }
    }
}

/// The light models give off under `ambient` at `hours`, their dye masks
/// taking the owner colour `tint` (linear). Both games light night masks
/// alike (`ApplyNightGlow`): a texel lights once it passes the square of
/// the day ratio, and while the day ratio is under 0.9 the light turns the
/// grey of its colour times (0.9, 0.3, 0), 1800's colour for buildings
/// without electricity (the editor knows of none) and 117's only one.
/// `deferred`: 117 keeps the night mask in five bits of its G-buffer, 31
/// steps (`GBUFFER_UDATA_MASK`), truncated.
pub fn glow(ambient: &Ambient, hours: f32, tint: [f32; 3], deferred: bool) -> ModelGlow {
    let day = match &ambient.sky {
        Some(sky) => sky.day_ratio(hours),
        None => SunMoon::at(hours, ambient.elevation, ambient.min_elevation).day_ratio(),
    };
    ModelGlow {
        tint,
        strength: ambient.glow,
        night: day * day,
        night_strength: ambient.night_glow,
        night_color: [0.9, 0.3, 0.],
        night_keep: linear_step(0.9, 1., f64::from(day)) as f32,
        night_steps: if deferred { 31. } else { 0. },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixed-colour preset's night lights every night mask, its day none;
    // the night light turns orange only once the day ratio is under 0.9.
    #[test]
    fn glow_follows_the_hour() {
        let ambient = Ambient::default();
        let night = glow(&ambient, 0., [1.; 3], true);
        assert_eq!(
            (night.night, night.night_keep, night.night_steps),
            (0., 0., 31.)
        );
        assert_eq!(night.night_color, [0.9, 0.3, 0.]);
        let noon = glow(&ambient, 12., [1.; 3], false);
        assert_eq!(
            (noon.night, noon.night_keep, noon.night_steps),
            (1., 1., 0.)
        );
        assert_eq!((noon.strength, noon.night_strength), (1.5, 1.5));
    }
}
