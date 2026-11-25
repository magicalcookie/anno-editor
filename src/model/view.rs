// Owns the Model workspace's viewport: its renderer and camera, the floor
// and grid under the model, the scene built from the model's pieces,
// picking and hover, the transform gizmo with its axes and snapping, the
// overlays drawn over the frame (skeleton, joints, light and effect
// markers, spot beams and area light shapes, cloth, grid) and the
// right-click menu on an entry.

use super::{ModelEditor, playback, sky::IslandSky};
use crate::editor::{Editor, Pane};
use crate::island::models::Piece;
use anno_core::files::ModelLook;
use anno_formats::model::{Model, Transform};
use anno_formats::xml::{Doc, Id};
use gpui_editor_kit::{
    camera::{Camera, Extent},
    gizmo,
    gpu::{
        Batch, Blend, Glow, Instances, Light, Line, LocalLight, Material, Mesh, MeshData, Outline,
        Renderer, Scene, Shadows, Viewport,
    },
    gpui::{prelude::*, *},
    icon::Icon,
    math::{self, Mat4, Vec3},
    model_view::{floor, floor_color},
    paint, theme, widgets,
};
use std::{cell::Cell, f32::consts::FRAC_PI_2, rc::Rc, sync::Arc};

/// Behind the model, as the asset preview's; the floor fades out to it.
const BACKDROP: u32 = gpui_editor_kit::model_view::BACKDROP;
/// Frames the hover waits for the GPU's pick under a resting cursor. The
/// pane's thread draws slower than the window on a software device: at 8
/// the wait ran out before the answer came.
const PICK_WAIT: u8 = 30;
/// Pixels around a marker or joint a click takes it within.
const MARKER_REACH: f32 = 9.;
const JOINT_REACH: f32 = 6.;
/// The island editor's selection orange, so a selection reads the same in
/// both workspaces.
const SELECTED: u32 = crate::colors::SELECTION;
/// A light whose cfg gives it no colour.
const LIGHT: u32 = 0xffd75e;
const PARTICLE: u32 = 0x6fd3ff;
const BONE: u32 = 0x9be86b;
/// Straight down to level. The kit's own orbit stops at 72° for ground
/// picking on islands; a model is looked at from the side too.
const TILT: (f32, f32) = (0.001, FRAC_PI_2 - 0.01);
/// The opening view: three-quarter from the front.
const HOME: (f32, f32) = (0.6, 0.95);
/// Perspective lens: orthographic flattened buildings, a wide one bent
/// their walls (the asset preview's).
const LENS: f32 = 0.5;

/// Toolbar glyphs the kit has no icon for, on its 24 px grid.
const BACK_ICON: Icon = Icon(&[
    &[[20., 12.], [4., 12.]],
    &[[10., 6.], [4., 12.], [10., 18.]],
]);
/// A disc and eight rays.
const SUN_ICON: Icon = Icon(&[
    &[
        [16.5, 12.],
        [15.9, 14.25],
        [14.25, 15.9],
        [12., 16.5],
        [9.75, 15.9],
        [8.1, 14.25],
        [7.5, 12.],
        [8.1, 9.75],
        [9.75, 8.1],
        [12., 7.5],
        [14.25, 8.1],
        [15.9, 9.75],
        [16.5, 12.],
    ],
    &[[19., 12.], [22., 12.]],
    &[[16.95, 16.95], [19.07, 19.07]],
    &[[12., 19.], [12., 22.]],
    &[[7.05, 16.95], [4.93, 19.07]],
    &[[5., 12.], [2., 12.]],
    &[[7.05, 7.05], [4.93, 4.93]],
    &[[12., 5.], [12., 2.]],
    &[[16.95, 7.05], [19.07, 4.93]],
]);
/// A cloud: the Sky button's, whose island skies come with their clouds.
const CLOUD_ICON: Icon = Icon(&[&[
    [6., 18.],
    [18., 18.],
    [20.5, 16.8],
    [21.5, 14.5],
    [20.5, 12.2],
    [18., 11.],
    [16.8, 8.],
    [14., 6.2],
    [11., 6.2],
    [8.6, 7.8],
    [7.5, 10.5],
    [5., 11.],
    [3., 13.],
    [2.6, 15.5],
    [4., 17.4],
    [6., 18.],
]]);
/// A crescent opening right: an outer arc of radius 8, an inner one of a
/// circle further right.
const MOON_ICON: Icon = Icon(&[&[
    [16., 5.07],
    [12., 4.],
    [8., 5.07],
    [5.07, 8.],
    [4., 12.],
    [5.07, 16.],
    [8., 18.93],
    [12., 20.],
    [16., 18.93],
    [12.9, 17.1],
    [11.22, 14.47],
    [10.79, 12.],
    [11.22, 9.53],
    [12.9, 6.9],
    [16., 5.07],
]]);

/// Sun from over the left shoulder of the opening view, so the faces it
/// shows are lit and shadows fall away behind the model; sky and ground as
/// the asset preview's, which separate leaves by their facing. A shadow
/// map of its own: self-shadowing is what gave buildings their depth.
const SUN: Light = Light {
    sun: [-0.1, 0.9, -0.45],
    sun_color: [0.6; 3],
    sky: [0.52; 3],
    ground: [0.3; 3],
    shadows: Some(Shadows {
        resolution: 2048,
        softness: 2.,
        cascades: 0,
        cascade_resolution: 2048,
        reach: 500.,
    }),
    fog: None,
};

/// Night: the same light from the same side, dimmed to a moonlit blue, so
/// the model's own lights carry it. The editor's own look, as `SUN`.
const MOON: Light = Light {
    sun_color: [0.09, 0.11, 0.16],
    sky: [0.07, 0.08, 0.12],
    ground: [0.04, 0.04, 0.05],
    ..SUN
};

/// The game's light units in the studio's, by day and by night: `SUN`
/// stands for a noon sun of about 4 (117's, as its sky gives it), and by
/// night the game's eye adapts to the dark. Both estimates, set by eye.
/// Under an island sky the scene is in the game's units, exposed as the
/// preset exposes it.
const LIGHT_SCALE: [f32; 2] = [0.15, 0.6];

/// What the viewport draws over the model, as bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlays(u8);

impl Overlays {
    /// The selected part's bones.
    pub const SKELETON: Overlays = Overlays(1);
    /// Its joints as dots a click picks.
    pub const JOINTS: Overlays = Overlays(2);
    /// Light and particle markers, and the selected effect's box.
    pub const MARKERS: Overlays = Overlays(4);
    pub const CLOTH: Overlays = Overlays(8);
    pub const GRID: Overlays = Overlays(16);
    pub const ALL: Overlays = Overlays(31);

    pub fn has(self, o: Overlays) -> bool {
        self.0 & o.0 != 0
    }

    pub fn toggle(&mut self, o: Overlays) {
        self.0 ^= o.0;
    }
}

/// The overlays as the View menu lists them, with the key that flips each.
pub(crate) const OVERLAYS: [(Overlays, &str, &str); 5] = [
    (Overlays::SKELETON, "Skeleton", "B"),
    (Overlays::JOINTS, "Joints", "J"),
    (Overlays::MARKERS, "Lights and effects", "L"),
    (Overlays::CLOTH, "Cloth", "C"),
    (Overlays::GRID, "Grid", "G"),
];

/// The model's axis views, on Blender's numpad keys (the number row here).
/// Front looks along +Z, the side the opening view shows.
const VIEWS: [(&str, &str, f32, f32); 3] = [
    ("Front", "1", 0., TILT.1),
    ("Right", "3", FRAC_PI_2, TILT.1),
    ("Top", "7", 0., TILT.0),
];

/// Snap steps each gizmo mode offers: metres, degrees, a scale factor.
const STEPS: [[(f32, &str); 4]; 3] = [
    [(0.05, "5 cm"), (0.1, "10 cm"), (0.25, "25 cm"), (1., "1 m")],
    [(5., "5°"), (15., "15°"), (45., "45°"), (90., "90°")],
    [
        (0.05, "0.05×"),
        (0.1, "0.1×"),
        (0.25, "0.25×"),
        (0.5, "0.5×"),
    ],
];

fn mode_index(mode: gizmo::Mode) -> usize {
    match mode {
        gizmo::Mode::Move => 0,
        gizmo::Mode::Rotate => 1,
        gizmo::Mode::Scale => 2,
    }
}

/// A popup over the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Menu {
    /// The toolbar's Show button's: overlays and camera views.
    View,
    /// Its Sky button's: the studio or an island sky.
    Light,
    /// A right click on an entry, at pane pixels.
    Entry { entry: Id, at: Point<Pixels> },
}

/// What the entry menu does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Duplicate,
    Delete,
    Frame,
    Detach,
    OpenProp,
}

/// What the cursor rests on. A mesh's pick reads back a frame or more after
/// the cursor stops, so the canvas asks again for a few frames and fills it
/// in; markers answer at once.
#[derive(Clone, Copy, Default)]
struct Hover {
    cursor: Option<Point<Pixels>>,
    entry: Option<Id>,
    /// Frames left to wait for the pick under `cursor`.
    tries: u8,
}

