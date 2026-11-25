// Owns the editor's graphics settings: what the island view leaves out or
// draws cheaper, for every island, saved to graphics.json beside theme.json,
// and the Graphics pane that sets them. What the view shows (props, water,
// rain) stays per island in View settings; this is what showing it costs.
use crate::editor::Editor;
use anno_core::ambient;
use gpui_editor_kit::gpu::{self, CloudMotion};
use gpui_editor_kit::gpui::{
    Context, DragMoveEvent, InteractiveElement, IntoElement, ParentElement, ScrollEvent,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_editor_kit::widgets::{self, Tooltip, checkbox, hint, section};
use gpui_editor_kit::{field, platform, theme};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// The island view's quality settings. Every field has a default, today's
/// look, so a file missing some (an older editor's) keeps the rest.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Graphics {
    /// 117's volumetric clouds in the sky.
    pub clouds: bool,
    pub cloud_shadows: CloudShadows,
    /// 117's haze over the island.
    pub haze: bool,
    /// Water reflecting the island (screen space).
    pub reflections: bool,
    /// TODO: Reduced still steps every band's FFT (117's eight slices);
    /// skipping the dropped ones there is where a real saving would be.
    pub waves: Waves,
    pub shadows: Shadows,
    /// Metres from the camera the sharp shadow cascades cover; past them
    /// the island's one map shades, softer.
    pub shadow_distance: f32,
    pub occlusion: bool,
    /// How far from the camera trees, bushes and ground cover draw, by the
    /// game's own View Distance (`ViewDistances`).
    pub foliage: Foliage,
    /// Whether props and ground cover too small on screen for the game to
    /// draw are left out, by the game's own least size (`ViewDistances`).
    /// TODO: a ground cover density slider. The preview thins only as it
    /// scatters (vegetation::scatter's `max`), on the CPU, so a drag would
    /// rescatter the island; thinning rows on the GPU would need the kit.
    pub small_props: SmallProps,
    /// Share of the pane's pixels the scene draws at, each way.
    pub render_scale: f32,
    pub idle_fps: IdleFps,
    /// Models move: parts turn and swing, skinned figures play their idle.
    /// Off, they stand in their bind pose and a still view can rest whole.
    pub animation: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloudShadows {
    #[default]
    Smooth,
    Stepped,
    Off,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Waves {
    #[default]
    Full,
    Reduced,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shadows {
    #[default]
    High,
    Medium,
    Low,
    Off,
}

/// Which of the game's View Distance levels foliage follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Foliage {
    /// The game's Medium, level 1.
    Near,
    /// The level the game starts with, for its normal camera.
    #[default]
    Game,
    /// That level for the game's extended camera.
    Far,
    /// Every distance, as before the games' cut.
    Off,
}

/// Whether props the game would find too small to draw are drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmallProps {
    /// Left out under the game's MinimumObjectSizeInPixels, at the View
    /// Distance level it starts with.
    #[default]
    Game,
    /// All drawn, however small.
    Off,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdleFps {
    #[default]
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "30")]
    Thirty,
    #[serde(rename = "15")]
    Fifteen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    High,
    Medium,
    Low,
    Custom,
}

/// Shadow distance bounds the pane offers. 500 m is the games' own
/// (1800's MAX_SHADOW_DISTANCE_WS); under 100 m the cascades end inside a
/// close-up view.
const SHADOW_DISTANCE: (f32, f32) = (100., 500.);
const RENDER_SCALE: (f32, f32) = (0.25, 1.);

impl Default for Graphics {
    /// Today's look: everything on, at full quality.
    fn default() -> Self {
        Self {
            clouds: true,
            cloud_shadows: CloudShadows::Smooth,
            haze: true,
            reflections: true,
            waves: Waves::Full,
            shadows: Shadows::High,
            shadow_distance: SHADOW_DISTANCE.1,
            occlusion: true,
            foliage: Foliage::Game,
            small_props: SmallProps::Game,
            render_scale: 1.,
            idle_fps: IdleFps::Off,
            animation: true,
        }
    }
}

