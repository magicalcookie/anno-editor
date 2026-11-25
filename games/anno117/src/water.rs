// Owns Anno 117's water as the island view draws it: the sea (`ocean`), the
// island's rivers and lakes (`inland`), and `Water`, the pass that keeps
// their GPU state and draws them in the kit's Water stage. The shaders are
// surface.wgsl after the kit's frame prelude, and ocean.wgsl for the FFT.
mod inland;
mod ocean;

use anno_core::water::CoastMap;
pub use inland::{RiverWaves, SeasonLook, Seasons};
pub use ocean::{Ocean, Waves};

use crate::sea;
use anno_core::ambient::Sea;
use anno_core::water::{Inland, IslandMapsReader, half};
use anno_formats::a7m::grids::Grids;
use anno_formats::vfs::Install;
use gpui_editor_kit::gpu::LockAny as _;
use gpui_editor_kit::gpu::{self, Stage};
use gpui_editor_kit::heightfield::Heightfield;
use gpui_editor_kit::texture::Image;
use std::sync::{Arc, Mutex};

/// 117's water for one island view: what `show` last set, the textures it
/// draws with (read once with the island), and the GPU state built on the
/// first frame and kept while the view lives.
pub struct Water {
    shown: Mutex<Shown>,
    surface: Mutex<gpu::OnDevice<ocean::Surface>>,
    /// The surface while a thread compiles its pipelines.
    building: Mutex<gpu::OnDevice<std::sync::mpsc::Receiver<ocean::Surface>>>,
    foam: Option<Image>,
    noise: Option<Image>,
    river_waves: Option<Arc<RiverWaves>>,
    /// Duckweed's colour and normal map, for lakes.
    duckweed: Option<[Image; 2]>,
    rain: Option<Arc<sea::Ripples>>,
    /// Egypt's river seasons (globals.xml) and gust textures.
    seasons: Option<Seasons>,
    gusts: Option<Arc<sea::Gusts>>,
    /// Egypt's painted flow map and where its islands lie on it.
    egypt_flow: Option<Image>,
    egypt_map: Option<sea::EgyptMap>,
}

#[derive(Default)]
struct Shown {
    ocean: Option<Ocean>,
    inland: Option<Arc<Inland>>,
    coast: CoastMap,
    maps: IslandMapsReader,
    rain: f32,
    detail: anno_core::ambient::Detail,
    /// The island is in Egypt, whose rivers have seasons.
    egypt: bool,
    /// 0 the dry season to 1 the flood: where the season stands, moving
    /// toward `flood_target` (`SEASON_SECONDS`), and the frame clock it
    /// last moved at.
    flood: f32,
    flood_target: f32,
    flood_clock: Option<f32>,
    /// How high the dry season and the flood raise the rivers.
    river_heights: [f32; 2],
    /// The island's corner in Egypt's session, where the flow map lies.
    egypt_corner: Option<[f32; 2]>,
}

impl Water {
    /// Reads the foam, noise, duckweed, river and rain ripples (about
    /// 60 MB), and Egypt's seasons and gusts from `install`.
    pub fn new(install: &Install) -> Self {
        let seasons = sea::seasons(install);
        let egypt_flow = (seasons.as_ref())
            .and_then(|s| s.flow_map.as_deref())
            .and_then(|p| sea::egypt_flow(install, p));
        Self {
            shown: Mutex::default(),
            surface: Mutex::default(),
            building: Mutex::default(),
            foam: sea::foam(install),
            noise: sea::noise(install),
            river_waves: sea::river_waves(install),
            duckweed: sea::duckweed(install),
            rain: sea::rain_ripples(install),
            seasons,
            gusts: sea::gusts(install),
            egypt_flow,
            egypt_map: sea::EgyptMap::read(install),
        }
    }
}

impl gpu::Pass for Water {
    fn stage(&self) -> Stage {
        Stage::Water
    }