pub struct View {
    renderer: Arc<Renderer>,
    viewport: Arc<Viewport>,
    pub camera: Camera,
    pub extent: Extent,
    /// Where the view was last painted.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    drag: Option<Drag>,
    /// The model's glow by day and by night, when night masks light too.
    glow: [Glow; 2],
    /// Lit as at midnight rather than noon: the moon for the sun, the
    /// lights the game lights at night.
    pub night: bool,
    /// The model's own lights this frame (`ModelEditor::light_up`).
    pub lights: Arc<Vec<LocalLight>>,
    /// The island skies the View menu offers (`sky::offered`), and the one
    /// the model stands under; `None` is the studio.
    pub skies: Vec<String>,
    pub island: Option<Arc<IslandSky>>,
    /// The particle entries: their sprites are effects, the rest of
    /// `sprites` is cloth.
    particles: Vec<Id>,
    /// The scene's meshes, kept so per-frame changes (an animation pose, a
    /// particle frame) swap a few without rebuilding the rest.
    batches: Vec<Batch>,
    /// What playback draws this frame: each piece posed (empty when the
    /// model stands as the cfg places it) and particle sprites and cloth.
    pub posed: Vec<super::playback::Posed>,
    pub sprites: Vec<Batch>,
    /// Skeleton bones and particle debug, in model space.
    pub overlay: Vec<[Vec3; 2]>,
    /// The floor under the model and the grid on it, built when the model
    /// is framed: the studio's, and under an island sky the lit floor and
    /// its rim fading into the sky.
    floor: Option<Batch>,
    grid: Option<Batch>,
    island_floor: Vec<Batch>,
    island_grid: Option<Batch>,
    /// The grid's spacing in metres.
    step: f32,
    /// The whole model's box as last framed.
    model_box: [Vec3; 2],
    pub shown: Overlays,
    /// Move and turn along the entry's own axes, not the model's. Scale is
    /// always along its own.
    pub local: bool,
    /// Every gizmo drag snaps, not only those with Ctrl held.
    pub snap: bool,
    /// The snap step per gizmo mode: metres, degrees, scale factor.
    pub steps: [f32; 3],
    pub menu: Option<Menu>,
    hover: Rc<Cell<Hover>>,
}

enum Drag {
    /// `from` is where the press was: a right click that did not turn the
    /// view opens the entry menu.
    Orbit {
        last: Point<Pixels>,
        from: Point<Pixels>,
    },
    Pan(Point<Pixels>),
    /// `parent` is the joint the entry hangs on, as it was at the press:
    /// the gizmo works in model space, the placement in the joint's.
    Gizmo {
        gesture: gizmo::Gesture,
        entry: Id,
        start: Transform,
        parent: Option<Mat4>,
        delta: Option<gizmo::Delta>,
    },
    /// A picked joint posed at the playhead: keys its animation there.
    Joint {
        gesture: gizmo::Gesture,
        start: Mat4,
        delta: Option<gizmo::Delta>,
    },
}

impl View {
    pub fn new(look: &ModelLook) -> View {
        let renderer = Arc::new(Renderer::with_families(look.families()));
        View {
            viewport: renderer.viewport(),
            renderer,
            camera: home(LENS),
            extent: Extent {
                center: [0.; 3],
                size: 1.,
            },
            bounds: Rc::default(),
            drag: None,
            glow: [
                look.glow(&look.scene_glow()),
                look.glow(&anno_core::ambient::ModelGlow {
                    night: 0.,
                    ..look.scene_glow()
                }),
            ],
            night: false,
            lights: Arc::default(),
            skies: Vec::new(),
            island: None,
            particles: Vec::new(),
            batches: Vec::new(),
            posed: Vec::new(),
            sprites: Vec::new(),
            overlay: Vec::new(),
            floor: None,
            grid: None,
            island_floor: Vec::new(),
            island_grid: None,
            step: 1.,
            model_box: [[-1.; 3], [1.; 3]],
            shown: Overlays::ALL,
            local: false,
            snap: false,
            steps: [0.1, 15., 0.1],
            menu: None,
            hover: Rc::default(),
        }
    }

    /// Shows the pieces; `frame` fits the camera to them and lays the
    /// floor under them again.
    pub fn show(&mut self, pieces: &[Piece], model: &Model, selected: Option<Id>, frame: bool) {
        self.batches = (pieces.iter())
            .map(|p| {
                Batch::new(
                    p.mesh.clone(),
                    Arc::new(Instances {
                        matrices: vec![p.local],
                        ids: vec![p.element.map(|e| u64::from(e.raw()))],
                    }),
                )
            })
            .collect();
        self.particles = model.particles.iter().map(|p| p.element).collect();
        if frame || self.floor.is_none() {
            let [lo, hi] = if pieces.is_empty() {
                // Nothing drawn (an effect or a light alone): a metre
                // around its markers, the floor at the ground under them.
                // A box about the origin put the floor a metre under a
                // swarm of fireflies that starts on the ground.
                let at = (model.lights.iter().map(|l| l.transform.position))
                    .chain(model.particles.iter().map(|p| p.transform.position));
                let [lo, hi] = math::bounds(at.chain([[0.; 3]]));
                [
                    [lo[0] - 0.5, lo[1].min(0.), lo[2] - 0.5],
                    [hi[0] + 0.5, hi[1].max(1.), hi[2] + 0.5],
                ]
            } else {
                math::bounds(pieces.iter().flat_map(|p| {
                    math::corners(p.mesh.bounds()).map(|c| math::transform(&p.local, c))
                }))
            };
            let size = (0..3).map(|i| hi[i] - lo[i]).fold(0.01, f32::max);
            let center: Vec3 = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
            self.extent = Extent {
                center,
                size: size * 0.95,
            };
            self.model_box = [lo, hi];
            self.lay_floor(center, lo[1], (hi[0] - lo[0]).max(hi[2] - lo[2]), size);
            if frame {
                self.camera = home(LENS);
            }
        }
        self.posed.clear();
        self.present(selected);
        self.outline(selected);
    }

    /// The floor and its grid under a model `size` across at its largest
    /// and `across` on the ground, its lowest point at `base`. Just under
    /// that point, so the base does not flicker against the floor; the
    /// grid a hair above the floor, so they do not fight for depth.
    fn lay_floor(&mut self, center: Vec3, base: f32, across: f32, size: f32) {
        let radius = size * 1.1;
        let shadow = across * 0.35;
        let at = [center[0], base - size * 0.002, center[2]];
        // About six lines a side at a round step: at a fixed metre a 1 m
        // prop got one line and a harbour a hundred.
        self.step = [0.1, 0.25, 0.5, 1., 2., 5., 10., 25., 50.]
            .into_iter()
            .find(|s| radius / s <= 6.)
            .unwrap_or(100.);
        let place = |mesh: Option<Arc<Mesh>>| {
            mesh.map(|m| {
                Batch::new(
                    m,
                    Arc::new(Instances {
                        matrices: vec![math::IDENTITY],
                        ids: vec![None],
                    }),
                )
            })
        };
        self.floor = place(floor(at, shadow, radius));
        let lifted = [at[0], at[1] + size * 0.001, at[2]];
        self.grid = place(grid(lifted, shadow, radius, self.step, false));
        self.island_floor = island_floor(at, radius)
            .into_iter()
            .filter_map(place)
            .collect();
        self.island_grid = place(grid(lifted, shadow, radius, self.step, true));
    }

    /// [`View::present`] for a playback frame: skipped while the last
    /// one is still uploading. A new scene starts its uploads over, so on a
    /// slow device a pose every tick would never reach the screen, and the
    /// skeleton lines (which upload at once) would run ahead of the meshes.
    pub fn present_frame(&self, selected: Option<Id>) {
        if !self.renderer.scene_waiting() {
            self.present(selected);
        }
    }

    /// The pieces as drawn now: posed where playback posed them.
    fn drawn(&self) -> Vec<Batch> {
        match self.posed.len() == self.batches.len() {
            false => self.batches.clone(),
            true => (self.batches.iter().zip(&self.posed))
                .filter(|(_, p)| p.shown)
                .map(|(b, p)| {
                    Batch::new(
                        p.mesh.clone().unwrap_or_else(|| b.mesh.clone()),
                        Arc::new(Instances {
                            matrices: vec![p.local],
                            ids: b.instances.ids.clone(),
                        }),
                    )
                })
                .collect(),
        }
    }

    fn is_effect(&self, b: &Batch) -> bool {
        let id = b.instances.ids.first().copied().flatten();
        (self.particles.iter()).any(|p| Some(u64::from(p.raw())) == id)
    }

    /// Sends the scene: pieces (posed where playback posed them), sprites,
    /// cloth, the floor and the overlays that are on.
    pub fn present(&self, selected: Option<Id>) {
        let mut batches = self.drawn();
        let cloth = self.shown.has(Overlays::CLOTH);
        batches.extend(
            (self.sprites.iter())
                .filter(|b| cloth || self.is_effect(b))
                .cloned(),
        );
        match &self.island {
            Some(_) => batches.extend(self.island_floor.iter().cloned()),
            None => batches.extend(self.floor.clone()),
        }
        if self.shown.has(Overlays::GRID) {
            match &self.island {
                Some(_) => batches.extend(self.island_grid.clone()),
                None => batches.extend(self.grid.clone()),
            }
        }
        let mut lines = Vec::new();
        if self.shown.has(Overlays::SKELETON) {
            lines.extend(self.overlay.iter().map(|[a, b]| Line {
                points: vec![*a, *b],
                width: 2.,
                color: BONE << 8 | 0xff,
            }));
        }
        // How far the selected effect reaches this frame: its sprites' box.
        if self.shown.has(Overlays::MARKERS)
            && let Some(s) = selected.filter(|s| self.particles.contains(s))
            && let Some(b) = self.box_of_sprites(s)
        {
            lines.extend(box_lines(b, PARTICLE << 8 | 0xa0));
        }
        let mut scene = Scene {
            batches,
            light: if self.night { MOON } else { SUN },
            local_lights: self.lights.clone(),
            lines: Arc::new(lines),
            glow: self.glow[usize::from(self.night)],
            ..Default::default()
        };
        if let Some(sky) = &self.island {
            sky.light(&mut scene, self.night, SUN.shadows);
        }
        self.renderer.set_scene(&scene);
    }

    /// What the game's light units are worth in the view now.
    pub fn light_scale(&self) -> f32 {
        match self.island {
            Some(_) => 1.,
            None => LIGHT_SCALE[usize::from(self.night)],
        }
    }

    pub fn outline(&self, selected: Option<Id>) {
        self.renderer.set_outline(selected.map(|e| Outline {
            rows: Vec::new(),
            ids: vec![u64::from(e.raw())],
            color: crate::colors::with_alpha(SELECTED, 0xff),
            width: 2,
        }));
    }