impl Graphics {
    /// A preset's settings; `Custom` is whatever was set by hand, so it
    /// gives the default.
    pub fn preset(preset: Preset) -> Self {
        let high = Self::default();
        match preset {
            Preset::High | Preset::Custom => high,
            // Looks the same at a glance: the cheaper choices whose
            // difference needs a side-by-side.
            // Reduced waves are left to Low: on Albion they saved 0.05 ms
            // and flattened the shore visibly.
            Preset::Medium => Self {
                cloud_shadows: CloudShadows::Stepped,
                shadows: Shadows::Medium,
                shadow_distance: 300.,
                idle_fps: IdleFps::Thirty,
                ..high
            },
            // For a slow GPU: what costs most goes, the haze and shadows
            // that place the island stay.
            Preset::Low => Self {
                clouds: false,
                cloud_shadows: CloudShadows::Off,
                reflections: false,
                waves: Waves::Reduced,
                shadows: Shadows::Low,
                shadow_distance: 200.,
                occlusion: false,
                foliage: Foliage::Near,
                render_scale: 0.75,
                idle_fps: IdleFps::Fifteen,
                animation: false,
                ..high
            },
        }
    }

    /// The preset these settings are, or `Custom`.
    pub fn which(&self) -> Preset {
        [Preset::High, Preset::Medium, Preset::Low]
            .into_iter()
            .find(|&p| Self::preset(p) == *self)
            .unwrap_or(Preset::Custom)
    }

    /// Numbers kept within what the pane offers: a hand-edited file
    /// cannot ask for a 0% scene or a negative distance.
    fn clamped(mut self) -> Self {
        let clamp = |v: f32, (lo, hi): (f32, f32), or: f32| {
            if v.is_finite() { v.clamp(lo, hi) } else { or }
        };
        let d = Self::default();
        self.shadow_distance = clamp(self.shadow_distance, SHADOW_DISTANCE, d.shadow_distance);
        self.render_scale = clamp(self.render_scale, RENDER_SCALE, d.render_scale);
        self
    }

    /// The sun's shadow maps at this quality; `None` when off.
    /// High is the games' own: four 2048² cascades in game, here three
    /// over the shadow distance, past them the island's 4096² map.
    pub fn shadow_maps(&self) -> Option<gpu::Shadows> {
        let (resolution, cascades, cascade_resolution) = match self.shadows {
            Shadows::High => (4096, 3, 2048),
            Shadows::Medium => (2048, 3, 1024),
            Shadows::Low => (2048, 2, 1024),
            Shadows::Off => return None,
        };
        Some(gpu::Shadows {
            resolution,
            // The games' filter: six taps 1.5 texels out.
            softness: 1.5,
            cascades,
            cascade_resolution,
            reach: self.shadow_distance,
        })
    }

    /// What the kit's renderer leaves out, foliage reaching as `distances`
    /// say for these settings.
    pub fn kit(&self, distances: &ViewDistances) -> gpu::Detail {
        gpu::Detail {
            min_pixels: distances.least_pixels(self.small_props),
            reach: distances.reach(self.foliage),
            cloud_shadows: match self.cloud_shadows {
                CloudShadows::Smooth => CloudMotion::Smooth,
                CloudShadows::Stepped => CloudMotion::Stepped,
                CloudShadows::Off => CloudMotion::Off,
            },
            idle_fps: match self.idle_fps {
                IdleFps::Off => None,
                IdleFps::Thirty => Some(30),
                IdleFps::Fifteen => Some(15),
            },
            live_ground: false,
        }
    }

    /// What the game's sky and water leave out.
    pub fn game(&self) -> ambient::Detail {
        ambient::Detail {
            clouds: self.clouds,
            haze: self.haze,
            reflections: self.reflections,
            fine_waves: self.waves == Waves::Full,
        }
    }

