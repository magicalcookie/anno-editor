use super::offscreen::Offscreen;
use super::*;
use crate::heightfield::{Grid, Heightfield, Rect};
use gpui::point;

fn scene(material: Material) -> (Scene, Arc<Mesh>) {
    let mesh = Arc::new(cube(0x40a0ff, material));
    let mut far = math::translate([0., 0., -3.]);
    far[0] = 2.;
    let scene = Scene {
        batches: vec![Batch {
            mesh: mesh.clone(),
            instances: Arc::new(Instances {
                matrices: vec![math::IDENTITY, far],
                ids: vec![Some(7), Some(8)],
            }),
        }],
        markers: Arc::new(vec![Marker {
            position: [0., 2., 0.],
            radius: 12.,
            color: 0xffd479,
            shape: Shape::Ring,
        }]),
        ..Default::default()
    };
    (scene, mesh)
}

fn single(mesh: MeshData, matrix: Mat4) -> Batch {
    Batch {
        mesh: Arc::new(Mesh::new(mesh).unwrap()),
        instances: Arc::new(Instances {
            matrices: vec![matrix],
            ids: vec![Some(1)],
        }),
    }
}

/// A horizontal square at height `y`, `half` units from the origin each way,
/// facing up, with UVs 0..1 across it.
fn quad(half: f32, y: f32, material: Material) -> MeshData {
    let corners = [[-1., -1.], [1., -1.], [1., 1.], [-1., 1.]];
    MeshData {
        positions: corners.map(|[x, z]| [x * half, y, z * half]).to_vec(),
        uvs: corners
            .map(|[x, z]| [(x + 1.) / 2., (z + 1.) / 2.])
            .to_vec(),
        normals: vec![[0., 1., 0.]; 4],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..Default::default()
    }
}

/// Lighting that leaves albedo unchanged, so colors can be checked exactly.
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
        actual
            .iter()
            .zip(expected)
            .all(|(a, e)| a.abs_diff(e) <= tolerance),
        "{actual:?} != {expected:?}"
    );
}

#[test]
fn invalid_meshes_are_rejected() {
    let mesh = |indices: Vec<u32>, uvs| {
        Mesh::new(MeshData {
            positions: vec![[0.; 3]; 3],
            uvs,
            indices,
            ..Default::default()
        })
    };
    assert!(mesh(vec![0, 1, 2], vec![]).is_ok());
    assert!(mesh(vec![0, 1, 3], vec![]).is_err());
    assert!(mesh(vec![0, 1], vec![]).is_err());
    assert!(mesh(vec![0, 1, 2], vec![[0.; 2]; 2]).is_err());
    assert!(
        Mesh::new(MeshData {
            texture: Some(Image::from(Texture {
                width: 2,
                height: 1,
                rgba: vec![255; 4]
            })),
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        Mesh::new(MeshData {
            positions: vec![[0.; 3]; 3],
            normals: vec![[0., 1., 0.]; 2],
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn splats_reject_mismatched_layers_and_controls() {
    let image = |w, h| Arc::new(Texture::from_rgba(w, h, vec![0; (w * h * 4) as usize]).unwrap());
    let map = |w: usize, h: usize| Arc::new(ControlMap::new(w, h, vec![0; w * h * 4]).unwrap());
    let good = Splat {
        layers: vec![image(4, 4).into(), image(4, 4).into()],
        tiling: vec![2., 3.],
        sharpness: vec![],
        normals: vec![],
        indices: vec![map(8, 8)],
        weights: vec![map(8, 8)],
    };
    assert!(good.validate().is_ok());
    let bad = [
        Splat {
            layers: vec![image(4, 4).into(), image(2, 2).into()],
            ..good.clone()
        },
        Splat {
            tiling: vec![2.],
            ..good.clone()
        },
        Splat {
            sharpness: vec![1., -1.],
            ..good.clone()
        },
        Splat {
            weights: vec![map(4, 4)],
            ..good.clone()
        },
        Splat {
            indices: vec![map(8, 8), map(8, 8), map(8, 8)],
            weights: vec![map(8, 8), map(8, 8), map(8, 8)],
            ..good.clone()
        },
    ];
    for splat in bad {
        assert!(splat.validate().is_err());
    }
}

#[test]
fn simplified_meshes_keep_their_shape_with_fewer_triangles() {
    // A 64x64 grid bent into a smooth hill, UVs across it.
    let n = 65;
    let mut data = MeshData::default();
    for z in 0..n {
        for x in 0..n {
            let (u, v) = (x as f32 / 64., z as f32 / 64.);
            data.positions
                .push([u, (u * std::f32::consts::PI).sin() * 0.3, v]);
            data.uvs.push([u, v]);
        }
    }
    for z in 0..n - 1 {
        for x in 0..n - 1 {
            let i = z * n + x;
            data.indices
                .extend([i, i + 1, i + n + 1, i, i + n + 1, i + n]);
        }
    }
    let coarse = data.simplified(0.15).unwrap();
    assert!(
        coarse.indices.len() * 4 < data.indices.len(),
        "{} of {}",
        coarse.indices.len(),
        data.indices.len()
    );
    // Compacted, every attribute still one per vertex, and the bounds kept.
    assert_eq!(coarse.uvs.len(), coarse.positions.len());
    assert!(
        coarse
            .indices
            .iter()
            .all(|&i| (i as usize) < coarse.positions.len())
    );
    let (full, small) = (Mesh::new(data.clone()).unwrap(), Mesh::new(coarse).unwrap());
    for a in 0..3 {
        assert!((full.bounds()[1][a] - small.bounds()[1][a]).abs() < 0.02);
    }
    // A single quad cannot get coarser.
    assert!(quad(1., 0., Material::Lit).simplified(0.5).is_none());
}

#[test]
fn lines_draw_over_the_scene_at_their_pixel_width() {
    let mut target = Offscreen::new(64, 64).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(quad(1., 0., Material::Flat), math::IDENTITY)],
        lines: Arc::new(vec![Line {
            points: vec![[-1., 0., 0.], [1., 0., 0.]],
            width: 4.,
            color: 0xff0000ff,
        }]),
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 2.,
    };
    let camera = Camera::default();
    let pixels = target.render(&renderer, camera, world).unwrap();
    let bounds = target.bounds();
    let on = camera.project([0.3, 0., 0.], bounds, world);
    let off = camera.project([0.3, 0., 0.5], bounds, world);
    assert_eq!(target.pixel(&pixels, on), [255, 0, 0]);
    assert_ne!(target.pixel(&pixels, off), [255, 0, 0]);
    // Overlay lines draw too, without a new scene.
    renderer.set_overlay(vec![Line {
        points: vec![[-1., 0., 0.5], [1., 0., 0.5]],
        width: 4.,
        color: 0x00ff00ff,
    }]);
    let pixels = target.frame(&renderer).unwrap();
    assert_eq!(target.pixel(&pixels, off), [0, 255, 0]);
    assert_eq!(target.pixel(&pixels, on), [255, 0, 0]);
}

#[test]
fn octahedral_normals_round_trip() {
    for n in [
        [0., 1., 0.],
        [0., 0., -1.],
        [0.3, -0.8, 0.52],
        [-0.6, 0.1, -0.79],
    ] {
        let n = math::normalize(n).unwrap();
        let packed = octahedral(n);
        let half = |v: u32| f32::from(v as u16 as i16) / 32767.;
        let (x, y) = (half(packed), half(packed >> 16));
        let mut d = [x, y, 1. - x.abs() - y.abs()];
        let t = (-d[2]).max(0.);
        d[0] += if d[0] >= 0. { -t } else { t };
        d[1] += if d[1] >= 0. { -t } else { t };
        let d = math::normalize(d).unwrap();
        assert!(math::dot(d, n) > 0.9999, "{n:?} -> {d:?}");
    }
}

#[test]
fn picks_name_the_instance_under_the_cursor_and_respect_cutouts() {
    let mut target = Offscreen::new(320, 320).unwrap();
    let renderer = Renderer::default();
    let (scene, _) = scene(Material::Flat);
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    // Straight down, orthographic.
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    let view = target.bounds();
    let at = |p: Vec3| camera.project(p, view, world);
    let mut pick = |p: Vec3| target.pick(&renderer, at(p)).unwrap();
    let hit = pick([0.1, 0.5, 0.1]).unwrap();
    assert_eq!(hit.id, Some(7));
    assert!((hit.position[1] - 0.5).abs() < 1e-2, "{:?}", hit.position);
    assert!((hit.position[0] - 0.1).abs() < 0.05, "{:?}", hit.position);
    assert_eq!(pick([0.9, 0.5, -3.]).and_then(|h| h.id), Some(8));
    assert!(pick([3., 0., 3.]).is_none());

    let mut data = cube(0xffffff, Material::Lit).data().clone();
    // Left half transparent.
    data.texture = Some(Image::from(
        Texture::from_rgba(2, 1, vec![0, 0, 0, 0, 255, 255, 255, 255]).unwrap(),
    ));
    renderer.set_scene(&Scene {
        batches: vec![single(data, math::IDENTITY)],
        light: neutral(),
        ..Default::default()
    });
    target.settle(&renderer).unwrap();
    let mut pick = |p: Vec3| target.pick(&renderer, at(p)).unwrap();
    // The top face (axis 1) maps u to z; solid where u > 0.5 means z > 0.
    assert!(pick([0., 0.5, 0.3]).is_some_and(|h| h.position[1] > 0.4));
    // Top and bottom faces share the mapping: nothing solid down this column.
    assert!(pick([0., 0.5, -0.3]).is_none());
}

#[test]
fn renders_instanced_meshes_and_markers_offscreen() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    let (scene, _) = scene(Material::Lit);
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::perspective();
    target.aim(&renderer, camera, world);
    assert!(renderer.pending());
    let pixels = target.settle(&renderer).unwrap();
    let bounds = target.bounds();
    let center = camera.project([0.; 3], bounds, world);
    let [r, g, b] = target.pixel(&pixels, center);
    assert!(b > 150 && b > r && g > r, "cube center color {r} {g} {b}");
    assert_eq!(target.pixel(&pixels, point(px(2.), px(2.))), [0, 0, 0]);
    let marker = camera.project([0., 2., 0.], bounds, world);
    // The ring's hole shows the background; its edge is marker yellow.
    assert_eq!(target.pixel(&pixels, marker), [0, 0, 0]);
    let [r, g, _] = target.pixel(&pixels, marker + point(px(9.), px(0.)));
    assert!(r > 200 && g > 150, "marker ring {r} {g}");
    let hit = target.pick(&renderer, center).unwrap();
    assert_eq!(hit.and_then(|h| h.id), Some(7));

    // The same scene through a perspective lens: still drawn and pickable.
    let lens = Camera { fov: 1., ..camera };
    target.aim(&renderer, lens, world);
    let pixels = target.frame(&renderer).unwrap();
    let center = lens.project([0., 0.5, 0.], bounds, world);
    let [r, _, b] = target.pixel(&pixels, center);
    assert!(b > 150 && b > r, "perspective cube color {r} {b}");
    let hit = target.pick(&renderer, center).unwrap();
    assert_eq!(hit.and_then(|h| h.id), Some(7));
}

#[test]
fn sun_shadows_darken_the_ground_and_respect_cutouts_and_opt_outs() {
    let mut target = Offscreen::new(320, 320).unwrap();
    // Sun up and toward +X: a caster at height 4 shadows the ground 2 units
    // toward -X of itself.
    let light = Light {
        sun: [1., 2., 0.],
        sun_color: [0.7; 3],
        sky: [0.3; 3],
        ground: [0.3; 3],
        shadows: Some(Shadows {
            resolution: 1024,
            softness: 1.,
        }),
        ..Default::default()
    };
    let ground = single(quad(10., 0., Material::Lit), math::IDENTITY);
    let block = |x: f32, z: f32, casts_shadow| {
        let mut data = cube(0xffffff, Material::Lit).data().clone();
        data.casts_shadow = casts_shadow;
        single(data, math::translate([x, 4., z]))
    };
    // A cutout card: transparent where u < 0.5, i.e. x < 4.
    let mut card = quad(1., 4., Material::Lit);
    card.positions.iter_mut().for_each(|p| p[0] += 4.);
    card.texture = Some(Image::from(
        Texture::from_rgba(2, 1, vec![0, 0, 0, 0, 255, 255, 255, 255]).unwrap(),
    ));
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![
            ground,
            block(0., -4., true),
            block(0., 4., false),
            single(card, math::IDENTITY),
        ],
        light,
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 22.,
    };
    let camera = Camera {
        fov: 0.9,
        ..Default::default()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    // Ambient 0.3 in shadow; 0.3 + 0.7 * cos(26.6°) = 0.93 in the sun.
    let shadowed = at([-2., 0., -4.]);
    let lit = at([-2., 0., 0.]);
    near(shadowed, [77; 3], 6);
    near(lit, [237; 3], 6);
    // The opted-out block casts nothing.
    near(at([-2., 0., 4.]), [237; 3], 6);
    // The card's opaque half shadows; its cut-out half lets the sun through.
    near(at([4.5 - 2., 0., 0.5]), [77; 3], 6);
    near(at([3.5 - 2., 0., 0.5]), [237; 3], 6);
    // Ground under the camera is pickable through the perspective lens.
    let cursor = camera.project([-2., 0., 0.], target.bounds(), world);
    let hit = target.pick(&renderer, cursor).unwrap();
    assert!(hit.is_some_and(|h| h.position[1].abs() < 2e-2), "{hit:?}");
}

#[test]
fn blended_water_mixes_with_what_is_behind_and_fades_near_the_shore() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let red = |half, y| {
        let mut ground = quad(half, y, Material::Lit);
        ground.colors = vec![0xff0000; 4];
        ground
    };
    let water = |fade| {
        Material::Blend(Blend {
            color: 0x0000ff,
            opacity: 0.5,
            fade,
            fresnel: 0.,
            depth: 0.,
        })
    };
    // Red ground under the left half only; water over everything, with a
    // fading strip over x > 2 where the ground sits just one unit below.
    let mut ground = red(1., -1.);
    ground.positions = vec![
        [-6., -1., -2.],
        [0., -1., -2.],
        [0., -1., 2.],
        [-6., -1., 2.],
    ];
    let mut shore = red(1., 0.);
    shore.positions = vec![[2., -1., -2.], [6., -1., -2.], [6., -1., 2.], [2., -1., 2.]];
    let mut sea = quad(1., 0., water(0.));
    sea.positions = vec![[-6., 0., -2.], [2., 0., -2.], [2., 0., 2.], [-6., 0., 2.]];
    let mut shallows = quad(1., 0., water(10.));
    shallows.positions = vec![[2., 0., -2.], [6., 0., -2.], [6., 0., 2.], [2., 0., 2.]];
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: [ground, shore, sea, shallows]
            .map(|m| single(m, math::IDENTITY))
            .to_vec(),
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 10.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    // Half blue over red, and half blue over the black background.
    near(at([-3., 0., 0.]), [128, 0, 128], 3);
    near(at([1., 0., 0.]), [0, 0, 128], 3);
    // One unit of a ten-unit fade leaves a tenth of the opacity.
    near(at([4., 0., 0.]), [242, 0, 13], 4);
}

#[test]
fn splat_normal_layers_bend_the_lit_normal() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let solid = |rgba: [u8; 4]| Image::from(Texture::from_rgba(4, 4, rgba.repeat(16)).unwrap());
    // Left half layer 0 with a flat normal map, right half layer 1 whose
    // normals lean toward +X, where the sun is.
    let control = |left: [u8; 4], right: [u8; 4]| {
        let row = [left, left, right, right].concat();
        Arc::new(ControlMap::new(4, 2, row.repeat(2)).unwrap())
    };
    let splat = Arc::new(Splat {
        layers: vec![solid([200, 200, 200, 255]), solid([200, 200, 200, 255])],
        tiling: vec![1., 1.],
        sharpness: vec![],
        normals: vec![solid([128, 128, 255, 255]), solid([230, 128, 180, 255])],
        indices: vec![control([0; 4], [1, 0, 0, 0])],
        weights: vec![control([255, 0, 0, 0], [255, 0, 0, 0])],
    });
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(quad(2., 0., Material::Splat(splat)), math::IDENTITY)],
        light: Light {
            sun: [1., 0.3, 0.],
            sun_color: [1.; 3],
            sky: [0.1; 3],
            ground: [0.1; 3],
            ..Default::default()
        },
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 5.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world))[0];
    let (flat, leaning) = (at([-1., 0., 0.]), at([1., 0., 0.]));
    assert!(leaning > flat + 40, "flat {flat}, leaning {leaning}");
}