    // The waves step for every view that prepares; one view is the usual
    // case, and a second steps them to the same time.
    fn prepare(&self, frame: &mut gpu::Prepare<'_>) -> bool {
        let mut shown = self.shown.lock_any();
        let shown = &mut *shown;
        if shown.coast.land()
            && let Some(ocean) = &mut shown.ocean
        {
            ocean.coast = shown.coast.coast().cloned();
        }
        if shown.maps.land()
            && let Some(ocean) = &mut shown.ocean
        {
            ocean.maps = shown.maps.maps().cloned();
        }
        let Some(ocean) = &shown.ocean else {
            return false;
        };
        // The island's first coast still measuring: the sea joins with it,
        // rather than breaking against nothing for a few frames.
        if ocean.coast.is_none() && shown.coast.measuring() {
            return false;
        }
        let gpu = Gpu {
            device: frame.device.clone(),
            queue: frame.queue.clone(),
        };
        let mut surface = self.surface.lock_any();
        if surface.get(frame.device).is_none() {
            // Compiling the sea's pipelines took 0.3 s on the first frame
            // of an island: built on a thread, the sea joins a few frames
            // later instead of the window stalling. Again on a new device
            // after a loss.
            let mut building = self.building.lock_any();
            match building.get(frame.device).map(|rx| rx.try_recv()) {
                Some(Ok(built)) => {
                    surface.set(frame.device, built);
                    building.clear();
                }
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return false,
                // Not started, or the build panicked: start (again).
                _ => {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let (device, queue) = (gpu.device.clone(), gpu.queue.clone());
                    let layout = frame.frame_layout.clone();
                    let spawned =
                        std::thread::Builder::new()
                            .name("117 water".into())
                            .spawn(move || {
                                let _ = tx.send(new_surface(&Gpu { device, queue }, &layout));
                            });
                    if spawned.is_ok() {
                        building.set(frame.device, rx);
                    }
                    return false;
                }
            }
        }
        let surface = surface.get_mut(frame.device).expect("built above");
        let (focus, time, timed) = (frame.focus, ocean.time.unwrap_or(frame.time), frame.timed);
        let detail = shown.detail;
        // The season moves straight toward the one asked for, as the
        // game's river height does (UpdateSeasonalRiverHeight).
        let dt = shown.flood_clock.map_or(0., |t| (frame.time - t).max(0.));
        shown.flood_clock = Some(frame.time);
        let step = dt / SEASON_SECONDS;
        shown.flood += (shown.flood_target - shown.flood).clamp(-step, step);
        let season = match (&self.seasons, shown.egypt) {
            (Some(s), true) => s.state(shown.flood, shown.river_heights),
            _ => (SeasonLook::DEFAULT, 0.),
        };
        frame.timed_work("water prepare", |encoder| {
            surface.prepare(
                &gpu,
                encoder,
                ocean,
                (shown.inland.as_ref()).map(|i| ocean::InlandShown {
                    inland: i,
                    waves: self.river_waves.as_ref(),
                    duckweed: self.duckweed.as_ref(),
                    rain: self.rain.as_ref().filter(|_| shown.rain > 0.),
                    raining: shown.rain,
                    global: season.0,
                    river_lift: season.1,
                    egypt: shown
                        .egypt
                        .then(|| self.seasons.as_ref().map_or(0., |s| s.gust_boost)),
                    // TODO: the game accumulates cAccumulatedWind from the
                    // weather's wind; the editor's sea has a fixed direction
                    // and its offshore wind speed, so the gusts drift at that.
                    wind: ocean.wind.map(|w| w * ocean.offshore.wind_speed * time),
                    gusts: self.gusts.as_ref(),
                    flow_file: match (&self.egypt_flow, &self.egypt_map, shown.egypt_corner) {
                        (Some(image), Some(map), Some(corner)) if shown.egypt => {
                            Some((image, corner, map.size.map(|v| v as f32)))
                        }
                        _ => None,
                    },
                }),
                focus,
                time,
                timed,
                detail,
            )
        })
    }

    fn record(&self, frame: &gpu::Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let gpu = Gpu {
            device: frame.device.clone(),
            queue: frame.queue.clone(),
        };
        if let Some(surface) = self.surface.lock_any().get(frame.device) {
            surface.draw(&gpu, pass, frame.scene);
        }
    }

    fn warm(&self, gpu: &gpu::Warm<'_>) {
        warm(gpu);
    }