    /// Settings from graphics.json's bytes: unknown fields are ignored,
    /// missing ones take their default, and anything unreadable gives the
    /// defaults. Never an error: graphics settings must not stop the
    /// editor from starting.
    fn parse(bytes: &[u8]) -> Self {
        serde_json::from_slice::<Self>(bytes)
            .map(Self::clamped)
            .unwrap_or_default()
    }
}

/// Over the last quarter of its reach foliage thins out: 1800 fades its
/// ground cover from 75% of its view distance, dithered (prop_terrain_
/// object.fxh). How 117 and 1800 fade other props there is not traced.
const THIN: f32 = 0.25;

/// A game's foliage distances per View Distance level, from its engine
/// settings (`Profile::engine_settings`). Past `ClusterStartDistance` the
/// games draw island props as distance clusters, coarse merged meshes,
/// and past `ClusterViewDistance` nothing: the editor turns foliage to its
/// impostors and stops it there. Buildings end at `ObjectViewDistance`,
/// which the editor leaves alone.
///
/// Each level also keeps `MinimumObjectSizeInPixels`: 117 13 px at the
/// level it starts with (16 at level 0 to 12 at 4), 1800 6 px (12 to 6).
/// How the games measure an object against it is not traced: 1800 reads
/// it into its level's settings (+0x28, beside the fade multiplier) and
/// 117's code names none of these settings, so the reads that use it were
/// not found. The editor takes it as pixels of the screen, the box's
/// widest extent at its nearest point, and pops props as the games' own
/// thresholds would read: neither file names a fade for it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewDistances {
    /// Per level from 0, for the normal camera then the extended one:
    /// cluster start and cluster end in metres, and the least size in
    /// screen pixels (0 where a file has none).
    levels: [Vec<[f32; 3]>; 2],
    /// The level the game starts with.
    start: usize,
}

impl ViewDistances {
    /// `module`'s, as `install` serves its `default.xml` and `extended.xml`;
    /// empty, so foliage reaches anywhere, where they do not read.
    pub fn read(
        install: &anno_formats::vfs::Install,
        module: &dyn anno_core::game::Module,
    ) -> Self {
        let Some((folder, start)) = module.scene().and_then(|s| s.engine_settings()) else {
            return Self::default();
        };
        let table = |file: &str| -> Vec<[f32; 3]> {
            let Ok(bytes) = install.read(&format!("{folder}{file}")) else {
                return Vec::new();
            };
            Self::parse(&String::from_utf8_lossy(&bytes))
        };
        Self {
            levels: [table("default.xml"), table("extended.xml")],
            start: start as usize,
        }
    }

    /// Each `QualityLevel<n>`'s cluster start and end and least size,
    /// from level 0 up to the first missing.
    fn parse(xml: &str) -> Vec<[f32; 3]> {
        use anno_formats::xml::{block, tag};
        let number = |level: &str, name| tag(level, name).and_then(|v| v.parse::<f32>().ok());
        (0..)
            .map_while(|n| block(xml, &format!("QualityLevel{n}")))
            .map_while(|level| {
                let start = number(level, "ClusterStartDistance")?;
                let end = number(level, "ClusterViewDistance")?;
                let least = number(level, "MinimumObjectSizeInPixels").unwrap_or(0.);
                Some([start, end, least])
            })
            .collect()
    }

    /// The level `foliage` follows, normal or extended camera.
    fn level(&self, foliage: Foliage) -> Option<[f32; 2]> {
        let (table, level) = match foliage {
            Foliage::Near => (0, 1),
            Foliage::Game => (0, self.start),
            Foliage::Far => (1, self.start),
            Foliage::Off => return None,
        };
        let [start, end, _] = *self.levels[table].get(level)?;
        Some([start, end])
    }