#[test]
fn splat_weights_interpolate_across_palette_tiles() {
    // 16 control texels across two 8-texel palette tiles: red layer 0 on
    // the left tile, green layer 1 on the right. Midway the weights blend
    // half and half, as they did before palettes, with no seam.
    let mut target = Offscreen::new(320, 240).unwrap();
    let solid = |rgba: [u8; 4]| Arc::new(Texture::from_rgba(4, 4, rgba.repeat(16)).unwrap());
    let row = |left: [u8; 4], right: [u8; 4]| [left.repeat(8), right.repeat(8)].concat();
    let control =
        |left, right| Arc::new(ControlMap::new(16, 2, row(left, right).repeat(2)).unwrap());
    let splat = Arc::new(Splat {
        layers: vec![
            solid([255, 0, 0, 255]).into(),
            solid([0, 255, 0, 255]).into(),
        ],
        tiling: vec![1., 1.],
        sharpness: vec![],
        normals: vec![],
        indices: vec![control([0; 4], [1, 0, 0, 0])],
        weights: vec![control([255, 0, 0, 0], [255, 0, 0, 0])],
    });
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(quad(2., 0., Material::Splat(splat)), math::IDENTITY)],
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 5.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    near(at([-1., 0., 0.]), [255, 0, 0], 1);
    near(at([1., 0., 0.]), [0, 255, 0], 1);
    near(at([0., 0., 0.]), [128, 128, 0], 24);
}

#[test]
fn splat_layers_follow_index_and_weight_textures() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let solid = |rgba: [u8; 4]| Arc::new(Texture::from_rgba(4, 4, rgba.repeat(16)).unwrap());
    // 4x2 control texels: the left two columns are all layer 0; the right
    // two are layer 1 (first pair) under half of layer 2 (second pair).
    let control = |left: [u8; 4], right: [u8; 4]| {
        let row = [left, left, right, right].concat();
        Arc::new(ControlMap::new(4, 2, row.repeat(2)).unwrap())
    };
    let splat = Arc::new(Splat {
        layers: vec![
            solid([255, 0, 0, 255]).into(),
            solid([0, 255, 0, 255]).into(),
            solid([0, 0, 255, 255]).into(),
        ],
        tiling: vec![1., 2., 3.],
        sharpness: vec![],
        normals: vec![],
        indices: vec![control([0; 4], [1, 0, 0, 0]), control([0; 4], [2, 0, 0, 0])],
        weights: vec![
            control([255, 0, 0, 0], [128, 0, 0, 0]),
            control([0; 4], [128, 0, 0, 0]),
        ],
    });
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(
            quad(2., 0., Material::Splat(splat.clone())),
            math::IDENTITY,
        )],
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 5.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    near(at([-1., 0., 0.]), [255, 0, 0], 1);
    near(at([1., 0., 0.]), [0, 128, 128], 2);

    // Painting the right half to layer 0 shows on the next frame, through
    // the same splat and scene.
    let rect = crate::heightfield::Rect {
        x0: 2,
        z0: 0,
        x1: 4,
        z1: 2,
    };
    splat.indices[0].write(rect, &[0; 16]);
    splat.weights[0].write(rect, &[255, 0, 0, 0].repeat(4));
    splat.weights[1].write(rect, &[0; 16]);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    near(at([1., 0., 0.]), [255, 0, 0], 1);
    near(at([-1., 0., 0.]), [255, 0, 0], 1);
    // The sharpening check below shares these index maps; put them back.
    splat.indices[0].write(rect, &[1, 0, 0, 0].repeat(4));

    // With sharpness 2 and opaque images a layer covers at 3x its weight:
    // a quarter of layer 2 over layer 1 shows three quarters.
    let sharp = Arc::new(Splat {
        sharpness: vec![1., 2., 2.],
        weights: vec![
            control([255, 0, 0, 0], [192, 0, 0, 0]),
            control([0; 4], [64, 0, 0, 0]),
        ],
        ..(*splat).clone()
    });
    renderer.set_scene(&Scene {
        batches: vec![single(quad(2., 0., Material::Splat(sharp)), math::IDENTITY)],
        light: neutral(),
        ..Default::default()
    });
    let pixels = target.settle(&renderer).unwrap();
    let [r, g, b] = target.pixel(
        &pixels,
        camera.project([1., 0., 0.], target.bounds(), world),
    );
    near([r, g, b], [0, 63, 192], 2);
}