    // The sea, and the sky's fill under the horizon where it ends
    // (sky/pass.wgsl fs_backdrop), cover all the kit's far sea would: drawn
    // under them it was a full-screen draw no pixel kept.
    fn replaces(&self) -> bool {
        true
    }

    // Also while a new coast or maps are made, so they show once they land.
    fn animating(&self) -> bool {
        let shown = self.shown.lock_any();
        shown.coast.measuring()
            || shown.maps.reading()
            || shown.ocean.as_ref().is_some_and(|o| o.time.is_none())
            || shown.flood != shown.flood_target
    }
}

impl anno_core::water::Water for Water {
    fn show(
        &self,
        sea: Option<&Arc<dyn Sea>>,
        field: &Arc<Heightfield>,
        hold: bool,
        inland: Option<Arc<Inland>>,
        time: Option<f32>,
    ) {
        let mut shown = self.shown.lock_any();
        let Some(sea) = sea.and_then(|s| s.as_any().downcast_ref::<sea::Water>()) else {
            shown.ocean = None;
            shown.inland = None;
            return;
        };
        shown.coast.want(field, hold);
        let coast = shown.coast.coast().cloned();
        let mut ocean = sea.ocean(coast, self.foam.clone(), self.noise.clone(), time);
        ocean.maps = shown.maps.maps().cloned();
        shown.ocean = Some(ocean);
        shown.inland = inland;
    }

    fn island_maps(&self, grids: Option<&Arc<Grids>>) {
        self.shown.lock_any().maps.want(grids);
    }

    fn rain(&self, rain: f32) {
        self.shown.lock_any().rain = rain;
    }

    fn detail(&self, detail: anno_core::ambient::Detail) {
        self.shown.lock_any().detail = detail;
    }

    // Egypt is the only 117 province with seasons.
    fn season(&self, path: &str, river_heights: Option<[f32; 2]>, flood: f32) {
        let corner = (self.egypt_map.as_ref())
            .filter(|_| river_heights.is_some())
            .and_then(|m| m.corner(path));
        let mut shown = self.shown.lock_any();
        shown.egypt_corner = corner;
        // An island opens in its season; only a change moves.
        if !shown.egypt {
            shown.flood = flood;
        }
        shown.egypt = river_heights.is_some();
        shown.river_heights = river_heights.unwrap_or(RIVER_HEIGHTS);
        shown.flood_target = flood;
    }
}

/// Seconds the editor takes from one season to the other. The game's
/// `RiverChangeDuration` is 480 s; a view that took that long would look
/// stuck, so the editor runs the same straight move 120 times faster.
const SEASON_SECONDS: f32 = 4.;

/// assets.xml's DrySeason (150490) and FloodSeason (150491) `RiverHeight`,
/// for when the assets aren't read yet.
pub const RIVER_HEIGHTS: [f32; 2] = [0.01, 0.5];

/// How high the dry season and the flood raise every River object: the
/// `RiverHeight` of the `Season` assets that set `IsRiverHeightAdjusted`,
/// the `FloodSeason` one the flood's. `None` without both.
pub fn river_heights(assets: &anno_formats::assets::Assets) -> Option<[f32; 2]> {
    let mut out = [None; 2];
    for a in assets.iter().filter(|a| a.template == "Season") {
        let values = assets.values(a.guid);
        let leaf = |name: &str| {
            let path = format!("Values/Season/{name}");
            (values.iter())
                .find(|(p, _)| *p == path)
                .map(|(_, t)| t.as_str())
        };
        if leaf("IsRiverHeightAdjusted") != Some("1") {
            continue;
        }
        // The dry season (150490) leaves SeasonType at the engine's default.
        let slot = usize::from(leaf("SeasonType") == Some("FloodSeason"));
        out[slot] = leaf("RiverHeight").and_then(|h| h.parse::<f32>().ok());
    }
    Some([out[0]?, out[1]?])
}

/// The sea's GPU state over the module and pipelines `compiled` built.
fn new_surface(gpu: &Gpu, frame_layout: &wgpu::BindGroupLayout) -> ocean::Surface {
    let shader = compiled(gpu, frame_layout);
    ocean::Surface::new(gpu, &shader, frame_layout, gpu::HDR_FORMAT)
}