    /// Screen pixels a placement must span to draw with `small`: the
    /// game's least size at the level it starts with, normal camera (117
    /// and 1800 keep the same for the extended one); 0 draws all.
    pub fn least_pixels(&self, small: SmallProps) -> f32 {
        match small {
            SmallProps::Game => self.levels[0].get(self.start).map_or(0., |l| l[2]),
            SmallProps::Off => 0.,
        }
    }

    /// What the renderer cuts foliage at for `foliage`; `None` for every
    /// distance.
    pub fn reach(&self, foliage: Foliage) -> Option<gpu::Reach> {
        let [impostor, end] = self.level(foliage)?;
        Some(gpu::Reach {
            impostor,
            end,
            thin: THIN,
        })
    }

    /// The `[impostor, end]` metres `foliage` reaches, for the pane.
    pub fn metres(&self, foliage: Foliage) -> Option<[f32; 2]> {
        self.level(foliage)
    }
}

/// The settings in force, read from graphics.json on first use.
static CURRENT: Mutex<Option<Graphics>> = Mutex::new(None);

fn path() -> std::path::PathBuf {
    platform::config_dir("anno-editor").join("graphics.json")
}

/// The settings every island view draws with. Tests draw with the
/// defaults, whatever the user's file says.
pub fn current() -> Graphics {
    *CURRENT.lock().unwrap().get_or_insert_with(|| {
        let read = std::fs::read(path()).ok().filter(|_| !cfg!(test));
        read.map_or_else(Graphics::default, |b| Graphics::parse(&b))
    })
}

/// Draw with `graphics` from now on and on the next start. A failed save
/// keeps them for this run, as the theme does.
pub fn set(graphics: Graphics) {
    *CURRENT.lock().unwrap() = Some(graphics);
    let path = path();
    let saved = std::fs::create_dir_all(path.parent().expect("a config folder")).and_then(|()| {
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&graphics).unwrap_or_default(),
        )
    });
    if let Err(e) = saved {
        eprintln!("graphics: cannot save {}: {e}", path.display());
    }
}

impl Editor {
    /// The Graphics pane: render quality for every island view.
    pub fn render_graphics(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_editor_kit::gpui::AnyElement {
        let g = current();
        let module = self.island.as_ref().map(|i| i.module());
        let clouds = module.is_none_or(|m| m.scene().is_some_and(|s| s.volumetric_clouds()));
        // Every change: saved, and the open island draws with it.
        fn apply(this: &mut Editor, g: Graphics) {
            set(g);
            if let Some(i) = &mut this.island {
                i.set_graphics(g);
            }
        }
        let row = |label: &'static str, control: gpui_editor_kit::gpui::AnyElement| {
            div()
                .px_2()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .min_h(px(28.))
                .child(div().text_size(px(11.)).child(label))
                .child(control)
        };
        let choice =
            |id: &'static str, options: Vec<(Graphics, SharedString)>, cx: &mut Context<Self>| {
                // Options carry the whole settings they set, so the
                // listener only applies them; the selected one is whichever
                // matches the field it changes.
                let selected = options
                    .iter()
                    .find(|(o, _)| *o == g)
                    .map(|(o, _)| *o)
                    .unwrap_or(g);
                widgets::segmented(id, options, &selected, cx, |this, o, _, cx| {
                    apply(this, o);
                    cx.notify();
                })
                .into_any_element()
            };
        let slider = |id: &'static str,
                      label: &'static str,
                      text: String,
                      fraction: f32,
                      set: fn(&mut Graphics, f32),
                      cx: &mut Context<Self>| {
            let me = SharedString::from(id);
            field::slider_field(id, label, text, fraction, false)
                .tooltip(Tooltip::text(label, "", "Drag to adjust · Scroll to nudge"))
                .on_drag_move(cx.listener(
                    move |this, e: &DragMoveEvent<field::SliderDrag>, _, cx| {
                        if e.drag(cx).0 != me {
                            return;
                        }
                        let mut g = current();
                        set(&mut g, field::slider_fraction(e));
                        if g != current() {
                            apply(this, g);
                        }
                        cx.notify();
                    },
                ))
                .on_scroll(cx.listener(move |this, e: &ScrollEvent, window, cx| {
                    let dy = f32::from(e.delta.pixel_delta(window.line_height()).y);
                    if dy == 0. {
                        return;
                    }
                    let mut g = current();
                    set(&mut g, fraction + dy.signum() * 0.05);
                    apply(this, g);
                    cx.notify();
                }))
                .into_any_element()
        };
        let preset = g.which();
        let fraction = |v: f32, (lo, hi): (f32, f32)| (v - lo) / (hi - lo);
        let with = |f: fn(&mut Graphics)| {
            let mut o = g;
            f(&mut o);
            o
        };