    /// Where the view was last painted, in window pixels; the window
    /// tests aim their clicks by it.
    #[cfg(test)]
    pub(super) fn painted(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    /// Where `p` (model space) shows in the pane as last painted.
    pub(super) fn screen(&self, p: Vec3) -> Point<Pixels> {
        self.camera.project(p, self.bounds.get(), self.extent)
    }

    /// How a sound at `p` is heard: its pan, -1 at the pane's left edge
    /// to 1 at its right (where it shows, off screen held at the edge), and
    /// its distance from where the camera looks. Not from the eye: 117's
    /// default camera stands 60 units up, yet 2,352 of its 2,977 sequence
    /// sounds fall silent past 10, so the game's listener is not there.
    /// Where it is instead is not traced; the view's middle is an estimate.
    pub(super) fn hear(&self, p: Vec3) -> (f32, f32) {
        let b = self.bounds.get();
        let half = f32::from(b.size.width) / 2.;
        if half <= 0. {
            // Not painted yet: from the model's middle, centred.
            return (
                0.,
                math::length(math::sub(p, self.camera.anchor(self.extent))),
            );
        }
        let focus = self.camera.view_center(b, self.extent);
        let x = f32::from(self.screen(p).x - b.origin.x);
        (
            ((x - half) / half).clamp(-1., 1.),
            math::length(math::sub(p, focus)),
        )
    }

    /// The box around what is drawn of `entry` now: its meshes as posed
    /// and its sprites or cloth.
    pub fn box_of(&self, entry: Id) -> Option<[Vec3; 2]> {
        let id = Some(u64::from(entry.raw()));
        let drawn = self.drawn();
        let corners: Vec<Vec3> = (drawn.iter().chain(&self.sprites))
            .filter(|b| b.instances.ids.first() == Some(&id))
            .flat_map(|b| {
                let m = b.instances.matrices[0];
                math::corners(b.mesh.bounds()).map(move |c| math::transform(&m, c))
            })
            .collect();
        (!corners.is_empty()).then(|| math::bounds(corners))
    }

    fn box_of_sprites(&self, entry: Id) -> Option<[Vec3; 2]> {
        let id = Some(u64::from(entry.raw()));
        let corners: Vec<Vec3> = (self.sprites.iter())
            .filter(|b| b.instances.ids.first() == Some(&id))
            .flat_map(|b| {
                let m = b.instances.matrices[0];
                math::corners(b.mesh.bounds()).map(move |c| math::transform(&m, c))
            })
            .collect();
        (!corners.is_empty()).then(|| math::bounds(corners))
    }

    /// Fits the camera around `[lo, hi]` from where it looks now; orbits
    /// then turn about its middle.
    pub fn fit(&mut self, [lo, hi]: [Vec3; 2]) {
        let center = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
        let size = (0..3).map(|i| hi[i] - lo[i]).fold(0.05, f32::max);
        self.camera.frame(center, size, self.extent);
    }

    /// The whole model, from where the camera looks now.
    pub fn frame_all(&mut self) {
        self.camera.center = None;
        self.camera.pan = [0.; 2];
        self.camera.zoom = 1.;
    }

    /// Back to the opening three-quarter view of the whole model.
    pub fn reset(&mut self) {
        self.camera = Camera {
            fov: self.camera.fov,
            ..home(LENS)
        };
    }

    /// The camera stands where the model opened: nobody has turned,
    /// zoomed or panned it since.
    pub fn at_home(&self) -> bool {
        self.camera == home(LENS)
    }

    /// The point in the middle of the pane at the camera's centre depth:
    /// what turning the view with nothing selected keeps in place.
    fn middle(&self) -> Vec3 {
        let c = self.camera;
        let s = c.scale(self.bounds.get(), self.extent);
        let (anchor, right, down) = (c.anchor(self.extent), c.right(), c.down());
        std::array::from_fn(|a| anchor[a] - (right[a] * c.pan[0] + down[a] * c.pan[1]) / s)
    }

    /// Turns to `yaw` and `tilt` about what is in the middle of the pane.
    pub fn look(&mut self, yaw: f32, tilt: f32) {
        let middle = self.middle();
        self.camera
            .begin_orbit(Some(middle), self.bounds.get(), self.extent);
        self.camera.yaw = yaw;
        self.camera.tilt = tilt;
    }

    fn orbit(&mut self, d: Point<Pixels>) {
        // Mirrored, the same yaw step turns the other way on screen.
        let sign = if self.camera.mirror { -1. } else { 1. };
        self.camera.yaw += f32::from(d.x) * 0.008 * sign;
        self.camera.tilt = (self.camera.tilt + f32::from(d.y) * 0.006).clamp(TILT.0, TILT.1);
    }

    /// Zooms at the cursor, out to the model about a third of the pane and
    /// in to about 5 cm across it: past either the wheel only lost the
    /// model or drove the eye into it. A zoom already past a limit (a tiny
    /// prop framed) is not snapped back, only kept from going further.
    fn zoom(&mut self, delta: f32, cursor: Point<Pixels>) {
        let (min, max) = (0.3f32, (self.extent.size / 0.05).max(1.));
        let z = self.camera.zoom;
        let want = z * (delta * 0.0025).exp();
        let to = if want > z {
            want.min(max.max(z))
        } else {
            want.max(min.min(z))
        };
        if to != z {
            let delta = (to / z).ln() / 0.0025;
            self.camera.zoom_at(delta, cursor, self.bounds.get());
        }
    }

    /// The gizmo for an entry at `at` (model space), as last painted.
    fn gizmo_frame(&self, mode: gizmo::Mode, at: &Mat4) -> Option<gizmo::Frame> {
        handles(
            mode,
            at,
            self.local,
            self.camera,
            self.bounds.get(),
            self.extent,
        )
    }
}

fn home(fov: f32) -> Camera {
    Camera {
        yaw: HOME.0,
        tilt: HOME.1,
        fov,
        // Anno's models are left-handed, as the island views.
        mirror: true,
        ..Default::default()
    }
}

/// Scale along the entry's own axes always; move and turn along them when
/// `local` is on.
fn handles(
    mode: gizmo::Mode,
    at: &Mat4,
    local: bool,
    camera: Camera,
    bounds: Bounds<Pixels>,
    extent: Extent,
) -> Option<gizmo::Frame> {
    let own = (local || mode == gizmo::Mode::Scale).then_some(at);
    gizmo::Frame::new(mode, math::translation(at), own, camera, bounds, extent)
}

/// What a gizmo drag has done so far, for the readout by the cursor.
fn readout(d: &gizmo::Delta, handle: gizmo::Handle) -> String {
    let axis = match handle {
        gizmo::Handle::Axis(a) => ["X", "Y", "Z"][a],
        gizmo::Handle::Plane(..) => "the plane",
        gizmo::Handle::Screen => "the view",
    };
    match *d {
        gizmo::Delta::Move([x, y, z]) => format!("Moved X {x:+.3} · Y {y:+.3} · Z {z:+.3} m"),
        gizmo::Delta::Rotate(_, a) => format!("Turned {:+.1}° about {axis}", a.to_degrees()),
        gizmo::Delta::Scale(Some(_), f) => format!("Scaled ×{f:.3} along {axis}"),
        gizmo::Delta::Scale(None, f) => format!("Scaled ×{f:.3}"),
    }
}

/// The 12 edges of a box as lines.
fn box_lines([lo, hi]: [Vec3; 2], color: u32) -> Vec<Line> {
    let mut lines = Vec::new();
    for i in 0..8 {
        for bit in [1, 2, 4] {
            // Each edge once: from the corner without the bit to the one with it.
            if i & bit == 0 {
                let a = corner(lo, hi, i);
                let b = corner(lo, hi, i | bit);
                lines.push(Line {
                    points: vec![a, b],
                    width: 1.5,
                    color,
                });
            }
        }
    }
    lines
}

/// Corner `i` of a box: bit 0 picks X's high end, bit 1 Y's, bit 2 Z's.
fn corner(lo: Vec3, hi: Vec3, i: usize) -> Vec3 {
    std::array::from_fn(|a| if i >> a & 1 == 1 { hi[a] } else { lo[a] })
}

/// `a` toward `b` by `t`, per channel of 0xRRGGBB.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0., 1.);
    (0..3).fold(0, |out, k| {
        let shift = 16 - k * 8;
        let (x, y) = ((a >> shift & 0xff) as f32, (b >> shift & 0xff) as f32);
        out | ((x + (y - x) * t).round() as u32) << shift
    })
}

fn hex(c: Rgba) -> u32 {
    let f = |v: f32| (v.clamp(0., 1.) * 255.).round() as u32;
    f(c.r) << 16 | f(c.g) << 8 | f(c.b)
}

/// The floor under a model under an island sky: earth-grey ground the sky
/// lights and the model shadows, solid out to 60% of `radius`, then a rim
/// blended into the sky by `radius`, so no edge shows against it.
fn island_floor(center: Vec3, radius: f32) -> [Option<Arc<Mesh>>; 2] {
    const SIDES: u32 = 64;
    const GROUND: u32 = 0x55514a;
    let solid = radius * 0.6;
    let around = |r: f32, k: u32| {
        let a = k as f32 / SIDES as f32 * std::f32::consts::TAU;
        [center[0] + r * a.cos(), center[1], center[2] + r * a.sin()]
    };
    let mut disc = MeshData {
        material: Material::Lit,
        casts_shadow: false,
        ..Default::default()
    };
    disc.positions.push(center);
    disc.positions.extend((0..SIDES).map(|k| around(solid, k)));
    disc.colors = vec![GROUND; disc.positions.len()];
    disc.normals = vec![[0., 1., 0.]; disc.positions.len()];
    for k in 0..SIDES {
        disc.indices.extend([0, 1 + k, 1 + (k + 1) % SIDES]);
    }
    let mut rim = MeshData {
        material: Material::Blend(Blend {
            opacity: 1.,
            ..Default::default()
        }),
        casts_shadow: false,
        ..Default::default()
    };
    for (r, clear) in [(solid, 0), (radius, 0xff)] {
        rim.positions.extend((0..SIDES).map(|k| around(r, k)));
        rim.colors.extend((0..SIDES).map(|_| clear << 24 | GROUND));
    }
    rim.normals = vec![[0., 1., 0.]; rim.positions.len()];
    // Wound as the studio's floor, facing up.
    for k in 0..SIDES {
        let next = (k + 1) % SIDES;
        let (i, o) = (k, SIDES + k);
        let (i1, o1) = (next, SIDES + next);
        rim.indices.extend([i, o, o1, i, o1, i1]);
    }
    [disc, rim].map(|d| Mesh::new(d).ok().map(Arc::new))
}