#[test]
fn large_textures_stream_across_frames_without_blocking() {
    let mut target = Offscreen::new(64, 64).unwrap();
    // 2048² RGBA is 16 MB: two frames' worth of upload budget.
    let side = 2048;
    let mut data = quad(1., 0., Material::Lit);
    data.texture = Some(Image::from(
        Texture::from_rgba(side, side, vec![200; (side * side * 4) as usize]).unwrap(),
    ));
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(data, math::IDENTITY)],
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 2.,
    };
    target.aim(&renderer, Camera::default(), world);
    let mut frames = 0;
    while renderer.pending() {
        let started = std::time::Instant::now();
        target.frame(&renderer).unwrap();
        frames += 1;
        assert!(frames < 10);
        // Generous: CPU copies under the budget, plus a debug-build submit.
        assert!(started.elapsed().as_millis() < 500);
    }
    assert!(frames >= 2, "a 16 MB texture fit in one frame's budget");
    let pixels = target.frame(&renderer).unwrap();
    near(target.pixel(&pixels, point(px(32.), px(32.))), [200; 3], 1);
}

#[test]
fn small_meshes_upload_many_per_frame() {
    let mut target = Offscreen::new(64, 64).unwrap();
    // Distinct meshes and textures, as a town load has: ~1000 of each.
    let count = 192;
    let batches = (0..count)
        .map(|i| {
            let mut data = quad(0.004, 0., Material::Lit);
            let shade = 40 + (i % 200) as u8;
            data.texture = Some(Image::from(
                Texture::from_rgba(16, 16, vec![shade; 16 * 16 * 4]).unwrap(),
            ));
            let x = (i % 16) as f32 / 8. - 1.;
            let z = (i / 16) as f32 / 8. - 1.;
            single(data, math::translate([x, 0., z]))
        })
        .collect();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches,
        light: neutral(),
        ..Default::default()
    });
    target.aim(
        &renderer,
        Camera::default(),
        Extent {
            center: [0.; 3],
            size: 2.,
        },
    );
    let mut frames = 0;
    while renderer.pending() {
        target.frame(&renderer).unwrap();
        frames += 1;
        assert!(frames <= count);
    }
    // 32 batches a frame at most, each a few microseconds; a frame that
    // pays milliseconds per item would upload one mesh at a time.
    assert!(frames < count / 4, "{frames} frames for {count} meshes");
}

/// A BC1 block of one opaque color, quantized to 5:6:5.
fn bc1(rgb: [u8; 3]) -> Vec<u8> {
    let [r, g, b] = rgb.map(u16::from);
    let c0 = (r >> 3) << 11 | (g >> 2) << 5 | b >> 3;
    // c0 > c1 picks four-color mode, where index 0 is c0, opaque.
    let mut block = c0.to_le_bytes().to_vec();
    block.extend([0; 6]);
    block
}

/// A BC7 mode 6 block of one color. Channels are 7 bits plus a shared
/// p-bit, so all four must be odd.
fn bc7(rgba: [u8; 4]) -> Vec<u8> {
    assert!(rgba.iter().all(|c| c % 2 == 1));
    let mut bits = 1_u128 << 6;
    for (i, c) in rgba.iter().enumerate() {
        // Both endpoints of each channel; index 0 everywhere picks the first.
        bits |= u128::from(c >> 1) << (7 + i * 14);
        bits |= u128::from(c >> 1) << (14 + i * 14);
    }
    bits |= 0b11 << 63;
    bits.to_le_bytes().to_vec()
}

/// A compressed image whose every block in level `l` is `block(l)`.
fn compressed(
    format: Block,
    width: u32,
    height: u32,
    levels: usize,
    block: impl Fn(usize) -> Vec<u8>,
) -> Image {
    let mips = (0..levels)
        .map(|l| {
            let (w, h) = ((width >> l).max(1), (height >> l).max(1));
            block(l).repeat(format.size(w, h) / format.bytes())
        })
        .collect();
    let image = crate::texture::Compressed {
        block: format,
        width,
        height,
        mips,
        srgb: false,
    };
    image.validate().unwrap();
    image.into()
}

#[test]
fn block_helpers_decode_to_their_colors() {
    let Image::Compressed(c) = compressed(Block::Bc7, 4, 4, 1, |_| bc7([201, 101, 51, 255])) else {
        unreachable!()
    };
    assert_eq!(c.decode(0).unwrap().rgba[..4], [201, 101, 51, 255]);
    let Image::Compressed(c) = compressed(Block::Bc1, 4, 4, 1, |_| bc1([128; 3])) else {
        unreachable!()
    };
    assert_eq!(c.decode(0).unwrap().rgba[..4], [132, 130, 132, 255]);
}

#[test]
fn meshes_no_scene_uses_are_freed() {
    let mut target = Offscreen::new(32, 32).unwrap();
    let renderer = Renderer::default();
    let world = Extent {
        center: [0.; 3],
        size: 4.,
    };
    let (scene, _) = scene(Material::Flat);
    renderer.set_scene(&scene);
    target.render(&renderer, Camera::default(), world).unwrap();
    let first = renderer.state.lock().unwrap().assets.meshes.len();
    assert!(first > 0);
    renderer.set_scene(&Scene {
        batches: vec![single(quad(1., 0., Material::Flat), math::IDENTITY)],
        ..Default::default()
    });
    drop(scene);
    target.settle(&renderer).unwrap();
    // One more frame collects what the old scene held.
    target.frame(&renderer).unwrap();
    assert_eq!(renderer.state.lock().unwrap().assets.meshes.len(), 1);
}

/// Points `renderer` at a device without BC support, so compressed images
/// take the CPU decode path.
fn without_bc(renderer: &Renderer) {
    renderer.state.lock().unwrap().assets.bc = Some(false);
}

#[test]
fn bc1_and_bc7_textures_render_as_stored_with_and_without_device_support() {
    for bc in [true, false] {
        let mut target = Offscreen::new(320, 240).unwrap();
        let textured = |x: f32, texture: Image| {
            let mut data = quad(1., 0., Material::Lit);
            data.positions.iter_mut().for_each(|p| p[0] += x);
            data.texture = Some(texture);
            single(data, math::IDENTITY)
        };
        // Mid gray catches an sRGB view, which would darken it.
        let gray = compressed(Block::Bc1, 8, 8, 4, |_| bc1([128; 3]));
        let orange = compressed(Block::Bc7, 16, 16, 5, |_| bc7([201, 101, 51, 255]));
        let renderer = Renderer::default();
        if !bc {
            without_bc(&renderer);
        }
        renderer.set_scene(&Scene {
            batches: vec![textured(-2., gray), textured(2., orange)],
            light: neutral(),
            ..Default::default()
        });
        let world = Extent {
            center: [0.; 3],
            size: 8.,
        };
        let camera = Camera::default();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
        near(at([-2., 0., 0.]), [132, 130, 132], 2);
        near(at([2., 0., 0.]), [201, 101, 51], 2);
        // On a BC-capable device the textures stay compressed.
        let state = renderer.state.lock().unwrap();
        let assets = &state.assets;
        if assets.bc == Some(true) {
            assert!(assets.textures.values().all(|t| !t.decode));
        }
        drop(state);
    }
}

#[test]
fn releasable_textures_let_go_once_uploaded_and_reload_for_a_new_device() {
    use crate::texture::Releasable;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Image::Compressed(orange) = compressed(Block::Bc7, 16, 16, 5, |_| bc7([201, 101, 51, 255]))
    else {
        unreachable!()
    };
    let reloads = Arc::new(AtomicUsize::new(0));
    let image = Arc::new(
        Releasable::new((*orange).clone(), {
            let (orange, reloads) = (orange.clone(), reloads.clone());
            move || {
                reloads.fetch_add(1, Ordering::Relaxed);
                Some((*orange).clone())
            }
        })
        .unwrap(),
    );
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::default();
    // A second renderer stands for a new device: it uploads again.
    for pass in 0..2 {
        let mut target = Offscreen::new(320, 240).unwrap();
        let renderer = Renderer::default();
        let mut data = quad(1., 0., Material::Lit);
        data.texture = Some(Image::Releasable(image.clone()));
        renderer.set_scene(&Scene {
            batches: vec![single(data, math::IDENTITY)],
            light: neutral(),
            ..Default::default()
        });
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        near(
            target.pixel(&pixels, camera.project([0.; 3], target.bounds(), world)),
            [201, 101, 51],
            2,
        );
        assert!(image.is_released(), "pass {pass}");
        assert_eq!(reloads.load(Ordering::Relaxed), pass);
    }
}

#[test]
fn minified_textures_sample_their_small_mips() {
    let mut target = Offscreen::new(320, 320).unwrap();
    // Level 0 red, every smaller level green.
    let checker = || {
        compressed(Block::Bc1, 64, 64, 7, |l| {
            bc1(if l == 0 { [255, 0, 0] } else { [0, 255, 0] })
        })
    };
    let at_x = |x: f32, half: f32| {
        let mut data = quad(half, 0., Material::Lit);
        data.positions.iter_mut().for_each(|p| p[0] += x);
        data.texture = Some(checker());
        single(data, math::IDENTITY)
    };
    for (half, expected) in [
        // 64 texels over about 5 pixels: level 3 or 4.
        (0.5, [0, 255, 0]),
        // 64 texels over about 290 pixels: level 0.
        (30., [255, 0, 0]),
    ] {
        for bc in [true, false] {
            let renderer = Renderer::default();
            if !bc {
                without_bc(&renderer);
            }
            renderer.set_scene(&Scene {
                batches: vec![at_x(0., half)],
                light: neutral(),
                ..Default::default()
            });
            let world = Extent {
                center: [0.; 3],
                size: 64.,
            };
            let camera = Camera::default();
            target.aim(&renderer, camera, world);
            let pixels = target.settle(&renderer).unwrap();
            let center = camera.project([0.; 3], target.bounds(), world);
            near(target.pixel(&pixels, center), expected, 4);
        }
    }
}