/// The water's shader module, with every pipeline over it built: once per
/// device (0.9 s cold). The module too, not only the pipelines: parsing
/// and validating it took each new island view 0.1-0.3 s.
fn compiled(gpu: &Gpu, frame_layout: &wgpu::BindGroupLayout) -> wgpu::ShaderModule {
    gpu::per_device(&gpu.device, "117 water", || {
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("117 water"),
                source: wgpu::ShaderSource::Wgsl(
                    gpu::ssr_module(&gpu.device, &[include_str!("water/surface.wgsl")]).into(),
                ),
            });
        // A surface made and let go builds the sea's, rivers' and physical
        // waves' pipelines (each `gpu::per_device`); the simulation the
        // first frame's waves step with, its own.
        drop(ocean::Surface::new(
            gpu,
            &shader,
            frame_layout,
            gpu::HDR_FORMAT,
        ));
        drop(ocean::Simulation::new(gpu));
        shader
    })
}

/// Compiles the sea's pipelines before an island needs them.
pub fn warm(warm: &gpu::Warm<'_>) {
    let gpu = Gpu {
        device: warm.device.clone(),
        queue: warm.queue.clone(),
    };
    compiled(&gpu, warm.frame_layout);
}

/// The device and queue the water's GPU code takes, cloned from the frame
/// (both are handles).
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

fn layout_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BindingType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty,
        count: None,
    }
}

fn uniform(dynamic: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: dynamic,
        min_binding_size: None,
    }
}