        let presets = {
            let options = [Preset::High, Preset::Medium, Preset::Low, Preset::Custom]
                .map(|p| (p, SharedString::from(format!("{p:?}"))));
            widgets::segmented_where(
                "graphics-preset",
                options,
                &preset,
                |p| (*p == Preset::Custom).then(|| "Set when an option below changes".into()),
                cx,
                |this, p, _, cx| {
                    if p != Preset::Custom {
                        apply(this, Graphics::preset(p));
                    }
                    cx.notify();
                },
            )
        };
        let cloud_shadows = choice(
            "graphics-cloud-shadows",
            vec![
                (
                    with(|g| g.cloud_shadows = CloudShadows::Smooth),
                    "Smooth".into(),
                ),
                (
                    with(|g| g.cloud_shadows = CloudShadows::Stepped),
                    "Stepped".into(),
                ),
                (with(|g| g.cloud_shadows = CloudShadows::Off), "Off".into()),
            ],
            cx,
        );
        let waves = choice(
            "graphics-waves",
            vec![
                (with(|g| g.waves = Waves::Full), "Full".into()),
                (with(|g| g.waves = Waves::Reduced), "Reduced".into()),
            ],
            cx,
        );
        let shadows = choice(
            "graphics-shadows",
            vec![
                (with(|g| g.shadows = Shadows::High), "High".into()),
                (with(|g| g.shadows = Shadows::Medium), "Medium".into()),
                (with(|g| g.shadows = Shadows::Low), "Low".into()),
                (with(|g| g.shadows = Shadows::Off), "Off".into()),
            ],
            cx,
        );
        let foliage = choice(
            "graphics-foliage",
            vec![
                (with(|g| g.foliage = Foliage::Near), "Nearer".into()),
                (with(|g| g.foliage = Foliage::Game), "Like the game".into()),
                (with(|g| g.foliage = Foliage::Far), "Farther".into()),
                (with(|g| g.foliage = Foliage::Off), "Off".into()),
            ],
            cx,
        );
        // The open island's game's distances; without one, 117's words.
        let reach = self
            .island
            .as_ref()
            .and_then(|i| i.view_distances.metres(g.foliage));
        let foliage_note = match (g.foliage, reach) {
            (Foliage::Off, _) => "Foliage draws at every distance, past where the game stops it.".to_string(),
            (f, Some([from, end])) => format!(
                "Trees, bushes and ground cover turn to impostors from {from:.0} m and thin out to none by {end:.0} m from the camera: {}.",
                match f {
                    Foliage::Near => "the game's Medium View Distance",
                    Foliage::Game => "the game's own View Distance",
                    _ => "the game's own View Distance with its extended camera",
                }
            ),
            (_, None) => "Like the game: trees, bushes and ground cover stop where the game's View Distance stops them.".to_string(),
        };
        let idle = choice(
            "graphics-idle",
            vec![
                (with(|g| g.idle_fps = IdleFps::Off), "Off".into()),
                (with(|g| g.idle_fps = IdleFps::Thirty), "30".into()),
                (with(|g| g.idle_fps = IdleFps::Fifteen), "15".into()),
            ],
            cx,
        );
        let distance = slider(
            "graphics-shadow-distance",
            "Distance",
            format!("{:.0} m", g.shadow_distance),
            fraction(g.shadow_distance, SHADOW_DISTANCE),
            |g, f| {
                let (lo, hi) = SHADOW_DISTANCE;
                // Whole 10 m: a drag does not land on 347.25.
                g.shadow_distance = ((lo + f.clamp(0., 1.) * (hi - lo)) / 10.).round() * 10.;
            },
            cx,
        );
        let small = choice(
            "graphics-small-props",
            vec![
                (
                    with(|g| g.small_props = SmallProps::Game),
                    "Like the game".into(),
                ),
                (with(|g| g.small_props = SmallProps::Off), "Off".into()),
            ],
            cx,
        );
        // The open island's game's least size; without one, 117's words.
        let least = self
            .island
            .as_ref()
            .map(|i| i.view_distances.least_pixels(SmallProps::Game))
            .filter(|&px| px > 0.);
        let small_note = match (g.small_props, least) {
            (SmallProps::Off, _) => {
                "Props and ground cover draw however small they are on screen.".to_string()
            }
            (SmallProps::Game, Some(px)) => format!(
                "Props and ground cover spanning under {px:.0} screen pixels are not drawn, as the game's View Distance hides them."
            ),
            (SmallProps::Game, None) => {
                "Like the game: props and ground cover too small on screen for the game to draw are not drawn.".to_string()
            }
        };
        let env_scale = std::env::var("ANNO_EDITOR_RENDER_SCALE").is_ok();
        let scale = slider(
            "graphics-render-scale",
            "Scale",
            format!("{:.0}%", g.render_scale * 100.),
            fraction(g.render_scale, RENDER_SCALE),
            |g, f| {
                let (lo, hi) = RENDER_SCALE;
                // Whole 5%.
                g.render_scale = ((lo + f.clamp(0., 1.) * (hi - lo)) * 20.).round() / 20.;
            },
            cx,
        );

