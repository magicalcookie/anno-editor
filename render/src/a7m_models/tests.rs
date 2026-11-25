use super::*;
use gpui_editor_kit::camera::{Camera, Extent};
use gpui_editor_kit::gpu::{
    Batch, ControlMap, Decal, Fog, GroundTint, HeightMap, Instances, Light, Material as Kind, Mesh,
    MeshSplatLayer, Offscreen, Piece, Renderer, Scene, Shadows, Terrain, TerrainAdapt, Tone,
    WorldTiling, adapting, cube,
};
use gpui_editor_kit::heightfield::{Grid, Heightfield, Rect};
use gpui_editor_kit::math::{self, Mat4, Vec3};
use gpui_editor_kit::texture::{Block, Compressed, Texture};

fn solid(rgba: [u8; 4]) -> Image {
    Image::from(Texture::solid(rgba))
}

/// A horizontal square at height `y`, `half` units from the origin each
/// way, facing up, with UVs 0..1 across it.
fn quad(half: f32, y: f32, material: Kind) -> MeshData {
    let corners = [[-1., -1.], [1., -1.], [1., 1.], [-1., 1.]];
    MeshData {
        positions: corners.map(|[x, z]| [x * half, y, z * half]).to_vec(),
        uvs: (corners.map(|[x, z]| [(x + 1.) / 2., (z + 1.) / 2.])).to_vec(),
        normals: vec![[0., 1., 0.]; 4],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..Default::default()
    }
}

