// Tests of what a placement row's bottom row (gpu::adapting) must leave
// alone, of surface families, and of the kit's
// defaults a scene's passes warm or replace (the far sea).
use super::*;

/// Ground rising 1.5 on x >= 2, for placements that bend to it.
fn ramp() -> Arc<Terrain> {
    let mut field = flat_field(33);
    let map = Arc::new(HeightMap::new(&field).unwrap());
    let rise = Rect {
        x0: 20,
        z0: 0,
        x1: 33,
        z1: 33,
    };
    raise(&mut field, &map, rise, 1.5);
    Arc::new(Terrain {
        heights: map,
        colors: Arc::new(ControlMap::new(33, 33, [90, 80, 60, 255].repeat(33 * 33)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Flat,
        texture: None,
    })
}

// A drag on the GPU moves rows that may bend to the terrain as it moves any
// other: the dragged frame is the one the landed table draws. Their bottom
// row carries adaption, not a projective row; read as part of the product
// it added the drag's translation to each column and sheared them.
#[test]
fn a_gpu_drag_moves_terrain_adapting_rows_rigidly() {
    let world = Extent {
        center: [0.; 3],
        size: 14.,
    };
    let camera = Camera {
        tilt: 0.7,
        yaw: 0.3,
        ..Default::default()
    };
    // A sheet that bends over the ramp's foot once dragged there, and a
    // rigid cube whose row carries all three adaption words.
    let starts = [
        math::translate([-2., 0., 1.]),
        math::translate([-1., 0.5, 3.]),
    ];
    let adapt = |k: usize, m: Mat4| match k {
        0 => adapting(m, true, false, 0.),
        _ => adapting(m, true, true, 0.5),
    };
    let delta = math::translate([3.5, 0., -2.5]);
    let draw = |rows: [Mat4; 2], drag: Option<Mat4>| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            terrain: Some(ramp()),
            light: neutral(),
            ..Default::default()
        });
        let mut bent = sheet(1.5, 0.3, 16, 0x00d0a060);
        bent.terrain_adapt = Some(TerrainAdapt::Relative);
        renderer.set_pieces(vec![
            Piece {
                levels: vec![(Arc::new(Mesh::new(bent).unwrap()), 0.)],
                impostor: None,
                pose: None,
            },
            Piece {
                levels: vec![(Arc::new(cube(0x3060ff, Material::Flat)), 0.)],
                impostor: None,
                pose: None,
            },
        ]);
        renderer.set_placements(&[0, 1], &rows);
        // Marked as the editor marks its selection: the sheet's marker
        // rides its row, so both frames show it at the same place.
        let ring = Marker {
            position: [0.; 3],
            radius: 5.,
            color: 0x00ff00,
            shape: Shape::Circle,
        };
        renderer.set_selection(vec![0, 1], &[(0, [0.; 3])], &ring);
        renderer.drag_selection(drag);
        let mut target = Offscreen::new(320, 240).unwrap();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        // The sheet's far corner over the ramp, and the cube.
        let picks = [[2.8, 1.8, -2.8], [2.5, 2., 0.5]].map(|p: Vec3| {
            let at = camera.project(p, target.bounds(), world);
            (target.pick(&renderer, at).unwrap()).and_then(|h| h.placement)
        });
        (pixels, picks)
    };
    let landed = std::array::from_fn(|k| adapt(k, math::multiply(&delta, &starts[k])));
    let (expected, expected_picks) = draw(landed, None);
    assert_eq!(expected_picks, [Some(0), Some(1)], "landed");
    let dragged = std::array::from_fn(|k| adapt(k, starts[k]));
    let (pixels, picks) = draw(dragged, Some(delta));
    dump("drag_landed", &expected, 320, 240);
    dump("drag_dragged", &pixels, 320, 240);
    assert_eq!(picks, expected_picks, "dragged");
    let (off, most) = difference(&pixels, &expected);
    assert_eq!(
        (off, most),
        (0, 0),
        "the dragged rows draw where the landed ones do"
    );
}

// A drag moves its rows whether or not anything is marked: the select pass
// once needed a marker buffer to bind and skipped the drag without one.
#[test]
fn a_gpu_drag_without_markers_moves_its_rows() {
    let world = Extent {
        center: [0.; 3],
        size: 14.,
    };
    let camera = Camera {
        tilt: 0.7,
        yaw: 0.3,
        ..Default::default()
    };
    let delta = math::translate([3., 0., -2.]);
    let draw = |row: Mat4, drag: Option<Mat4>| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(cube(0x3060ff, Material::Flat)), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&[0], &[row]);
        let ring = Marker {
            position: [0.; 3],
            radius: 5.,
            color: 0x00ff00,
            shape: Shape::Circle,
        };
        renderer.set_selection(vec![0], &[], &ring);
        renderer.drag_selection(drag);
        let mut target = Offscreen::new(320, 240).unwrap();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = camera.project([2., 0.5, -2.], target.bounds(), world);
        let pick = (target.pick(&renderer, at).unwrap()).and_then(|h| h.placement);
        (pixels, pick)
    };
    let start = math::translate([-1., 0., 0.]);
    let (expected, landed) = draw(math::multiply(&delta, &start), None);
    assert_eq!(landed, Some(0), "landed");
    let (pixels, pick) = draw(start, Some(delta));
    assert_eq!(pick, landed, "dragged");
    assert_eq!(
        difference(&pixels, &expected),
        (0, 0),
        "the dragged row draws where the landed one does"
    );
}

// Shadows fit the casters' and receivers' boxes. A scene of placements
// alone has no batches, whose empty box, merged as two points, made the fit
// infinite and dropped every shadow.
#[test]
fn placements_alone_cast_sun_shadows() {
    let world = Extent {
        center: [0.; 3],
        size: 22.,
    };
    let camera = Camera {
        tilt: 1.2,
        ..Default::default()
    };
    // Sun up and toward +X: the block at height 4 shadows the floor 2 units
    // toward -X of itself.
    let light = |shadows: bool| Light {
        sun: [1., 2., 0.],
        sun_color: [0.7; 3],
        sky: [0.3; 3],
        ground: [0.3; 3],
        shadows: shadows.then(|| Shadows {
            resolution: 1024,
            softness: 1.,
            ..Default::default()
        }),
        ..Default::default()
    };
    let draw = |shadows: bool| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: light(shadows),
            ..Default::default()
        });
        renderer.set_pieces(vec![
            Piece {
                levels: vec![(
                    Arc::new(Mesh::new(quad(10., 0., Material::Lit)).unwrap()),
                    0.,
                )],
                impostor: None,
                pose: None,
            },
            Piece {
                levels: vec![(Arc::new(cube(0xffffff, Material::Lit)), 0.)],
                impostor: None,
                pose: None,
            },
        ]);
        renderer.set_placements(&[0, 1], &[math::IDENTITY, math::translate([0., 4., 0.])]);
        let mut target = Offscreen::new(320, 320).unwrap();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world))[0];
        (at([-2., 0., 0.]), at([-2., 0., 6.]))
    };
    let (shaded, lit) = draw(true);
    let (bare, _) = draw(false);
    assert!(
        shaded + 40 < lit && shaded + 40 < bare,
        "the block's shadow darkens the floor ({shaded} under it, \
         {lit} beside it, {bare} without shadows)"
    );
}