#[test]
fn compressed_splat_layers_share_the_smallest_size() {
    let control = |left: [u8; 4], right: [u8; 4]| {
        let row = [left, left, right, right].concat();
        Arc::new(ControlMap::new(4, 2, row.repeat(2)).unwrap())
    };
    let layer =
        |side: u32, levels, rgb| compressed(Block::Bc1, side, side, levels, move |_| bc1(rgb));
    let splat = Arc::new(Splat {
        // 16² starts two levels in, 8² one, to meet at 4².
        layers: vec![
            layer(16, 5, [255, 0, 0]),
            layer(8, 4, [0, 255, 0]),
            layer(4, 3, [0, 0, 255]),
        ],
        tiling: vec![1., 2., 3.],
        sharpness: vec![],
        normals: vec![],
        indices: vec![control([0; 4], [1, 0, 0, 0]), control([0; 4], [2, 0, 0, 0])],
        weights: vec![
            control([255, 0, 0, 0], [128, 0, 0, 0]),
            control([0; 4], [128, 0, 0, 0]),
        ],
    });
    // An 8² layer cannot reach 4² without mips.
    let bad = Splat {
        layers: vec![layer(8, 1, [255, 0, 0]), layer(4, 3, [0, 0, 255])],
        ..(*splat).clone()
    };
    assert!(bad.validate().is_err());
    let layout = Layout::new(&splat.layers).unwrap();
    assert_eq!(
        (layout.width, layout.levels, layout.skips.as_slice()),
        (4, 3, &[2, 1, 0][..])
    );

    for bc in [true, false] {
        let mut target = Offscreen::new(320, 240).unwrap();
        let renderer = Renderer::default();
        if !bc {
            without_bc(&renderer);
        }
        renderer.set_scene(&Scene {
            batches: vec![single(
                quad(2., 0., Material::Splat(splat.clone())),
                math::IDENTITY,
            )],
            light: neutral(),
            ..Default::default()
        });
        let world = Extent {
            center: [0.; 3],
            size: 5.,
        };
        let camera = Camera::default();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
        near(at([-1., 0., 0.]), [255, 0, 0], 1);
        near(at([1., 0., 0.]), [0, 128, 128], 2);
    }

    // A splat rebuilt over the same layers (undoing paint) shares the
    // uploaded layer array instead of uploading its own.
    let mut target = Offscreen::new(64, 64).unwrap();
    let renderer = Renderer::default();
    let world = Extent {
        center: [0.; 3],
        size: 5.,
    };
    let rebuilt = || Material::Splat(Arc::new((*splat).clone()));
    renderer.set_scene(&Scene {
        batches: vec![
            single(quad(1., 0., rebuilt()), math::IDENTITY),
            single(quad(1., 0., rebuilt()), math::translate([2., 0., 0.])),
        ],
        light: neutral(),
        ..Default::default()
    });
    target.render(&renderer, Camera::default(), world).unwrap();
    let state = renderer.state.lock().unwrap();
    let splats: Vec<_> = state.assets.splats.values().collect();
    assert_eq!(splats.len(), 2);
    assert!(Arc::ptr_eq(&splats[0].layers, &splats[1].layers));
}

#[test]
fn picking_respects_compressed_cutouts_unless_alpha_is_ignored() {
    // 8x4 BC1: the left block transparent (three-color mode, index 3), the
    // right one opaque white.
    let transparent = [0, 0, 255, 255, 255, 255, 255, 255];
    let white = bc1([255; 3]);
    let image = crate::texture::Compressed {
        block: Block::Bc1,
        width: 8,
        height: 4,
        mips: vec![[transparent.to_vec(), white].concat()],
        srgb: false,
    };
    let mut data = cube(0xffffff, Material::Lit).data().clone();
    data.texture = Some(image.into());
    let renderer = Renderer::default();
    let mut target = Offscreen::new(320, 320).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 4.,
    };
    let camera = Camera::default();
    let view = target.bounds();
    let at = |p: Vec3| camera.project(p, view, world);
    for (ignore, solid_below) in [(false, false), (true, true)] {
        data.ignore_alpha = ignore;
        renderer.set_scene(&Scene {
            batches: vec![single(data.clone(), math::IDENTITY)],
            light: neutral(),
            ..Default::default()
        });
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        // The top face maps u to z: solid where z > 0.
        let hit = target.pick(&renderer, at([0., 0.5, 0.3])).unwrap();
        assert!(hit.is_some_and(|h| h.position[1] > 0.4), "{hit:?}");
        let hit = target.pick(&renderer, at([0., 0.5, -0.3])).unwrap();
        assert_eq!(hit.is_some(), solid_below, "ignore alpha {ignore}");
    }
}

/// A flat `side`² heightfield of zeros at 0.5 spacing, centered on the origin.
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

/// Sets every sample of `rect` to `h` and hands the edit to `map`.
fn raise(field: &mut Heightfield, map: &HeightMap, rect: crate::heightfield::Rect, h: f32) {
    for z in rect.z0..rect.z1 {
        for x in rect.x0..rect.x1 {
            let i = field.grid.index(x, z);
            field.heights[i] = h;
        }
    }
    map.update(rect, &field.heights);
}

#[test]
fn terrain_takes_colors_from_its_map_and_thins_out_with_distance() {
    let mut target = Offscreen::new(256, 256).unwrap();
    // 129² samples over 64 units: four tiles of 64 cells.
    let side = 129;
    let field = flat_field(side);
    let half = (side - 1) as f32 * 0.25;
    // Red west of the middle, blue east.
    let texels: Vec<u8> = (0..side * side)
        .flat_map(|i| {
            if i % side < side / 2 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            }
        })
        .collect();
    let colors = Arc::new(ControlMap::new(side, side, texels).unwrap());
    let terrain = Arc::new(Terrain {
        heights: Arc::new(HeightMap::new(&field).unwrap()),
        colors: colors.clone(),
        color_gain: 1.,
        tints: false,
        material: Material::Lit,
        texture: None,
    });
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        terrain: Some(terrain.clone()),
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: half * 2.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let bounds = target.bounds();
    let at = |x: f32| camera.project([x, 0., 0.], bounds, world);
    assert_eq!(target.pixel(&pixels, at(-half * 0.5)), [255, 0, 0]);
    assert_eq!(target.pixel(&pixels, at(half * 0.5)), [0, 0, 255]);
    let near = renderer.drawn().triangles;
    // Recoloring writes texels; no new scene, no meshes.
    colors.write(
        Rect {
            x0: 0,
            z0: 0,
            x1: side / 2,
            z1: side,
        },
        &[0, 255, 0, 255].repeat(side / 2 * side),
    );
    let pixels = target.frame(&renderer).unwrap();
    assert_eq!(target.pixel(&pixels, at(-half * 0.5)), [0, 255, 0]);
    // A hundred times farther, the four tiles fall to coarse strides.
    let far = Camera {
        zoom: camera.zoom / 100.,
        ..camera
    };
    target.aim(&renderer, far, world);
    target.frame(&renderer).unwrap();
    let distant = renderer.drawn().triangles;
    assert!(distant * 16 < near, "{distant} triangles far, {near} near");
}

#[test]
fn displaced_tiles_follow_height_updates_without_a_new_scene() {
    let mut target = Offscreen::new(320, 320).unwrap();
    // 33² samples over 16 units; four 8-unit tiles share one flat grid mesh.
    let mut field = flat_field(33);
    let map = Arc::new(HeightMap::new(&field).unwrap());
    let corner = Grid {
        width: 17,
        depth: 17,
        spacing: 0.5,
        origin: [0.; 2],
    };
    let tile = Heightfield::new(corner, vec![0.; 17 * 17]).tile(0, 0, 1);
    // No UVs: the texture spans the grid, red on the left half, green on the
    // right. Eight texels keep the filter from wrapping across the edge.
    let grid_mesh = Arc::new(
        Mesh::new(MeshData {
            positions: tile.positions,
            indices: tile.indices,
            texture: Some(Image::from(
                Texture::from_rgba(
                    8,
                    1,
                    [[255, 0, 0, 255].repeat(4), [0, 255, 0, 255].repeat(4)].concat(),
                )
                .unwrap(),
            )),
            heights: Some(map.clone()),
            ..Default::default()
        })
        .unwrap(),
    );
    let mut batches = [(-8., -8.), (0., -8.), (-8., 0.), (0., 0.)]
        .map(|(x, z)| Batch {
            mesh: grid_mesh.clone(),
            instances: Arc::new(Instances {
                matrices: vec![math::translate([x, 0., z])],
                ids: vec![],
            }),
        })
        .to_vec();
    let mut lid = quad(2., 1., Material::Flat);
    lid.colors = vec![0x0000ff; 4];
    batches.push(single(lid, math::IDENTITY));
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches,
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 16.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |target: &Offscreen, pixels: &[u8], p: Vec3| {
        target.pixel(pixels, camera.project(p, target.bounds(), world))
    };
    near(at(&target, &pixels, [-5., 0., 3.]), [255, 0, 0], 2);
    near(at(&target, &pixels, [5., 0., -3.]), [0, 255, 0], 2);
    near(at(&target, &pixels, [-1., 0., 0.]), [0, 0, 255], 2);

    // Samples 13..20 span world -1.5..=1.5: raise them through the lid.
    let rect = crate::heightfield::Rect {
        x0: 13,
        z0: 13,
        x1: 20,
        z1: 20,
    };
    raise(&mut field, &map, rect, 3.);
    assert!(!renderer.pending());
    let pixels = target.frame(&renderer).unwrap();
    near(at(&target, &pixels, [-1., 0., 0.]), [255, 0, 0], 2);
    near(at(&target, &pixels, [1., 0., 0.]), [0, 255, 0], 2);
    // Past the raised samples the ground ramps back under the lid.
    near(at(&target, &pixels, [1.9, 0., 1.9]), [0, 0, 255], 2);
    near(at(&target, &pixels, [-5., 0., 3.]), [255, 0, 0], 2);
    // Picks find what is visible: the lid where it shows, nothing where
    // terrain without ids covers it.
    let cursor = camera.project([1.9, 1., 1.9], target.bounds(), world);
    let hit = target.pick(&renderer, cursor).unwrap();
    assert!(
        hit.is_some_and(|h| (h.position[1] - 1.).abs() < 2e-2),
        "{hit:?}"
    );
    for p in [[-5., 0., 3.], [-1., 0., 0.]] {
        let cursor = camera.project(p, target.bounds(), world);
        assert!(target.pick(&renderer, cursor).unwrap().is_none(), "{p:?}");
    }

    // More edits in one frame than the device log keeps: a full re-upload.
    for _ in 0..100 {
        raise(&mut field, &map, rect, 0.);
    }
    let pixels = target.frame(&renderer).unwrap();
    near(at(&target, &pixels, [-1., 0., 0.]), [0, 0, 255], 2);
    near(at(&target, &pixels, [1., 0., 0.]), [0, 0, 255], 2);
}

