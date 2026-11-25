// Owns the island skies the Model viewport can light a model by in place of
// its studio: one of the game's ambient presets as an island shows it, its
// sky behind the model and its sun, sky light and exposure on it, at noon
// for Day and midnight for Night. The preset is read, baked and drawn by
// what islands use (the game's `SceneParts`, `ambient::Sky::bake`, its
// `SkyPass`), and lit as `IslandEditor::present` lights an island; only the
// choosing is here.

use anno_core::ambient::{self, Ambient, Baked, SkyPass, SunMoon};
use anno_core::files::ModelLook;
use anno_core::game::Module;
use anno_formats::vfs::Install;
use gpui_editor_kit::gpu::{Glow, Light, Occlusion, Scene, Shadows, Sky, Tone};
use std::sync::Arc;

/// How many presets the View menu offers: the first the island picker
/// lists for the model's region, the region's own first.
pub const OFFERED: usize = 4;

/// The hours Day and Night show.
const HOURS: [f32; 2] = [12., 0.];

/// The presets the View menu offers for the model at `path`: the first
/// the island picker lists for an island of its region. None for a game
/// whose islands take their light from elsewhere (2070's).
pub fn offered(module: &dyn Module, install: &Install, path: &str) -> Vec<String> {
    let (Some(scene), Some(islands)) = (module.scene(), module.islands()) else {
        return Vec::new();
    };
    let region = islands.rules().island_region(path);
    let mut names = scene.ambient_names(install, region);
    names.truncate(OFFERED);
    names
}

/// An ambient preset read and baked for the view's two hours.
pub struct IslandSky {
    pub name: String,
    ambient: Ambient,
    /// Noon's and midnight's bakes; `None` for a preset lit by fixed
    /// colours.
    baked: [Option<Baked>; 2],
    /// The game's own sky, drawn from the camera; `None` leaves the kit's
    /// backdrop from the bake.
    pub pass: Option<Arc<dyn SkyPass>>,
    tone: Tone,
    glow: [Glow; 2],
}

impl std::fmt::Debug for IslandSky {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IslandSky")
            .field("name", &self.name)
            .finish()
    }
}

impl IslandSky {
    /// Reads preset `name` and bakes its sky; `pass` is the game's sky pass
    /// to draw it with, made anew when `None`. Off the UI thread: a 117
    /// bake takes about 9 ms, the first read of a preset's maps longer.
    pub fn load(
        module: &dyn Module,
        install: &Install,
        look: &ModelLook,
        name: &str,
        pass: Option<Arc<dyn SkyPass>>,
    ) -> Option<IslandSky> {
        let scene = module.scene()?;
        let (name, ambient) = scene.ambients(install, &[name]).into_iter().next()?;
        let baked = HOURS.map(|h| ambient.sky.as_ref().map(|s| s.bake(h)));
        // As an island's: a sky with a curve of its own (1800's) shows its
        // light as is; the rest take the game's curve and eye adaptation
        // over its light without ray tracing.
        let tone = match ambient.sky.as_ref().and_then(|s| s.curve()) {
            Some(curve) => Tone {
                exposure: ambient.exposure,
                sun_occlusion: ambient.sun_occlusion,
                ambient: 1.,
                curve,
                auto: None,
            },
            None => Tone {
                exposure: ambient.exposure,
                sun_occlusion: ambient.sun_occlusion,
                ambient: ambient.ambient * crate::island::NO_RT_AMBIENT,
                curve: scene.tone_curve(),
                auto: scene.auto_exposure(),
            },
        };
        let glow = HOURS.map(|h| {
            let mut glow = look.glow(&ambient::glow(&ambient, h, look.dye, look.deferred_glow));
            if let Some(sky) = &ambient.sky {
                sky.host_words(&mut glow.host);
            }
            glow
        });
        Some(IslandSky {
            pass: pass.or_else(|| scene.sky(install)),
            name,
            ambient,
            baked,
            tone,
            glow,
        })
    }

    /// Lights `scene` as an island of this preset is lit at noon or, by
    /// `night`, at midnight: its sun (the moon by night), sky and exposure,
    /// its sky behind everything and `shadows` from its sun.
    pub fn light(&self, scene: &mut Scene, night: bool, shadows: Option<Shadows>) {
        let k = usize::from(night);
        let a = &self.ambient;
        let baked = self.baked[k].as_ref();
        let at = match &a.sky {
            Some(sky) => sky.sun_moon(HOURS[k]),
            None => SunMoon::at(HOURS[k], a.elevation, a.min_elevation),
        };
        // Presets lit by fixed colours keep the sun a little above the
        // horizon, as islands do.
        let sun = baked.map_or([at.sun[0], at.sun[1].max(0.05), at.sun[2]], |b| b.toward);
        scene.light = Light {
            sun,
            sun_color: baked.map_or(a.sun, |b| b.light),
            // Lights only without a sky map; with one the map lights.
            sky: a.sky_light,
            ground: a.ground,
            shadows,
            fog: None,
        };
        scene.tone = Some(self.tone);
        // The island's HBAO+ at the preset's strength.
        scene.occlusion = Some(Occlusion {
            radius: 2.5,
            strength: a.occlusion,
            min_pixels: 32.,
            ..Default::default()
        });
        scene.glow = self.glow[k];
        scene.environment = self.pass.as_ref().and_then(|p| p.environment());
        scene.sky = Some(Sky {
            zenith: baked.map_or(a.zenith, Baked::zenith),
            horizon: baked.map_or(a.horizon, Baked::horizon),
            // The floor, not the sea, lies under the model.
            sea: None,
            sun_disc: baked.map_or([0.; 3], |b| b.disc),
            ambient: baked.and_then(|b| b.ambient),
            radiance: baked.map(|b| b.map.clone()),
        });
        if let Some(pass) = &self.pass {
            pass.show(a.sky.as_ref(), HOURS[k]);
            scene.passes.extend(pass.passes());
        }
    }
}

impl crate::editor::Editor {
    /// Lights the open model by island sky `name`, read and baked off the
    /// UI thread, or by the studio with `None`.
    pub(crate) fn model_sky(
        &mut self,
        name: Option<String>,
        cx: &mut gpui_editor_kit::gpui::Context<Self>,
    ) {
        let Some(m) = &mut self.model else { return };
        let Some(name) = name else {
            m.view.island = None;
            m.pose();
            return self.notify_pane(crate::editor::Pane::ModelView, cx);
        };
        let Some(install) = self.install.clone() else {
            return;
        };
        let (module, look, path) = (self.module, self.module.model_look(), m.path.clone());
        // One sky pass for the editor's model, kept across presets.
        let pass = m.view.island.as_ref().and_then(|s| s.pass.clone());
        self.message(format!("Reading the {name} sky…"), false);
        cx.spawn(async move |this, cx| {
            let sky = cx
                .background_executor()
                .spawn(async move { IslandSky::load(module, &install, &look, &name, pass) })
                .await;
            this.update(cx, |this, cx| {
                let Some(m) = this.model.as_mut().filter(|m| m.path == path) else {
                    return;
                };
                match sky {
                    Some(sky) => {
                        let note = format!("Lit by the {} sky", sky.name);
                        m.view.island = Some(Arc::new(sky));
                        m.pose();
                        this.message(note, false);
                    }
                    None => this.message("That sky did not read", true),
                }
                this.notify_pane(crate::editor::Pane::ModelView, cx);
            })
            .ok();
        })
        .detach();
    }
}