/// Grid lines on the floor every `step` metres from the model's origin, as
/// thin flat quads: the model hides the lines behind it, which the kit's
/// scene lines, drawn over everything, did not (the grid crossed the
/// building). Each line fades into the floor toward the disc's edge; the
/// lines through the origin take the gizmo's X and Z colours. On the
/// `island` floor they are pale and lit as it is, fading out by alpha.
fn grid(center: Vec3, shadow: f32, radius: f32, step: f32, island: bool) -> Option<Arc<Mesh>> {
    // Pieces per line, so the colour follows the floor's rings along it.
    const PIECES: usize = 24;
    // About 2 px at the opening zoom; thinner lines broke up into dots on
    // the software renderer.
    let half_width = radius * 0.0012;
    let mut data = MeshData {
        material: match island {
            true => Material::Blend(Blend {
                opacity: 1.,
                ..Default::default()
            }),
            false => Material::Flat,
        },
        ..Default::default()
    };
    let axes = theme::get().axes.map(hex);
    // `axis` 0: lines along Z at x = off; 1: along X at z = off.
    for axis in 0..2 {
        let c = center[axis * 2];
        let first = ((c - radius) / step).ceil() as i32;
        let last = ((c + radius) / step).floor() as i32;
        for k in first..=last {
            let off = k as f32 * step;
            let d = off - c;
            let chord = (radius * radius - d * d).max(0.).sqrt();
            if chord < step * 0.5 {
                continue;
            }
            let tint = match (k, axis) {
                (0, 0) => Some(axes[2]),
                (0, _) => Some(axes[0]),
                _ => None,
            };
            let along = center[2 - axis * 2];
            let at = |t: f32, s: f32| match axis {
                0 => [off + s, center[1], t],
                _ => [t, center[1], off + s],
            };
            for p in 0..PIECES {
                let t0 = along - chord + 2. * chord * p as f32 / PIECES as f32;
                let t1 = along - chord + 2. * chord * (p + 1) as f32 / PIECES as f32;
                let base = data.positions.len() as u32;
                for (t, s) in [
                    (t0, -half_width),
                    (t0, half_width),
                    (t1, -half_width),
                    (t1, half_width),
                ] {
                    let v = at(t, s);
                    let r = math::length([v[0] - center[0], v[2] - center[2]]);
                    let under = floor_color(r, shadow, radius);
                    let fade = (1. - r / radius).clamp(0., 1.);
                    data.positions.push(v);
                    data.colors.push(match (island, tint) {
                        // The top byte is a blended mesh's transparency.
                        (true, _) => {
                            let clear = ((1. - 0.4 * fade) * 255.).round() as u32;
                            clear << 24 | tint.unwrap_or(0xe8e4dc)
                        }
                        (false, Some(c)) => mix(under, c, 0.5 * fade),
                        (false, None) => mix(under, 0xffffff, 0.12 * fade),
                    });
                }
                // Wound as the floor disc, facing up.
                data.indices.extend(match axis {
                    0 => [base, base + 1, base + 2, base + 1, base + 3, base + 2],
                    _ => [base, base + 2, base + 1, base + 2, base + 3, base + 1],
                });
            }
        }
    }
    data.normals = vec![[0., 1., 0.]; data.positions.len()];
    Mesh::new(data).ok().map(Arc::new)
}

/// A light's or particle effect's marker.
#[derive(Clone, Copy, Debug)]
pub(super) struct Mark {
    pub entry: Id,
    pub at: Vec3,
    kind: MarkKind,
    /// 0xRRGGBB: the light's own colour at full brightness.
    color: u32,
    /// How far a light lights, in metres (`lights::Lights::reach`).
    pub range: f32,
    pub cone: Option<Cone>,
    pub area: Option<Area>,
}

/// An area light's shape in model space about its middle: `u` and `v` are
/// half its extent along its own X and Y, as the game's shaders take them
/// (`functions_area_lighting.fxh`): a quad's corners at the middle ± u ± v,
/// a disc's two half-axes, a line from the middle - u to + u, `v` there
/// half its thickness. That `Width` and `Height` are the full size along
/// X and Y, scaled with the placement, is an estimate: the engine fills
/// the shader's vectors in code. 10 of 117's 21 portrait quads face their
/// figure with their -Z, which a quad in the XY plane does.
#[derive(Clone, Copy, Debug)]
pub(super) struct Area {
    pub shape: Shape,
    pub u: Vec3,
    pub v: Vec3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shape {
    Quad,
    Disc,
    Line,
}

impl Area {
    /// Its outline in model space about `at`: a closed loop, or a line's
    /// two ends.
    pub fn outline(&self, at: Vec3) -> Vec<Vec3> {
        const ROUND: usize = 48;
        let p = |a: f32, b: f32| {
            math::add(
                at,
                math::add(math::scale(self.u, a), math::scale(self.v, b)),
            )
        };
        match self.shape {
            Shape::Quad => vec![p(-1., -1.), p(1., -1.), p(1., 1.), p(-1., 1.), p(-1., -1.)],
            Shape::Disc => (0..=ROUND)
                .map(|i| {
                    let t = i as f32 / ROUND as f32 * std::f32::consts::TAU;
                    p(t.cos(), t.sin())
                })
                .collect(),
            Shape::Line => vec![p(-1., 0.), p(1., 0.)],
        }
    }
}

/// A spot light's beam: the way it points, in model space, and its cones'
/// half angles off that way in radians: `Theta` inside, lit fully, and
/// `Phi` out to its edge. 117 keeps their cosines and tests them against
/// the angle off the aim (lights::Kind::Spot).
#[derive(Clone, Copy, Debug)]
pub(super) struct Cone {
    pub aim: Vec3,
    pub inner: f32,
    pub outer: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkKind {
    Point,
    Spot,
    /// Quad, disc and line lights.
    Area,
    Effect,
}

/// A light's marker from its cfg entry placed at `placed`: its `Type`,
/// colour and how far it lights (`reach`). Types: 0 point, 2 spot, 5 quad,
/// 6 disc and 8 line (what the shipped cfgs name them).
fn light_mark(doc: &Doc, entry: Id, placed: &Mat4, reach: f32) -> Mark {
    let num =
        |name: &str| (doc.child(entry, name)).and_then(|c| doc.text(c).trim().parse::<f32>().ok());
    let kind = match num("Type").unwrap_or(0.) as u32 {
        2 => MarkKind::Spot,
        5 | 6 | 8 => MarkKind::Area,
        _ => MarkKind::Point,
    };
    // A spot shines down its own -Y: 117's portrait spots aim at the figure
    // that way, and 1800's airship searchlight at the ground ahead of it.
    let down = [-placed[4], -placed[5], -placed[6]];
    let cone = (kind == MarkKind::Spot)
        .then(|| math::normalize(down))
        .flatten()
        // Every shipped light names both; 0.5 and 0.8 are what most carry.
        .map(|aim| Cone {
            aim,
            inner: num("Theta").unwrap_or(0.5),
            outer: num("Phi").unwrap_or(0.8),
        });
    let shape = match num("Type").unwrap_or(0.) as u32 {
        5 => Some(Shape::Quad),
        6 => Some(Shape::Disc),
        8 => Some(Shape::Line),
        _ => None,
    };
    // Every 117 light names both, 1 the most common; 133 of 1800's 1917.
    let half = |k: usize, size: Option<f32>| {
        math::scale(
            [placed[k], placed[k + 1], placed[k + 2]],
            size.unwrap_or(1.) * 0.5,
        )
    };
    let area = shape.map(|shape| Area {
        shape,
        u: half(0, num("Width")),
        v: half(4, num("Height")),
    });
    let diffuse = ["Diffuse.r", "Diffuse.g", "Diffuse.b"].map(|n| num(n).unwrap_or(0.).max(0.));
    // Its hue at full brightness: dim lights (0.001) would vanish.
    let top = diffuse.iter().copied().fold(0., f32::max);
    let color = match top > 1e-6 {
        true => (diffuse.iter()).fold(0, |c, v| c << 8 | (v / top * 255.).round() as u32),
        false => LIGHT,
    };
    Mark {
        entry,
        at: math::translation(placed),
        kind,
        color,
        range: reach,
        cone,
        area,
    }
}

/// What the viewport calls an entry of `list`, one of [`super::LISTS`].
fn kind_of(list: Option<&str>) -> &'static str {
    match list {
        Some("Models") => "mesh",
        Some("InstancingModels") => "stem",
        Some("InstancingBBClusters") => "leaf cluster",
        Some("Files") => "prop",
        Some("Decals" | "TerrainDecals") => "decal",
        Some("Particles" | "Effects") => "particle effect",
        Some("Lights") => "light",
        Some("Clothes") => "cloth",
        _ => "entry",
    }
}

impl ModelEditor {
    /// The joint an entry hangs on, posed as playback last left it: its
    /// placement is in that joint's space. `None` for the model's own.
    fn joint_space(&self, entry: Id) -> Option<Mat4> {
        let link = self.rig(entry)?.link?;
        playback::joint(&self.model, &self.player.poses, link)
    }

    /// The selection's gizmo: the entry, where its origin shows in model
    /// space, and the joint space its placement is in. `pivot()` alone is
    /// the placement, which for an entry on a joint is not where it shows.
    pub(super) fn handle_at(&self) -> Option<(Id, Mat4, Option<Mat4>)> {
        let (entry, local) = self.pivot()?;
        let parent = self.joint_space(entry);
        let at = parent.map_or(local, |p| math::multiply(&p, &local));
        Some((entry, at, parent))
    }