#[test]
fn displaced_terrain_shadows_and_normals_follow_sculpting() {
    let mut target = Offscreen::new(320, 320).unwrap();
    // Sun up and toward +X, as in the shadow test: a top at height 4 shades
    // the ground 2 units toward -X.
    let light = Light {
        sun: [1., 2., 0.],
        sun_color: [0.7; 3],
        sky: [0.3; 3],
        ground: [0.3; 3],
        shadows: Some(Shadows {
            resolution: 1024,
            softness: 1.,
        }),
        ..Default::default()
    };
    let mut field = flat_field(41);
    let map = Arc::new(HeightMap::new(&field).unwrap());
    let tile = field.tile(0, 0, 1);
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(
            MeshData {
                positions: tile.positions,
                uvs: tile.uvs,
                indices: tile.indices,
                heights: Some(map.clone()),
                ..Default::default()
            },
            math::IDENTITY,
        )],
        light,
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 22.,
    };
    let camera = Camera {
        fov: 0.9,
        ..Default::default()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |target: &Offscreen, pixels: &[u8], p: Vec3| {
        target.pixel(pixels, camera.project(p, target.bounds(), world))
    };
    // 0.3 + 0.7 * cos(26.6°) on flat ground facing up.
    near(at(&target, &pixels, [-2.2, 0., 0.]), [237; 3], 6);

    // A pillar over world -0.5..=0.5, its sides sloping out to ±1.
    let rect = crate::heightfield::Rect {
        x0: 19,
        z0: 19,
        x1: 22,
        z1: 22,
    };
    raise(&mut field, &map, rect, 4.);
    let pixels = target.frame(&renderer).unwrap();
    near(at(&target, &pixels, [-2.2, 0., 0.]), [77; 3], 6);
    near(at(&target, &pixels, [-2.2, 0., 3.]), [237; 3], 6);
    near(at(&target, &pixels, [0., 4., 0.]), [237; 3], 6);
    // The east side's normal is (4, 1, 0) normalized: 0.3 + 0.7 * 0.651.
    near(at(&target, &pixels, [0.75, 2., 0.]), [193; 3], 8);
    // The west side faces away from the sun: ambient only.
    near(at(&target, &pixels, [-0.75, 2., 0.]), [77; 3], 6);
}

#[test]
fn placements_cull_pick_levels_by_screen_size_and_move_in_place() {
    placements_cull_pick_levels(true);
}

#[test]
fn per_piece_placements_cull_pick_levels_and_move_in_place() {
    placements_cull_pick_levels(false);
}

#[test]
fn placements_behind_nearer_ones_are_culled_from_the_arena() {
    for arena in [true, false] {
        let renderer = Renderer::default();
        if !arena {
            without_arena(&renderer);
        }
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(cube(0xff0000, Material::Flat)), 0.)],
            impostor: None,
        }]);
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
        // A cube right behind the first on the line of sight, eleven times
        // as far: wholly hidden, with room for the pyramid's coarse texels.
        let eye = camera.eye(target.bounds(), world).unwrap();
        let hidden: Vec3 = std::array::from_fn(|a| eye[a] * -10.);
        renderer.set_placements(&[0, 0], &[math::IDENTITY, math::translate(hidden)]);
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        target.frame(&renderer).unwrap();
        target.frame(&renderer).unwrap();
        // Only the arena culls by occlusion; per-piece draws keep both.
        let expected = if arena { 1 } else { 2 };
        assert_eq!(renderer.drawn().placed[0], expected, "arena {arena}");
        // Moving the near cube away reveals the far one in that same frame:
        // the second phase finds it against the new depth.
        renderer.move_placements(0, &[math::translate([0., 50., 0.])]);
        let center = camera.project([0.; 3], target.bounds(), world);
        let hit = target.pick(&renderer, center).unwrap().unwrap();
        assert_eq!(hit.placement, Some(1), "arena {arena}");
    }
}

#[test]
fn thinning_meshes_drop_their_last_ranked_parts_far_away() {
    // Two quads facing up: the left always drawn (rank 0), the right ranked
    // last. Kept whole when `full` is tiny; at a quarter, the right goes.
    let quads = |full: f32| {
        let mut data = MeshData {
            material: Material::Flat,
            ..Default::default()
        };
        let mut parts = Vec::new();
        for (x, rank) in [(-1.5f32, 0f32), (1.5, 1.)] {
            let base = data.positions.len() as u32;
            data.positions.extend(
                [[-1., 0., -1.], [1., 0., -1.], [1., 0., 1.], [-1., 0., 1.]]
                    .map(|p: Vec3| [p[0] + x, 0., p[2]]),
            );
            data.indices.extend([0, 2, 1, 0, 3, 2].map(|k| base + k));
            parts.extend([[x, 0., 0., rank]; 4]);
        }
        data.colors = vec![0xff0000; 8];
        data.thinning = Some(Thinning {
            parts,
            full,
            least: 0.25,
        });
        Arc::new(Mesh::new(data).unwrap())
    };
    for (full, right) in [(1e-3, [255, 0, 0]), (1e6, [0, 0, 0])] {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(quads(full), 0.)],
            impostor: None,
        }]);
        renderer.set_placements(&[0], &[math::IDENTITY]);
        let mut target = Offscreen::new(320, 240).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 8.,
        };
        let camera = Camera::default();
        let pixels = target.render(&renderer, camera, world).unwrap();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
        assert_eq!(at([-1.5, 0., 0.]), [255, 0, 0], "full {full}");
        assert_eq!(at([1.5, 0., 0.]), right, "full {full}");
    }
}

#[test]
fn simplified_thinning_meshes_keep_every_part_in_rank_order() {
    // 20 clumps of an 8x8 grid on a bent card, 128 triangles each, then 20
    // single-triangle cards; ranks interleaved.
    let mut data = MeshData::default();
    let mut parts = Vec::new();
    for k in 0..40u32 {
        let rank = (k + 1) as f32 / 40.;
        let x0 = k as f32 * 2.;
        let base = data.positions.len() as u32;
        if k % 2 == 0 {
            let n = 9;
            for z in 0..n {
                for x in 0..n {
                    let (u, v) = (x as f32 / 8., z as f32 / 8.);
                    data.positions.push([x0 + u, (u * 3.).sin() * 0.1, v]);
                    data.uvs.push([u, v]);
                }
            }
            for z in 0..n - 1 {
                for x in 0..n - 1 {
                    let i = base + z * n + x;
                    data.indices
                        .extend([i, i + 1, i + n + 1, i, i + n + 1, i + n]);
                }
            }
        } else {
            data.positions
                .extend([[x0, 0., 0.], [x0 + 0.5, 0., 0.], [x0, 0., 0.5]]);
            data.uvs.extend([[0., 0.], [1., 0.], [0., 1.]]);
            data.indices.extend([base, base + 1, base + 2]);
        }
        parts.resize(data.positions.len(), [x0, 0., 0., rank]);
    }
    data.thinning = Some(Thinning {
        parts,
        full: 20.,
        least: 0.2,
    });
    let coarse = data.simplified(0.4).unwrap();
    assert!(coarse.indices.len() < data.indices.len() / 2);
    let t = coarse.thinning.as_ref().unwrap();
    assert_eq!(t.parts.len(), coarse.positions.len());
    let mut tris: HashMap<u32, usize> = HashMap::new();
    for tri in coarse.indices.as_chunks::<3>().0 {
        *tris
            .entry(t.parts[tri[0] as usize][3].to_bits())
            .or_default() += 1;
    }
    // Every clump is still there; single cards untouched.
    assert_eq!(tris.len(), 40);
    for k in (1..40).step_by(2) {
        assert_eq!(tris[&((k + 1) as f32 / 40.).to_bits()], 1);
    }
    // Triangles stay in rank order, as prefix draws need.
    Mesh::new(coarse).unwrap();
}

#[test]
fn compact_meshes_draw_as_their_full_data_does() {
    // Two thinning quads, the right ranked last, as the tests above.
    let quads = |compact: bool| {
        let mut data = MeshData {
            material: Material::Flat,
            ..Default::default()
        };
        let mut parts = Vec::new();
        for (x, rank) in [(-1.5f32, 0f32), (1.5, 1.)] {
            let base = data.positions.len() as u32;
            data.positions.extend(
                [[-1., 0., -1.], [1., 0., -1.], [1., 0., 1.], [-1., 0., 1.]]
                    .map(|p: Vec3| [p[0] + x, 0., p[2]]),
            );
            data.indices.extend([0, 2, 1, 0, 3, 2].map(|k| base + k));
            parts.extend([[x, 0., 0., rank]; 4]);
        }
        data.colors = vec![0xff8040; 8];
        data.thinning = Some(Thinning {
            parts,
            full: 1e6,
            least: 0.25,
        });
        let mesh = if compact {
            Mesh::compact(data).unwrap()
        } else {
            Mesh::new(data).unwrap()
        };
        assert_eq!(mesh.triangles(), 4);
        Arc::new(mesh)
    };
    assert!(quads(true).data().positions.is_empty(), "kept compressed");
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    for arena in [true, false] {
        let shots: Vec<Vec<u8>> = [false, true]
            .map(|compact| {
                let renderer = Renderer::default();
                if !arena {
                    without_arena(&renderer);
                }
                renderer.set_scene(&Scene {
                    light: neutral(),
                    ..Default::default()
                });
                renderer.set_pieces(vec![Piece {
                    levels: vec![(quads(compact), 0.)],
                    impostor: None,
                }]);
                renderer.set_placements(&[0], &[math::IDENTITY]);
                let mut target = Offscreen::new(320, 240).unwrap();
                target.render(&renderer, Camera::default(), world).unwrap()
            })
            .into();
        assert!(shots[0] == shots[1], "arena {arena}");
        assert!(shots[0].iter().any(|&b| b > 0), "drawn");
    }
}