fn storage(read_only: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Storage { read_only },
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

/// Identity of a shared value, for "did this change" checks.
fn key<T: ?Sized>(arc: &Arc<T>) -> usize {
    Arc::as_ptr(arc) as *const () as usize
}

fn image_key(image: &Image) -> usize {
    match image {
        Image::Rgba(t) => key(t),
        Image::Compressed(c) => key(c),
        Image::Releasable(r) => key(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::water::Coast;
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{
        Batch, Instances, Light, Material, Mesh, MeshData, Offscreen, Renderer, Scene, Tone,
    };
    use gpui_editor_kit::math::{self, Vec3};

    impl Water {
        /// Shows `ocean` as given, without a preset or an install.
        fn bare(ocean: Ocean) -> Self {
            let water = Self {
                shown: Mutex::default(),
                surface: Mutex::default(),
                building: Mutex::default(),
                foam: None,
                noise: None,
                river_waves: None,
                duckweed: None,
                rain: None,
                seasons: None,
                gusts: None,
                egypt_flow: None,
                egypt_map: None,
            };
            water.shown.lock_any().ocean = Some(ocean);
            water
        }
    }

    // Well inland, under ground standing above the sea, the coast map cuts
    // the sea out (the game's LakeCutout over marsh), but only where ground
    // lies behind it: seen from under land that runs off the island's edge,
    // as DLC 3's quadrants do, the sea shows instead of the empty sky.
    #[test]
    fn inland_sea_is_cut_only_over_ground() {
        let mut target = Offscreen::new(160, 120).unwrap();
        let inland = Arc::new(Coast {
            width: 4,
            height: 4,
            origin: [-40., -40.],
            size: [80., 80.],
            texels: vec![[-100., 0., 0., 0.]; 16],
            depth_scale: 5.,
        });
        let floor = Batch::new(
            Arc::new(
                Mesh::new(MeshData {
                    positions: vec![
                        [-40., -0.5, -40.],
                        [40., -0.5, -40.],
                        [40., -0.5, 40.],
                        [-40., -0.5, 40.],
                    ],
                    normals: vec![[0., 1., 0.]; 4],
                    colors: vec![0xd8d0b8; 4],
                    indices: vec![0, 1, 2, 0, 2, 3],
                    material: Material::Lit,
                    ..Default::default()
                })
                .unwrap(),
            ),
            Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![Some(1)],
            }),
        );
        let world = Extent {
            center: [0.; 3],
            size: 60.,
        };
        let camera = Camera::default();
        let mut draw = |ground: bool, coast: Option<Option<Arc<Coast>>>| {
            let renderer = Renderer::default();
            renderer.set_scene(&Scene {
                batches: if ground { vec![floor.clone()] } else { vec![] },
                light: Light {
                    sun: [0.3, 1., 0.2],
                    sun_color: [3.; 3],
                    sky: [0.8, 0.9, 1.1],
                    ground: [0.3; 3],
                    ..Default::default()
                },
                tone: Some(Tone::default()),
                passes: coast
                    .into_iter()
                    .map(|coast| {
                        Arc::new(Water::bare(Ocean {
                            time: Some(0.),
                            coast,
                            ..ocean::tests::roman()
                        })) as _
                    })
                    .collect(),
                ..Default::default()
            });
            let pixels = target.render(&renderer, camera, world).unwrap();
            let s = camera.project([0.; 3], target.bounds(), world);
            let i = (f32::from(s.y) as usize * 160 + f32::from(s.x) as usize) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let off = |a: [u8; 3], b: [u8; 3]| (0..3).map(|k| a[k].abs_diff(b[k])).max().unwrap();
        let empty = draw(false, None);
        let sea = draw(false, Some(Some(inland.clone())));
        assert!(off(sea, empty) > 8, "sea {sea:?}, nothing {empty:?}");
        let bare = draw(true, None);
        let cut = draw(true, Some(Some(inland)));
        assert!(off(cut, bare) <= 2, "cut {cut:?}, ground {bare:?}");
        let open = draw(true, Some(None));
        assert!(off(open, bare) > 8, "open sea {open:?}, ground {bare:?}");
    }

    // Plugged into a scene's Water stage, the sea covers a seabed with
    // water that reads blue, darker where the floor lies deeper, and its
    // surface moves with wave time.
    #[test]
    fn the_ocean_tints_by_depth_and_moves_with_time() {
        let mut target = Offscreen::new(320, 240).unwrap();
        // Sloping from 0.2 units under water at -x to 6 under at +x.
        let floor = MeshData {
            positions: vec![
                [-40., -0.2, -40.],
                [40., -6., -40.],
                [40., -6., 40.],
                [-40., -0.2, 40.],
            ],
            normals: vec![[0., 1., 0.]; 4],
            colors: vec![0xd8d0b8; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            material: Material::Lit,
            ..Default::default()
        };
        let floor = Batch::new(
            Arc::new(Mesh::new(floor).unwrap()),
            Arc::new(Instances {
                matrices: vec![math::IDENTITY],
                ids: vec![Some(1)],
            }),
        );
        let renderer = Renderer::default();
        let light = Light {
            sun: [0.3, 1., 0.2],
            sun_color: [3.; 3],
            sky: [0.8, 0.9, 1.1],
            ground: [0.3; 3],
            ..Default::default()
        };
        let at_time = |time: f32| Scene {
            batches: vec![floor.clone()],
            light,
            tone: Some(Tone::default()),
            passes: vec![Arc::new(Water::bare(Ocean {
                time: Some(time),
                ..ocean::tests::roman()
            }))],
            ..Default::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 60.,
        };
        let camera = Camera::default();
        renderer.set_scene(&at_time(0.));
        let first = target.render(&renderer, camera, world).unwrap();
        let at = |pixels: &[u8], p: Vec3| {
            let s = camera.project(p, target.bounds(), world);
            let (x, y) = (f32::from(s.x) as usize, f32::from(s.y) as usize);
            let i = (y * 320 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let (shallow, deep) = (at(&first, [-25., 0., 0.]), at(&first, [25., 0., 0.]));
        assert!(deep[2] > deep[0], "deep water is blue {deep:?}");
        let luma = |c: [u8; 3]| u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2]);
        assert!(
            luma(shallow) > luma(deep) + 30,
            "shallow {shallow:?}, deep {deep:?}"
        );
        renderer.set_scene(&at_time(2.));
        let later = target.render(&renderer, camera, world).unwrap();
        let moved = first
            .chunks(4)
            .zip(later.chunks(4))
            .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 3))
            .count();
        assert!(moved > 500, "{moved} pixels moved");
    }
}