// An impostor's quad is cut to what its views cover plus a margin in screen
// pixels. A row that may bend to the terrain draws the quad a plain row
// does: its bottom row is no part of the point's w. Taken as one, the
// margin measured pixels toward the world origin's place on screen, and far
// from it crowns lost their filtered edges.
#[test]
fn impostors_of_adapting_placements_keep_their_crown_edges() {
    // Trees small on screen, well away from the origin: the coarse levels
    // their pixels blend reach past the covered texels by most of a pixel.
    let world = Extent {
        center: [60., 0., 60.],
        size: 48.,
    };
    let camera = Camera {
        fov: 0.9,
        tilt: 0.9,
        yaw: 0.6,
        ..Camera::perspective()
    };
    let draw = |adapt: bool| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        // A crown on a trunk, its box well off its origin.
        let mut data = cube(0xffffff, Material::Lit).data().clone();
        for p in &mut data.positions {
            *p = [p[0] * 2., p[1] * 2. + 2., p[2] * 2.];
        }
        data.texture = Some(Texture::solid([40, 200, 60, 255]).into());
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
            // Below a billion pixels per metre: always the impostor.
            impostor: Some(1e9),
            pose: None,
        }]);
        let rows: Vec<Mat4> = (0..144)
            .map(|k| {
                let at = [40. + 3.5 * (k % 12) as f32, 0., 40. + 3.5 * (k / 12) as f32];
                let m = math::translate(at);
                // No terrain to bend to: the row draws as the plain one.
                if adapt {
                    adapting(m, true, true, 1.5)
                } else {
                    m
                }
            })
            .collect();
        renderer.set_placements(&vec![0; rows.len()], &rows);
        let mut target = Offscreen::new(320, 240).unwrap();
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        // The bake lands a frame after the arena.
        target.frame(&renderer).unwrap();
        let pixels = target.frame(&renderer).unwrap();
        assert!(renderer.lock().assets.placed.atlas.is_some());
        pixels
    };
    let plain = draw(false);
    let adapting = draw(true);
    dump("impostor_plain", &plain, 320, 240);
    dump("impostor_adapting", &adapting, 320, 240);
    let covered = plain.chunks(4).filter(|p| p[1] > 100).count();
    assert!(covered > 2000, "the trees draw: {covered} pixels");
    let differing = (plain.chunks(4).zip(adapting.chunks(4)))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 0, "of {covered} crown pixels");
}