#[test]
fn thinning_pieces_draw_and_thin_their_coarser_levels() {
    thinning_levels(true);
    thinning_levels(false);
}

fn thinning_levels(arena: bool) {
    // As above, with a green coarser level taking every size: the left quad
    // draws green, and the right goes as at full detail.
    let quads = |color: u32| {
        let mut data = MeshData {
            material: Material::Flat,
            ..Default::default()
        };
        let mut parts = Vec::new();
        for (x, rank) in [(-1.5f32, 0f32), (1.5, 1.)] {
            let base = data.positions.len() as u32;
            data.positions.extend(
                [[-1., 0., -1.], [1., 0., -1.], [1., 0., 1.], [-1., 0., 1.]]
                    .map(|p: Vec3| [p[0] + x, 0., p[2]]),
            );
            data.indices.extend([0, 2, 1, 0, 3, 2].map(|k| base + k));
            parts.extend([[x, 0., 0., rank]; 4]);
        }
        data.colors = vec![color; 8];
        data.thinning = Some(Thinning {
            parts,
            full: 1e6,
            least: 0.25,
        });
        Arc::new(Mesh::new(data).unwrap())
    };
    let renderer = Renderer::default();
    if !arena {
        without_arena(&renderer);
    }
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(quads(0xff0000), 1e9), (quads(0x00ff00), 0.)],
        impostor: None,
    }]);
    renderer.set_placements(&[0], &[math::IDENTITY]);
    let mut target = Offscreen::new(320, 240).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::default();
    let pixels = target.render(&renderer, camera, world).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    assert_eq!(at([-1.5, 0., 0.]), [0, 255, 0], "arena {arena}");
    // Without the arena nothing thins: the last bucket draws whole.
    let right = if arena { [0, 0, 0] } else { [0, 255, 0] };
    assert_eq!(at([1.5, 0., 0.]), right, "arena {arena}");
}

#[test]
fn thinning_pieces_past_sixteen_thousand_draws_still_share_the_arena() {
    // 1,300 thinning pieces take 16 draws each: 20,800, as many as a 768 m
    // island's trees need.
    let mut data = cube(0xff0000, Material::Flat).data().clone();
    data.thinning = Some(Thinning {
        parts: vec![[0.; 4]; data.positions.len()],
        full: 10.,
        least: 0.2,
    });
    let mesh = Arc::new(Mesh::new(data).unwrap());
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let pieces = (0..1300)
        .map(|_| Piece {
            levels: vec![(mesh.clone(), 0.)],
            impostor: None,
        })
        .collect();
    renderer.set_pieces(pieces);
    let rows: Vec<u32> = (0..1300).collect();
    let transforms: Vec<Mat4> = (0..1300)
        .map(|i| math::translate([i as f32, 0., 0.]))
        .collect();
    renderer.set_placements(&rows, &transforms);
    let mut target = Offscreen::new(64, 64).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    target.render(&renderer, Camera::default(), world).unwrap();
    let state = renderer.state.lock().unwrap();
    assert!(state.assets.placed.draws.len() > 1 << 14);
    if state.device.as_ref().is_some_and(|d| d.arena.is_some()) {
        assert!(state.assets.placed.arena.is_some(), "per-piece draws");
    }
}

#[test]
fn placements_past_two_to_the_seventeen_rows_share_the_arena() {
    // 140,000 rows: one more row bit than a fixed 17/15 split had. Only the
    // last row sits in view, so a draw that misread the split would show
    // row 8,927 (off to the side) or nothing.
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(cube(0xff0000, Material::Flat)), 0.)],
        impostor: None,
    }]);
    const ROWS: u32 = 140_000;
    let rows = vec![0; ROWS as usize];
    let mut transforms = vec![math::translate([1e4, 0., 0.]); ROWS as usize];
    transforms[ROWS as usize - 1] = math::IDENTITY;
    renderer.set_placements(&rows, &transforms);
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
    {
        let state = renderer.state.lock().unwrap();
        assert!(state.assets.placed.packs());
        if state.device.as_ref().is_some_and(|d| d.arena.is_some()) {
            assert!(state.assets.placed.arena.is_some(), "per-piece draws");
        }
    }
    let center = camera.project([0.; 3], target.bounds(), world);
    assert_eq!(target.pixel(&pixels, center), [255, 0, 0]);
    let hit = target.pick(&renderer, center).unwrap().unwrap();
    assert_eq!(hit.placement, Some(ROWS - 1));
}

#[test]
fn thinning_buckets_share_one_material_in_visible_entries() {
    // 70,000 rows leave 15 bits; 2,100 thinning pieces take 33,600 bucket
    // draws, past them, but share two meshes: two materials. Only the last
    // row, of the last piece (green, unlike the rest), sits in view: an
    // entry with the wrong material draws red.
    let quad = |color: u32| {
        let mut data = MeshData {
            material: Material::Flat,
            ..Default::default()
        };
        data.positions = vec![[-1., 0., -1.], [1., 0., -1.], [1., 0., 1.], [-1., 0., 1.]];
        data.indices = vec![0, 2, 1, 0, 3, 2];
        data.colors = vec![color; 4];
        data.thinning = Some(Thinning {
            parts: vec![[0., 0., 0., 0.]; 4],
            full: 1e-3,
            least: 0.25,
        });
        Arc::new(Mesh::new(data).unwrap())
    };
    const PIECES: u32 = 2_100;
    const ROWS: u32 = 70_000;
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let red = quad(0xff0000);
    let mut pieces: Vec<Piece> = (0..PIECES - 1)
        .map(|_| Piece {
            levels: vec![(red.clone(), 0.)],
            impostor: None,
        })
        .collect();
    pieces.push(Piece {
        levels: vec![(quad(0x00ff00), 0.)],
        impostor: None,
    });
    renderer.set_pieces(pieces);
    let mut piece_of: Vec<u32> = (0..ROWS).map(|r| r % (PIECES - 1)).collect();
    piece_of[ROWS as usize - 1] = PIECES - 1;
    let mut transforms = vec![math::translate([1e4, 0., 0.]); ROWS as usize];
    transforms[ROWS as usize - 1] = math::IDENTITY;
    renderer.set_placements(&piece_of, &transforms);
    {
        let placed = &renderer.state.lock().unwrap().assets.placed;
        assert!(placed.draws.len() as u32 > 1 << (32 - placed.row_bits()));
        assert!(placed.packs());
    }
    let mut target = Offscreen::new(320, 240).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    if (renderer.state.lock().unwrap().device.as_ref()).is_some_and(|d| d.arena.is_some()) {
        let placed = &renderer.state.lock().unwrap().assets.placed;
        assert!(placed.arena.is_some(), "per-piece draws");
    }
    let center = camera.project([0.; 3], target.bounds(), world);
    assert_eq!(target.pixel(&pixels, center), [0, 255, 0]);
    let hit = target.pick(&renderer, center).unwrap().unwrap();
    assert_eq!(hit.placement, Some(ROWS - 1));
}

/// Keeps placements on per-piece draws even where the device could pack
/// them into one arena.
fn without_arena(renderer: &Renderer) {
    renderer.state.lock().unwrap().assets.packed = Some(false);
}

fn placements_cull_pick_levels(arena: bool) {
    // One piece: a red full-detail cube and a green stand-in below 10 px/m.
    let renderer = Renderer::default();
    if !arena {
        without_arena(&renderer);
    }
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![
            (Arc::new(cube(0xff0000, Material::Flat)), 10.),
            (Arc::new(cube(0x00ff00, Material::Flat)), 0.),
        ],
        impostor: None,
    }]);
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
    let bounds = target.bounds();
    // The second cube sits 30 times as far away, off to the side so the
    // near one does not hide it; the third behind the camera, culled.
    let eye = camera.eye(bounds, world).unwrap();
    let side = math::normalize(math::cross(eye, [0., 1., 0.])).unwrap();
    let distance = math::length(eye) * 30.;
    let far: Vec3 = std::array::from_fn(|a| eye[a] * -29. + side[a] * distance * 0.15);
    let behind: Vec3 = std::array::from_fn(|a| eye[a] * 2.);
    let rows = [
        math::IDENTITY,
        math::translate(far),
        math::translate(behind),
    ];
    renderer.set_placements(&[0, 0, 0], &rows);
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    // Counts come back a frame late.
    target.frame(&renderer).unwrap();
    let drawn = renderer.drawn();
    assert_eq!(drawn.placed, [1, 1, 0, 0], "{drawn:?}");
    let center = camera.project([0.; 3], bounds, world);
    assert_eq!(target.pixel(&pixels, center), [255, 0, 0]);
    let hit = target.pick(&renderer, center).unwrap().unwrap();
    assert_eq!((hit.placement, hit.id), (Some(0), None));

    // Moving the near row away leaves its pixel; nothing else is re-sent.
    renderer.move_placements(0, &[math::translate([1.2, 0., 0.])]);
    let pixels = target.frame(&renderer).unwrap();
    assert_ne!(target.pixel(&pixels, center), [255, 0, 0]);
    let moved = camera.project([1.2, 0., 0.], bounds, world);
    assert_eq!(target.pixel(&pixels, moved), [255, 0, 0]);
}