    /// Light and particle markers where they show.
    pub(super) fn marks(&self) -> Vec<Mark> {
        let placed = |entry: Id, t: &Transform| {
            let m = t.matrix();
            (self.joint_space(entry)).map_or(m, |p| math::multiply(&p, &m))
        };
        let lights = self.model.lights.iter().map(|l| {
            let at = placed(l.element, &l.transform);
            let reach = self.lights.reach(l.element, &at).unwrap_or(0.);
            light_mark(&self.doc, l.element, &at, reach)
        });
        let effects = self.model.particles.iter().map(|p| Mark {
            entry: p.element,
            at: math::translation(&placed(p.element, &p.transform)),
            kind: MarkKind::Effect,
            color: PARTICLE,
            range: 0.,
            cone: None,
            area: None,
        });
        lights.chain(effects).collect()
    }

    /// The light or particle whose marker is under `cursor`.
    fn mark_at(&self, cursor: Point<Pixels>) -> Option<Id> {
        if !self.view.shown.has(Overlays::MARKERS) {
            return None;
        }
        (self.marks().iter())
            .map(|k| (k.entry, distance(self.view.screen(k.at), cursor)))
            .filter(|(_, d)| d.is_finite() && *d <= MARKER_REACH)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }

    /// The entry under `cursor`: a marker, else the mesh the GPU picked
    /// there (asked on hover, so a click finds it waiting).
    fn entry_at(&self, cursor: Point<Pixels>) -> Option<Id> {
        self.mark_at(cursor).or_else(|| {
            let hit = self.view.viewport.pick(cursor)?;
            Some(Id::from_raw(hit.id? as u32))
        })
    }

    /// The shown part's joints in model space, posed: the part's entry and
    /// each joint's position. The part is the picked joint's, else the
    /// selection.
    pub(super) fn joint_dots(&self) -> Option<(Id, Vec<Vec3>)> {
        let shown = self.joint.map(|(e, _)| e).or(self.selected)?;
        let part = self.model.parts.iter().position(|p| p.element == shown)?;
        let p = &self.model.parts[part];
        let skeleton = p.mesh.as_ref()?.skeleton.as_ref()?;
        let pose = match self.player.poses.get(part) {
            Some(pose) if pose.len() == skeleton.len() => &pose[..],
            _ => &skeleton.bind[..],
        };
        let at = p.transform.matrix();
        let dots = pose.iter().map(|j| math::transform(&at, j.translation));
        Some((p.element, dots.collect()))
    }

    fn joint_at(&self, cursor: Point<Pixels>) -> Option<(Id, u16)> {
        if !self.view.shown.has(Overlays::JOINTS) {
            return None;
        }
        let (part, dots) = self.joint_dots()?;
        (dots.iter().enumerate())
            .map(|(j, p)| (j, distance(self.view.screen(*p), cursor)))
            .filter(|(_, d)| d.is_finite() && *d <= JOINT_REACH)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(j, _)| (part, j as u16))
    }

    /// What F fits for `entry`: what is drawn of it, with a light's reach
    /// and shape or a little room around a marker that draws nothing.
    pub fn framing(&self, entry: Id) -> Option<[Vec3; 2]> {
        let drawn = self.view.box_of(entry);
        let mark = self.marks().into_iter().find(|k| k.entry == entry);
        let around = mark.map(|k| {
            let r = k.range.clamp(0.25, self.view.extent.size * 0.5);
            let shape = k.area.map(|a| a.outline(k.at)).unwrap_or_default();
            math::bounds(
                [k.at.map(|v| v - r), k.at.map(|v| v + r)]
                    .into_iter()
                    .chain(shape),
            )
        });
        match (drawn, around) {
            (Some(a), Some(b)) => Some(math::bounds([a[0], a[1], b[0], b[1]])),
            (a, b) => a.or(b),
        }
    }

    /// Fits the view to the selection, the whole model with none.
    pub fn frame_selection(&mut self) {
        match self.selected.and_then(|s| self.framing(s)) {
            Some(b) => self.view.fit(b),
            None => self.view.frame_all(),
        }
    }

    /// Lights the view as at midnight, or back at noon: the sun, and the
    /// lights the game lights by the hour.
    pub fn set_night(&mut self, night: bool) {
        self.view.night = night;
        self.pose();
    }

    /// Flips an overlay and sends the scene again.
    pub fn toggle_overlay(&mut self, o: Overlays) {
        self.view.shown.toggle(o);
        self.view.present(self.selected);
    }
}

/// `trigger` with `menu` open under it, drawn over everything else. The
/// toolbar is centred, so a menu placed from the pane's edge (as View's
/// was) sat nowhere near its button.
fn under(trigger: Stateful<Div>, menu: Option<Stateful<Div>>) -> Div {
    div().relative().child(trigger).when_some(menu, |d, menu| {
        d.child(deferred(menu.top_full().mt(px(6.))).with_priority(1))
    })
}

fn distance(a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    f32::from(a.x - b.x).hypot(f32::from(a.y - b.y))
}

/// Paints a spot light's beam: its outer cone as a rim at its range and
/// four edges back to the light, the inner cone's rim fainter; brighter
/// while selected. One without a range still reaches `BEAM` px, to show
/// where it points.
fn paint_cone(
    k: &Mark,
    cone: &Cone,
    camera: Camera,
    b: Bounds<Pixels>,
    extent: Extent,
    selected: bool,
    w: &mut Window,
) {
    const BEAM: f32 = 48.;
    const ROUND: usize = 32;
    let apex = camera.project(k.at, b, extent);
    let per_metre = distance(
        camera.project(math::add(k.at, camera.right()), b, extent),
        apex,
    );
    if !per_metre.is_finite() || per_metre < 1e-3 {
        return;
    }
    let length = k.range.max(BEAM / per_metre);
    let end = math::add(k.at, math::scale(cone.aim, length));
    // Two axes across the beam; any not along it starts them.
    let across = match cone.aim[1].abs() < 0.9 {
        true => [0., 1., 0.],
        false => [1., 0., 0.],
    };
    let Some(u) = math::normalize(math::cross(cone.aim, across)) else {
        return;
    };
    let v = math::cross(cone.aim, u);
    // The half angle either side of the aim; past ~80° the rim runs off to
    // infinity.
    let radius = |half: f32| length * half.clamp(0., 1.4).tan();
    let rim_at = |r: f32, turn: f32| {
        let (sin, cos) = turn.sin_cos();
        let off = math::add(math::scale(u, cos * r), math::scale(v, sin * r));
        camera.project(math::add(end, off), b, extent)
    };
    let rim = |r: f32| -> Vec<Point<Pixels>> {
        (0..=ROUND)
            .map(|i| rim_at(r, i as f32 / ROUND as f32 * std::f32::consts::TAU))
            .collect()
    };
    let (alpha, width) = if selected { (0.85, 1.5) } else { (0.4, 1.) };
    let color = rgb(k.color).alpha(alpha);
    let outer = radius(cone.outer);
    paint::line(&rim(outer), width, color, w);
    for q in 0..4 {
        let edge = rim_at(outer, q as f32 * std::f32::consts::FRAC_PI_2);
        paint::line(&[apex, edge], width, color, w);
    }
    if cone.inner > 0. && cone.inner < cone.outer {
        let inner = rgb(k.color).alpha(alpha * 0.55);
        paint::line(&rim(radius(cone.inner)), 1., inner, w);
    }
}

/// Paints an area light's outline in its colour, brighter while selected.
fn paint_area(
    k: &Mark,
    area: &Area,
    camera: Camera,
    b: Bounds<Pixels>,
    extent: Extent,
    selected: bool,
    w: &mut Window,
) {
    let points: Vec<Point<Pixels>> = (area.outline(k.at).into_iter())
        .map(|p| camera.project(p, b, extent))
        .collect();
    // A corner behind the eye has no place on screen.
    if points
        .iter()
        .any(|p| !f32::from(p.x).is_finite() || !f32::from(p.y).is_finite())
    {
        return;
    }
    let (alpha, width) = if selected { (0.9, 2.) } else { (0.55, 1.5) };
    paint::line(&points, width, rgb(k.color).alpha(alpha), w);
}

/// Paints a light's or effect's marker: lights in their own colour, shaped
/// by kind (a dot, a diamond for a spot, a square in an area light's
/// middle),
/// effects as a ring. The selected one gets the selection's ring and, for
/// a light, a circle as far as it reaches.
fn paint_mark(
    at: Point<Pixels>,
    k: &Mark,
    selected: bool,
    hovered: bool,
    reach: f32,
    w: &mut Window,
) {
    let casing = rgb(crate::colors::CASING);
    let color = rgb(k.color);
    let shape = |r: f32| -> Vec<Point<Pixels>> {
        let p = |x: f32, y: f32| at + point(px(x), px(y));
        match k.kind {
            MarkKind::Spot => vec![p(0., -r), p(r, 0.), p(0., r), p(-r, 0.)],
            _ => vec![p(-r, -r), p(r, -r), p(r, r), p(-r, r)],
        }
    };
    // A spot's reach shows as its cone.
    if selected && k.kind != MarkKind::Effect && k.cone.is_none() && reach > 6. {
        paint::circle(at, reach, 1.5, rgb(k.color).alpha(0.55), w);
    }
    match k.kind {
        MarkKind::Point => {
            paint::filled_circle(at, 6.5, casing, w);
            paint::filled_circle(at, 5., color, w);
        }
        MarkKind::Spot => {
            paint::polygon(&shape(8.), casing, w);
            paint::polygon(&shape(6.), color, w);
        }
        MarkKind::Area => {
            paint::polygon(&shape(5.5), casing, w);
            paint::polygon(&shape(4.), color, w);
        }
        MarkKind::Effect => {
            paint::circle(at, 5., 4., casing, w);
            paint::circle(at, 5., 2., color, w);
            paint::filled_circle(at, 1.5, color, w);
        }
    }
    if selected {
        paint::circle(at, 9.5, 2., rgb(SELECTED), w);
    } else if hovered {
        paint::circle(at, 9., 1.5, rgb(0xffffff).alpha(0.8), w);
    }
}