/// FNV-1a over a frame's bytes.
fn digest(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Lit placements of every kind the kit's own families serve, toned, fogged
/// and shadowed over a ground that tints them, with no host family
/// registered: a mip-coloured floor at a slant, a cutout card, a
/// splat-mapped slab, plain cubes and one glowing by its normal map's blue,
/// a normal- and surface-mapped quad, a world-tiled quad under the ground's
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
    raise(&mut field, &map, rise, 1.5);
    // Colours that change across the ground, so a tinted surface shows
    // where it stands.
    let colors: Vec<u8> = (0..33 * 33)
        .flat_map(|i| [60 + 5 * (i % 33) as u8, 200 - 4 * (i / 33) as u8, 90, 255])
        .collect();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        terrain: Some(Arc::new(Terrain {
            heights: map,
            colors: Arc::new(ControlMap::new(33, 33, colors).unwrap()),
            color_gain: 1.,
            tints: true,
            material: Material::Lit,
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
        glow: Glow {
            strength: 1.5,
            ..Default::default()
        },
        ..Default::default()
    });
    let solid = |rgba: [u8; 4]| Image::from(Texture::solid(rgba));
    // Each mip its own colour: a footprint picked differently shows.
    let mut floor = sheet(3., 0.05, 12, 0xffffff);
    floor.material = Material::Lit;
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
    floor.texture = Some(compressed(Block::Bc1, 64, 64, 7, move |l| bc1(mips[l])));
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
    let mut slab = quad(1., 0., Material::Lit);
    slab.texture = Some(solid([128, 128, 128, 255]));
    slab.ignore_alpha = true;
    slab.normal_map = Some(solid([128, 128, 255, 90]));
    slab.splat = Some(Arc::new(MeshSplat {
        control: Texture::from_rgba(8, 1, control).unwrap().into(),
        layers: [
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
        sharpness: 8.,
        alpha_floor: 0.1,
        fallback: [0.3; 3],
        tint: Some(2.),
        mix_normals: true,
        scale_invariant: true,
        world_tiling: false,
    }));
    let mut white = cube(0xffffff, Material::Lit).data().clone();
    white.texture = Some(solid([255; 4]));
    let mut steady = white.clone();
    steady.texture = Some(solid([128, 128, 128, 255]));
    steady.normal_map = Some(solid([128, 128, 180, 0]));
    steady.emissive = Some([0.5, 0.4, 0.2]);
    let mut warm = white.clone();
    warm.texture = Some(solid([255, 200, 120, 255]));
    let mut dark = white.clone();
    dark.texture = Some(solid([60, 200, 90, 255]));
    dark.surface_map = Some(solid([0, 0, 0, 120]));
    let mut mapped = quad(0.9, 0., Material::Lit);
    mapped.texture = Some(solid([200, 200, 200, 255]));
    mapped.normal_map = Some(solid([170, 128, 255, 220]));
    mapped.surface_map = Some(solid([255, 0, 0, 160]));
    let mut tiled = quad(1., 0., Material::Lit);
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
    bent.material = Material::Lit;
    bent.terrain_adapt = Some(TerrainAdapt::Relative);
    bent.terrain_tint = 0.5;
    let mut decal = quad(
        1.2,
        0.,
        Material::Decal(Decal {
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
        floor, card, slab, white, steady, warm, dark, mapped, tiled, bent, decal,
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

/// `lit_scene`'s frame by adapter and driver: recorded when the host's own
/// surface variants left the kit for families of their own. A change to how
/// the kit's families compose that is meant to leave their pixels alone
/// must leave these alone; one that is meant to change them records them
/// anew, and says so. An adapter not listed here only prints its digest.
const LIT_SCENE_DIGESTS: &[(&str, u64)] = &[(
    "NVIDIA GeForce RTX 4080 SUPER (620.70.06)",
    0xdcf0_c3f6_a939_26f5,
)];

// With no host family registered, the kit's own families draw everything
// they serve as they did when the digests were recorded, and pick the
// placements they draw.
#[test]
fn the_kits_own_families_draw_alone() {
    let (renderer, camera, world, centres) = lit_scene();
    assert_eq!(renderer.families().count(), 1, "the kit's family alone");
    let mut target = Offscreen::new(320, 240).unwrap();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    dump("lit", &pixels, 320, 240);
    for (row, centre) in centres {
        let at = camera.project(centre, target.bounds(), world);
        let hit = target.pick(&renderer, at).unwrap();
        assert_eq!(hit.and_then(|h| h.placement), Some(row), "row {row}");
    }
    let info = Offscreen::new(8, 8).unwrap().device().adapter_info();
    let adapter = format!("{} ({})", info.name, info.driver_info);
    let digest = digest(&pixels);
    eprintln!("(\"{adapter}\", {digest:#x}),");
    let Some(&(_, recorded)) = LIT_SCENE_DIGESTS.iter().find(|(a, _)| *a == adapter) else {
        eprintln!("no digest recorded for {adapter}: its frame goes unchecked");
        return;
    };
    assert_eq!(digest, recorded, "on {adapter}");
}

/// An upright card on z = 0, 2 across and 2 high, `n` cells a side. UVs
/// run 0..1 across it, v down.
fn card(n: u32) -> MeshData {
    let mut data = MeshData::default();
    for y in 0..=n {
        for x in 0..=n {
            let (u, v) = (x as f32 / n as f32, y as f32 / n as f32);
            data.positions.push([2. * u - 1., 2. * v, 0.]);
            data.uvs.push([u, 1. - v]);
        }
    }
    for y in 0..n {
        for x in 0..n {
            let i = y * (n + 1) + x;
            data.indices
                .extend([i, i + 1, i + n + 1, i + 1, i + n + 2, i + n + 1]);
        }
    }
    data
}

/// `rgb` with alpha rising across u, so a card's cut runs up its middle.
fn ramp_alpha(rgb: [u8; 3]) -> Image {
    let texels = (0..64).flat_map(|x| [rgb[0], rgb[1], rgb[2], (x * 255 / 63) as u8]);
    Image::from(Texture::from_rgba(64, 1, texels.collect()).unwrap())
}

/// Two cards on lit ground under a low sun behind them, the near one of
/// `near`'s family and the far one of `far`'s, both out of `families`: the
/// near card's cut half shows the far card, and its
/// solid half alone shadows the ground before it. Row 0 is the near card,
/// 1 the far one.
fn cut_scene(families: &Arc<Families>, [near, far]: [FamilyId; 2]) -> (Renderer, Camera, Extent) {
    let renderer = Renderer::with_families(families.clone());
    let mut ground = quad(6., 0., Material::Lit);
    ground.texture = Some(Texture::solid([200, 200, 200, 255]).into());
    renderer.set_scene(&Scene {
        // A batch: the shadow map is fitted to nothing when placements
        // are a scene's only casters.
        batches: vec![single(ground, math::IDENTITY)],
        light: Light {
            sun: [0., 1., -1.],
            sun_color: [0.7; 3],
            sky: [0.35; 3],
            ground: [0.25; 3],
            shadows: Some(Shadows::default()),
            fog: None,
        },
        ..Default::default()
    });
    let cards = [([40, 200, 60], near), ([60, 80, 220], far)].map(|(rgb, family)| {
        let mut data = card(8);
        data.texture = Some(ramp_alpha(rgb));
        data.family = family;
        data
    });
    renderer.set_pieces(
        (cards.into_iter())
            .map(|m| Piece {
                levels: vec![(Arc::new(Mesh::new(m).unwrap()), 0.)],
                impostor: None,
                pose: None,
            })
            .collect(),
    );
    renderer.set_placements(
        &[0, 1],
        &[math::IDENTITY, math::translate([-0.6, 0., -1.5])],
    );
    let world = Extent {
        center: [0.; 3],
        size: 9.,
    };
    // Straight at the cards, from the side their shadows fall to.
    let camera = Camera {
        fov: 0.9,
        zoom: 1.6,
        tilt: 0.9,
        yaw: 0.,
        ..Camera::perspective()
    };
    (renderer, camera, world)
}

/// `renderer`'s frame at rest through `camera`: the shadow map lands a
/// frame after the arena, and counts come back a frame late.
fn rest(target: &mut Offscreen, renderer: &Renderer, camera: Camera, world: Extent) -> Vec<u8> {
    target.aim(renderer, camera, world);
    target.settle(renderer).unwrap();
    target.frame(renderer).unwrap();
    target.frame(renderer).unwrap()
}

// A family's coverage cuts the same pixels wherever a pass cuts: the
// visibility pass and the shadow map both ask the dispatcher. The kit's own
// family shows the far card through the near one's cut half, picks what
// shows, and shadows the ground under the solid half alone.
#[test]
fn a_registered_family_draws_and_cuts_the_same_in_every_pass() {
    let (renderer, camera, world) = cut_scene(&Families::kit(), [FamilyId::LIT; 2]);
    let mut target = Offscreen::new(320, 240).unwrap();
    let pixels = rest(&mut target, &renderer, camera, world);
    dump("cut", &pixels, 320, 240);
    let bounds = target.bounds();
    let at = |p: Vec3| camera.project(p, bounds, world);
    let [solid, cut, lit, shadowed] = [
        [0.5, 1., 0.],
        [-0.3, 1., -1.5],
        [-0.5, 0., 1.],
        [0.5, 0., 1.],
    ]
    .map(|p| target.pixel(&pixels, at(p)));
    assert!(solid[1] > solid[2] + 40, "near card {solid:?}");
    assert!(cut[2] > cut[1] + 40, "far card {cut:?}");
    assert!(
        lit[0] > shadowed[0] + 40,
        "ground {lit:?} beside {shadowed:?} in the solid half's shadow"
    );
    let picks = [[0.5, 1., 0.], [-0.3, 1., -1.5], [-0.5, 0., 1.]].map(|p: Vec3| {
        let hit = target.pick(&renderer, at(p)).unwrap();
        hit.and_then(|h| h.placement)
    });
    assert_eq!(picks, [Some(0), Some(1), None]);
}

/// A host family named `probe`: the kit's coverage, and its surface after
/// running `body`.
fn probe(body: &str) -> Family {
    Family {
        name: "probe".into(),
        wgsl: format!(
            "fn probe_coverage(material: GpuDraw, c: Cover) -> f32 {{ return c.albedo.a; }}\n\
             fn probe_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {{\n    \
             {body}\n    return lit_surface(material, p, t);\n}}\n"
        ),
        light: false,
        vertex: false,
        coverage_slots: 0,
        bake: false,
    }
}

// A family shades pixels from the visibility buffer's resolve, a compute
// stage, where a screen derivative, an implicit-level sample or a discard
// is not the surface's, or not there at all.
// Registration refuses each and names the family. It finds them by calling
// the family's functions from a compute entry point: naga lets all three
// through while only fragment stages reach them, and refuses the compute
// entry point, which is checked here on naga itself first.
#[test]
fn a_family_using_derivatives_is_refused_at_registration() {
    use wgpu::naga::{self, valid};
    // What naga refuses `source` for, if anything.
    let refused = |source: &str| {
        let module = naga::front::wgsl::parse_str(source).unwrap();
        valid::Validator::new(valid::ValidationFlags::all(), valid::Capabilities::all())
            .validate(&module)
            .err()
            .map(|e| e.into_inner())
    };
    let kit = Families::kit().module(placed::Module::Arena);
    let call = "var p: LitPixel; var t: LitTexels; let s = probe_surface(p_draws[0], p, t);";
    for (what, body) in [
        ("a derivative", "let d = dpdx(p.uv);"),
        (
            "an implicit-level sample",
            "let x = textureSample(t_tint, s_albedo, p.uv);",
        ),
        ("a discard", "if p.uv.x < 0.0 { discard; }"),
    ] {
        let family = probe(body);
        let fragment = format!(
            "{kit}{}@fragment fn probe_fs() -> @location(0) vec4<f32> {{ {call} \
             return vec4<f32>(s.light, 1.0); }}\n",
            family.wgsl
        );
        assert!(refused(&fragment).is_none(), "{what} from a fragment stage");
        let compute = format!(
            "{kit}{}@compute @workgroup_size(1) fn probe_cs() {{ {call} }}\n",
            family.wgsl
        );
        assert!(
            matches!(
                refused(&compute),
                Some(valid::ValidationError::EntryPoint {
                    source: valid::EntryPointError::ForbiddenStageOperations,
                    ..
                })
            ),
            "{what} from a compute stage"
        );
        let error = Families::new(vec![family]).unwrap_err().to_string();
        assert!(
            error.contains("surface family `probe`") && error.contains("screen derivative"),
            "{what}: {error}"
        );
    }
    // Within the rules it registers, after the kit's own.
    let families = Families::new(vec![probe("")]).unwrap();
    assert_eq!(families.count(), 2);
    assert_eq!(families.id("lit"), Some(FamilyId::LIT));
    assert!(families.id("probe").is_some_and(|id| id != FamilyId::LIT));
    assert_eq!(families.id("ore"), None);
    // A name outside the family's prefix could clash with another's.
    let mut loose = probe("");
    loose.wgsl += "fn helper() -> f32 { return 1.0; }\n";
    let error = Families::new(vec![loose]).unwrap_err().to_string();
    assert!(
        error.contains("surface family `probe`") && error.contains("`helper`"),
        "{error}"
    );
}

/// A family no Anno game has. `bands` cuts every other band across u away,
/// whatever the albedo's alpha holds besides; turns its albedo's channels
/// round, under a finer copy of itself sampled through the kit's accessor;
/// and lights in two tones, sunlit or not.
fn bands() -> Family {
    let wgsl = r#"
fn bands_coverage(material: GpuDraw, c: Cover) -> f32 {
    return step(0.5, fract(c.uv.x * 4.0)) * c.albedo.a;
}

fn bands_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let n = surface_normal(p.world, cross(p.dp[0], p.dp[1]), p.normal, (material.flags & 1u) != 0u);
    let finer = array<vec2<f32>, 2>(p.duv[0] * 8.0, p.duv[1] * 8.0);
    let fine = family_texel(material.albedo, p.uv * 8.0, finer);
    let albedo = mix(t.albedo.rgb, decode(fine.rgb), 0.5).brg * p.color;
    return Lit(plain(albedo, n), vec3<f32>(0.0));
}

fn bands_light(material: GpuDraw, p: LitPixel, s: Lit) -> vec3<f32> {
    let sunlit = step(0.0, dot(s.surface.normal, lights.sun.xyz)) * shadow(p.world, s.surface.geometry);
    return s.surface.albedo * mix(0.4, 1.0, sunlit);
}
"#;
    Family {
        name: "bands".into(),
        wgsl: wgsl.into(),
        light: true,
        vertex: false,
        coverage_slots: 0,
        bake: false,
    }
}

// A host's family draws as the kit's own does: its coverage cuts the bands
// in the visibility pass and the shadow maps alike, its surface and light
// colour what is left, and picks name what shows. None of it is the kit's
// picture of the same cards.
#[test]
fn a_host_family_draws_its_own_pixels_and_picks_alike() {
    let families = Families::new(vec![bands()]).unwrap();
    let id = families.id("bands").unwrap();
    let mut target = Offscreen::new(320, 240).unwrap();
    let bands = {
        let (renderer, camera, world) = cut_scene(&families, [id; 2]);
        let pixels = rest(&mut target, &renderer, camera, world);
        dump("bands", &pixels, 320, 240);
        let bounds = target.bounds();
        let at = |p: Vec3| camera.project(p, bounds, world);
        // The near card keeps u in 0.625..0.75 (x 0.25..0.5) and cuts the
        // band past it; the far card's kept band shows through the near
        // card's cut half.
        let [kept, cut, behind, shadowed, lit] = [
            [0.375, 1., 0.],
            [0.62, 1., 0.],
            [-0.225, 1., -1.5],
            [0.375, 0., 1.],
            [0.62, 0., 1.],
        ]
        .map(|p| target.pixel(&pixels, at(p)));
        assert!(kept[2] > kept[1] + 40, "near band {kept:?}");
        assert!(
            cut[0] == cut[1] && cut[1] == cut[2],
            "{cut:?} through the cut band"
        );
        assert!(behind[0] > behind[1] + 40, "far band {behind:?}");
        assert!(
            lit[0] > shadowed[0] + 40,
            "ground {lit:?} beside {shadowed:?} in the band's shadow"
        );
        let picks = [[0.375, 1., 0.], [0.62, 1., 0.], [-0.225, 1., -1.5]].map(|p: Vec3| {
            let hit = target.pick(&renderer, at(p)).unwrap();
            hit.and_then(|h| h.placement)
        });
        assert_eq!(picks, [Some(0), None, Some(1)]);
        pixels
    };
    // The same cards as the kit's family, in the same registry.
    let (renderer, camera, world) = cut_scene(&families, [FamilyId::LIT; 2]);
    let kit = rest(&mut target, &renderer, camera, world);
    let (off, _) = difference(&bands, &kit);
    assert!(off > 2000, "{off} pixels apart from the kit's cards");
}

// Renderers of different family sets take turns on one device: each draws
// with its own set's pipelines, and going back to one finds its picture as
// it was. A registry built again from the same families draws the same.
#[test]
fn renderers_of_two_family_sets_take_turns_on_one_device() {
    let host = Families::new(vec![bands()]).unwrap();
    let id = host.id("bands").unwrap();
    let (kit_renderer, camera, world) = cut_scene(&Families::kit(), [FamilyId::LIT; 2]);
    let (host_renderer, ..) = cut_scene(&host, [id; 2]);
    let mut target = Offscreen::new(320, 240).unwrap();
    // As a host warms its set before its first island opens.
    target.warm(|warm| warm.families(&host));
    let mut draw = |renderer: &Renderer| rest(&mut target, renderer, camera, world);
    let (kit_first, host_first) = (draw(&kit_renderer), draw(&host_renderer));
    let (off, _) = difference(&kit_first, &host_first);
    assert!(off > 2000, "{off} pixels apart");
    assert!(draw(&kit_renderer) == kit_first, "the kit's set, again");
    assert!(draw(&host_renderer) == host_first, "the host's set, again");
    let again = Families::new(vec![bands()]).unwrap();
    let (renderer, ..) = cut_scene(&again, [again.id("bands").unwrap(); 2]);
    assert!(draw(&renderer) == host_first, "an equal registry");
}

// A registry checks each family against every module it joins and names
// the one at fault, whatever the fault: the others, and the kit, are not
// what an error blames, and without the broken one they register.
#[test]
fn a_broken_family_is_refused_by_name() {
    let ore = |coverage: &str, surface: &str| Family {
        name: "ore".into(),
        wgsl: format!(
            "fn ore_coverage(material: GpuDraw, c: Cover) -> f32 {{ {coverage} }}\n\
             fn ore_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {{ {surface} }}\n"
        ),
        light: false,
        vertex: false,
        coverage_slots: 0,
        bake: false,
    };
    let (covers, shades) = ("return c.albedo.a;", "return lit_surface(material, p, t);");
    let started = std::time::Instant::now();
    let families = Families::new(vec![bands(), ore(covers, shades)]).unwrap();
    eprintln!("two families checked in {:?}", started.elapsed());
    assert_eq!(families.count(), 3);
    let mut missing = ore(covers, shades);
    missing.wgsl = format!("fn ore_coverage(material: GpuDraw, c: Cover) -> f32 {{ {covers} }}\n");
    let mut lightless = ore(covers, shades);
    lightless.light = true;
    for (what, family, says) in [
        (
            "a syntax error",
            ore("return c.albedo.a", shades),
            "expected",
        ),
        ("no surface", missing, "ore_surface"),
        ("no light though it says so", lightless, "ore_light"),
        (
            "the texture array itself",
            ore(
                covers,
                "let x = textureSampleLevel(t_albedos[material.albedo], s_albedo, p.uv, 0.0); \
                 return lit_surface(material, p, t);",
            ),
            "family_texel",
        ),
        // visbuf.wgsl's, which the arena's own module lacks.
        (
            "a helper not every module has",
            ore(
                covers,
                "let h = vis_hash(1u, 2u, 3u, 0u); return lit_surface(material, p, t);",
            ),
            "vis_hash",
        ),
        // The check calls coverage from a compute stage.
        (
            "a derivative in its coverage",
            ore("return dpdx(c.uv.x);", shades),
            "screen derivative",
        ),
        (
            "a result of another type",
            ore(covers, "return plain(t.albedo.rgb, p.normal);"),
            "",
        ),
    ] {
        let error = Families::new(vec![bands(), family])
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("surface family `ore`") && error.contains(says),
            "{what}: {error}"
        );
    }
    let named = |name: &str| {
        let mut family = bands();
        family.name = name.into();
        Families::new(vec![bands(), family])
            .unwrap_err()
            .to_string()
    };
    assert!(named("bands").contains("twice"));
    assert!(named("lit").contains("the kit's"));
    assert!(named("Ore_2").contains("lower-case"));
}

/// A family that shows one bit of its row's instance word: row r's pixels
/// are white where bit r % 32 is set and black where it is not, so 32 rows
/// sharing a word read all of it back exactly.
fn bits() -> Family {
    let wgsl = r#"
fn bits_coverage(material: GpuDraw, c: Cover) -> f32 {
    return 1.0;
}

fn bits_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let row = p.pick & 0x7fffffffu;
    let on = f32((p.word >> (row % 32u)) & 1u);
    return Lit(plain(vec3<f32>(on), p.normal), vec3<f32>(0.0));
}

fn bits_light(material: GpuDraw, p: LitPixel, s: Lit) -> vec3<f32> {
    return s.surface.albedo;
}
"#;
    Family {
        name: "bits".into(),
        wgsl: wgsl.into(),
        light: true,
        vertex: false,
        coverage_slots: 0,
        bake: false,
    }
}

/// Words the readback covers: none, all ones, and a signalling NaN's
/// pattern, which a word passed through a float would come back quieted.
const WORDS: [u32; 3] = [0, u32::MAX, 0x7fa0_0001];

/// 96 rows of one quad mesh in `bits`, a grid of 12 by 8 one unit apart,
/// row r at column r % 12 and line r / 12: 32 rows for each of `WORDS`.
fn bits_scene() -> (Renderer, Camera, Extent, Vec<Mat4>) {
    let families = Families::new(vec![bits()]).unwrap();
    let renderer = Renderer::with_families(families.clone());
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let mut data = quad(0.4, 0., Material::Lit);
    data.family = families.id("bits").unwrap();
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: None,
        pose: None,
    }]);
    let rows: Vec<Mat4> = (0..96)
        .map(|r| math::translate([(r % 12) as f32 - 5.5, 0., (r / 12) as f32 - 3.5]))
        .collect();
    renderer.set_placements(&vec![0; rows.len()], &rows);
    let world = Extent {
        center: [0.; 3],
        size: 14.,
    };
    (renderer, Camera::default(), world, rows)
}