#[test]
fn scaled_placements_keep_detail_further_out() {
    // The two-level cube 30 times as far as the view: plain it draws the
    // stand-in, scaled up 40 times its triangles are large again. Foliage
    // (pieces with an impostor) goes by world metres and stays coarse.
    for (impostor, scaled_level) in [(None, [1, 0, 0, 0]), (Some(0.01), [0, 1, 0, 0])] {
        scaled_levels(impostor, scaled_level);
    }
}

fn scaled_levels(impostor: Option<f32>, scaled_level: [usize; 4]) {
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![
            (Arc::new(cube(0xff0000, Material::Flat)), 10.),
            (Arc::new(cube(0x00ff00, Material::Flat)), 0.),
        ],
        impostor,
    }]);
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
    let eye = camera.eye(target.bounds(), world).unwrap();
    let far = math::translate(eye.map(|e| e * -29.));
    let mut scaled = far;
    for i in [0, 5, 10] {
        scaled[i] = 40.;
    }
    target.aim(&renderer, camera, world);
    for (row, expect) in [(far, [0, 1, 0, 0]), (scaled, scaled_level)] {
        renderer.set_placements(&[0], &[row]);
        target.settle(&renderer).unwrap();
        // Counts come back a frame late.
        target.frame(&renderer).unwrap();
        assert_eq!(renderer.drawn().placed, expect);
    }
}

#[test]
fn placed_pieces_sample_their_own_textures() {
    for arena in [true, false] {
        let renderer = Renderer::default();
        if !arena {
            without_arena(&renderer);
        }
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        let textured = |rgba: [u8; 4]| {
            let mut data = cube(0xffffff, Material::Lit).data().clone();
            data.texture = Some(Texture::solid(rgba).into());
            Piece {
                levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
                impostor: None,
            }
        };
        renderer.set_pieces(vec![textured([0, 0, 255, 255]), textured([0, 255, 0, 255])]);
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
        renderer.set_placements(&[0, 1], &[math::IDENTITY, math::translate([1.2, 0., 0.])]);
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        // The device here offers the arena's features; without them both
        // runs would test the per-piece path.
        let state = renderer.state.lock().unwrap();
        assert_eq!(state.assets.placed.arena.is_some(), arena);
        drop(state);
        let bounds = target.bounds();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, bounds, world));
        assert_eq!(at([0.; 3]), [0, 0, 255], "arena {arena}");
        assert_eq!(at([1.2, 0., 0.]), [0, 255, 0], "arena {arena}");
    }
}

#[test]
fn placed_pieces_with_textures_larger_than_a_frame_finish_uploading() {
    let mut target = Offscreen::new(64, 64).unwrap();
    // 2048² RGBA is 16 MB: two frames' worth of upload budget.
    let side = 2048;
    let mut data = cube(0xffffff, Material::Lit).data().clone();
    data.texture = Some(Image::from(
        Texture::from_rgba(side, side, vec![200; (side * side * 4) as usize]).unwrap(),
    ));
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: None,
    }]);
    renderer.set_placements(&[0], &[math::IDENTITY]);
    let world = Extent {
        center: [0.; 3],
        size: 2.,
    };
    target.aim(&renderer, Camera::default(), world);
    let mut frames = 0;
    while renderer.pending() {
        target.frame(&renderer).unwrap();
        frames += 1;
        assert!(frames < 10, "the texture's upload restarted");
    }
}

#[test]
fn impostors_show_their_baked_mesh_and_pick_as_placements() {
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let mut data = cube(0xffffff, Material::Lit).data().clone();
    data.texture = Some(Texture::solid([0, 0, 255, 255]).into());
    // Below a billion pixels per metre: always the impostor.
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: Some(1e9),
    }]);
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
    renderer.set_placements(&[0], &[math::IDENTITY]);
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    // The bake lands a frame after the arena; counts a frame after that.
    target.frame(&renderer).unwrap();
    let pixels = target.frame(&renderer).unwrap();
    target.frame(&renderer).unwrap();
    assert!(renderer.state.lock().unwrap().assets.placed.atlas.is_some());
    assert_eq!(
        renderer.drawn().placed,
        [0, 1, 0, 0],
        "drawn as the level past its mesh"
    );
    let bounds = target.bounds();
    let center = camera.project([0.; 3], bounds, world);
    assert_eq!(target.pixel(&pixels, center), [0, 0, 255]);
    // Well outside the cube's silhouette the quad's empty corners discard.
    let outside = camera.project([2.5, 0., 0.], bounds, world);
    assert_eq!(target.pixel(&pixels, outside), [0, 0, 0]);
    let hit = target.pick(&renderer, center).unwrap().unwrap();
    assert_eq!(hit.placement, Some(0));
}

#[test]
fn host_texture_upload_rejects_invalid_images_before_allocation() {
    let off = Offscreen::new(8, 8).unwrap();
    let device = &off.gpu.device;
    assert!(StreamedTexture::new(device, vec![], true, true).is_err());
    let invalid = Texture {
        width: 4,
        height: 4,
        rgba: vec![0; 4],
    };
    assert!(StreamedTexture::new(device, vec![invalid.into()], false, true).is_err());
    let solid: Image = Texture::solid([255; 4]).into();
    assert!(StreamedTexture::new(device, vec![solid.clone(), solid.clone()], false, true).is_err());
    let array = StreamedTexture::new(device, vec![solid.clone(), solid], true, true).unwrap();
    let mut budget = Budget::new();
    assert!(array.stream(&off.gpu.queue, &mut budget));
    assert_eq!(budget.used(), 8);
}

/// Four quads in a row along X, each with a normal and surface map, drawn
/// as batches or as placements (with or without the arena); the pixel at
/// each quad's centre.
fn mapped_quads(
    maps: [([u8; 4], [u8; 4]); 4],
    light: Light,
    tone: Option<Tone>,
    path: u8,
) -> [[u8; 3]; 4] {
    let solid = |rgba: [u8; 4]| Image::from(Texture::from_rgba(4, 4, rgba.repeat(16)).unwrap());
    let xs = [-3., -1., 1., 3.];
    let meshes = maps.map(|(normal, surface)| {
        Arc::new(
            Mesh::new(MeshData {
                texture: Some(solid([200, 200, 200, 255])),
                normal_map: Some(solid(normal)),
                surface_map: Some(solid(surface)),
                ..quad(0.9, 0., Material::Lit)
            })
            .unwrap(),
        )
    });
    let renderer = Renderer::default();
    let mut scene = Scene {
        light,
        tone,
        ..Default::default()
    };
    if path == 0 {
        scene.batches = (meshes.iter().zip(xs))
            .map(|(m, x)| Batch {
                mesh: m.clone(),
                instances: Arc::new(Instances {
                    matrices: vec![math::translate([x, 0., 0.])],
                    ids: vec![Some(1)],
                }),
            })
            .collect();
    } else {
        if path == 2 {
            without_arena(&renderer);
        }
        let pieces = meshes.iter().map(|m| Piece {
            levels: vec![(m.clone(), 0.)],
            impostor: None,
        });
        renderer.set_pieces(pieces.collect());
        renderer.set_placements(&[0, 1, 2, 3], &xs.map(|x| math::translate([x, 0., 0.])));
    }
    renderer.set_scene(&scene);
    let mut target = Offscreen::new(320, 240).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 9.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    xs.map(|x| target.pixel(&pixels, camera.project([x, 0., 0.], target.bounds(), world)))
}

#[test]
fn normal_and_surface_maps_bend_and_darken_lit_meshes() {
    // Not metal, unoccluded.
    let open = [0, 0, 0, 255];
    let maps = [
        ([128, 128, 255, 0], open),
        // Leaning toward +U (world +X) and +V (world +Z), where the sun is.
        ([230, 128, 180, 0], open),
        ([128, 230, 180, 0], open),
        // Flat, fully occluded: the sky no longer reaches it.
        ([128, 128, 255, 0], [0, 0, 0, 0]),
    ];
    let light = Light {
        sun: [1., 0.4, 1.],
        sun_color: [0.6; 3],
        sky: [0.4; 3],
        ground: [0.4; 3],
        ..Default::default()
    };
    for path in 0..3 {
        let [flat, u, v, occluded] = mapped_quads(maps, light, None, path).map(|p| i32::from(p[0]));
        assert!(
            u > flat + 25 && v > flat + 25,
            "path {path}: flat {flat}, +U {u}, +V {v}"
        );
        assert!(
            occluded < flat - 40,
            "path {path}: flat {flat}, occluded {occluded}"
        );
    }
}

#[test]
fn toned_scenes_expose_tone_map_and_highlight_gloss() {
    let open = [0, 0, 0, 255];
    let flat = |gloss| ([128, 128, 255, gloss], open);
    // Sky only: albedo 200 decodes to linear, adds the rough environment
    // reflection, then exposure, ACES and sRGB.
    let sky = Light {
        sun_color: [0.; 3],
        sky: [1.; 3],
        ground: [1.; 3],
        ..Default::default()
    };
    let tone = Tone {
        exposure: 1.5,
        sun_occlusion: 0.,
    };
    let [p, ..] = mapped_quads([flat(0); 4], sky, Some(tone), 0);
    let linear = (200f32 / 255.).powf(2.2) + 0.04 * 0.4524 - 0.0024;
    let v = linear * 1.5 * 0.6;
    let aces = (v * (2.51 * v + 0.03)) / (v * (2.43 * v + 0.59) + 0.14);
    let expected = ((1.055 * aces.powf(1. / 2.4) - 0.055) * 255.).round() as u8;
    near(p, [expected; 3], 4);
    // Sun overhead and camera straight down: a mirror-glossy surface
    // shows the highlight, a rough one barely does.
    let sun = Light {
        sun: [0., 1., 0.],
        sun_color: [1.; 3],
        sky: [0.1; 3],
        ground: [0.1; 3],
        ..Default::default()
    };
    for path in 0..3 {
        let [rough, _, _, glossy] = mapped_quads(
            [flat(0), flat(0), flat(0), flat(255)],
            sun,
            Some(tone),
            path,
        );
        assert!(
            glossy[0] > rough[0] + 30,
            "path {path}: rough {rough:?}, glossy {glossy:?}"
        );
    }
}