/// A flat `Flat` sheet `half` units each way at height `y`, `n` cells a
/// side, so a bent sheet follows the ground between its vertices.
fn sheet(half: f32, y: f32, n: u32, color: u32) -> MeshData {
    let mut data = MeshData {
        material: Kind::Flat,
        ..Default::default()
    };
    for z in 0..=n {
        for x in 0..=n {
            let at = |k: u32| -half + 2. * half * k as f32 / n as f32;
            data.positions.push([at(x), y, at(z)]);
            data.colors.push(color);
        }
    }
    for z in 0..n {
        for x in 0..n {
            let i = z * (n + 1) + x;
            (data.indices).extend([i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
        }
    }
    data
}

/// A BC1 block of one opaque colour, quantized to 5:6:5.
fn bc1(rgb: [u8; 3]) -> Vec<u8> {
    let [r, g, b] = rgb.map(u16::from);
    let c0 = (r >> 3) << 11 | (g >> 2) << 5 | b >> 3;
    // c0 > c1 picks four-colour mode, where index 0 is c0, opaque.
    let mut block = c0.to_le_bytes().to_vec();
    block.extend([0; 6]);
    block
}

/// A BC1 image whose every block in level `l` is `block(l)`.
fn bc1_mips(width: u32, height: u32, levels: usize, block: impl Fn(usize) -> Vec<u8>) -> Image {
    let format = Block::Bc1;
    let mips = (0..levels)
        .map(|l| {
            let (w, h) = ((width >> l).max(1), (height >> l).max(1));
            block(l).repeat(format.size(w, h) / format.bytes())
        })
        .collect();
    let image = Compressed {
        block: format,
        width,
        height,
        mips,
        srgb: false,
    };
    image.validate().unwrap();
    image.into()
}

/// A flat `side`² heightfield of zeros at 0.5 spacing, centred on the origin.
fn flat_field(side: usize) -> Heightfield {
    let half = (side - 1) as f32 * 0.25;
    let grid = Grid {
        width: side,
        depth: side,
        spacing: 0.5,
        origin: [-half; 2],
    };
    Heightfield::new(grid, vec![0.; side * side])
}

/// Lighting that leaves albedo unchanged, so colours can be checked exactly.
fn neutral() -> Light {
    Light {
        sun_color: [0.; 3],
        sky: [1.; 3],
        ground: [1.; 3],
        ..Default::default()
    }
}

fn near(actual: [u8; 3], expected: [u8; 3], tolerance: u8) {
    assert!(
        (actual.iter().zip(expected)).all(|(a, e)| a.abs_diff(e) <= tolerance),
        "{actual:?} != {expected:?}"
    );
}

/// FNV-1a over a frame's bytes, as the kit's lit scene digests them.
fn digest(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// The kit's lit scene as it stood before the dye and night masks and
/// rooms left the kit, those cubes now this family's: placements of every
/// kind, toned, fogged and shadowed over a ground that tints them, a
/// mip-coloured floor at a slant, a cutout card, a splat-mapped slab, cubes
/// with a dye mask, steady glow, a night mask and a window's room, a
/// normal- and surface-mapped quad, a world-tiled quad under the ground's
/// tint, a sheet bent over the ramp's foot, and a glowing decal on the
/// floor. Returns the renderer, camera, world, and per pickable row a point
/// it shows at.
fn lit_scene() -> (Renderer, Camera, Extent, Vec<(u32, Vec3)>) {
    let mut field = flat_field(33);
    let map = Arc::new(HeightMap::new(&field).unwrap());
    let rise = Rect {
        x0: 20,
        z0: 0,
        x1: 33,
        z1: 33,
    };
    for z in rise.z0..rise.z1 {
        for x in rise.x0..rise.x1 {
            let i = field.grid.index(x, z);
            field.heights[i] = 1.5;
        }
    }
    map.update(rise, &field.heights);
    // Colours that change across the ground, so a tinted surface shows
    // where it stands.
    let colors: Vec<u8> = (0..33 * 33)
        .flat_map(|i| [60 + 5 * (i % 33) as u8, 200 - 4 * (i / 33) as u8, 90, 255])
        .collect();
    let renderer = Renderer::with_families(families());
    renderer.set_scene(&Scene {
        terrain: Some(Arc::new(Terrain {
            heights: map,
            colors: Arc::new(ControlMap::new(33, 33, colors).unwrap()),
            color_gain: 1.,
            tints: true,
            material: Kind::Lit,
            texture: None,
        })),
        light: Light {
            sun: [0.4, 1., 0.3],
            sun_color: [2.5; 3],
            sky: [0.5, 0.6, 0.8],
            ground: [0.3, 0.25, 0.2],
            shadows: Some(Shadows::default()),
            fog: Some(Fog {
                color: [0.6, 0.7, 0.8],
                start: 8.,
                end: 40.,
            }),
        },
        tone: Some(Tone::default()),
        glow: scene_glow(&ModelGlow {
            tint: [0.1, 0.2, 1.],
            night: 0.4,
            night_color: [1., 0.8, 0.5],
            night_keep: 0.5,
            ..Default::default()
        }),
        ..Default::default()
    });
    // Each mip its own colour: a footprint picked differently shows.
    let mut floor = sheet(3., 0.05, 12, 0xffffff);
    floor.material = Kind::Lit;
    floor.normals = vec![[0., 1., 0.]; floor.positions.len()];
    floor.uvs = (floor.positions.iter())
        .map(|p| [p[0] * 2., p[2] * 2.])
        .collect();
    let mips = [
        [220, 40, 40],
        [40, 200, 40],
        [40, 60, 220],
        [220, 200, 40],
        [200, 40, 200],
        [40, 200, 200],
        [250, 250, 250],
    ];
    floor.texture = Some(bc1_mips(64, 64, 7, move |l| bc1(mips[l])));
    // Solid on its right half.
    let card = MeshData {
        positions: vec![
            [-0.6, 0., 0.],
            [0.6, 0., 0.],
            [0.6, 1.2, 0.],
            [-0.6, 1.2, 0.],
        ],
        uvs: vec![[0., 1.], [1., 1.], [1., 0.], [0., 0.]],
        indices: vec![0, 1, 2, 0, 2, 3],
        texture: Some(Image::from(
            Texture::from_rgba(2, 1, vec![0, 0, 0, 0, 40, 200, 60, 255]).unwrap(),
        )),
        ..Default::default()
    };
    // Two layers, the first with a normal map, bending the mesh's own.
    let control: Vec<u8> = (0..8)
        .flat_map(|x| {
            if x < 4 {
                [255, 0, 0, 0]
            } else {
                [0, 255, 0, 0]
            }
        })
        .collect();
    let mut slab = quad(1., 0., Kind::Lit);
    slab.texture = Some(solid([128, 128, 128, 255]));
    slab.ignore_alpha = true;
    slab.normal_map = Some(solid([128, 128, 255, 90]));
    slab.splat = Some(Arc::new(mesh_splat(
        Texture::from_rgba(8, 1, control).unwrap().into(),
        [
            Some(MeshSplatLayer {
                albedo: solid([200, 40, 40, 255]),
                normal: Some(solid([150, 110, 255, 200])),
                tiling: 3.,
            }),
            Some(MeshSplatLayer {
                albedo: solid([40, 40, 200, 128]),
                normal: None,
                tiling: 2.,
            }),
            None,
            None,
        ],
        &Splat {
            map: String::new(),
            layers: Default::default(),
            mix_albedo: true,
            mix_normal: true,
            scale_invariant: true,
            world_tiling: false,
        },
    )));
    let mut white = cube(0xffffff, Kind::Lit).data().clone();
    white.texture = Some(solid([255; 4]));
    let mut tinted = white.clone();
    join(
        &mut tinted,
        Maps {
            dye: Some(solid([255, 0, 0, 255])),
            glow: None,
        },
    );
    let mut steady = white.clone();
    steady.texture = Some(solid([128, 128, 128, 255]));
    steady.normal_map = Some(solid([128, 128, 180, 0]));
    join(
        &mut steady,
        Maps {
            dye: None,
            glow: Some(Glow {
                color: [0.5, 0.4, 0.2],
                steady: true,
                night_map: None,
                night_gain: 1.,
                room: None,
            }),
        },
    );
    let lit = Glow {
        color: [1., 0.9, 0.6],
        steady: false,
        night_map: Some(solid([255, 200, 120, 255])),
        night_gain: 1.,
        room: None,
    };
    let mut night = white.clone();
    join(
        &mut night,
        Maps {
            dye: None,
            glow: Some(lit.clone()),
        },
    );
    let mut window = white.clone();
    join(
        &mut window,
        Maps {
            dye: None,
            glow: Some(Glow {
                room: Some(Room {
                    map: solid([60, 200, 90, 255]),
                    depth: 0.2,
                    scale: 2.,
                    offset: [0.; 2],
                }),
                ..lit
            }),
        },
    );
    let mut mapped = quad(0.9, 0., Kind::Lit);
    mapped.texture = Some(solid([200, 200, 200, 255]));
    mapped.normal_map = Some(solid([170, 128, 255, 220]));
    mapped.surface_map = Some(solid([255, 0, 0, 160]));
    let mut tiled = quad(1., 0., Kind::Lit);
    tiled.texture = Some(Image::from(
        Texture::from_rgba(2, 1, vec![255, 120, 40, 255, 40, 160, 255, 255]).unwrap(),
    ));
    tiled.ignore_alpha = true;
    tiled.world_tiling = Some(WorldTiling {
        scale: 1.5,
        diagonal: true,
    });
    tiled.terrain_tint = 1.;
    // Rigid 0: every vertex follows the ground.
    let mut bent = sheet(1.5, 0.3, 16, 0x00d0a060);
    bent.material = Kind::Lit;
    bent.terrain_adapt = Some(TerrainAdapt::Relative);
    bent.terrain_tint = 0.5;
    let mut decal = quad(
        1.2,
        0.,
        Kind::Decal(Decal {
            opacity: 0.7,
            bias: 0.01,
            grazing: true,
            ground: false,
            edge: None,
        }),
    );
    decal.texture = Some(solid([230, 220, 60, 255]));
    decal.normal_map = Some(solid([128, 128, 200, 128]));
    decal.emissive = Some([0.4, 0.3, 0.1]);
    let meshes = [
        floor, card, slab, tinted, steady, night, window, mapped, tiled, bent, decal,
    ];
    renderer.set_pieces(
        (meshes.into_iter())
            .map(|m| Piece {
                levels: vec![(Arc::new(Mesh::new(m).unwrap()), 0.)],
                impostor: None,
                pose: None,
            })
            .collect(),
    );
    // Turned 0.6 about y.
    let (sin, cos) = 0.6f32.sin_cos();
    let mut turned = math::translate([-1., 0., -1.]);
    (turned[0], turned[2], turned[8], turned[10]) = (cos, -sin, sin, cos);
    let cubes = [-4.5, -3.3, -2.1, -0.9];
    let mut rows = vec![
        math::translate([-3., 0., 2.]),
        turned,
        math::translate([0.6, 0.1, 3.5]),
    ];
    rows.extend(cubes.map(|x| math::translate([x, 0.5, -3.])));
    rows.extend([
        // On the ramp's top.
        math::translate([3.6, 1.53, 1.5]),
        math::translate([3.6, 1.53, -2.5]),
        // Across its foot: half on the low ground, half bent up it.
        adapting(math::translate([2., 0., -1.]), true, false, 0.),
        // On the floor, which the bias lifts it off.
        math::translate([-3., 0.05, 2.]),
    ]);
    let pieces: Vec<u32> = (0..rows.len() as u32).collect();
    renderer.set_placements(&pieces, &rows);
    // The card on its solid half; the floor beside the decal, which is
    // never picked.
    let mut centres = vec![
        (0, [-5., 0.05, 4.]),
        (1, [-1. + 0.3 * cos, 0.6, -1. - 0.3 * sin]),
        (2, [0.6, 0.1, 3.5]),
    ];
    centres.extend((3..).zip(cubes).map(|(row, x)| (row, [x, 1., -3.])));
    centres.extend([
        (7, [3.6, 1.53, 1.5]),
        (8, [3.6, 1.53, -2.5]),
        (9, [1., 0.3, -1.]),
    ]);
    let world = Extent {
        center: [0.; 3],
        size: 14.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.2,
        tilt: 0.85,
        yaw: 0.4,
        ..Camera::perspective()
    };
    (renderer, camera, world, centres)
}

/// `lit_scene`'s frame by adapter and driver: the kit's lit scene digest
/// as recorded before the dye and night masks and rooms moved out of the
/// kit into this family (through the visibility buffer). Equal since: the
/// move left their pixels alone. An adapter not listed here only prints
/// its digest.
const LIT_SCENE_DIGESTS: &[(&str, u64)] = &[(
    "NVIDIA GeForce RTX 4080 SUPER (620.70.06)",
    0xfec3_2a4d_72a4_3887,
)];

// Through this family the kit's old lit scene draws as it did when the
// variants were the kit's, and picks the same placements.
#[test]
fn the_lit_scene_draws_the_kits_recorded_frame() {
    let (renderer, camera, world, centres) = lit_scene();
    let mut target = Offscreen::new(320, 240).unwrap();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    for (row, centre) in centres {
        let at = camera.project(centre, target.bounds(), world);
        let hit = target.pick(&renderer, at).unwrap();
        assert_eq!(hit.and_then(|h| h.placement), Some(row), "row {row}");
    }
    let info = Offscreen::new(8, 8).unwrap().device().adapter_info();
    let adapter = format!("{} ({})", info.name, info.driver_info);
    let digest = digest(&pixels);
    eprintln!("(\"{adapter}\", {digest:#x}),");
    let Some((_, recorded)) = LIT_SCENE_DIGESTS.iter().find(|(a, _)| *a == adapter) else {
        eprintln!("no digest recorded for {adapter}: its frame goes unchecked");
        return;
    };
    assert_eq!(digest, *recorded, "on {adapter}");
}

// Four cubes under neutral light: a white one fully dyed blue by its mask,
// a grey one whose normal map's blue lights it at half its colour, a white
// one with a night mask, lit red by night and plain by day, and one all
// window, showing its green room by day and the room's light by night.
// Placements and plain batches draw them alike.
#[test]
fn dye_and_night_masks_and_rooms_draw_as_placements_and_batches() {
    let cubes = || {
        let mut white = cube(0xffffff, Kind::Lit).data().clone();
        white.texture = Some(solid([255; 4]));
        let mut tinted = white.clone();
        join(
            &mut tinted,
            Maps {
                dye: Some(solid([255, 0, 0, 255])),
                glow: None,
            },
        );
        let mut steady = white.clone();
        steady.texture = Some(solid([128, 128, 128, 255]));
        steady.normal_map = Some(solid([128, 128, 255, 0]));
        let glow = Glow {
            color: [0.5; 3],
            steady: true,
            night_map: None,
            night_gain: 1.,
            room: None,
        };
        join(
            &mut steady,
            Maps {
                dye: None,
                glow: Some(glow),
            },
        );
        let lit = Glow {
            color: [1.; 3],
            steady: false,
            night_map: Some(solid([255; 4])),
            night_gain: 1.,
            room: None,
        };
        let mut night = white.clone();
        join(
            &mut night,
            Maps {
                dye: None,
                glow: Some(lit.clone()),
            },
        );
        let mut window = white;
        let room = Room {
            map: solid([0, 255, 0, 255]),
            depth: 0.2,
            scale: 2.,
            offset: [0.; 2],
        };
        join(
            &mut window,
            Maps {
                dye: None,
                glow: Some(Glow {
                    room: Some(room),
                    ..lit
                }),
            },
        );
        [tinted, steady, night, window].map(|d| Arc::new(Mesh::new(d).unwrap()))
    };
    let at = [-1.8, -0.6, 0.6, 1.8].map(|x| math::translate([x, 0., 0.]));
    let glow = |night: f32| {
        scene_glow(&ModelGlow {
            tint: [0., 0., 1.],
            night,
            night_color: [1., 0., 0.],
            night_keep: 0.,
            ..Default::default()
        })
    };
    let draw = |placed: bool, glow: gpu::Glow| {
        let meshes = cubes();
        let (renderer, batches) = match placed {
            true => {
                let renderer = Renderer::with_families(families());
                renderer.set_pieces(
                    (meshes.iter())
                        .map(|m| Piece {
                            levels: vec![(m.clone(), 0.)],
                            impostor: None,
                            pose: None,
                        })
                        .collect(),
                );
                renderer.set_placements(&[0, 1, 2, 3], &at);
                (renderer, Vec::new())
            }
            false => {
                let batches = (meshes.iter().zip(at))
                    .map(|(m, matrix)| Batch {
                        mesh: m.clone(),
                        instances: Arc::new(Instances {
                            matrices: vec![matrix],
                            ids: vec![None],
                        }),
                    })
                    .collect();
                (Renderer::with_families(families()), batches)
            }
        };
        renderer.set_scene(&Scene {
            batches,
            light: neutral(),
            glow,
            ..Default::default()
        });
        let mut target = Offscreen::new(320, 240).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 8.,
        };
        let camera = Camera {
            fov: 0.9,
            zoom: 2.,
            ..Camera::perspective()
        };
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let bounds = target.bounds();
        at.map(|m: Mat4| {
            target.pixel(
                &pixels,
                camera.project(math::translation(&m), bounds, world),
            )
        })
    };
    for placed in [true, false] {
        let [tinted, steady, night, room] = draw(placed, glow(0.));
        near(tinted, [0, 0, 255], 1);
        // Shading gives way: 128/255 times 0.5, nothing of the grey lit.
        near(steady, [64, 64, 64], 2);
        near(night, [255, 0, 0], 1);
        // The room's grey (0.7154 of its green) in the night colour.
        near(room, [182, 0, 0], 2);
        let [_, _, day, room] = draw(placed, glow(1.));
        near(day, [255; 3], 1);
        near(room, [0, 255, 0], 1);
    }
}

// A grey slab taking the island's tint whole: through the family's
// registry it takes the tint as the a7m ground decodes it, a mid grey
// texel ×1 (2 · 128/255, to the 2.2), where the kit alone would read the
// texel's RGB as the multiplier and halve it.
#[test]
fn models_take_the_island_tint_as_the_ground_decodes_it() {
    let slab = MeshData {
        texture: Some(solid([200, 200, 200, 255])),
        terrain_tint: 1.,
        ..quad(3., 0., Kind::Lit)
    };
    let draw = |renderer: Renderer| {
        let map = ControlMap::new(4, 4, [128, 128, 128, 255].repeat(16)).unwrap();
        renderer.set_scene(&Scene {
            batches: vec![Batch::new(
                Arc::new(Mesh::new(slab.clone()).unwrap()),
                Arc::new(Instances {
                    matrices: vec![math::IDENTITY],
                    ids: vec![None],
                }),
            )],
            light: neutral(),
            ground_tint: Some(GroundTint {
                map: Arc::new(map),
                origin: [-3., -3.],
                size: [6., 6.],
            }),
            ..Default::default()
        });
        let mut target = Offscreen::new(64, 64).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 7.,
        };
        let camera = Camera::default();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        target.pixel(&pixels, camera.project([0.; 3], target.bounds(), world))
    };
    near(draw(Renderer::with_families(families())), [202; 3], 2);
    near(draw(Renderer::default()), [100; 3], 2);
}

// 1800 glows by its texture times the cfg's emissive colour, gamma to
// linear; 117's G-buffer keeps the albedo and folds the colour's grey,
// over 4, into the night mask.
#[test]
fn glow_takes_each_games_colour_rule() {
    let glow = anno_formats::model::Glow {
        color: [2.; 3],
        night_map: None,
        interior: None,
    };
    let (color, gain) = emissive(&glow, [0.5; 3], false);
    assert!((color[0] - 2f32.powf(2.2)).abs() < 1e-4);
    assert_eq!(gain, 1.);
    let (color, gain) = emissive(&glow, [0.5; 3], true);
    assert_eq!(color, [0.5; 3]);
    assert!((gain - 2f32.powf(2.2) / 4.).abs() < 1e-3);
}

// A mesh joins the family only with a dye or night mask; one that only
// glows steadily stays the kit's with its colour as emissive, and what it
// joins with reads back.
#[test]
fn only_dye_and_night_masks_join_the_family() {
    let base = || MeshData {
        texture: Some(solid([255; 4])),
        ..quad(1., 0., Kind::Lit)
    };
    let steady = Glow {
        color: [0.25; 3],
        steady: true,
        night_map: None,
        night_gain: 0.5,
        room: None,
    };
    let mut data = base();
    join(
        &mut data,
        Maps {
            dye: None,
            glow: Some(steady.clone()),
        },
    );
    assert_eq!((worn(&data), data.emissive), (None, Some([0.25; 3])));
    let mut data = base();
    join(
        &mut data,
        Maps {
            dye: None,
            glow: Some(Glow {
                night_map: Some(solid([255; 4])),
                room: Some(Room {
                    map: solid([0, 255, 0, 255]),
                    depth: 0.2,
                    scale: 2.,
                    offset: [0.5, 0.],
                }),
                ..steady
            }),
        },
    );
    assert_eq!(
        worn(&data),
        Some(Worn {
            dye: false,
            steady: true,
            night: true,
            night_gain: 0.5,
            room: Some([0.2, 2., 0.5, 0.]),
        })
    );
    assert_eq!(
        (data.slots.len(), data.slot_repeats.clone()),
        (2, vec![1., 2.])
    );
    assert_eq!(data.emissive, None);
}