impl Editor {
    pub fn render_model_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(m) = &self.model else {
            return widgets::empty_state_icon(Icon::CUBE, "No model open")
                .child(widgets::hint(
                    "Click a model in Models on the left, or find one by name.",
                ))
                .child(
                    widgets::hint("Point at one to preview it in the Inspector.")
                        .text_color(theme::get().muted),
                )
                .child(
                    div()
                        .flex()
                        .pt_2()
                        .gap_2()
                        .child(
                            widgets::action("Open model…", widgets::Tone::Primary)
                                .tooltip(widgets::Tooltip::text("Open model", "Ctrl P", ""))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.open_model_picker(super::picker::Want::Open, cx);
                                })),
                        )
                        .child(
                            widgets::action("Browse in Assets", widgets::Tone::Secondary).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.browse_game_models(cx);
                                }),
                            ),
                        ),
                )
                .into_any_element();
        };
        let v = &m.view;
        let (viewport, camera, extent) = (v.viewport.clone(), v.camera, v.extent);
        let (bounds, renderer, hover) = (v.bounds.clone(), v.renderer.clone(), v.hover.clone());
        // A picked joint of a playing part takes the gizmo: it keys.
        let pivot = (m.joint_matrix()).or_else(|| m.handle_at().map(|(_, at, _)| at));
        let (mode, local) = (m.gizmo, v.local);
        let (active, readout_text) = match &v.drag {
            Some(Drag::Gizmo { gesture, delta, .. } | Drag::Joint { gesture, delta, .. }) => (
                Some(gesture.handle),
                delta.as_ref().map(|d| readout(d, gesture.handle)),
            ),
            _ => (None, None),
        };
        let playing = m.playback.playing;
        let host = window.current_view();
        let marks = match v.shown.has(Overlays::MARKERS) {
            true => m.marks(),
            false => Vec::new(),
        };
        let hovered = hover.get().entry;
        let selected = m.selected;
        let picked = m.joint;
        let joints = (v.shown.has(Overlays::JOINTS))
            .then(|| m.joint_dots())
            .flatten();
        let drawing = canvas(
            |_, _, _| (),
            move |b, _, window, cx| {
                // The hint and pills over the view are laid out from the
                // last paint's bounds: a pane that just appeared or changed
                // size needs one more frame to get them right. On the next
                // frame: a notify while this one paints is dropped with it.
                let mut again = bounds.replace(b) != b;
                viewport.paint(camera, extent, b, window);
                window.with_content_mask(Some(ContentMask { bounds: b }), |window| {
                    if let Some((part, dots)) = &joints {
                        for (j, p) in dots.iter().enumerate() {
                            let at = camera.project(*p, b, extent);
                            if picked == Some((*part, j as u16)) {
                                paint::filled_circle(at, 5., rgb(crate::colors::CASING), window);
                                paint::filled_circle(at, 4., rgb(SELECTED), window);
                            } else {
                                paint::filled_circle(at, 2.5, rgb(BONE), window);
                            }
                        }
                    }
                    for k in &marks {
                        let sel = Some(k.entry) == selected;
                        if let Some(cone) = &k.cone {
                            paint_cone(k, cone, camera, b, extent, sel, window);
                        }
                        if let Some(area) = &k.area {
                            paint_area(k, area, camera, b, extent, sel, window);
                        }
                    }
                    for k in &marks {
                        let at = camera.project(k.at, b, extent);
                        // A light's reach on screen: its range at its depth.
                        let edge = math::add(k.at, math::scale(camera.right(), k.range));
                        let reach = distance(camera.project(edge, b, extent), at);
                        let sel = Some(k.entry) == selected;
                        paint_mark(at, k, sel, Some(k.entry) == hovered, reach, window);
                    }
                    if let Some(pivot) = pivot
                        && let Some(f) = handles(mode, &pivot, local, camera, b, extent)
                    {
                        f.paint(active, window);
                    }
                });
                // The mesh under a resting cursor, once the GPU answers.
                let mut h = hover.get();
                if let Some(c) = h.cursor
                    && h.tries > 0
                {
                    match viewport.pick(c) {
                        Some(hit) => {
                            h.entry = hit.id.map(|id| Id::from_raw(id as u32));
                            h.tries = 0;
                        }
                        None => {
                            h.tries -= 1;
                            if h.tries == 0 {
                                h.entry = None;
                            }
                        }
                    }
                    hover.set(h);
                    again = true;
                }
                if again {
                    window.on_next_frame(move |_, cx| cx.notify(host));
                }
                // Uploads spread over frames, and playback moves: a cached
                // pane does not paint again by itself.
                if renderer.pending() || playing {
                    cx.notify(host);
                }
            },
        )
        .absolute()
        .inset_0();
        let pane = v.bounds.get().size;
        // What the cursor is on, or what the drag has done: the gizmo's
        // numbers otherwise show only after the drag, in the inspector.
        let status = readout_text.unwrap_or_else(|| {
            let grid = match v.step {
                s if s < 1. => format!("Grid {} cm", (s * 100.).round()),
                s => format!("Grid {s} m"),
            };
            match hovered.filter(|e| m.doc.is_attached(*e)) {
                Some(e) => format!("{} · {} | {grid}", m.label(e), kind_of(m.list_of(e))),
                // What is open and whose it is, as the Island view's chip
                // says it, when the cursor is on nothing.
                None => format!(
                    "{} · {} | {grid}",
                    crate::island::pretty(
                        self.asset_words(),
                        crate::project::models::model_name(&m.path)
                    ),
                    if m.in_project {
                        "In this mod"
                    } else {
                        "Game original"
                    },
                ),
            }
        });
        let mut view = div()
            .id("model-view")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BACKDROP))
            .child(drawing)
            // Centred, as the Island view's, so the gutters match whatever
            // room is left over.
            .child(
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .right_2()
                    .flex()
                    .justify_center()
                    .child(self.model_tool_bar(cx)),
            )
            // The keys the toolbar's tooltips name (W E R, F, N) stay out:
            // the pointer's part only, as the Island view's hint.
            .child(
                widgets::hint_pill(
                    "Click: select · Double-click: frame · Right-click: menu | Right-drag: orbit · Middle-drag: pan · Scroll: zoom",
                )
                .absolute()
                .left_2()
                .bottom_2()
                .max_w(pane.width * 0.7),
            );
        view = view.child(
            div()
                .absolute()
                .right_2()
                .bottom_2()
                .flex()
                .flex_col()
                .items_end()
                .gap_2()
                .child(
                    div()
                        .id("model-view-orientation")
                        .cursor_pointer()
                        .child(gizmo::orientation(camera, false))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(m) = &mut this.model {
                                m.view.reset();
                            }
                            this.notify_pane(Pane::ModelView, cx);
                        })),
                )
                .child(widgets::hint_pill(&status)),
        );
        let m = self.model.as_ref().unwrap();
        // The toolbar's own menus open under their buttons (model_tool_bar).
        if let Some(Menu::Entry { entry, at }) = m.view.menu {
            view = view.child(self.model_entry_menu(entry, at, pane, cx));
        }
        view = view
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| this.model_view_press(e, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    let Some(m) = &mut this.model else { return };
                    m.view.menu = None;
                    // Turn about the selection, else about what is in the
                    // middle of the pane: the model's centre swung a
                    // zoomed-in detail off screen.
                    let pivot = (m.selected.and_then(|s| m.framing(s)))
                        .map(|[lo, hi]| std::array::from_fn(|i| (lo[i] + hi[i]) / 2.))
                        .unwrap_or_else(|| m.view.middle());
                    let (b, extent) = (m.view.bounds.get(), m.view.extent);
                    m.view.camera.begin_orbit(Some(pivot), b, extent);
                    m.view.drag = Some(Drag::Orbit {
                        last: e.position,
                        from: e.position,
                    });
                    this.notify_pane(Pane::ModelView, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, e: &MouseDownEvent, _, _| {
                    if let Some(m) = &mut this.model {
                        m.view.menu = None;
                        m.view.drag = Some(Drag::Pan(e.position));
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                    this.model_view_right_up(e.position, cx)
                }),
            );
        for button in [MouseButton::Left, MouseButton::Middle] {
            view = view.on_mouse_up(
                button,
                cx.listener(|this, _, _, cx| {
                    let Some(m) = &mut this.model else { return };
                    if matches!(
                        m.view.drag.take(),
                        Some(Drag::Gizmo { .. } | Drag::Joint { .. })
                    ) {
                        this.model_edited(cx);
                    }
                }),
            );
        }
        view.on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
            this.model_view_move(e, cx);
        }))
        .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
            let Some(m) = &mut this.model else { return };
            let delta = match e.delta {
                ScrollDelta::Pixels(p) => f32::from(p.y),
                ScrollDelta::Lines(p) => p.y * 32.,
            };
            m.view.zoom(delta, e.position);
            this.notify_pane(Pane::ModelView, cx);
        }))
        .into_any_element()
    }

    /// The floating tool panel over the view, shaped as the Island view's:
    /// the model, gizmo, light and camera commands in captioned groups, and
    /// under a rule the gizmo's settings.
    fn model_tool_bar(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let m = self.model.as_ref().expect("a model is open");
        let k = mode_index(m.gizmo);
        let menu = m.view.menu;
        // A command that cannot run now stays in place, dimmed, its tooltip
        // saying why, so the strip keeps its shape.
        let off = |b: Stateful<Div>| b.opacity(0.4).cursor_default();

        let back = match self.previous_model() {
            Some(path) => widgets::tool_button("model-back", BACK_ICON, "Back", false)
                .tooltip(widgets::Tooltip::text(
                    "Back",
                    "Alt ←",
                    format!("Open {} again", path.rsplit('/').next().unwrap_or(path)),
                ))
                .on_click(cx.listener(|this, _, _, cx| this.model_back(cx))),
            None => off(widgets::tool_button("model-back", BACK_ICON, "Back", false)).tooltip(
                widgets::Tooltip::text("Back", "Alt ←", "Nothing was open before this model"),
            ),
        };
        let model = div()
            .flex()
            .gap(px(1.))
            .child(
                widgets::tool_button("model-open", Icon::FOLDER, "Open", false)
                    .tooltip(widgets::Tooltip::text(
                        "Open model",
                        "Ctrl P",
                        "Another cfg or prp by name; the ones opened before come first",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_model_picker(super::picker::Want::Open, cx);
                    })),
            )
            .child(back);

        let mut edit = div().flex().gap(px(1.));
        for (mode, icon, label, key, detail) in [
            (
                gizmo::Mode::Move,
                Icon::MOVE,
                "Move",
                "W",
                "Drag the gizmo to move the selection",
            ),
            (
                gizmo::Mode::Rotate,
                Icon::ROTATE,
                "Rotate",
                "E",
                "Drag the gizmo to turn the selection",
            ),
            (
                gizmo::Mode::Scale,
                Icon::SCALE,
                "Scale",
                "R",
                "Drag the gizmo to scale the selection along its own axes",
            ),
        ] {
            edit = edit.child(
                widgets::tool_button(
                    ("model-gizmo", mode_index(mode)),
                    icon,
                    label,
                    m.gizmo == mode,
                )
                .tooltip(widgets::Tooltip::text(label, key, detail))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.gizmo = mode;
                    }
                    cx.notify();
                })),
            );
        }

        // Day and Night as two tools, N flipping between them (model_key);
        // Sky picks what lights either.
        let mut light = div().flex().gap(px(1.));
        for (night, icon, label, detail) in [
            (
                false,
                SUN_ICON,
                "Day",
                "Noon: the sun, and the lights the game lights by day",
            ),
            (
                true,
                MOON_ICON,
                "Night",
                "Midnight: the moon, glowing night masks, and the lights the game lights by night",
            ),
        ] {
            light = light.child(
                widgets::tool_button(
                    ("model-hour", night as usize),
                    icon,
                    label,
                    m.view.night == night,
                )
                .tooltip(widgets::Tooltip::text(label, "N", detail))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.set_night(night);
                    }
                    cx.notify();
                })),
            );
        }
        let sky = widgets::tool_button("model-sky", CLOUD_ICON, "Sky", menu == Some(Menu::Light))
            .tooltip(widgets::Tooltip::text(
                "Sky",
                "",
                match &m.view.island {
                    Some(s) => format!(
                        "Lit by the island sky {}; pick another, or the studio",
                        crate::island::preset_name(self.asset_words(), &s.name)
                    ),
                    None => {
                        "Lit by the studio; pick an island sky of the model's region".to_owned()
                    }
                },
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.menu = Some(Menu::Light);
                }
                this.notify_pane(Pane::ModelView, cx);
            }));
        let light = light.child(under(
            sky,
            (menu == Some(Menu::Light)).then(|| self.model_light_menu(cx)),
        ));

        // Camera commands with names, not only keys, as the Island view's.
        let selected = m.selected.is_some();
        let fit = widgets::tool_button("model-fit", Icon::FIT, "Fit", false)
            .tooltip(widgets::Tooltip::text(
                "Fit model",
                if selected { "" } else { "F" },
                "Frame the whole model; Ctrl Home also turns back to the opening view",
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.frame_all();
                }
                this.notify_pane(Pane::ModelView, cx);
            }));
        let frame = widgets::tool_button("model-frame", Icon::FRAME, "Frame", false);
        let frame = match selected {
            true => frame
                .tooltip(widgets::Tooltip::text(
                    "Frame selection",
                    "F",
                    "Zoom onto the selection",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.frame_selection();
                    }
                    this.notify_pane(Pane::ModelView, cx);
                })),
            false => off(frame).tooltip(widgets::Tooltip::text(
                "Frame selection",
                "F",
                "Select something first",
            )),
        };
        let show = widgets::tool_button("model-show", Icon::EYE, "Show", menu == Some(Menu::View))
            .tooltip(widgets::Tooltip::text(
                "Show",
                "",
                "Skeleton, joints, lights and effects, cloth and grid; the axis views",
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.menu = Some(Menu::View);
                }
                this.notify_pane(Pane::ModelView, cx);
            }));
        let camera = div()
            .flex()
            .gap(px(1.))
            .child(fit)
            .child(frame)
            .child(under(
                show,
                (menu == Some(Menu::View)).then(|| self.model_view_menu(cx)),
            ));

        let separator = || widgets::toolbar_separator().mt(px(18.));
        let icons = div()
            .id("model-tools")
            .flex()
            .px_1()
            .pt(px(2.))
            .pb_1()
            .gap_1()
            .items_start()
            .child(widgets::tool_group("Model", model))
            .child(separator())
            .child(widgets::tool_group("Edit", edit))
            .child(separator())
            .child(widgets::tool_group("Light", light))
            .child(separator())
            .child(widgets::tool_group("View", camera));

        // Scale always runs along the entry's own axes: the choice is moot
        // there, so it shows Local without being one.
        let space = widgets::segmented(
            "model-gizmo-space",
            [(false, "World".into()), (true, "Local".into())],
            &(m.view.local || m.gizmo == gizmo::Mode::Scale),
            cx,
            |this, local, _, _| {
                if let Some(m) = &mut this.model {
                    m.view.local = local;
                }
            },
        );
        let snap = widgets::overlay_toggle("model-snap", "Snap", m.view.snap)
            .tooltip(widgets::Tooltip::text(
                "Snap",
                "",
                "Snap to the step beside it; holding Ctrl snaps while this is off",
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.snap = !m.view.snap;
                }
                this.notify_pane(Pane::ModelView, cx);
            }));
        let steps = widgets::segmented(
            ("model-snap-step", k),
            STEPS[k].map(|(s, label)| (s, label.into())),
            &m.view.steps[k],
            cx,
            move |this, step, _, _| {
                if let Some(m) = &mut this.model {
                    m.view.steps[k] = step;
                }
            },
        );
        // The gizmo's settings under the icons, as the Island's Select
        // options: the row keeps its shape with nothing selected, and the
        // note at its end says what it waits for.
        let options = div()
            .id("model-tool-options")
            .flex()
            .flex_wrap()
            .items_center()
            .min_h(px(36.))
            .px_2()
            .py_1()
            .gap_2()
            .border_t_1()
            .border_color(theme::get().floating_border)
            .child(space)
            .child(snap)
            // Dimmed while off: the step is a setting, used once Snap is on
            // or Ctrl is held.
            .child(div().when(!m.view.snap, |d| d.opacity(0.6)).child(steps))
            .when(!selected, |d| {
                d.child(
                    div()
                        .px_1()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(match m.gizmo {
                            gizmo::Mode::Move => "Click a mesh, prop, light or effect to move it",
                            gizmo::Mode::Rotate => "Click a mesh, prop, light or effect to turn it",
                            gizmo::Mode::Scale => "Click a mesh, prop, light or effect to scale it",
                        }),
                )
            });
        widgets::floating_surface("model-tools-panel")
            .flex_col()
            .items_stretch()
            .child(icons)
            .child(options)
    }

    /// The Sky button's menu: the studio, or an island sky.
    fn model_light_menu(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let m = self.model.as_ref().expect("a model is open");
        let shown = m.view.island.as_ref().map(|s| s.name.clone());
        let mut popup = widgets::menu_popup(0.)
            .w(px(240.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.menu = None;
                }
                this.notify_pane(Pane::ModelView, cx);
            }))
            .child(widgets::menu_header("Lit by"))
            .child(
                widgets::menu_item_radio("Studio", shown.is_none(), "").on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.view.menu = None;
                        }
                        this.model_sky(None, cx);
                    },
                )),
            );
        if !m.view.skies.is_empty() {
            popup = popup.child(widgets::menu_section("Island sky"));
        }
        for name in m.view.skies.clone() {
            let on = shown.as_ref() == Some(&name);
            let label = crate::island::preset_name(self.asset_words(), &name);
            popup = popup.child(
                widgets::menu_item_radio(label, on, "").on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.view.menu = None;
                        }
                        this.model_sky(Some(name.clone()), cx);
                    },
                )),
            );
        }
        popup
    }

    /// The Show button's menu: overlays, then camera views.
    fn model_view_menu(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let m = self.model.as_ref().expect("a model is open");
        // Right edges aligned: Show ends the panel, and a menu growing
        // right from it ran past a narrow pane.
        let mut popup = widgets::menu_popup_right(0.)
            .w(px(240.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.menu = None;
                }
                this.notify_pane(Pane::ModelView, cx);
            }))
            .child(widgets::menu_header("Show"));
        for (o, label, key) in OVERLAYS {
            popup = popup.child(
                widgets::menu_item_checked(label, m.view.shown.has(o), key).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.toggle_overlay(o);
                        }
                        this.notify_pane(Pane::ModelView, cx);
                    },
                )),
            );
        }
        let camera = |label: &'static str, key: &'static str, f: fn(&mut ModelEditor)| {
            widgets::menu_item(label, key).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.menu = None;
                    f(m);
                }
                this.notify_pane(Pane::ModelView, cx);
            }))
        };
        popup = popup.child(widgets::menu_section("Camera")).child(camera(
            "Reset view",
            "Ctrl Home",
            |m| m.view.reset(),
        ));
        for (k, (label, key, ..)) in VIEWS.into_iter().enumerate() {
            popup = popup.child(widgets::menu_item(label, key).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(m) = &mut this.model {
                        m.view.menu = None;
                        let (_, _, yaw, tilt) = VIEWS[k];
                        m.view.look(yaw, tilt);
                    }
                    this.notify_pane(Pane::ModelView, cx);
                },
            )));
        }
        popup
            .child(camera("Opposite side", "9", |m| {
                let c = m.view.camera;
                m.view.look(c.yaw + std::f32::consts::PI, c.tilt);
            }))
            .child(
                widgets::menu_item_checked("Perspective", m.view.camera.fov > 0., "5").on_click(
                    cx.listener(|this, _, _, cx| {
                        if let Some(m) = &mut this.model {
                            m.view.camera.fov = if m.view.camera.fov > 0. { 0. } else { LENS };
                        }
                        this.notify_pane(Pane::ModelView, cx);
                    }),
                ),
            )
    }

    /// The right-click menu on `entry`, at `at` in the pane: flipped above
    /// or left of the pointer where it would run off the pane.
    fn model_entry_menu(
        &self,
        entry: Id,
        at: Point<Pixels>,
        pane: Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let m = self.model.as_ref().expect("a model is open");
        let linked = m.rig(entry).and_then(|r| r.link).is_some();
        let prop = m.list_of(entry) == Some("Files");
        let (w, h) = (
            220.,
            34. + 28. * (3 + usize::from(linked) + usize::from(prop)) as f32,
        );
        let x = match f32::from(at.x) + w > f32::from(pane.width) - 8. {
            true => (f32::from(at.x) - w).max(8.),
            false => f32::from(at.x),
        };
        let y = match f32::from(at.y) + h > f32::from(pane.height) - 8. {
            true => (f32::from(at.y) - h).max(8.),
            false => f32::from(at.y),
        };
        let act = |label: &'static str, key: &'static str, a: Action| {
            widgets::menu_item(label, key)
                .on_click(cx.listener(move |this, _, _, cx| this.model_view_act(a, cx)))
        };
        div()
            .id("model-entry-menu")
            .occlude()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .bg(theme::get().popup)
            .rounded(px(6.))
            .border_1()
            .border_color(theme::get().floating_border)
            .shadow_lg()
            .p_1()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if let Some(m) = &mut this.model {
                    m.view.menu = None;
                }
                this.notify_pane(Pane::ModelView, cx);
            }))
            .child(widgets::menu_header(format!(
                "{} · {}",
                m.label(entry),
                kind_of(m.list_of(entry))
            )))
            .child(act("Duplicate", "Ctrl D", Action::Duplicate))
            .child(act("Delete", "Del", Action::Delete))
            .child(act("Frame", "F", Action::Frame))
            .when(linked, |d| {
                d.child(act("Detach from joint", "", Action::Detach))
            })
            .when(prop, |d| d.child(act("Open prop", "", Action::OpenProp)))
    }

    /// Runs an entry menu item on the entry the menu is for.
    pub(crate) fn model_view_act(&mut self, action: Action, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        let Some(Menu::Entry { entry, .. }) = m.view.menu.take() else {
            return;
        };
        if !m.doc.is_attached(entry) {
            return;
        }
        match action {
            Action::Duplicate => m.duplicate(entry),
            Action::Delete => m.remove(entry),
            Action::Frame => {
                m.select(Some(entry));
                m.frame_selection();
            }
            Action::Detach => m.attach(entry, None),
            Action::OpenProp => {
                let cfg = (m.model.props.iter())
                    .find(|p| p.element == entry)
                    .map(|p| p.cfg.clone());
                if let Some(cfg) = cfg {
                    return self.open_model(&cfg, cx);
                }
            }
        }
        self.model_edited(cx);
    }

    /// A left press: a gizmo handle starts a drag, a joint dot picks the
    /// joint, a marker or mesh selects, empty space clears the selection.
    /// A double click also frames what it hit, the whole model on nothing.
    fn model_view_press(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let cursor = e.position;
        let Some(m) = &mut self.model else { return };
        m.view.menu = None;
        if let Some(start) = m.joint_matrix()
            && let Some(frame) = m.view.gizmo_frame(m.gizmo, &start)
            && let Some(gesture) = gizmo::Gesture::begin(frame, cursor)
        {
            m.checkpoint();
            m.view.drag = Some(Drag::Joint {
                gesture,
                start,
                delta: None,
            });
            return cx.notify();
        }
        if let Some((entry, at, parent)) = m.handle_at()
            && let Some(frame) = m.view.gizmo_frame(m.gizmo, &at)
            && let Some(gesture) = gizmo::Gesture::begin(frame, cursor)
        {
            let start = m.placement(entry).expect("pivot has a placement");
            m.checkpoint();
            m.view.drag = Some(Drag::Gizmo {
                gesture,
                entry,
                start,
                parent,
                delta: None,
            });
            return cx.notify();
        }
        if let Some(joint) = m.joint_at(cursor) {
            m.joint = Some(joint);
            m.pose();
            return self.model_edited(cx);
        }
        let hit = m.entry_at(cursor);
        m.select(hit);
        if e.click_count == 2 {
            m.frame_selection();
        }
        self.model_edited(cx);
    }

    /// A right release: a click that did not turn the view opens the menu
    /// on the entry under it, selecting it.
    fn model_view_right_up(&mut self, cursor: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        let Some(Drag::Orbit { from, .. }) = m.view.drag.take() else {
            return;
        };
        if distance(from, cursor) >= 4. {
            return;
        }
        let Some(entry) = m.entry_at(cursor) else {
            return;
        };
        m.select(Some(entry));
        let at = cursor - m.view.bounds.get().origin;
        m.view.menu = Some(Menu::Entry { entry, at });
        self.model_edited(cx);
    }

    fn model_view_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        // The toolbar's steps, when Snap is on or Ctrl held.
        let ctrl = e.modifiers.control || e.modifiers.platform;
        let [step, turn, scale] = m.view.steps;
        let snap = (m.view.snap || ctrl).then(|| gizmo::Snap {
            step,
            turn: turn.to_radians(),
            scale,
        });
        let Some(drag) = &mut m.view.drag else {
            if e.pressed_button.is_some() {
                return;
            }
            // Ask the GPU what is under the cursor now; the canvas waits
            // for the answer. A marker answers at once.
            m.view.viewport.pick(e.position);
            let mark = m.mark_at(e.position);
            let mut h = m.view.hover.get();
            h.cursor = Some(e.position);
            match mark {
                Some(entry) => (h.entry, h.tries) = (Some(entry), 0),
                None => h.tries = PICK_WAIT,
            }
            m.view.hover.set(h);
            // Every move: the gizmo lights the handle under the cursor as
            // it paints.
            return self.notify_pane(Pane::ModelView, cx);
        };
        if e.pressed_button.is_none() {
            m.view.drag = None;
            return;
        }
        match drag {
            Drag::Orbit { last, .. } => {
                let delta = e.position - *last;
                *last = e.position;
                m.view.orbit(delta);
                // Billboards turn with the camera; playing poses anyway.
                if !m.playback.playing && m.faces_camera() {
                    m.pose();
                }
                return self.notify_pane(Pane::ModelView, cx);
            }
            Drag::Pan(last) => {
                let delta = e.position - *last;
                *last = e.position;
                m.view.camera.pan_by(delta);
                return self.notify_pane(Pane::ModelView, cx);
            }
            Drag::Gizmo {
                gesture,
                entry,
                start,
                parent,
                delta,
            } => {
                let Some(d) = gesture.delta(e.position, snap) else {
                    return;
                };
                *delta = Some(d);
                let (entry, parent) = (*entry, *parent);
                let from = start.matrix();
                let from = parent.map_or(from, |p| math::multiply(&p, &from));
                let moved = d.apply(&from, math::translation(&from));
                // Back into the joint's space the placement is written in.
                let local = match parent.and_then(|p| math::inverse(&p)) {
                    Some(inverse) => math::multiply(&inverse, &moved),
                    None => moved,
                };
                m.set_placement(entry, Transform::from_matrix(&local), false);
            }
            Drag::Joint {
                gesture,
                start,
                delta,
            } => {
                let Some(d) = gesture.delta(e.position, snap) else {
                    return;
                };
                *delta = Some(d);
                let moved = d.apply(start, math::translation(start));
                if let Err(e) = m.key_joint_at(&moved, false) {
                    m.message = Some((format!("Not keyed: {e:#}"), true));
                }
            }
        }
        cx.notify();
    }

    /// Keys the viewport and hierarchy take when nothing is being typed.
    pub(crate) fn model_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(m) = &mut self.model else {
            return false;
        };
        let k = &event.keystroke;
        let ctrl = k.modifiers.control || k.modifiers.platform;
        let overlay = (OVERLAYS.iter())
            .find(|(_, _, key)| key.eq_ignore_ascii_case(&k.key))
            .map(|(o, ..)| *o);
        let view = VIEWS.iter().find(|(_, key, ..)| *key == k.key);
        match k.key.as_str() {
            "w" if !ctrl => m.gizmo = gizmo::Mode::Move,
            "e" if !ctrl => m.gizmo = gizmo::Mode::Rotate,
            "r" if !ctrl => m.gizmo = gizmo::Mode::Scale,
            "f" if !ctrl => m.frame_selection(),
            // The hierarchy takes Home only while it has the keys
            // (src/editor.rs); Ctrl Home resets from anywhere.
            "home" => m.view.reset(),
            _ if view.is_some() && !ctrl => {
                let (_, _, yaw, tilt) = *view.unwrap();
                m.view.look(yaw, tilt);
            }
            "9" if !ctrl => {
                let c = m.view.camera;
                m.view.look(c.yaw + std::f32::consts::PI, c.tilt);
            }
            "5" if !ctrl => {
                m.view.camera.fov = if m.view.camera.fov > 0. { 0. } else { LENS };
            }
            "n" if !ctrl => m.set_night(!m.view.night),
            _ if overlay.is_some() && !ctrl => m.toggle_overlay(overlay.unwrap()),
            "space" if !ctrl => m.playback.playing = !m.playback.playing,
            "d" if ctrl => match m.selected {
                Some(s) => m.duplicate(s),
                None => return false,
            },
            "delete" | "backspace" if !ctrl => match m.selected {
                Some(s) => m.remove(s),
                None => return false,
            },
            "escape" if m.view.menu.is_some() => m.view.menu = None,
            "escape" => m.select(None),
            _ => return false,
        }
        self.model_edited(cx);
        if self.model.as_ref().is_some_and(|m| m.playback.playing) {
            self.model_tick(cx);
        }
        true
    }
}