#[test]
fn sky_fills_what_geometry_leaves_and_the_sea_reaches_the_horizon() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    let (mut scene, _) = scene(Material::Lit);
    scene.markers = Arc::new(Vec::new());
    scene.light = Light {
        sun: [0., 1., -1.],
        ..neutral()
    };
    scene.sky = Some(Sky {
        zenith: [0., 0., 1.],
        horizon: [1., 0., 0.],
        sea: Some(Sea {
            level: -5.,
            color: [0., 0.5, 0.],
        }),
    });
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 6.,
    };
    // Looking nearly level: sky above the middle, sea below, the cube at
    // the centre.
    let camera = Camera {
        tilt: 1.45,
        fov: 0.9,
        ..Camera::perspective()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |x: f32, y: f32| target.pixel(&pixels, point(px(x), px(y)));
    let top = at(160., 2.);
    assert!(top[2] > 200 && top[0] < 120, "sky near the zenith {top:?}");
    let bottom = at(160., 237.);
    assert!(
        bottom[1] > bottom[0] && bottom[1] > bottom[2],
        "sea {bottom:?}"
    );
    let cube = target.pixel(&pixels, camera.project([0.; 3], target.bounds(), world));
    near(cube, [0x40, 0xa0, 0xff], 3);
}

#[test]
fn deep_water_turns_opaque_past_its_depth() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let mut ground = quad(4., -1., Material::Lit);
    ground.colors = vec![0xff0000; 4];
    let water = |depth| {
        Material::Blend(Blend {
            color: 0x0000ff,
            opacity: 0.5,
            depth,
            ..Default::default()
        })
    };
    // The floor one unit down: past a half-unit depth on the left, within
    // a four-unit one on the right.
    let mut deep = quad(1., 0., water(0.5));
    deep.positions = vec![[-4., 0., -2.], [0., 0., -2.], [0., 0., 2.], [-4., 0., 2.]];
    let mut shallow = quad(1., 0., water(4.));
    shallow.positions = vec![[0., 0., -2.], [4., 0., -2.], [4., 0., 2.], [0., 0., 2.]];
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: [ground, deep, shallow]
            .map(|m| single(m, math::IDENTITY))
            .to_vec(),
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 10.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    near(at([-2., 0., 0.]), [0, 0, 255], 3);
    let shallow = at([2., 0., 0.]);
    assert!(shallow[0] > 60 && shallow[2] < 250, "shallow {shallow:?}");
}

#[test]
fn wide_water_does_not_coarsen_the_shadow_map() {
    let mut target = Offscreen::new(320, 320).unwrap();
    let light = Light {
        sun: [1., 2., 0.],
        sun_color: [0.7; 3],
        sky: [0.3; 3],
        ground: [0.3; 3],
        shadows: Some(Shadows {
            resolution: 1024,
            softness: 1.,
        }),
        ..Default::default()
    };
    // A 0.4-unit block 1 up shadows the ground 0.5 toward -X. Fitted around
    // the 600-unit sea as well, a shadow texel would be bigger than it.
    let mut block = math::translate([0., 1., 0.]);
    for c in 0..3 {
        block[c * 5] = 0.2;
    }
    let sea = Material::Blend(Blend {
        color: 0x2040ff,
        opacity: 0.8,
        ..Default::default()
    });
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![
            single(quad(4., 0., Material::Lit), math::IDENTITY),
            single(cube(0xffffff, Material::Lit).data().clone(), block),
            single(quad(300., -0.5, sea), math::IDENTITY),
        ],
        light,
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 3.,
    };
    let camera = Camera {
        fov: 0.9,
        ..Default::default()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    near(at([-0.5, 0., 0.]), [77; 3], 8);
    near(at([-0.5, 0., 1.]), [237; 3], 6);
}

#[test]
fn models_take_the_ground_tint_under_them() {
    for arena in [true, false] {
        let mut target = Offscreen::new(256, 256).unwrap();
        let side = 129;
        let field = flat_field(side);
        // Half red, as a multiplier of 2 * 128 / 255 on red, none elsewhere.
        let texels = [128, 0, 0, 255].repeat(side * side);
        let terrain = Arc::new(Terrain {
            heights: Arc::new(HeightMap::new(&field).unwrap()),
            colors: Arc::new(ControlMap::new(side, side, texels).unwrap()),
            color_gain: 2.,
            tints: true,
            material: Material::Lit,
            texture: None,
        });
        let block = |tint| {
            let mut data = cube(0xffffff, Material::Lit).data().clone();
            data.terrain_tint = tint;
            Piece {
                levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
                impostor: None,
            }
        };
        let renderer = Renderer::default();
        if !arena {
            without_arena(&renderer);
        }
        renderer.set_scene(&Scene {
            terrain: Some(terrain),
            light: neutral(),
            ..Default::default()
        });
        renderer.set_pieces(vec![block(1.), block(0.5), block(0.)]);
        let at = [-6., 0., 6.].map(|x| math::translate([x, 5., 0.]));
        renderer.set_placements(&[0, 1, 2], &at);
        let world = Extent {
            center: [0.; 3],
            size: 20.,
        };
        let camera = Camera::default();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let px = |x: f32| {
            target.pixel(
                &pixels,
                camera.project([x, 5.5, 0.], target.bounds(), world),
            )
        };
        // Full: red 255 * 1.004, green and blue gone. Half: halfway.
        near(px(-6.), [255, 0, 0], 2);
        near(px(0.), [255, 128, 128], 3);
        near(px(6.), [255, 255, 255], 2);
    }
}

#[test]
fn occlusion_darkens_ground_where_it_meets_a_block_only() {
    let shot = |occlusion| {
        let mut target = Offscreen::new(320, 320).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches: vec![
                single(quad(6., 0., Material::Lit), math::IDENTITY),
                single(
                    cube(0xffffff, Material::Lit).data().clone(),
                    math::translate([0., 0.5, 0.]),
                ),
            ],
            light: neutral(),
            occlusion,
            ..Default::default()
        });
        let world = Extent {
            center: [0.; 3],
            size: 10.,
        };
        let camera = Camera {
            fov: 0.9,
            ..Camera::perspective()
        };
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world))[0];
        // Ground at the block's foot, ground in the open, the block's top.
        [at([0.6, 0., 0.]), at([2.5, 0., 2.5]), at([0., 1., 0.])]
    };
    let [foot, open, top] = shot(Some(Occlusion {
        radius: 1.,
        strength: 1.,
    }));
    assert!(foot < 200, "foot {foot}");
    assert!(open > 245 && top > 245, "open {open}, top {top}");
    assert_eq!(shot(None)[0], 255);
}

#[test]
fn outline_marks_set_one_bit_per_marked_row_and_handle() {
    use super::{Outline, outline_marks};
    let outline = Outline {
        rows: vec![0, 33],
        ids: vec![7],
        color: 0xffa733ff,
        width: 2,
    };
    // Handles: batch instance ids by pick handle - 1.
    let marks = outline_marks(&outline, &[Some(5), Some(7), None]);
    // Two words of placed rows (row 33 is in the second), one of handles.
    assert_eq!(marks[0], 2);
    assert_eq!(marks[1], 0xffa733ff);
    assert_eq!(marks[2], 2);
    assert_eq!(marks[4], 1);
    assert_eq!(marks[5], 1 << 1);
    // Id 7 is handle 2, bit 1.
    assert_eq!(marks[6], 1 << 1);
    assert_eq!(marks.len(), 7);
}

// A selection dragged on the GPU: the selected row draws, picks and carries
// its marker at the drag's place while the table keeps where it began, and
// the scene's own markers stay ahead of the selection's. At the drag's end
// the row draws what the table holds.
#[test]
fn selected_rows_drag_and_carry_their_markers_on_the_gpu() {
    for arena in [true, false] {
        let renderer = Renderer::default();
        if !arena {
            without_arena(&renderer);
        }
        renderer.set_scene(&Scene {
            light: neutral(),
            markers: Arc::new(vec![Marker {
                position: [-3., 0., 3.],
                radius: 6.,
                color: 0x0000ff,
                shape: Shape::Square,
            }]),
            ..Default::default()
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(cube(0xff0000, Material::Flat)), 0.)],
            impostor: None,
        }]);
        // The piece sits 1 m east in its item: the marker goes to the
        // item's origin, west of the cube.
        let local = math::translate([1., 0., 0.]);
        let start = math::multiply(&math::translate([2., 0., 0.]), &local);
        renderer.set_placements(&[0, 0], &[math::IDENTITY, start]);
        let ring = Marker {
            position: [0.; 3],
            radius: 6.,
            color: 0x00ff00,
            shape: Shape::Circle,
        };
        renderer.set_selection(vec![1], &[(1, [-1., 0., 0.])], &ring);
        renderer.drag_selection(Some(math::translate([0., 0., -3.])));
        let mut target = Offscreen::new(320, 240).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 12.,
        };
        let camera = Camera::default();
        let pixels = target.render(&renderer, camera, world).unwrap();
        let bounds = target.bounds();
        let at = |p: Vec3| camera.project(p, bounds, world);
        assert_eq!(
            target.pixel(&pixels, at([2., 2., -3.])),
            [0, 255, 0],
            "arena {arena}: marker"
        );
        assert_eq!(
            target.pixel(&pixels, at([-3., 0., 3.])),
            [0, 0, 255],
            "arena {arena}: scene marker"
        );
        let hit = |target: &mut Offscreen, p: Vec3| {
            target
                .pick(&renderer, at(p))
                .unwrap()
                .and_then(|h| h.placement)
        };
        assert_eq!(
            hit(&mut target, [3., 0., -3.]),
            Some(1),
            "arena {arena}: dragged"
        );
        assert_eq!(hit(&mut target, [3., 0., 0.]), None, "arena {arena}: left");
        // The host moves the table where the drag landed, then ends it.
        renderer.move_placements(
            1,
            &[math::multiply(&math::translate([0., 0., -3.]), &start)],
        );
        renderer.drag_selection(None);
        target.render(&renderer, camera, world).unwrap();
        assert_eq!(
            hit(&mut target, [3., 0., -3.]),
            Some(1),
            "arena {arena}: landed"
        );
    }
}