        // Last: it holds `cx` for the rest of the list.
        let toggle = |label: &'static str, on: bool, tip: &'static str, f: fn(&mut Graphics)| {
            checkbox(label, on, "")
                .px_2()
                .tooltip(Tooltip::text(label, "", tip))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let mut g = current();
                    f(&mut g);
                    apply(this, g);
                    cx.notify();
                }))
        };
        let note = |text: &'static str| div().px_2().child(hint(text));
        let heading = |text: &'static str| div().px_2().child(section(text));
        // Each option says what it changes in the picture, never what it
        // saves: that depends on the machine, and View › GPU pass times
        // measures it there.
        let motion = match g.cloud_shadows {
            CloudShadows::Smooth => "Cloud shadows drift across the ground every frame.",
            CloudShadows::Stepped => "Cloud shadows move in steps, every quarter second.",
            CloudShadows::Off => "Clouds cast no shadows on the ground.",
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .py_2()
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("Every island · saved for next time"),
            )
            .child(div().px_2().pb_1().child(presets))
            .when(clouds, |d| {
                d.child(heading("Sky"))
                    .child(toggle(
                        "Clouds",
                        g.clouds,
                        "Volumetric clouds in the sky; off, the sky is clear",
                        |g| g.clouds = !g.clouds,
                    ))
                    .child(toggle(
                        "Haze",
                        g.haze,
                        "Haze over the island; off, the distance stays clear",
                        |g| g.haze = !g.haze,
                    ))
            })
            .child(heading("Cloud shadows"))
            .child(row("Motion", cloud_shadows))
            .child(note(motion))
            .child(heading("Water"))
            .child(toggle(
                "Reflections",
                g.reflections,
                "Water reflects the island; off, it reflects the sky alone",
                |g| g.reflections = !g.reflections,
            ))
            .child(row("Wave detail", waves))
            .child(note(
                "Reduced leaves out the finest ripples near the camera, so the shore looks flatter.",
            ))
            .child(heading("Shadows"))
            .child(row("Quality", shadows))
            .when(g.shadows != Shadows::Off, |d| {
                d.child(row("Sharp to", distance))
            })
            .child(note(match g.shadows {
                Shadows::High => "Past Sharp to, shadows come from one map over the whole island and are softer.",
                Shadows::Medium => "Shadow maps at half resolution, so shadows are softer. Past Sharp to, they come from one map over the whole island.",
                Shadows::Low => "Shadow maps at half resolution with one cascade fewer, so shadows are softer. Past Sharp to, they come from one map over the whole island.",
                Shadows::Off => "The sun casts no shadows.",
            }))
            .child(toggle(
                "Ambient occlusion",
                g.occlusion,
                "Darkens creases, tree feet and wall bases",
                |g| g.occlusion = !g.occlusion,
            ))
            .child(heading("Distance"))
            .child(row("Foliage", foliage))
            .child(div().px_2().child(hint(foliage_note)))
            .child(row("Small props", small))
            .child(div().px_2().child(hint(small_note)))
            .child(heading("Frames"))
            .child(row("Render scale", scale))
            .child(note(if env_scale {
                "ANNO_EDITOR_RENDER_SCALE is set and wins over this."
            } else {
                "Draws the island at this share of the pane's resolution and scales it up: edges turn softer, lines and markers stay sharp."
            }))
            .child(row("Idle cap (fps)", idle))
            .child(note(
                "While the camera rests, waves, clouds and drift redraw at most this often.",
            ))
            .child(heading("Models"))
            .child(toggle(
                "Animation",
                g.animation,
                "Turning, swinging and idling models; off, they stand still",
                |g| g.animation = !g.animation,
            ))
            .child(note(if g.animation {
                "Cranes and mills turn, flags and lanterns swing, and figures play their idle. While the camera rests, the view still draws them every frame."
            } else {
                "Models stand in the pose they were modelled in, and a still view draws nothing again but water, sky and clouds."
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_through_their_file() {
        for g in [Preset::High, Preset::Medium, Preset::Low].map(Graphics::preset) {
            let bytes = serde_json::to_vec(&g).unwrap();
            assert_eq!(Graphics::parse(&bytes), g);
        }
        let custom = Graphics {
            shadow_distance: 250.,
            small_props: SmallProps::Off,
            ..Graphics::preset(Preset::Low)
        };
        assert_eq!(custom.which(), Preset::Custom);
        assert_eq!(
            Graphics::parse(&serde_json::to_vec(&custom).unwrap()),
            custom
        );
    }

    #[test]
    fn a_missing_or_bad_file_gives_defaults() {
        assert_eq!(Graphics::parse(b""), Graphics::default());
        assert_eq!(Graphics::parse(b"not json {"), Graphics::default());
        assert_eq!(Graphics::parse(b"[1, 2]"), Graphics::default());
        assert_eq!(
            Graphics::parse(br#"{"shadows": "ultra"}"#),
            Graphics::default()
        );
        // Missing fields default, unknown ones are ignored, numbers clamp.
        let g = Graphics::parse(br#"{"clouds": false, "future": 3, "render_scale": 9}"#);
        assert_eq!(
            g,
            Graphics {
                clouds: false,
                ..Default::default()
            }
        );
        assert_eq!(Graphics::default().which(), Preset::High);
    }

    #[test]
    fn a_file_from_when_the_pane_chose_a_geometry_path_still_loads() {
        // The pane's Geometry path choice went with the classic path; its
        // key in an older graphics.json is ignored and the rest stands.
        for path in ["auto", "classic", "visbuf", "compute"] {
            let bytes =
                format!(r#"{{"geometry": "{path}", "clouds": false, "shadows": "medium"}}"#);
            let old = Graphics::parse(bytes.as_bytes());
            assert_eq!(
                old,
                Graphics {
                    clouds: false,
                    shadows: Shadows::Medium,
                    ..Default::default()
                },
                "{path}"
            );
        }
    }

    // The games' engine settings, cut to four levels: 117's default.xml
    // with the rest of each level dropped.
    const ENGINE_SETTINGS: &str = "<EngineSettings>\n  <ViewDistance>\n    <QualityLevel0>\n      <ObjectViewDistance>375.000000</ObjectViewDistance>\n      <ClusterStartDistance>180.000000</ClusterStartDistance>\n      <ClusterViewDistance>400.000000</ClusterViewDistance>\n    </QualityLevel0>\n    <QualityLevel1>\n      <ClusterStartDistance>200.000000</ClusterStartDistance>\n      <ClusterViewDistance>450.000000</ClusterViewDistance>\n      <MinimumObjectSizeInPixels>15.000000</MinimumObjectSizeInPixels>\n      <LODDistances>\n        <LOD0>0.000000</LOD0>\n      </LODDistances>\n    </QualityLevel1>\n    <QualityLevel2>\n      <ClusterStartDistance>200.000000</ClusterStartDistance>\n      <ClusterViewDistance>500.000000</ClusterViewDistance>\n    </QualityLevel2>\n    <QualityLevel3>\n      <ClusterStartDistance>220.000000</ClusterStartDistance>\n      <ClusterViewDistance>575.0000</ClusterViewDistance>\n      <MinimumObjectSizeInPixels>13.000000</MinimumObjectSizeInPixels>\n    </QualityLevel3>\n  </ViewDistance>\n</EngineSettings>";

    #[test]
    fn foliage_follows_the_games_view_distance_levels() {
        let levels = ViewDistances::parse(ENGINE_SETTINGS);
        assert_eq!(levels.len(), 4);
        assert_eq!(levels[1], [200., 450., 15.]);
        // A level without a least size draws everything.
        assert_eq!(levels[0][2], 0.);
        let mut extended = levels.clone();
        extended[3] = [220., 900., 13.];
        let d = ViewDistances {
            levels: [levels, extended],
            start: 3,
        };
        let reach = |f| d.reach(f).map(|r| (r.impostor, r.end));
        assert_eq!(reach(Foliage::Game), Some((220., 575.)));
        assert_eq!(reach(Foliage::Near), Some((200., 450.)));
        assert_eq!(reach(Foliage::Far), Some((220., 900.)));
        assert_eq!(reach(Foliage::Off), None);
        let g = Graphics::default();
        assert_eq!(g.kit(&d).reach, d.reach(g.foliage));
        // Small props follow the starting level's least size.
        assert_eq!(g.kit(&d).min_pixels, 13.);
        assert_eq!(d.least_pixels(SmallProps::Off), 0.);
        // An island of a game without the files reaches anywhere and
        // draws props of any size.
        assert_eq!(ViewDistances::default().reach(Foliage::Game), None);
        assert_eq!(ViewDistances::default().least_pixels(SmallProps::Game), 0.);
        assert!(ViewDistances::parse("<EngineSettings/>").is_empty());
    }

    #[test]
    fn high_is_todays_look() {
        let g = Graphics::default();
        // Today's look but for the games' foliage reach.
        let kit = g.kit(&ViewDistances::default());
        assert_eq!(kit, gpu::Detail::default());
        assert_eq!(g.game(), ambient::Detail::default());
        let s = g.shadow_maps().unwrap();
        assert_eq!(
            (
                s.resolution,
                s.cascades,
                s.cascade_resolution,
                s.reach,
                s.softness
            ),
            (4096, 3, 2048, 500., 1.5)
        );
        assert!(Graphics::preset(Preset::Low).shadow_maps().is_some());
        let off = Graphics {
            shadows: Shadows::Off,
            ..g
        };
        assert!(off.shadow_maps().is_none());
    }
}