/// Each row's word as `bits` drew it, read from the centre of its quad
/// where `rows` place them, and the row picked there.
fn read_words(
    target: &mut Offscreen,
    renderer: &Renderer,
    camera: Camera,
    world: Extent,
    rows: &[Mat4],
) -> (Vec<u32>, Vec<Option<u32>>) {
    let pixels = target.frame(renderer).unwrap();
    let mut words = vec![0u32; rows.len()];
    let mut picks = Vec::with_capacity(rows.len());
    for (r, m) in rows.iter().enumerate() {
        let at = camera.project(math::translation(m), target.bounds(), world);
        let [v, ..] = target.pixel(&pixels, at);
        assert!(v < 30 || v > 225, "row {r}: {v} is neither bit");
        if v > 225 {
            // Every row of a word's 32 shows its own bit.
            words[r / 32 * 32] |= 1 << (r % 32);
        }
        picks.push(target.pick(renderer, at).unwrap().and_then(|h| h.placement));
    }
    let words = (0..rows.len()).map(|r| words[r / 32 * 32]).collect();
    (words, picks)
}

// The instance word reaches a family bit for bit: 0 for rows
// whose host never sets one, then whatever the host writes, all ones and a
// signalling NaN's pattern included, rows of one mesh each with their own.
// A GPU drag moves rows and leaves their words, and so does the commit
// that lands them; picks follow the rows.
#[test]
fn instance_words_reach_families_exactly() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let (renderer, camera, world, rows) = bits_scene();
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    let (words, picks) = read_words(&mut target, &renderer, camera, world, &rows);
    assert!(words.iter().all(|&w| w == 0), "unset words");
    let all: Vec<Option<u32>> = (0..rows.len() as u32).map(Some).collect();
    assert_eq!(picks, all, "picks");
    let set: Vec<u32> = (0..rows.len()).map(|r| WORDS[r / 32]).collect();
    renderer.set_instance_words(0, &set);
    target.settle(&renderer).unwrap();
    let (words, picks) = read_words(&mut target, &renderer, camera, world, &rows);
    assert_eq!(words, set, "set words");
    assert_eq!(picks, all, "picks with words");
    {
        let placed = &renderer.lock().assets.placed;
        let bounds = math::bounds(
            rows.iter()
                .flat_map(|m| [[-0.4, 0., -0.4], [0.4, 0., 0.4]].map(|p| math::transform(m, p))),
        );
        assert_eq!(placed.bounds, bounds, "words leave the bounds alone");
    }
    // The all-ones rows dragged half a cell over on the GPU, clear of
    // their neighbours' centres: they show their word where they are
    // dragged to, and the rows left read as before.
    let delta = math::translate([0.5, 0.1, 0.]);
    let dragged: Vec<u32> = (32..64).collect();
    let marker = Marker {
        position: [0.; 3],
        radius: 4.,
        color: 0x00ff00,
        shape: Shape::Circle,
    };
    renderer.set_selection(dragged.clone(), &[], &marker);
    renderer.drag_selection(Some(delta));
    let landed: Vec<Mat4> = (rows.iter().enumerate())
        .map(|(r, m)| match dragged.contains(&(r as u32)) {
            true => math::multiply(&delta, m),
            false => *m,
        })
        .collect();
    target.settle(&renderer).unwrap();
    let (words, picks) = read_words(&mut target, &renderer, camera, world, &landed);
    assert_eq!(words, set, "dragged");
    assert_eq!(picks, all, "picks while dragged");
    // The host lands them, as the editor commits a drag.
    renderer.move_placements(32, &landed[32..64]);
    renderer.drag_selection(None);
    target.settle(&renderer).unwrap();
    let (words, picks) = read_words(&mut target, &renderer, camera, world, &landed);
    assert_eq!(words, set, "committed");
    assert_eq!(picks, all, "picks once committed");
}

// The kit reads nothing of the words: the kit's own family, its impostors
// and their shadows draw the same whatever words the rows carry.
#[test]
fn instance_words_leave_the_kits_family_and_impostors_alone() {
    let world = Extent {
        center: [60., 0., 60.],
        size: 48.,
    };
    let camera = Camera {
        fov: 0.9,
        tilt: 0.9,
        yaw: 0.6,
        ..Camera::perspective()
    };
    let draw = |words: Option<u32>| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: Light {
                sun: [1., 2., 0.],
                sun_color: [0.7; 3],
                shadows: Some(Shadows::default()),
                ..neutral()
            },
            ..Default::default()
        });
        let mut data = cube(0xffffff, Material::Lit).data().clone();
        data.texture = Some(Texture::solid([40, 200, 60, 255]).into());
        let floor = Mesh::new(quad(30., 0., Material::Lit)).unwrap();
        renderer.set_pieces(vec![
            Piece {
                levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
                impostor: Some(1e9),
                pose: None,
            },
            Piece {
                levels: vec![(Arc::new(floor), 0.)],
                impostor: None,
                pose: None,
            },
        ]);
        let mut rows: Vec<Mat4> = (0..64)
            .map(|k| math::translate([46. + 4. * (k % 8) as f32, 1., 46. + 4. * (k / 8) as f32]))
            .collect();
        rows.push(math::translate([60., 0., 60.]));
        let mut piece_of = vec![0; 64];
        piece_of.push(1);
        renderer.set_placements(&piece_of, &rows);
        if let Some(w) = words {
            renderer.set_instance_words(0, &vec![w; rows.len()]);
        }
        let mut target = Offscreen::new(320, 240).unwrap();
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        target.frame(&renderer).unwrap();
        let pixels = target.frame(&renderer).unwrap();
        let placed = &renderer.lock().assets.placed;
        assert!(placed.atlas.is_some(), "impostors baked");
        (pixels, placed.bounds)
    };
    let (plain, bounds) = draw(None);
    for word in &WORDS[1..] {
        let (pixels, with) = draw(Some(*word));
        assert_eq!(with, bounds, "{word:#x}: bounds");
        assert!(pixels == plain, "{word:#x}: pixels");
    }
}

/// A family that shows one bit of its mesh's payload: row r's pixels are
/// white where bit r % 32 of payload word `w` is set, `w` the row's
/// instance word.
fn payload_bits() -> Family {
    let wgsl = r#"
fn pbits_coverage(material: GpuDraw, c: Cover) -> f32 {
    return 1.0;
}

fn pbits_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let row = p.pick & 0x7fffffffu;
    let on = f32((family_word(material, p.word) >> (row % 32u)) & 1u);
    return Lit(plain(vec3<f32>(on), p.normal), vec3<f32>(0.0));
}

fn pbits_light(material: GpuDraw, p: LitPixel, s: Lit) -> vec3<f32> {
    return s.surface.albedo;
}
"#;
    Family {
        name: "pbits".into(),
        wgsl: wgsl.into(),
        light: true,
        vertex: false,
        coverage_slots: 0,
        bake: false,
    }
}

// A host family reads its mesh's payload word for word on every tier, the
// most a mesh holds, a signalling NaN's pattern among them; the kit's own
// family takes none.
#[test]
fn a_family_reads_its_payload_exactly_on_every_tier() {
    let payload: Vec<u32> = (0..MAX_PAYLOAD as u32)
        .map(|k| match k {
            0..3 => WORDS[k as usize],
            _ => 0x9e37_79b9u32.wrapping_mul(k + 1) ^ (k << 27),
        })
        .collect();
    let families = Families::new(vec![payload_bits()]).unwrap();
    let mut lit = quad(0.4, 0., Material::Lit);
    lit.payload = vec![1];
    assert!(Mesh::new(lit).is_err(), "the kit's family takes no payload");
    let mut long = quad(0.4, 0., Material::Lit);
    long.family = families.id("pbits").unwrap();
    long.payload = vec![0; MAX_PAYLOAD + 1];
    assert!(Mesh::new(long).is_err(), "past the most a mesh holds");
    let rows: Vec<Mat4> = (0..payload.len() * 32)
        .map(|r| math::translate([(r % 32) as f32 - 15.5, 0., (r / 32) as f32 - 7.5]))
        .collect();
    let words: Vec<u32> = (0..rows.len() as u32).map(|r| r / 32).collect();
    let mut target = Offscreen::new(704, 384).unwrap();
    let renderer = Renderer::with_families(families.clone());
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let mut data = quad(0.4, 0., Material::Lit);
    data.family = families.id("pbits").unwrap();
    data.payload = payload.clone();
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&vec![0; rows.len()], &rows);
    renderer.set_instance_words(0, &words);
    let (camera, world) = (
        Camera::default(),
        Extent {
            center: [0.; 3],
            size: 33.,
        },
    );
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let mut read = vec![0u32; payload.len()];
    for (r, m) in rows.iter().enumerate() {
        let at = camera.project(math::translation(m), target.bounds(), world);
        let [v, ..] = target.pixel(&pixels, at);
        assert!(v < 30 || v > 225, "row {r}: {v} is neither bit");
        read[r / 32] |= u32::from(v > 225) << (r % 32);
    }
    assert_eq!(read, payload);
}

/// A scene of every lit thing the kit draws itself, for `lighting`: lit
/// ground, a lit mesh and a blended one, and a lit placement. Seen from
/// above; returns the pixel over each.
fn lit_everything(lighting: Lighting) -> [[u8; 3]; 4] {
    let families = Families::with_lighting(Vec::new(), lighting).unwrap();
    let renderer = Renderer::with_families(families);
    let field = flat_field(33);
    let ground = Arc::new(Terrain {
        heights: Arc::new(HeightMap::new(&field).unwrap()),
        colors: Arc::new(ControlMap::new(33, 33, [90, 80, 60, 255].repeat(33 * 33)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Lit,
        texture: None,
    });
    let water = quad(
        1.,
        0.3,
        Material::Blend(Blend {
            color: 0x2060ff,
            opacity: 1.,
            fade: 0.,
            fresnel: 0.,
            depth: 0.,
            additive: false,
            unlit: false,
        }),
    );
    renderer.set_scene(&Scene {
        terrain: Some(ground),
        batches: vec![
            single(
                cube(0xc08040, Material::Lit).data().clone(),
                math::translate([-3., 0.5, 0.]),
            ),
            single(water, math::translate([3., 0., 0.])),
        ],
        light: Light {
            sun: [0.3, 1., 0.2],
            sun_color: [0.6; 3],
            sky: [0.4; 3],
            ground: [0.3; 3],
            ..Default::default()
        },
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(cube(0x40a0c0, Material::Lit)), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&[0], &[math::translate([0., 0.5, 0.])]);
    let world = Extent {
        center: [0.; 3],
        size: 14.,
    };
    let camera = Camera::default();
    let mut target = Offscreen::new(320, 240).unwrap();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    [[0., 0., -4.], [-3., 1., 0.], [3., 0.3, 0.], [0., 1., 0.]]
        .map(|p| target.pixel(&pixels, camera.project(p, target.bounds(), world)))
}

// A host lights the scene with a `shade` and a `fog` of its own, each on
// its own: everything lit the kit draws takes them, its ground, meshes and
// blended meshes and its placements. No Anno lighting in sight: the shade paints all it lights
// green, the fog all it fogs magenta.
#[test]
fn a_host_shade_and_fog_light_everything_the_kit_draws() {
    let green = Lighting {
        shade: Some(
            "fn shade(world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool) -> vec3<f32> { return scene_green() + emissive; }\n\
             fn scene_green() -> vec3<f32> { return vec3<f32>(0.0, 1.0, 0.0); }\n"
                .into(),
        ),
        ..Default::default()
    };
    let magenta = Lighting {
        shade: None,
        fog: Some(
            "fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return vec3<f32>(1.0, 0.0, 1.0); }\n"
                .into(),
        ),
        ..Default::default()
    };
    let names = ["ground", "mesh", "blended mesh", "placement"];
    let kit = lit_everything(Lighting::default());
    for (name, rgb) in names.iter().zip(kit) {
        assert!(
            rgb != [0, 255, 0] && rgb != [255, 0, 255],
            "{name}: {rgb:?}"
        );
    }
    for (lighting, want) in [(&green, [0, 255, 0]), (&magenta, [255, 0, 255])] {
        let drawn = lit_everything(lighting.clone());
        for (name, rgb) in names.iter().zip(drawn) {
            assert!(
                rgb.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2),
                "{name}: {rgb:?} under {lighting:?}"
            );
        }
    }
    // A lighting is checked as a family is: a derivative or a loose name
    // is refused, blamed on the lighting.
    for (fog, says) in [
        (
            "fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return color * dpdx(world.x); }",
            "screen derivative",
        ),
        (
            "fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> { return haze(color); }\nfn haze(c: vec3<f32>) -> vec3<f32> { return c; }",
            "`haze`",
        ),
    ] {
        let lighting = Lighting {
            fog: Some(fog.into()),
            ..Default::default()
        };
        let error = Families::with_lighting(Vec::new(), lighting)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("scene lighting") && error.contains(says),
            "{error}"
        );
    }
}

/// A family showing its mesh's first slot (`MeshData::slots`) over the
/// mesh's UVs, sampled as the albedo is.
fn swatch() -> Family {
    let wgsl = r#"
fn swatch_coverage(material: GpuDraw, c: Cover) -> f32 {
    return 1.0;
}

fn swatch_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let texel = family_texel(family_slot(material, 0u), p.uv, p.duv);
    return Lit(plain(decode(texel.rgb), p.normal), vec3<f32>(0.0));
}
"#;
    Family {
        name: "swatch".into(),
        wgsl: wgsl.into(),
        light: false,
        vertex: false,
        coverage_slots: 0,
        bake: false,
    }
}

/// `residency_renderer` for a `swatch` square whose slot holds `image`
/// and whose own albedo is a small solid.
fn slot_renderer(image: Image, whole: bool) -> Renderer {
    let families = Families::new(vec![swatch()]).unwrap();
    let renderer = Renderer::with_families(families.clone());
    {
        let residency = &mut renderer.lock().assets.placed.residency;
        residency.hold = Some(std::time::Duration::ZERO);
        residency.whole = whole;
    }
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let mut data = quad(2., 0., Material::Lit);
    data.texture = Some(Texture::solid([200, 200, 200, 255]).into());
    data.family = families.id("swatch").unwrap();
    data.slots = vec![image];
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&[0], &[math::IDENTITY]);
    renderer
}

/// The level the family slot's 256² texture holds from.
fn slot_base(renderer: &Renderer) -> u32 {
    let state = renderer.lock();
    let arena = state.assets.placed.arena.as_ref().expect("the arena");
    (arena.textures.iter().flatten())
        .find(|t| t.layout.width == 256)
        .expect("the slot's texture")
        .resident_base()
}

// A family's own slot is held as its albedo would be: close up its finest
// level streams in and draws what every level held draws; far away the
// fine levels go. Without its residency entry the slot stays at the floor
// every texture starts from.
#[test]
fn a_familys_slots_hold_the_levels_their_distance_needs() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = slot_renderer(levelled().into(), false);
    let whole = slot_renderer(levelled().into(), true);
    let settle = |target: &mut Offscreen, renderer: &Renderer, zoom: f32| {
        target.aim(renderer, residency_camera(zoom, 0.6), RESIDENCY_WORLD);
        target.settle(renderer).unwrap()
    };
    settle(&mut target, &renderer, 0.3);
    let far = slot_base(&renderer);
    assert!(far >= 2, "far away it holds from level {far}");
    let near = settle(&mut target, &renderer, 6.);
    assert_eq!(slot_base(&renderer), 0, "close up");
    assert!(
        near == settle(&mut target, &whole, 6.),
        "close up, as every level draws"
    );
    settle(&mut target, &renderer, 0.3);
    assert_eq!(slot_base(&renderer), far, "away again");
}

/// A pass that stays loading until its warm-up has run, as a game's does
/// while its pipelines compile on a thread of its own.
#[derive(Default)]
struct Cold {
    warmed_on: std::sync::Mutex<Option<std::thread::ThreadId>>,
    /// Prepares that found it still compiling.
    cold: std::sync::atomic::AtomicU32,
}

impl Pass for Cold {
    fn stage(&self) -> Stage {
        Stage::Water
    }

    fn prepare(&self, _: &mut Prepare<'_>) -> bool {
        let warm = self.warmed_on.lock().unwrap().is_some();
        if !warm {
            self.cold.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        warm
    }

    fn record(&self, _: &Record<'_>, _: &mut wgpu::RenderPass<'_>) {}

    fn warm(&self, _: &Warm<'_>) {
        *self.warmed_on.lock().unwrap() = Some(std::thread::current().id());
    }
}

// An offscreen settle warms its scene's passes on its own thread before it
// draws a frame, so no frame finds them compiling and their compile time
// never counts against the settle's bound.
#[test]
fn an_offscreen_settle_warms_its_passes_before_drawing() {
    let cold = Arc::new(Cold::default());
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        passes: vec![cold.clone()],
        ..Default::default()
    });
    let mut target = Offscreen::new(32, 32).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 4.,
    };
    target
        .render(&renderer, Camera::perspective(), world)
        .unwrap();
    assert_eq!(
        *cold.warmed_on.lock().unwrap(),
        Some(std::thread::current().id()),
        "warmed on the settling thread"
    );
    assert_eq!(
        cold.cold.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "no frame found it compiling"
    );
    assert!(!renderer.pending());
}

/// A family that turns its albedo's channels round (green to blue) and
/// reads nothing the impostor bake lacks; `bake` says so or not, `light`
/// lights it itself.
fn turned(bake: bool, light: bool) -> Family {
    let own_light = if light {
        "fn turned_light(material: GpuDraw, p: LitPixel, s: Lit) -> vec3<f32> {\n    \
         return lit_light(material, p, s);\n}\n"
    } else {
        ""
    };
    let wgsl = format!(
        "fn turned_coverage(material: GpuDraw, c: Cover) -> f32 {{\n    return c.albedo.a;\n}}\n\
         fn turned_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {{\n    \
         return Lit(plain(t.albedo.rgb.brg * p.color, p.normal), vec3<f32>(0.0));\n}}\n{own_light}"
    );
    Family {
        name: "turned".into(),
        wgsl,
        light,
        vertex: false,
        coverage_slots: 0,
        bake,
    }
}

// The impostor bake shades through the piece's family: one that says it
// bakes (`Family::bake`) gets impostors in its own colours, where the
// kit's composition would bake the albedo as it is. One that does not, or
// lights itself, keeps its meshes, and so does its colour far off.
#[test]
fn a_family_that_bakes_draws_its_own_impostors() {
    let world = Extent {
        center: [0.; 3],
        size: 12.,
    };
    let camera = Camera {
        fov: 0.9,
        tilt: 0.7,
        ..Camera::perspective()
    };
    let draw = |family: Option<Family>| {
        let families = Families::new(family.into_iter().collect()).unwrap();
        let renderer = Renderer::with_families(families.clone());
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        let mut data = cube(0xffffff, Material::Lit).data().clone();
        data.texture = Some(Texture::solid([40, 200, 60, 255]).into());
        data.family = families.id("turned").unwrap_or(FamilyId::LIT);
        // Always far enough for its impostor, once baked.
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
            impostor: Some(1e9),
            pose: None,
        }]);
        let mut big = math::IDENTITY;
        (big[0], big[5], big[10]) = (3., 3., 3.);
        renderer.set_placements(&[0], &[big]);
        let mut target = Offscreen::new(160, 120).unwrap();
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        let pixels = target.frame(&renderer).unwrap();
        let centre = camera.project([0., 0., 0.], target.bounds(), world);
        let baked = renderer.lock().assets.placed.atlas.is_some();
        (baked, target.pixel(&pixels, centre))
    };
    let (baked, kit) = draw(None);
    assert!(baked && kit[1] > kit[2] + 40, "the kit's, green: {kit:?}");
    let (baked, own) = draw(Some(turned(true, false)));
    assert!(baked, "a family that bakes gets impostors");
    assert!(
        own[2] > own[1] + 40,
        "baked through its surface, blue: {own:?}"
    );
    for (bake, light) in [(false, false), (true, true)] {
        let (baked, meshes) = draw(Some(turned(bake, light)));
        assert!(!baked, "bake {bake}, light {light}: no impostors");
        assert!(
            meshes[2] > meshes[1] + 40,
            "bake {bake}, light {light}: its meshes, blue: {meshes:?}"
        );
    }
}

/// The colours `strata`'s first five slots hold, one per band across u.
const STRATA: [[u8; 3]; 5] = [
    [220, 30, 30],
    [30, 220, 30],
    [30, 30, 220],
    [220, 220, 30],
    [30, 220, 220],
];

/// A family as large as the survey's (ripples, parallax: 5-7 slots): it
/// shows slot `k` of its first five in band `k` across u, all through the
/// kit's accessor, scaled by its payload's first word (an RGBA8 colour);
/// its coverage is its sixth slot's alpha, at the finest level held
/// (`coverage_slots` asks residency for it in shadow maps too).
fn strata() -> Family {
    let wgsl = r#"
fn strata_coverage(material: GpuDraw, c: Cover) -> f32 {
    let still = array<vec2<f32>, 2>(vec2<f32>(0.0), vec2<f32>(0.0));
    return family_texel(family_slot(material, 5u), c.uv, still).a * c.albedo.a;
}

fn strata_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let band = min(u32(max(p.uv.x, 0.0) * 5.0), 4u);
    let texel = family_texel(family_slot(material, band), p.uv, p.duv);
    let scale = unpack4x8unorm(family_word(material, 0u)).rgb;
    return Lit(plain(decode(texel.rgb) * scale, p.normal), vec3<f32>(0.0));
}
"#;
    Family {
        name: "strata".into(),
        wgsl: wgsl.into(),
        light: false,
        vertex: false,
        coverage_slots: 1 << 5,
        bake: false,
    }
}

/// `strata`'s six slots: the five band colours, then `mask`.
fn strata_slots(mask: Image) -> Vec<Image> {
    (STRATA.iter())
        .map(|&[r, g, b]| Image::from(Texture::solid([r, g, b, 255])))
        .chain([mask])
        .collect()
}

// A family with six slots and a payload binds them all and draws each in
// its own band, placed and as a plain batch: the arena's array and a kit
// mesh's fixed bindings are reached through the same accessor. Its sixth
// slot cuts it where its coverage reads no alpha.
#[test]
fn an_extra_slot_family_draws_every_slot_on_every_path() {
    let families = Families::new(vec![strata()]).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 6.,
    };
    let camera = Camera {
        fov: 0.9,
        tilt: 1.5,
        ..Camera::perspective()
    };
    // Opaque but for a hole in its top left quarter (u, v under 0.25).
    let hole = (0..64 * 64).flat_map(|i| {
        let (u, v) = (i % 64, i / 64);
        [255, 255, 255, if u < 16 && v < 16 { 0 } else { 255 }]
    });
    let mask = Image::from(Texture::from_rgba(64, 64, hole.collect()).unwrap());
    for placed in [true, false] {
        let renderer = Renderer::with_families(families.clone());
        let mut data = quad(2., 0., Material::Lit);
        data.texture = Some(Texture::solid([128, 128, 128, 255]).into());
        data.family = families.id("strata").unwrap();
        data.slots = strata_slots(mask.clone());
        // Halves green: the payload reaches the surface beside the slots.
        data.payload = vec![u32::from_le_bytes([255, 128, 255, 255])];
        let mesh = Arc::new(Mesh::new(data).unwrap());
        let batches = (!placed)
            .then(|| Batch {
                mesh: mesh.clone(),
                instances: Arc::new(Instances {
                    matrices: vec![math::IDENTITY],
                    ids: vec![Some(1)],
                }),
            })
            .into_iter()
            .collect();
        renderer.set_scene(&Scene {
            batches,
            light: neutral(),
            ..Default::default()
        });
        if placed {
            renderer.set_pieces(vec![Piece {
                levels: vec![(mesh, 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&[0], &[math::IDENTITY]);
        }
        let mut target = Offscreen::new(320, 240).unwrap();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        dump(&format!("strata_placed_{placed}"), &pixels, 320, 240);
        let at = |x: f32, z: f32| camera.project([x, 0., z], target.bounds(), world);
        for (k, [r, g, b]) in STRATA.into_iter().enumerate() {
            // Band k's middle, on the side the hole leaves alone.
            let x = -2. + 0.8 * k as f32 + 0.4;
            let want = [r, (u16::from(g) * 128 / 255) as u8, b];
            let got = target.pixel(&pixels, at(x, 1.));
            assert!(
                got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 12),
                "placed {placed}: band {k} {got:?}, not {want:?}"
            );
        }
        // u and v under 0.25: x and z under -1.
        assert_eq!(
            target.pixel(&pixels, at(-1.6, -1.6)),
            [0; 3],
            "placed {placed}: cut where slot 5 has no alpha"
        );
        let hit = target.pick(&renderer, at(0., 1.)).unwrap().unwrap();
        if placed {
            assert_eq!(hit.placement, Some(0));
        } else {
            assert_eq!(hit.id, Some(1));
        }
    }
}

// A family's coverage slot is held for the shadow maps as its albedo is
// (`Family::coverage_slots`): a card standing out of view whose shadow
// falls into it is cut by its mask's finest level, which only the shadow
// map's cull asks for. Left out of `coverage_slots`, the mask stays at the
// level every texture starts from, which here covers the card whole, and
// the card's shadow lies on the ground.
#[test]
fn a_coverage_slot_is_held_for_the_shadows_that_cut_by_it() {
    // Level 0 (128²) clear, every coarser level opaque: the 64² floor
    // covers, the finest level cuts the card away.
    let mask: Image = {
        // Both colours 0: three-colour mode, where index 3 is clear.
        let clear = [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        let opaque = bc1([255, 255, 255]);
        compressed(Block::Bc1, 128, 128, 8, |l| match l {
            0 => clear.to_vec(),
            _ => opaque.clone(),
        })
    };
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera {
        fov: 0.9,
        tilt: 1.5,
        ..Camera::perspective()
    };
    let shadowed = |coverage_slots: u8| {
        let families = Families::new(vec![Family {
            coverage_slots,
            ..strata()
        }])
        .unwrap();
        let renderer = Renderer::with_families(families.clone());
        {
            let residency = &mut renderer.lock().assets.placed.residency;
            residency.hold = Some(std::time::Duration::ZERO);
        }
        let mut ground = quad(20., 0., Material::Lit);
        ground.texture = Some(Texture::solid([200, 200, 200, 255]).into());
        renderer.set_scene(&Scene {
            batches: vec![single(ground, math::IDENTITY)],
            light: Light {
                sun: [0., 1., 1.],
                sun_color: [0.7; 3],
                sky: [0.35; 3],
                ground: [0.25; 3],
                shadows: Some(Shadows::default()),
                fog: None,
            },
            ..Default::default()
        });
        let mut data = card(8);
        data.texture = Some(Texture::solid([128, 128, 128, 255]).into());
        data.family = families.id("strata").unwrap();
        data.slots = strata_slots(mask.clone());
        data.payload = vec![u32::MAX];
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        // 8 m across and high, standing out of the view; the sun
        // behind it throws its shadow 8 m into the view.
        let mut row = math::translate([0., 0., 7.]);
        (row[0], row[5], row[10]) = (4., 4., 4.);
        renderer.set_placements(&[0], &[row]);
        let mut target = Offscreen::new(320, 240).unwrap();
        let pixels = rest(&mut target, &renderer, camera, world);
        dump(
            &format!("coverage_slot_{coverage_slots}"),
            &pixels,
            320,
            240,
        );
        let at = |x: f32, z: f32| camera.project([x, 0., z], target.bounds(), world);
        let (under, beside) = (
            target.pixel(&pixels, at(0., 2.)),
            target.pixel(&pixels, at(0., -3.)),
        );
        (under, beside)
    };
    let (held_under, held_beside) = shadowed(1 << 5);
    let (floor_under, floor_beside) = shadowed(0);
    assert!(
        floor_under[0] + 40 < floor_beside[0],
        "the mask at its floor, the card shadows: {floor_under:?} by {floor_beside:?}"
    );
    assert!(
        held_under[0].abs_diff(held_beside[0]) <= 4,
        "the mask held, the card is cut away: {held_under:?} by {held_beside:?}"
    );
}

// The kit's far sea is a default of the Water stage, apart from its sky:
// a Water pass that replaces it leaves the backdrop to fill under the
// horizon, and a sky that replaces the backdrop leaves the sea where it
// was. Neither takes the other with it.
#[test]
fn the_kits_far_sea_is_replaced_apart_from_its_sky() {
    let world = Extent {
        center: [0.; 3],
        size: 6.,
    };
    // Looking nearly level: sky above the middle, sea below.
    let camera = Camera {
        tilt: 1.45,
        fov: 0.9,
        ..Camera::perspective()
    };
    let draw = |passes: Vec<Arc<dyn Pass>>| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: Light {
                sun: [0., 1., -1.],
                ..neutral()
            },
            sky: Some(Sky {
                zenith: [0., 0., 1.],
                horizon: [1., 0., 0.],
                sea: Some(Sea {
                    level: -5.,
                    color: [0., 0.5, 0.],
                }),
                radiance: None,
                sun_disc: [0.; 3],
                ambient: None,
            }),
            passes,
            ..Default::default()
        });
        let mut target = Offscreen::new(320, 240).unwrap();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = |y: f32| target.pixel(&pixels, point(px(160.), px(y)));
        (at(2.), at(237.))
    };
    let (sky, sea) = draw(Vec::new());
    assert!(sky[2] > 200 && sky[0] < 120, "the kit's sky {sky:?}");
    assert!(sea[1] > sea[0] && sea[1] > sea[2], "the kit's sea {sea:?}");
    assert_eq!(
        draw(vec![Fill::new(Stage::Water, false, None, 0.)]),
        (sky, sea),
        "water beside the sea"
    );
    let (kept, under) = draw(vec![Fill::new(Stage::Water, true, None, 0.)]);
    assert_eq!(kept, sky, "the sea replaced, the sky stays");
    assert_eq!(under, [255, 0, 0], "the backdrop's horizon under it");
    let (none, kept) = draw(vec![Fill::new(Stage::Sky, true, None, 0.)]);
    assert_eq!(none, [0; 3], "the backdrop replaced");
    assert_eq!(kept, sea, "the sea stays");
}
