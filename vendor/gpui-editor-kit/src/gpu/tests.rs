use super::offscreen::Offscreen;
use super::*;
use crate::heightfield::{Grid, Heightfield, Rect};
use gpui::point;

mod baselines;
mod families;
mod ground;
mod handoff;
mod loss;
mod posed;
mod queue;
mod reflection;
mod scene;

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

// Levels from Mesh::compact_levels keep only their triangles, over the
// full mesh's vertices: they draw what the same simplification as a mesh
// of its own draws, through the arena.
#[test]
fn compact_levels_draw_from_the_full_mesh_vertices() {
    // The bent grid of simplified_meshes_keep_their_shape_with_fewer_triangles,
    // coloured along it so a vertex out of place shows.
    let n = 65u32;
    let mut data = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    for z in 0..n {
        for x in 0..n {
            let (u, v) = (x as f32 / 64., z as f32 / 64.);
            data.positions.push([
                u * 4. - 2.,
                (u * std::f32::consts::PI).sin() * 1.2,
                v * 4. - 2.,
            ]);
            data.colors
                .push(((u * 255.) as u32) << 16 | ((v * 255.) as u32) << 8 | 0x60);
        }
    }
    for z in 0..n - 1 {
        for x in 0..n - 1 {
            let i = z * n + x;
            data.indices
                .extend([i, i + 1, i + n + 1, i, i + n + 1, i + n]);
        }
    }
    let own = Arc::new(Mesh::compact(data.simplified(0.15).unwrap()).unwrap());
    let (full, levels) = Mesh::compact_levels(data, &[0.15, 0.9]).unwrap();
    assert_eq!(levels.len(), 1, "no level past what still simplifies");
    let level = &levels[0];
    assert!(Arc::ptr_eq(level.shares().unwrap(), &full));
    // Simplified after the full mesh's reordering, a triangle or so apart.
    assert!(level.triangles().abs_diff(own.triangles()) * 50 <= own.triangles());
    assert_eq!(level.geometry().positions, full.geometry().positions);
    // Always the coarse level: it draws below every threshold.
    let render = |mesh: &Arc<Mesh>| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(full.clone(), 1e9), (mesh.clone(), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&[0], &[math::IDENTITY]);
        let mut target = Offscreen::new(240, 180).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 6.,
        };
        let camera = Camera {
            fov: 0.9,
            zoom: 2.,
            tilt: 0.8,
            ..Camera::perspective()
        };
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        assert_eq!(renderer.drawn().placed, [0, 1, 0, 0]);
        pixels
    };
    let (off, most) = difference(&render(&own), &render(level));
    assert!(
        off <= 240 * 180 / 500 && most <= 16,
        "{off} pixels differ, by up to {most}"
    );
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
fn outline_bands_the_selection_and_leaves_the_rest() {
    let mut target = Offscreen::new(128, 128).unwrap();
    let renderer = Renderer::default();
    // 0.25 units at 64 px a unit: an edge 16 px from the middle.
    renderer.set_scene(&Scene {
        batches: vec![single(quad(0.25, 0., Material::Flat), math::IDENTITY)],
        light: neutral(),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 2.,
    };
    let camera = Camera::default();
    let plain = target.render(&renderer, camera, world).unwrap();
    renderer.set_outline(Some(Outline {
        rows: Vec::new(),
        ids: vec![1],
        color: 0x00ff00ff,
        width: 2,
    }));
    let pixels = target.frame(&renderer).unwrap();
    let bounds = target.bounds();
    let at = |p: Vec3| camera.project(p, bounds, world);
    // The band runs 3.5 to 5.5 px out (outline.wgsl), on every side.
    for p in [
        [0.32, 0., 0.],
        [-0.32, 0., 0.],
        [0., 0., 0.32],
        [0., 0., -0.32],
    ] {
        assert_eq!(target.pixel(&pixels, at(p)), [0, 255, 0], "{p:?}");
    }
    for p in [[0.8, 0., 0.8], [-0.8, 0., 0.], [0.5, 0., -0.5]] {
        assert_eq!(
            target.pixel(&pixels, at(p)),
            target.pixel(&plain, at(p)),
            "{p:?}"
        );
    }
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

// At half the render scale the scene still fills the pane where it did,
// keeps its colors, and picks name the same instances under the cursor.
#[test]
fn a_lower_render_scale_draws_and_picks_where_full_size_does() {
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::default();
    let shoot = |scale: f32| {
        let mut target = Offscreen::new(320, 320).unwrap();
        let renderer = Renderer::default();
        renderer.set_render_scale(scale);
        let (scene, _) = scene(Material::Flat);
        renderer.set_scene(&scene);
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let view = target.bounds();
        let at = |p: Vec3| camera.project(p, view, world);
        let colors =
            [[0.1, 0.5, 0.1], [0.9, 0.5, -3.], [3., 0., 3.]].map(|p| target.pixel(&pixels, at(p)));
        let picks = [[0.1, 0.5, 0.1], [0.9, 0.5, -3.], [3., 0., 3.]]
            .map(|p| target.pick(&renderer, at(p)).unwrap().and_then(|h| h.id));
        (colors, picks)
    };
    let (full, full_picks) = shoot(1.);
    let (half, half_picks) = shoot(0.5);
    assert_eq!(full_picks, [Some(7), Some(8), None]);
    assert_eq!(half_picks, full_picks);
    for (a, b) in full.iter().zip(&half) {
        assert!((0..3).all(|c| a[c].abs_diff(b[c]) <= 2), "{a:?} vs {b:?}");
    }
}

#[test]
fn stats_report_the_devices_memory() {
    let mut target = Offscreen::new(64, 64).unwrap();
    // A backend without an allocator report has nothing to show.
    let Some(report) = target.device().generate_allocator_report() else {
        return;
    };
    assert!(report.total_allocated_bytes > 0);
    let renderer = Renderer::default();
    let (scene, _) = scene(Material::Lit);
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    target.aim(&renderer, Camera::perspective(), world);
    target.settle(&renderer).unwrap();
    let stats = renderer.stats();
    assert!(stats.vram_used > 0, "{stats:?}");
    assert!(stats.vram_reserved >= stats.vram_used, "{stats:?}");
}

#[test]
fn pass_times_sum_by_name_and_stay_put() {
    let mut target = Offscreen::new(64, 64).unwrap();
    let renderer = Renderer::default();
    let (scene, _) = scene(Material::Lit);
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    target.aim(&renderer, Camera::perspective(), world);
    target.settle(&renderer).unwrap();
    let names = |s: &Stats| -> Vec<&str> {
        (s.pass_avg.iter())
            .filter(|(n, _)| !n.is_empty())
            .map(|p| p.0)
            .collect()
    };
    for _ in 0..4 {
        target.time_frame(&renderer).unwrap();
    }
    let stats = renderer.stats();
    // A device without timestamp queries times nothing.
    if stats.passes.iter().all(|(n, _)| n.is_empty()) {
        return;
    }
    let first = names(&stats);
    assert!(first.contains(&"opaque"), "{first:?}");
    let mut unique = first.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), first.len(), "one row per name: {first:?}");
    let sum: f32 = (stats.passes.iter())
        .filter(|(n, _)| *n == "opaque")
        .map(|p| p.1)
        .sum();
    assert!(
        sum > 0.
            && stats
                .pass_avg
                .iter()
                .any(|&(n, ms)| n == "opaque" && ms > 0.)
    );
    // A second view drawing in turn: the rows are both views' together and
    // keep their order whichever view read back last.
    let mut other = Offscreen::new(64, 64).unwrap();
    other.aim(&renderer, Camera::perspective(), world);
    for _ in 0..4 {
        other.time_frame(&renderer).unwrap();
        assert_eq!(names(&renderer.stats())[..first.len()], first[..]);
        target.time_frame(&renderer).unwrap();
        assert_eq!(names(&renderer.stats())[..first.len()], first[..]);
    }
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
            ..Default::default()
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

// In the dark, a point light lights the ground under it by the inverse
// square until its reach, and a spot only inside its cone, by its power
// falloff; nothing past either.
#[test]
fn local_lights_light_within_their_reach_and_cone() {
    let mut target = Offscreen::new(320, 320).unwrap();
    let renderer = Renderer::default();
    let dark = Light {
        sun_color: [0.; 3],
        sky: [0.; 3],
        ground: [0.; 3],
        ..Default::default()
    };
    renderer.set_scene(&Scene {
        batches: vec![single(quad(10., 0., Material::Lit), math::IDENTITY)],
        light: dark,
        local_lights: Arc::new(vec![
            LocalLight {
                position: [-4., 1., 0.],
                reach: 3.,
                color: [1.; 3],
                falloff: Falloff::InverseSquare,
                spot: None,
            },
            LocalLight {
                position: [4., 2., 0.],
                reach: 5.,
                color: [0.8; 3],
                falloff: Falloff::Power(1.),
                spot: Some(Spot {
                    aim: [0., -1., 0.],
                    cos_inner: 0.2f32.cos(),
                    cos_outer: 0.35f32.cos(),
                }),
            },
        ]),
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
    // A metre under the point: 2 / (0.01 + 1 + √1.01) · (1 - 1/9) = 0.88.
    near(at([-4., 0., 0.]), [225; 3], 6);
    near(at([-4., 0., 3.5]), [0; 3], 1);
    // Two metres under the spot: 0.8 · (5 - 2) / 5 = 0.48.
    near(at([4., 0., 0.]), [122; 3], 6);
    // 0.64 rad off its aim, past its outer half angle.
    near(at([4., 0., 1.5]), [0; 3], 1);
    near(at([0., 0., 0.]), [0; 3], 1);
}

// A splat-shaded mesh that is not the ground still casts its shadow, on
// every path: the splat's words must not stand in for the alpha cut.
// Trunk's case was a classic splat (1800's shipped cliff meshes); here the
// kit's only splat on a mesh is `MeshSplat`.
#[test]
fn splat_shaded_meshes_cast_shadows() {
    let layer = |rgba: [u8; 4]| MeshSplatLayer {
        albedo: Texture::solid(rgba).into(),
        normal: None,
        tiling: 1.,
    };
    let mut card = quad(1., 4., Material::Lit);
    card.texture = Some(Texture::solid([200, 200, 200, 255]).into());
    card.alpha_ref = Some(0.3);
    card.splat = Some(Arc::new(MeshSplat {
        control: Texture::from_rgba(1, 1, vec![255, 0, 0, 0]).unwrap().into(),
        layers: [Some(layer([200, 200, 200, 255])), None, None, None],
        sharpness: 8.,
        alpha_floor: 0.1,
        fallback: [0.3; 3],
        tint: None,
        mix_normals: false,
        scale_invariant: true,
        world_tiling: false,
    }));
    let mesh = Arc::new(Mesh::new(card).unwrap());
    let world = Extent {
        center: [0.; 3],
        size: 22.,
    };
    let camera = Camera {
        fov: 0.9,
        ..Default::default()
    };
    let mut target = Offscreen::new(320, 320).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(quad(10., 0., Material::Lit), math::IDENTITY)],
        light: Light {
            sun: [1., 2., 0.],
            sun_color: [0.7; 3],
            sky: [0.3; 3],
            ground: [0.3; 3],
            shadows: Some(Shadows {
                resolution: 1024,
                softness: 1.,
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(mesh.clone(), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&[0], &[math::IDENTITY]);
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    // The card at height 4 shadows the ground 2 units toward -X.
    near(at([-2., 0., 0.]), [77; 3], 6);
    near(at([-2., 0., 4.]), [237; 3], 6);
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
            additive: false,
            unlit: false,
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

// An additive surface adds its colour to what is behind, and over nothing
// still shows; a vertex's top colour byte makes a blended one clearer.
#[test]
fn additive_and_faded_blends_add_and_thin() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let mut ground = quad(1., -1., Material::Lit);
    ground.colors = vec![0x0000ff; 4];
    ground.positions = vec![
        [-6., -1., -2.],
        [0., -1., -2.],
        [0., -1., 2.],
        [-6., -1., 2.],
    ];
    let mut glow = quad(
        1.,
        0.,
        Material::Blend(Blend {
            color: 0xffffff,
            opacity: 1.,
            additive: true,
            ..Default::default()
        }),
    );
    glow.colors = vec![0xff0000; 4];
    glow.positions = vec![[-6., 0., -2.], [2., 0., -2.], [2., 0., 2.], [-6., 0., 2.]];
    // Three quarters clear: a quarter of white over black.
    let mut faded = quad(
        1.,
        0.,
        Material::Blend(Blend {
            opacity: 1.,
            ..Default::default()
        }),
    );
    faded.colors = vec![0xc0ffffff; 4];
    faded.positions = vec![[2., 0., -2.], [6., 0., -2.], [6., 0., 2.], [2., 0., 2.]];
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: [ground, glow, faded]
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
    let [r, _, b] = at([-3., 0., 0.]);
    assert!(r > 200 && b > 100, "red added over blue: {r} {b}");
    let [r, g, b] = at([1., 0., 0.]);
    assert!(r > 200 && g < 30 && b < 30, "red over nothing: {r} {g} {b}");
    let [r, ..] = at([4., 0., 0.]);
    assert!((40..100).contains(&r), "a quarter of white: {r}");
}

// A 4096² tint (64 MB) fills over several frames, unbound until full,
// and an edit made while it fills shows on rows written before it as well
// as after: a grey slab taking the tint whole, darkened over its southern
// half. The kit's lighting reads a texel's RGB as the multiplier.
#[test]
fn a_large_tint_fills_over_frames_and_keeps_edits_made_meanwhile() {
    use crate::heightfield::Rect;
    let mut target = Offscreen::new(320, 240).unwrap();
    let side = 4096;
    let (plain, dark) = ([255u8, 255, 255, 255], [255u8, 115, 255, 255]);
    let map = Arc::new(ControlMap::new(side, side, plain.repeat(side * side)).unwrap());
    let renderer = Renderer::default();
    let slab = MeshData {
        texture: Some(
            Texture::from_rgba(4, 4, [200, 200, 200, 255].repeat(16))
                .unwrap()
                .into(),
        ),
        terrain_tint: 1.,
        ..quad(3., 0., Material::Lit)
    };
    renderer.set_scene(&Scene {
        batches: vec![single(slab, math::IDENTITY)],
        light: neutral(),
        ground_tint: Some(GroundTint {
            map: map.clone(),
            origin: [-3., -3.],
            size: [6., 6.],
        }),
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 7.,
    };
    let camera = Camera::default();
    target.aim(&renderer, camera, world);
    // The ground is up, and the frame after starts the tint: 8 MB of its
    // 64, rows by the south edge first.
    while renderer.drawn().batches == 0 {
        target.frame(&renderer).unwrap();
    }
    target.frame(&renderer).unwrap();
    assert!(renderer.pending(), "still filling");
    let half = side / 2;
    map.write(
        Rect {
            x0: 0,
            z0: 0,
            x1: side,
            z1: half,
        },
        &dark.repeat(side * half),
    );
    let pixels = target.settle(&renderer).unwrap();
    let at = |z: f32| target.pixel(&pixels, camera.project([0., 0., z], target.bounds(), world))[1];
    // Rows by the south edge went up before the edit, the middle's after.
    for z in [-2.6, -1.] {
        assert!(at(z).abs_diff(90) <= 3, "z {z}: {}", at(z));
    }
    assert!(at(2.).abs_diff(200) <= 3, "z 2: {}", at(2.));
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
        // Not the first frame, which also compiles every pipeline: with the
        // ocean's that alone neared 500 ms in a loaded test run.
        if frames > 1 {
            let took = started.elapsed().as_millis();
            assert!(took < 500, "frame {frames} took {took} ms");
        }
    }
    assert!(frames >= 2, "a 16 MB texture fit in one frame's budget");
    let pixels = target.frame(&renderer).unwrap();
    near(target.pixel(&pixels, point(px(32.), px(32.))), [200; 3], 1);
}

#[test]
fn small_meshes_upload_many_per_frame() {
    let mut target = Offscreen::new(64, 64).unwrap();
    // Distinct meshes and textures, as a town load has: ~1000 of each.
    let count: usize = 192;
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
    // Offscreen frames upload by bytes and batches alone, so the count is
    // the same on a loaded machine: all 192 fit one frame's batch cap, and
    // the frame after presents them. A window frame's 3 ms clock is the
    // profiling harness's to watch, not a pass/fail test's.
    assert!(
        frames <= count.div_ceil(super::UPLOAD_BATCHES) + 1,
        "{frames} frames for {count} meshes"
    );
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
    let first = renderer.lock().assets.meshes.len();
    assert!(first > 0);
    renderer.set_scene(&Scene {
        batches: vec![single(quad(1., 0., Material::Flat), math::IDENTITY)],
        ..Default::default()
    });
    drop(scene);
    target.settle(&renderer).unwrap();
    // One more frame collects what the old scene held.
    target.frame(&renderer).unwrap();
    assert_eq!(renderer.lock().assets.meshes.len(), 1);
}

/// Points `renderer` at a device without BC support, so compressed images
/// take the CPU decode path.
// Compressed images of different sizes share one array from the mip of
// the smallest: 16² starts two levels in, 8² one, to meet at 4². An 8²
// image without mips cannot reach 4².
#[test]
fn compressed_images_share_an_array_from_the_smallest_size() {
    let image = |side: u32, levels| compressed(Block::Bc1, side, side, levels, |_| bc1([9; 3]));
    let images = [image(16, 5), image(8, 4), image(4, 3)];
    let layout = Layout::new(&images).unwrap();
    assert_eq!(
        (layout.width, layout.levels, layout.skips.as_slice()),
        (4, 3, &[2, 1, 0][..])
    );
    assert!(StreamedTexture::check(&images).is_ok());
    assert!(StreamedTexture::check(&[image(8, 1), image(4, 3)]).is_err());
}

fn without_bc(renderer: &Renderer) {
    renderer.lock().assets.bc = Some(false);
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
        let state = renderer.lock();
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
            move |_| {
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
    // 129² samples over 64 units: four tiles of 64 cells. Samples alternate
    // between two heights, so the ground keeps its fine strides near (flat,
    // it would draw coarse at any distance), while its central differences,
    // and so its shading, stay those of flat ground.
    let side = 129;
    let mut field = flat_field(side);
    for (i, h) in field.heights.iter_mut().enumerate() {
        *h = 0.2 * ((i % side + i / side) % 2) as f32;
    }
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
            ..Default::default()
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
fn placements_behind_nearer_ones_are_culled_from_the_arena() {
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(cube(0xff0000, Material::Flat)), 0.)],
        impostor: None,
        pose: None,
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
    assert_eq!(renderer.drawn().placed[0], 1);
    // Moving the near cube away reveals the far one in that same frame:
    // the second phase finds it against the new depth.
    renderer.move_placements(0, &[math::translate([0., 50., 0.])]);
    let center = camera.project([0.; 3], target.bounds(), world);
    let hit = target.pick(&renderer, center).unwrap().unwrap();
    assert_eq!(hit.placement, Some(1));
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
            pose: None,
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
    let shots: Vec<Vec<u8>> = [false, true]
        .map(|compact| {
            let renderer = Renderer::default();
            renderer.set_scene(&Scene {
                light: neutral(),
                ..Default::default()
            });
            renderer.set_pieces(vec![Piece {
                levels: vec![(quads(compact), 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&[0], &[math::IDENTITY]);
            let mut target = Offscreen::new(320, 240).unwrap();
            target.render(&renderer, Camera::default(), world).unwrap()
        })
        .into();
    assert!(shots[0] == shots[1]);
    assert!(shots[0].iter().any(|&b| b > 0), "drawn");
}

#[test]
fn thinning_pieces_draw_and_thin_their_coarser_levels() {
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
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(quads(0xff0000), 1e9), (quads(0x00ff00), 0.)],
        impostor: None,
        pose: None,
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
    assert_eq!(at([-1.5, 0., 0.]), [0, 255, 0]);
    assert_eq!(at([1.5, 0., 0.]), [0, 0, 0]);
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
            pose: None,
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
    target.frame(&renderer).unwrap();
    let state = renderer.lock();
    assert!(state.assets.placed.draws.len() > 1 << 14);
    if state.device.as_ref().is_some_and(|d| d.arena.is_some()) {
        assert!(state.assets.placed.arena.is_some(), "no arena");
        // The pieces repeat one mesh, so the arena draws it once per
        // bucket, not once per piece and bucket.
        drop(state);
        let drawn = renderer.drawn().arena_draws;
        assert!(
            drawn[1] > 0 && drawn.iter().all(|&n| n <= placed::THIN_BUCKETS as usize),
            "arena draws per pass {drawn:?}"
        );
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
        pose: None,
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
        let state = renderer.lock();
        assert!(state.assets.placed.packs());
        if state.device.as_ref().is_some_and(|d| d.arena.is_some()) {
            assert!(state.assets.placed.arena.is_some(), "no arena");
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
            pose: None,
        })
        .collect();
    pieces.push(Piece {
        levels: vec![(quad(0x00ff00), 0.)],
        impostor: None,
        pose: None,
    });
    renderer.set_pieces(pieces);
    let mut piece_of: Vec<u32> = (0..ROWS).map(|r| r % (PIECES - 1)).collect();
    piece_of[ROWS as usize - 1] = PIECES - 1;
    let mut transforms = vec![math::translate([1e4, 0., 0.]); ROWS as usize];
    transforms[ROWS as usize - 1] = math::IDENTITY;
    renderer.set_placements(&piece_of, &transforms);
    {
        let placed = &renderer.lock().assets.placed;
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
    if (renderer.lock().device.as_ref()).is_some_and(|d| d.arena.is_some()) {
        let placed = &renderer.lock().assets.placed;
        assert!(placed.arena.is_some(), "no arena");
    }
    let center = camera.project([0.; 3], target.bounds(), world);
    assert_eq!(target.pixel(&pixels, center), [0, 255, 0]);
    let hit = target.pick(&renderer, center).unwrap().unwrap();
    assert_eq!(hit.placement, Some(ROWS - 1));
}

#[test]
fn placements_cull_pick_levels_by_screen_size_and_move_in_place() {
    // One piece: a red full-detail cube and a green stand-in below 10 px/m.
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
        impostor: None,
        pose: None,
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
        pose: None,
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

// The arena keeps a piece's indices in 16 bits, two to a word, unless a
// level of it has more than 65,536 vertices (placed::NARROW): such pieces
// sort last in their class and draw from 32-bit runs. Both kinds draw
// what the same meshes draw as plain batches.
#[test]
fn arena_draws_meshes_past_sixteen_bit_indices_as_others() {
    let side = 300u32;
    let mut grid = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    for z in 0..side {
        for x in 0..side {
            let (u, v) = (x as f32 / (side - 1) as f32, z as f32 / (side - 1) as f32);
            grid.positions.push([u * 4. - 2., 0., v * 4. - 2.]);
            grid.normals.push([0., 1., 0.]);
            // A colour per vertex: a wrong index shows.
            grid.colors
                .push(((u * 255.) as u32) << 16 | ((v * 255.) as u32) << 8 | 0x40);
        }
    }
    for z in 0..side - 1 {
        for x in 0..side - 1 {
            let i = z * side + x;
            grid.indices
                .extend([i, i + side, i + 1, i + 1, i + side, i + side + 1]);
        }
    }
    assert!(grid.positions.len() > 1 << 16);
    let pieces = [
        Arc::new(Mesh::new(grid).unwrap()),
        Arc::new(cube(0x40a0ff, Material::Lit)),
    ]
    .map(|m| Piece {
        levels: vec![(m, 0.)],
        impostor: None,
        pose: None,
    });
    let pieces = Vec::from(pieces);
    let rows = [math::IDENTITY, math::translate([0.5, 0.5, 0.])];
    let render = |placed: bool| {
        let renderer = Renderer::default();
        let batches = (pieces.iter().zip(rows))
            .filter(|_| !placed)
            .map(|(p, matrix)| Batch {
                mesh: p.levels[0].0.clone(),
                instances: Arc::new(Instances {
                    matrices: vec![matrix],
                    ids: vec![Some(1)],
                }),
            })
            .collect();
        renderer.set_scene(&Scene {
            batches,
            light: neutral(),
            ..Default::default()
        });
        if placed {
            renderer.set_pieces(
                (pieces.iter())
                    .map(|p| Piece {
                        levels: p.levels.clone(),
                        impostor: None,
                        pose: None,
                    })
                    .collect(),
            );
            renderer.set_placements(&[0, 1], &rows);
        }
        let mut target = Offscreen::new(240, 180).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 6.,
        };
        let camera = Camera {
            fov: 0.9,
            zoom: 2.,
            tilt: 0.8,
            ..Camera::perspective()
        };
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let state = renderer.lock();
        assert_eq!(state.assets.placed.arena.is_some(), placed);
        pixels
    };
    let (off, most) = difference(&render(false), &render(true));
    assert!(
        off <= 240 * 180 / 500 && most <= 16,
        "{off} pixels differ, by up to {most}"
    );
}

#[test]
fn placed_pieces_sample_their_own_textures() {
    let renderer = Renderer::default();
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
            pose: None,
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
    assert!(renderer.lock().assets.placed.arena.is_some());
    let bounds = target.bounds();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, bounds, world));
    assert_eq!(at([0.; 3]), [0, 0, 255]);
    assert_eq!(at([1.2, 0., 0.]), [0, 255, 0]);
}

// Two cubes under neutral light: a plain grey one, and a grey one whose
// normal map's blue lights it at half its emissive colour, its shading
// given way. Placements and plain batches draw them alike, and the
// scene's strength scales the light.
#[test]
fn emissive_light_draws_as_placements_and_batches() {
    let cubes = || {
        let mut plain = cube(0xffffff, Material::Lit).data().clone();
        plain.texture = Some(Texture::solid([128, 128, 128, 255]).into());
        plain.normal_map = Some(Texture::solid([128, 128, 255, 0]).into());
        let mut steady = plain.clone();
        steady.emissive = Some([0.5; 3]);
        [plain, steady].map(|d| Arc::new(Mesh::new(d).unwrap()))
    };
    let at = [-0.8, 0.8].map(|x| math::translate([x, 0., 0.]));
    let draw = |placed: bool, strength: f32| {
        let renderer = Renderer::default();
        let meshes = cubes();
        let batches = match placed {
            true => {
                renderer.set_pieces(
                    (meshes.iter())
                        .map(|m| Piece {
                            levels: vec![(m.clone(), 0.)],
                            impostor: None,
                            pose: None,
                        })
                        .collect(),
                );
                renderer.set_placements(&[0, 1], &at);
                Vec::new()
            }
            false => (meshes.iter().zip(at))
                .map(|(m, matrix)| Batch {
                    mesh: m.clone(),
                    instances: Arc::new(Instances {
                        matrices: vec![matrix],
                        ids: vec![None],
                    }),
                })
                .collect(),
        };
        renderer.set_scene(&Scene {
            batches,
            light: neutral(),
            glow: Glow {
                strength,
                ..Default::default()
            },
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
        at.map(|m| {
            target.pixel(
                &pixels,
                camera.project(math::translation(&m), bounds, world),
            )
        })
    };
    for placed in [true, false] {
        let [plain, steady] = draw(placed, 1.);
        near(plain, [128; 3], 2);
        // Shading gives way: 128/255 times 0.5, nothing of the grey lit.
        near(steady, [64; 3], 2);
        let [_, dim] = draw(placed, 0.5);
        near(dim, [32; 3], 2);
    }
}

// A mesh with the game's alpha reference cuts where Anno's AlphaTest would,
// on every path, its impostor too: at 0.3, alpha 0.4 stays and 0.2 goes,
// where without a reference both go. Anno 117's crowds keep their 0.4
// skins so; its marsh grass cards lose their black ground.
#[test]
fn alpha_reference_decides_the_cut_on_every_path() {
    let mut data = quad(1., 0., Material::Lit);
    data.texture = Some(
        Texture::from_rgba(2, 1, vec![255, 255, 255, 102, 255, 255, 255, 51])
            .unwrap()
            .into(),
    );
    let world = Extent {
        center: [0.; 3],
        size: 4.,
    };
    let camera = Camera::default();
    // White where the quad is drawn, at its alpha-0.4 and alpha-0.2 halves.
    // The impostor's bake lands a frame after the arena.
    let drawn = |renderer: &Renderer| {
        let mut target = Offscreen::new(160, 160).unwrap();
        target.aim(renderer, camera, world);
        let mut pixels = target.settle(renderer).unwrap();
        for _ in 0..2 {
            pixels = target.frame(renderer).unwrap();
        }
        let bounds = target.bounds();
        [-0.5, 0.5].map(|x| {
            let p = target.pixel(&pixels, camera.project([x, 0., 0.], bounds, world));
            p.iter().all(|&c| c > 250)
        })
    };
    for (reference, expected) in [(None, [false, false]), (Some(0.3), [true, false])] {
        data.alpha_ref = reference;
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches: vec![single(data.clone(), math::IDENTITY)],
            light: neutral(),
            ..Default::default()
        });
        assert_eq!(drawn(&renderer), expected, "batch, reference {reference:?}");
        let mesh = Arc::new(Mesh::new(data.clone()).unwrap());
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(mesh.clone(), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&[0], &[math::IDENTITY]);
        assert_eq!(drawn(&renderer), expected, "reference {reference:?}");
        // Below a billion pixels per metre: always the impostor.
        renderer.set_pieces(vec![Piece {
            levels: vec![(mesh.clone(), 0.)],
            impostor: Some(1e9),
            pose: None,
        }]);
        renderer.set_placements(&[0], &[math::IDENTITY]);
        assert_eq!(
            drawn(&renderer),
            expected,
            "impostor, reference {reference:?}"
        );
        assert_eq!(renderer.drawn().placed, [0, 1, 0, 0]);
    }
}

#[test]
fn alpha_cut_takes_the_reference_back_through_the_smoothstep() {
    assert_eq!(super::alpha_cut(None), 0);
    // smoothstep(0, 0.9, 0.327) is 0.3.
    assert_eq!(super::alpha_cut(Some(0.3)), 83);
    assert_eq!(super::alpha_cut(Some(0.5)), 115);
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
        pose: None,
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

/// Every level of a 256² BC1 texture in a color of its own, so a frame
/// shows which levels it sampled.
fn levelled() -> crate::texture::Compressed {
    const COLORS: [[u8; 3]; 9] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [255, 0, 255],
        [0, 255, 255],
        [255, 128, 0],
        [128, 0, 255],
        [255, 255, 255],
    ];
    let Image::Compressed(c) = compressed(Block::Bc1, 256, 256, 9, |l| bc1(COLORS[l])) else {
        unreachable!()
    };
    (*c).clone()
}

/// A renderer with one placed 4 m square showing `image`, whose
/// placement textures let unasked levels go at once, or with `whole`
/// keep every level.
fn residency_renderer(image: Image, whole: bool) -> Renderer {
    let renderer = Renderer::default();
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
    data.texture = Some(image);
    renderer.set_pieces(vec![Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&[0], &[math::IDENTITY]);
    renderer
}

/// The layout level the placed texture's top holds.
fn resident_base(renderer: &Renderer) -> u32 {
    let state = renderer.lock();
    let arena = state.assets.placed.arena.as_ref().expect("the arena");
    let texture = arena.textures.iter().flatten().next().expect("a texture");
    texture.resident_base()
}

const RESIDENCY_WORLD: Extent = Extent {
    center: [0.; 3],
    size: 8.,
};

fn residency_camera(zoom: f32, tilt: f32) -> Camera {
    Camera {
        fov: 0.9,
        zoom,
        tilt,
        ..Camera::perspective()
    }
}

#[test]
fn placement_textures_hold_the_levels_their_distance_needs() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = residency_renderer(levelled().into(), false);
    let whole = residency_renderer(levelled().into(), true);
    let settle = |target: &mut Offscreen, renderer: &Renderer, zoom: f32| {
        target.aim(renderer, residency_camera(zoom, 0.6), RESIDENCY_WORLD);
        target.settle(renderer).unwrap()
    };
    // Far: the square spans a few pixels and keeps its coarse levels.
    settle(&mut target, &renderer, 0.3);
    let far = resident_base(&renderer);
    assert!(far >= 2, "far away it holds from level {far}");
    // Close: 256 texels over most of the frame want level 0, streamed in,
    // and the frame is the one every level draws.
    let near = settle(&mut target, &renderer, 6.);
    assert_eq!(resident_base(&renderer), 0);
    assert!(
        near == settle(&mut target, &whole, 6.),
        "close up, as every level draws"
    );
    // Away again: the levels go once nobody asks for them.
    settle(&mut target, &renderer, 0.3);
    assert_eq!(resident_base(&renderer), far);
}

#[test]
fn resident_levels_sample_as_every_level_would() {
    // Distances a quarter level apart, square on and grazing (where 8x
    // anisotropic filtering picks finer levels along the long axis): the
    // frame with levels left out is the one every level draws, so nothing
    // sampled a level that was not there.
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = residency_renderer(levelled().into(), false);
    let whole = residency_renderer(levelled().into(), true);
    let mut evicted = 0;
    let world = Extent {
        size: 48.,
        ..RESIDENCY_WORLD
    };
    for tilt in [0.05, 0.7, 1.15] {
        for step in 0..36 {
            let zoom = 0.25 * 2f32.powf(step as f32 / 4.);
            let camera = residency_camera(zoom, tilt);
            target.aim(&renderer, camera, world);
            let left_out = target.settle(&renderer).unwrap();
            target.aim(&whole, camera, world);
            let all = target.settle(&whole).unwrap();
            let base = resident_base(&renderer);
            evicted += usize::from(base > 0);
            assert!(
                left_out == all,
                "tilt {tilt}, zoom {zoom}: from level {base}"
            );
        }
    }
    assert!(evicted > 30, "only {evicted} views left levels out");
}

// The level the sampler picks for footprints of every stretch from 1 to 12
// and three turns, on top-down quads whose UVs map screen pixels to
// `major` by `minor` texels: never finer than the residency bound (the
// larger of the minor axis and an eighth of the major) less TEXEL_MARGIN,
// so leaving those levels out changes no pixel. Slow (2,880 frames); run
// it on a new GPU vendor.
#[test]
#[ignore]
fn texel_margin_covers_the_samplers_rounding() {
    // Level l red = 32 l: what reads back names the levels blended.
    let image = compressed(Block::Bc1, 256, 256, 9, |l| bc1([(32 * l) as u8, 0, 0]));
    let mut target = Offscreen::new(64, 64).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 2.,
    };
    let camera = Camera {
        tilt: 0.05,
        ..Camera::default()
    };
    let bounds = target.bounds();
    let centre = camera.project([0., 0., 0.], bounds, world);
    let unit = camera.project([1., 0., 0.], bounds, world);
    let pixels_per_unit = (f32::from(unit.x) - f32::from(centre.x)).abs();
    let mut worst = (f32::NEG_INFINITY, 0., 0., 0.);
    for step in 0..48 {
        let major = 2f32.powf(1. + step as f32 / 16.);
        let ratios = [
            1.0f32, 1.03, 1.1, 1.2, 1.3, 1.4, 1.5, 1.7, 2.0, 2.2, 2.6, 3.0, 3.5, 4.0, 4.5, 6.0,
            7.5, 8.0, 9.0, 12.0,
        ];
        for ratio in ratios {
            for angle in [0.0f32, 0.4, 0.785] {
                let minor = major / ratio;
                let (c, s) = (angle.cos(), angle.sin());
                let m = [[c * major, -s * minor], [s * major, c * minor]]
                    .map(|r| r.map(|v| v * pixels_per_unit / 256.));
                let mut data = quad(1., 0., Material::Lit);
                data.uvs = (data.positions.iter())
                    .map(|p| {
                        let [x, z] = [p[0], p[2]];
                        [
                            0.5 + m[0][0] * x + m[0][1] * z,
                            0.5 + m[1][0] * x + m[1][1] * z,
                        ]
                    })
                    .collect();
                data.texture = Some(image.clone());
                let renderer = Renderer::default();
                renderer.set_scene(&Scene {
                    batches: vec![single(data, math::IDENTITY)],
                    light: neutral(),
                    ..Default::default()
                });
                target.aim(&renderer, camera, world);
                let pixels = target.settle(&renderer).unwrap();
                // BC1 keeps red in 5 bits: 32 l reads back as 33 l.
                let level = target.pixel(&pixels, centre)[0] as f32 / 33.;
                let finest = (level + 0.03).floor();
                let bound = minor.max(major / 8.).log2();
                // Residency keeps floor(bound - margin) and finer.
                let need = bound - finest - 1.;
                if need > worst.0 {
                    worst = (need, major, ratio, angle);
                }
            }
        }
    }
    let (need, major, ratio, angle) = worst;
    eprintln!("worst: {need:.3} levels (major {major:.2}, stretch {ratio}, turned {angle})");
    assert!(need < placed::TEXEL_MARGIN);
}

#[test]
fn released_texels_are_read_again_to_stream_levels_back_in() {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let image = {
        let reads = reads.clone();
        crate::texture::Releasable::new(levelled(), move |_| {
            reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(levelled())
        })
        .unwrap()
    };
    let Image::Releasable(handle) = Image::from(image) else {
        unreachable!()
    };
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = residency_renderer(Image::Releasable(handle.clone()), false);
    let whole = residency_renderer(levelled().into(), true);
    target.aim(&renderer, residency_camera(0.3, 0.6), RESIDENCY_WORLD);
    target.settle(&renderer).unwrap();
    // Nobody asks for more: the texels go.
    target.frame(&renderer).unwrap();
    assert!(handle.is_released());
    let camera = residency_camera(6., 0.6);
    target.aim(&renderer, camera, RESIDENCY_WORLD);
    let near = target.settle(&renderer).unwrap();
    assert!(reads.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    assert_eq!(resident_base(&renderer), 0);
    target.aim(&whole, camera, RESIDENCY_WORLD);
    assert!(near == target.settle(&whole).unwrap());
}

#[test]
fn a_texture_held_from_its_tail_reads_its_chain_only_when_views_ask() {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let full = levelled();
    let floor = super::floor_level(full.width, full.height, full.mips.len());
    assert!(floor > 0, "256² keeps levels under its 64² floor");
    let tail = crate::texture::Compressed {
        width: full.width >> floor,
        height: full.height >> floor,
        mips: full.mips[floor..].to_vec(),
        ..full.clone()
    };
    let image = {
        let reads = reads.clone();
        crate::texture::Releasable::from_tail(full.width, full.height, full.mips.len(), tail, {
            move |first| {
                reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut c = levelled();
                c.mips.drain(..first);
                (c.width, c.height) = (c.width >> first, c.height >> first);
                Some(c)
            }
        })
        .unwrap()
    };
    let image = Image::from(image);
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = residency_renderer(image, false);
    let whole = residency_renderer(levelled().into(), true);
    // Far: the floor is all the view asks for, filled from the tail.
    let camera = residency_camera(0.3, 0.6);
    target.aim(&renderer, camera, RESIDENCY_WORLD);
    let far = target.settle(&renderer).unwrap();
    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    target.aim(&whole, camera, RESIDENCY_WORLD);
    assert!(far == target.settle(&whole).unwrap());
    // Close: the chain is read, and the frame is the one every level draws.
    let camera = residency_camera(6., 0.6);
    target.aim(&renderer, camera, RESIDENCY_WORLD);
    let near = target.settle(&renderer).unwrap();
    assert!(reads.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    assert_eq!(resident_base(&renderer), 0);
    target.aim(&whole, camera, RESIDENCY_WORLD);
    assert!(near == target.settle(&whole).unwrap());
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
        pose: None,
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
    assert!(renderer.lock().assets.placed.atlas.is_some());
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

// Foliage turns to its impostor and then stops at `Detail::reach` from the
// eye, however large it is on screen; placements without one draw on.
#[test]
fn reach_turns_far_foliage_to_impostors_then_drops_it() {
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    let mut data = cube(0xffffff, Material::Lit).data().clone();
    data.texture = Some(Texture::solid([0, 0, 255, 255]).into());
    let mesh = Arc::new(Mesh::new(data).unwrap());
    // Piece 0 has an impostor that its size never asks for; piece 1 none.
    renderer.set_pieces(vec![
        Piece {
            levels: vec![(mesh.clone(), 0.)],
            impostor: Some(0.),
            pose: None,
        },
        Piece {
            levels: vec![(mesh, 0.)],
            impostor: None,
            pose: None,
        },
    ]);
    let mut target = Offscreen::new(320, 240).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 16.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.5,
        tilt: 1.1,
        ..Camera::perspective()
    };
    // Foliage near the pivot and far behind it, and a plain cube far.
    let toward = camera.toward();
    let back = |d: f32| [-toward[0] * d, 0.5, -toward[2] * d];
    let rows = [
        math::translate(back(0.)),
        math::translate(back(6.)),
        math::translate(back(6.)),
    ];
    renderer.set_placements(&[0, 0, 1], &rows);
    let eye = camera.eye(target.bounds(), world).unwrap();
    let from_eye = |p: Vec3| math::length(math::sub(p, eye));
    let (near, far) = (from_eye(back(0.)), from_eye(back(6.)));
    assert!(far > near + 4.);
    let mut drawn = |reach: Option<Reach>| {
        renderer.set_detail(Detail {
            reach,
            ..Default::default()
        });
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        // The bake lands a frame after the arena; counts a frame after that.
        target.frame(&renderer).unwrap();
        target.frame(&renderer).unwrap();
        target.frame(&renderer).unwrap();
        renderer.drawn().placed
    };
    assert_eq!(drawn(None), [3, 0, 0, 0], "every mesh, unreached");
    let mid = (near + far) * 0.5;
    let cut = |impostor, end| {
        Some(Reach {
            impostor,
            end,
            thin: 0.,
        })
    };
    assert_eq!(drawn(cut(1e6, mid)), [2, 0, 0, 0], "far foliage drops");
    assert_eq!(
        drawn(cut(mid, 1e6)),
        [2, 1, 0, 0],
        "far foliage is an impostor"
    );
    assert_eq!(drawn(cut(0., 1e6)), [1, 2, 0, 0], "both are");
    // The band drops each at its own distance within it: none before it,
    // all past its end.
    let band = |end| {
        Some(Reach {
            impostor: 1e6,
            end,
            thin: 0.5,
        })
    };
    assert_eq!(drawn(band(far * 3.)), [3, 0, 0, 0], "before the band");
    assert_eq!(
        drawn(band(near * 0.5)),
        [1, 0, 0, 0],
        "the plain cube alone"
    );
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
    let mut budget = Budget::untimed();
    assert!(array.stream(&off.gpu.queue, &mut budget));
    assert_eq!(budget.used(), 8);
}

/// Four quads in a row along X, each with a normal and surface map, drawn
/// as batches or as placements; the pixel at
/// each quad's centre.
fn mapped_quads(
    maps: [([u8; 4], [u8; 4]); 4],
    light: Light,
    tone: Option<Tone>,
    placed: bool,
) -> [[u8; 3]; 4] {
    mapped_quads_specular(maps, [Specular::FULL; 4], light, tone, placed)
}

/// `mapped_quads` with each quad's `MeshData::specular`.
fn mapped_quads_specular(
    maps: [([u8; 4], [u8; 4]); 4],
    specular: [Specular; 4],
    light: Light,
    tone: Option<Tone>,
    placed: bool,
) -> [[u8; 3]; 4] {
    let solid = |rgba: [u8; 4]| Image::from(Texture::from_rgba(4, 4, rgba.repeat(16)).unwrap());
    let xs = [-3., -1., 1., 3.];
    let meshes = std::array::from_fn::<_, 4, _>(|i| {
        let (normal, surface) = maps[i];
        Arc::new(
            Mesh::new(MeshData {
                texture: Some(solid([200, 200, 200, 255])),
                normal_map: Some(solid(normal)),
                surface_map: Some(solid(surface)),
                specular: specular[i],
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
    if !placed {
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
        let pieces = meshes.iter().map(|m| Piece {
            levels: vec![(m.clone(), 0.)],
            impostor: None,
            pose: None,
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
    for placed in [false, true] {
        let [flat, u, v, occluded] =
            mapped_quads(maps, light, None, placed).map(|p| i32::from(p[0]));
        assert!(
            u > flat + 25 && v > flat + 25,
            "placed {placed}: flat {flat}, +U {u}, +V {v}"
        );
        assert!(
            occluded < flat - 40,
            "placed {placed}: flat {flat}, occluded {occluded}"
        );
    }
}

// Metal shows nothing but specular, so each term a mesh leaves out goes
// dark as a batch and as a placement: the sky's reflection under a sky-only light, the
// sun's highlight under a sun-only one, and both on matte metal that fades
// them (Anno's BRDF_DISABLE_SPEC_ON_ZERO_GLOSS, black there in game too).
#[test]
fn meshes_light_with_the_specular_terms_they_keep() {
    let metal = |gloss| ([128, 128, 255, gloss], [255, 0, 0, 255]);
    let sky = Light {
        sun_color: [0.; 3],
        sky: [1.; 3],
        ground: [1.; 3],
        ..Default::default()
    };
    let sun = Light {
        sun: [0., 1., 0.],
        sun_color: [1.; 3],
        sky: [0.; 3],
        ground: [0.; 3],
        ..Default::default()
    };
    let tone = Tone {
        exposure: 1.,
        sun_occlusion: 0.,
        ambient: 1.,
        curve: Curve::Aces {
            lift_edge: 0.,
            lift_power: 1.,
            scale: 1.,
        },
        auto: None,
    };
    let fades = Specular::MATTE_FADES;
    for placed in [false, true] {
        let [full, no_sky, glossy, matte] = mapped_quads_specular(
            [metal(255), metal(255), metal(255), metal(0)],
            [Specular::FULL, Specular::NO_SKY, fades, fades],
            sky,
            Some(tone),
            placed,
        );
        assert!(full[0] > 100, "placed {placed}: full {full:?}");
        assert!(no_sky[0] < 5, "placed {placed}: no sky {no_sky:?}");
        assert!(glossy[0] > 100, "placed {placed}: glossy fading {glossy:?}");
        assert!(matte[0] < 5, "placed {placed}: matte fading {matte:?}");
        let [full, no_sun, no_sky, _] = mapped_quads_specular(
            [metal(255); 4],
            [
                Specular::FULL,
                Specular::NO_SUN,
                Specular::NO_SKY,
                Specular::FULL,
            ],
            sun,
            Some(tone),
            placed,
        );
        assert!(full[0] > 100, "placed {placed}: sun on full {full:?}");
        assert!(no_sun[0] < 5, "placed {placed}: no sun {no_sun:?}");
        assert!(no_sky[0] > 100, "placed {placed}: sun on no sky {no_sky:?}");
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
        ambient: 1.,
        curve: Curve::Aces {
            lift_edge: 0.,
            lift_power: 1.,
            scale: 0.6,
        },
        auto: None,
    };
    let [p, ..] = mapped_quads([flat(0); 4], sky, Some(tone), false);
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
    for placed in [false, true] {
        let [rough, _, _, glossy] = mapped_quads(
            [flat(0), flat(0), flat(0), flat(255)],
            sun,
            Some(tone),
            placed,
        );
        assert!(
            glossy[0] > rough[0] + 30,
            "placed {placed}: rough {rough:?}, glossy {glossy:?}"
        );
    }
    // The shoulder: gamma 2.2, then exposure, then the shoulder above max
    // linear bending toward white.
    let lite = Tone {
        curve: Curve::Shoulder {
            gamma: 2.2,
            max_linear: 0.5,
            white: 1.3,
        },
        ..tone
    };
    let [p, ..] = mapped_quads([flat(0); 4], sky, Some(lite), false);
    let (o, w) = (0.5f32, 1.3f32);
    let g = linear.powf(1. / 2.2) * 1.5;
    let k = -(w * (o - 1.) - o * o + o) / (w - 1.);
    let shoulder = k * (g - o) / (g - o + k) + o;
    near(p, [(shoulder * 255.).round() as u8; 3], 4);
}

// Automatic exposure brings a scene lit a hundred times brighter out the
// same, where a fixed exposure leaves it far apart.
#[test]
fn auto_exposure_brings_dim_and_bright_scenes_to_the_target() {
    let open = [0, 0, 0, 255];
    let lit = |level: f32| Light {
        sun_color: [0.; 3],
        sky: [level; 3],
        ground: [level; 3],
        ..Default::default()
    };
    let fixed = Tone {
        exposure: 1.,
        ..Default::default()
    };
    let auto = Tone {
        auto: Some(AutoExposure {
            log_min: -14.,
            log_max: 6.,
            low: 0.,
            count: 1.,
            target: 0.18,
            min: 1e-3,
            max: 1e3,
            tolerance: 0.,
            speed_up: 1.,
            speed_down: 1.,
        }),
        ..fixed
    };
    let shade = |level, tone| {
        mapped_quads(
            [([128, 128, 255, 0], open); 4],
            lit(level),
            Some(tone),
            false,
        )[0][1]
    };
    let (dim, bright) = (shade(0.02, fixed), shade(2., fixed));
    assert!(bright > dim + 100, "fixed: dim {dim}, bright {bright}");
    let (dim, bright) = (shade(0.02, auto), shade(2., auto));
    assert!(
        dim.abs_diff(bright) <= 6,
        "auto: dim {dim}, bright {bright}"
    );
}

// SkyMap::light gives back a map that already holds diffuse light, where
// irradiance convolves the map as radiance: for 1 + n.y / 2 the first is
// 1.5 facing up and 0.5 facing down, the second 1 ± 1/3.
#[test]
fn sky_map_light_keeps_the_map_and_irradiance_convolves_it() {
    let (w, h) = (64, 32);
    let texels = (0..w * h)
        .map(|i| [1. + 0.5 * SkyMap::direction(w, h, i % w, i / w)[1]; 3])
        .collect();
    let map = SkyMap {
        width: w,
        height: h,
        texels,
    };
    let eval = |m: [[[f32; 4]; 4]; 3], n: [f32; 3]| {
        let p = [n[0], n[1], n[2], 1.];
        (0..4)
            .flat_map(|i| (0..4).map(move |j| (i, j)))
            .map(|(i, j)| p[i] * m[0][i][j] * p[j])
            .sum::<f32>()
    };
    let (light, irradiance) = (map.light(), map.irradiance());
    assert!((eval(light, [0., 1., 0.]) - 1.5).abs() < 0.02);
    assert!((eval(light, [0., -1., 0.]) - 0.5).abs() < 0.02);
    assert!((eval(irradiance, [0., 1., 0.]) - 4. / 3.).abs() < 0.02);
    assert!((eval(irradiance, [0., -1., 0.]) - 2. / 3.).abs() < 0.02);
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
        radiance: None,
        sun_disc: [0.; 3],
        ambient: None,
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

// A sky map is what the sky shows and what lights the scene: under an even
// half-grey sky the background is that grey, and a surface takes half its
// color whichever way it faces, in place of the light's own sky and ground.
#[test]
fn a_sky_map_draws_the_sky_and_lights_the_scene() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    let (mut scene, _) = scene(Material::Lit);
    scene.markers = Arc::new(Vec::new());
    scene.light = neutral();
    scene.sky = Some(Sky {
        zenith: [0., 0., 1.],
        horizon: [1., 0., 0.],
        sea: None,
        radiance: Some(Arc::new(SkyMap {
            width: 16,
            height: 8,
            texels: vec![[0.5; 3]; 16 * 8],
        })),
        sun_disc: [0.; 3],
        ambient: None,
    });
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 6.,
    };
    let camera = Camera {
        tilt: 1.45,
        fov: 0.9,
        ..Camera::perspective()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    near(target.pixel(&pixels, point(px(160.), px(2.))), [128; 3], 2);
    let cube = target.pixel(&pixels, camera.project([0.; 3], target.bounds(), world));
    near(cube, [0x20, 0x50, 0x80], 3);
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
            ..Default::default()
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
            pose: None,
        }
    };
    let renderer = Renderer::default();
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
        ..Default::default()
    }));
    assert!(foot < 200, "foot {foot}");
    assert!(open > 245 && top > 245, "open {open}, top {top}");
    assert_eq!(shot(None)[0], 255);
}

/// A Blend-stage pass that multiplies the occlusion over every pixel itself
/// (Pass::applies_occlusion), with two-source blending.
#[derive(Default)]
struct Occluder {
    pipeline: std::sync::Mutex<Option<wgpu::RenderPipeline>>,
}

impl Pass for Occluder {
    fn stage(&self) -> Stage {
        Stage::Blend
    }

    fn applies_occlusion(&self) -> bool {
        true
    }

    fn prepare(&self, frame: &mut Prepare<'_>) -> bool {
        let mut pipeline = self.pipeline.lock().unwrap();
        if pipeline.is_some() {
            return true;
        }
        let source = format!(
            "enable dual_source_blending;
            {FRAME_WGSL}
            struct Out {{
                @location(0) @blend_src(0) color: vec4<f32>,
                @location(0) @blend_src(1) under: vec4<f32>,
            }}
            @vertex fn vs(@builtin(vertex_index) v: u32) -> @builtin(position) vec4<f32> {{
                let p = vec2<f32>(f32((v << 1u) & 2u), f32(v & 2u)) * 2.0 - 1.0;
                return vec4<f32>(p, 0.0, 1.0);
            }}
            @fragment fn fs(@builtin(position) clip: vec4<f32>) -> Out {{
                return Out(vec4<f32>(0.0), vec4<f32>(occlusion_factor(vec2<i32>(clip.xy)), 1.0));
            }}"
        );
        let d = frame.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test occluder"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test occluder"),
            bind_group_layouts: &[Some(frame.frame_layout)],
            immediate_size: 0,
        });
        let multiply = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::Src1,
            operation: wgpu::BlendOperation::Add,
        };
        *pipeline = Some(d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test occluder"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HDR_FORMAT,
                    blend: Some(wgpu::BlendState {
                        color: multiply,
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        }));
        true
    }

    fn record(&self, _: &Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(pipeline) = &*self.pipeline.lock().unwrap() {
            pass.set_pipeline(pipeline);
            pass.draw(0..3, 0..1);
        }
    }
}

// A Blend-stage pass that applies the occlusion itself leaves the image the
// kit's own pass for it leaves: the same factor, multiplied in once.
#[test]
fn a_blend_pass_can_apply_the_occlusion_in_the_kits_place() {
    let shot = |passes: Vec<Arc<dyn Pass>>| {
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
            occlusion: Some(Occlusion {
                radius: 1.,
                strength: 1.,
                ..Default::default()
            }),
            passes,
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
        (
            [at([0.6, 0., 0.]), at([2.5, 0., 2.5]), at([0., 1., 0.])],
            target.device().features(),
        )
    };
    let (kit, features) = shot(vec![]);
    if !features.contains(wgpu::Features::DUAL_SOURCE_BLENDING) {
        eprintln!("skipping: no two-source blending");
        return;
    }
    let (pass, _) = shot(vec![Arc::new(Occluder::default())]);
    assert!(kit[0] < 200, "the block's foot is occluded: {kit:?}");
    for (k, p) in kit.iter().zip(&pass) {
        assert!(k.abs_diff(*p) <= 1, "kit {kit:?}, pass {pass:?}");
    }
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
    let marks = outline_marks(&outline, &[Some(5), Some(7), None], [10., 20., 0.5]);
    // Two words of placed rows (row 33 is in the second), one of handles.
    assert_eq!(marks[0], 2);
    assert_eq!(marks[1], 0xffa733ff);
    assert_eq!(marks[2], 2);
    // Where the scene lies in the target, for mapping pane pixels.
    assert_eq!(f32::from_bits(marks[4]), 10.);
    assert_eq!(f32::from_bits(marks[6]), 0.5);
    assert_eq!(marks[8], 1);
    assert_eq!(marks[9], 1 << 1);
    // Id 7 is handle 2, bit 1.
    assert_eq!(marks[10], 1 << 1);
    assert_eq!(marks.len(), 11);
}

// A selection dragged on the GPU: the selected row draws, picks and carries
// its marker at the drag's place while the table keeps where it began, and
// the scene's own markers stay ahead of the selection's. At the drag's end
// the row draws what the table holds.
#[test]
fn selected_rows_drag_and_carry_their_markers_on_the_gpu() {
    let renderer = Renderer::default();
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
        pose: None,
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
        "marker"
    );
    assert_eq!(
        target.pixel(&pixels, at([-3., 0., 3.])),
        [0, 0, 255],
        "scene marker"
    );
    let hit = |target: &mut Offscreen, p: Vec3| {
        target
            .pick(&renderer, at(p))
            .unwrap()
            .and_then(|h| h.placement)
    };
    assert_eq!(hit(&mut target, [3., 0., -3.]), Some(1), "dragged");
    assert_eq!(hit(&mut target, [3., 0., 0.]), None, "left");
    // The host moves the table where the drag landed, then ends it.
    renderer.move_placements(
        1,
        &[math::multiply(&math::translate([0., 0., -3.]), &start)],
    );
    renderer.drag_selection(None);
    target.render(&renderer, camera, world).unwrap();
    assert_eq!(hit(&mut target, [3., 0., -3.]), Some(1), "landed");
}

// A compact mesh hands its geometry back: positions and UVs exact,
// normals to snorm16 precision on both hemispheres, the same triangles
// (meshopt's index codec may rotate one, keeping its winding).
// The arena's 16-bit grids (gpu::Quant): what the games store as halves
// comes back exactly, down to the grid's step; anything else within half
// a step.
#[test]
fn arena_grids_keep_half_floats() {
    // Nearest half float (11 significant bits).
    let half = |v: f32| {
        if v == 0. {
            return v;
        }
        let e = v.abs().log2().floor() as i32 - 10;
        (v / 2f32.powi(e)).round() * 2f32.powi(e)
    };
    // A 40 m building and UVs over two tiles.
    let (lo, hi) = ([-20., 0., -20., 0., -1.], [20., 40., 20., 1., 1.]);
    let q = Quant::new(lo, hi);
    let steps = q.steps();
    assert_eq!(steps[0], 2f32.powi(-10), "a 40 m mesh's grid");
    let back = |a: usize, v: f32| q.origin[a] + q.snap(a, v) as f32 * steps[[0, 0, 0, 1, 2][a]];
    for k in 0..4000 {
        let t = (k as f32 * 0.618_034).fract();
        for a in 0..5 {
            let v = lo[a] + (hi[a] - lo[a]) * t;
            let step = steps[[0, 0, 0, 1, 2][a]];
            assert!((back(a, v) - v).abs() <= step * 0.5, "axis {a}: {v}");
            let h = half(v);
            if (lo[a]..=hi[a]).contains(&h) && h.abs() >= step * 1024. {
                assert_eq!(back(a, h), h, "axis {a}: half {h}");
            }
        }
    }
    // Every corner of the box fits the 16 bits.
    for a in 0..5 {
        assert!(q.snap(a, hi[a]) <= 65535 && back(a, hi[a]) >= hi[a] - steps[[0, 0, 0, 1, 2][a]]);
    }
}

#[test]
fn compact_geometry_round_trips() {
    let data = MeshData {
        positions: vec![[0., 0., 0.], [1., 2., 3.], [-4., 0.5, 2.]],
        uvs: vec![[0., 0.], [1., 0.5], [0.25, 1.]],
        normals: vec![[0., 1., 0.], [0.6, 0., -0.8], [-0.48, 0.6, 0.64]],
        indices: vec![0, 1, 2, 2, 1, 0],
        ..Default::default()
    };
    let back = Mesh::compact(data.clone()).unwrap().geometry();
    assert_eq!(back.positions, data.positions);
    assert_eq!(back.uvs, data.uvs);
    let turned = |t: &[u32]| {
        (0..3)
            .map(|k| [t[k], t[(k + 1) % 3], t[(k + 2) % 3]])
            .min()
            .unwrap()
    };
    let tris = |i: &[u32]| i.chunks(3).map(turned).collect::<Vec<_>>();
    assert_eq!(tris(&back.indices), tris(&data.indices));
    for (a, b) in back.normals.iter().zip(&data.normals) {
        assert!((0..3).all(|k| (a[k] - b[k]).abs() < 1e-3), "{a:?} {b:?}");
    }
}

/// A flat `Flat` sheet `half` units each way at height `y`, `n` cells a
/// side, so a bent sheet follows the ground between its vertices.
fn sheet(half: f32, y: f32, n: u32, color: u32) -> MeshData {
    let mut data = MeshData {
        material: Material::Flat,
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
            data.indices
                .extend([i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
        }
    }
    data
}

#[test]
fn placements_bend_to_the_terrain_when_their_mesh_and_row_allow_it() {
    // 33² samples over 16 units, raised to 3 on x >= 1.5. A sheet 0.5 over
    // its placement's origin on the ground, x -4..4, sinks under the block
    // unless it bends onto it.
    let mut field = flat_field(33);
    let map = Arc::new(HeightMap::new(&field).unwrap());
    let block = Rect {
        x0: 19,
        z0: 0,
        x1: 33,
        z1: 33,
    };
    raise(&mut field, &map, block, 3.);
    let terrain = Arc::new(Terrain {
        heights: map.clone(),
        colors: Arc::new(ControlMap::new(33, 33, [40, 40, 40, 255].repeat(33 * 33)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Flat,
        texture: None,
    });
    let world = Extent {
        center: [0.; 3],
        size: 16.,
    };
    let camera = Camera::default();
    let over = [3., 0., 0.];
    let target = std::cell::RefCell::new(Offscreen::new(320, 320).unwrap());
    let height = |adapt: Option<TerrainAdapt>, rigid: u32, allow: bool| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            terrain: Some(terrain.clone()),
            light: neutral(),
            ..Default::default()
        });
        let mut data = sheet(4., 0.5, 32, rigid << 24 | 0xff0000);
        data.terrain_adapt = adapt;
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        let row = adapting(math::IDENTITY, allow, false, 0.);
        renderer.set_placements(&[0], &[row]);
        let mut target = target.borrow_mut();
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        let cursor = camera.project([over[0], 3.5, over[2]], target.bounds(), world);
        let hit = target.pick(&renderer, cursor).unwrap();
        (hit.map(|h| (h.placement, h.position[1])), renderer)
    };
    let relative = Some(TerrainAdapt::Relative);
    // Rigid: buried under the block, which has no pick handle.
    assert_eq!(height(None, 0, true).0, None);
    assert_eq!(height(relative, 0, false).0, None);
    assert_eq!(height(relative, 255, true).0, None);
    // Relative keeps its 0.5 over the ground; absolute lies on it.
    let (hit, renderer) = height(relative, 0, true);
    let (placement, y) = hit.unwrap();
    assert_eq!(placement, Some(0));
    assert!((y - 3.5).abs() < 0.05, "{y}");
    let (placement, y) = height(Some(TerrainAdapt::Absolute), 0, true).0.unwrap();
    assert_eq!(placement, Some(0));
    assert!((y - 3.).abs() < 0.05, "{y}");
    // Sculpting moves it with no new scene or placements.
    raise(&mut field, &map, block, 1.);
    let mut target = target.borrow_mut();
    target.aim(&renderer, camera, world);
    let cursor = camera.project([over[0], 1.5, over[2]], target.bounds(), world);
    let y = target.pick(&renderer, cursor).unwrap().unwrap().position[1];
    assert!((y - 1.5).abs() < 0.05, "{y}");
}

#[test]
fn decals_blend_over_placements_by_coverage_and_are_never_picked() {
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::default();
    let ground = || {
        let mut q = quad(4., 0., Material::Flat);
        q.colors = vec![0xff0000; 4];
        q
    };
    // Lying right on the ground: only the bias lifts it.
    let decal = |opacity: f32, mask: Option<u8>| {
        let mut q = quad(
            2.,
            0.,
            Material::Decal(Decal {
                opacity,
                bias: 0.01,
                grazing: true,
                ground: false,
                edge: None,
            }),
        );
        q.colors = vec![0x0000ff; 4];
        q.alpha_map =
            mask.map(|g| Image::from(Texture::from_rgba(1, 1, vec![0, g, 0, 255]).unwrap()));
        q
    };
    let piece = |data: MeshData| Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: None,
        pose: None,
    };
    // One target throughout: every new one is a Vulkan instance, and the
    // loader crashes now and then creating them on parallel test threads.
    let mut target = Offscreen::new(160, 160).unwrap();
    let mut draw = |opacity: f32, mask: Option<u8>| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        renderer.set_pieces(vec![piece(ground()), piece(decal(opacity, mask))]);
        renderer.set_placements(&[0, 1], &[math::IDENTITY, math::IDENTITY]);
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let centre = camera.project([0.; 3], target.bounds(), world);
        let hit = target.pick(&renderer, centre).unwrap().unwrap();
        assert_eq!(hit.placement, Some(0), "the ground, not the decal");
        target.pixel(&pixels, centre)
    };
    near(draw(1., None), [0, 0, 255], 2);
    near(draw(1., Some(255)), [0, 0, 255], 2);
    // A mask with no green hides it.
    near(draw(1., Some(0)), [255, 0, 0], 2);
    let [r, g, b] = draw(0.5, None);
    assert!(
        r > 100 && b > 100 && g < 5 && r.abs_diff(b) < 4,
        "{r} {g} {b}"
    );
}

// A decal handed over as a scene batch, as a model view shows a ground
// tile with nothing placed, blends over the batch under it the way a
// placed one does.
#[test]
fn decal_batches_blend_over_what_is_under_them() {
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::default();
    let mut ground = quad(4., 0., Material::Flat);
    ground.colors = vec![0xff0000; 4];
    let mut decal = quad(
        2.,
        0.,
        Material::Decal(Decal {
            opacity: 1.,
            bias: 0.01,
            grazing: true,
            ground: false,
            edge: None,
        }),
    );
    decal.colors = vec![0x0000ff; 4];
    let mut target = Offscreen::new(160, 160).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![
            single(ground, math::IDENTITY),
            single(decal, math::IDENTITY),
        ],
        light: neutral(),
        ..Default::default()
    });
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    near(at([0.; 3]), [0, 0, 255], 2);
    // Past the decal's edge the ground shows.
    near(at([3., 0., 0.]), [255, 0, 0], 2);
}

#[test]
fn overlapping_decals_blend_in_row_order_on_every_cull() {
    // Translucent decals of one piece blend in the order their draw lists
    // them. The cull appends visible rows with atomics, in whatever order
    // the GPU runs it: 1800 moderate_m_01's summit decals came out in
    // another order in one redraw of twenty-odd, a few pixels 3 of 255 off.
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera::default();
    let ground = || {
        let mut q = quad(4., 0., Material::Flat);
        q.colors = vec![0x808080; 4];
        q
    };
    // Four colours across, so each turn lays another over the centre.
    let texels = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
    ];
    let mut decal = quad(
        2.,
        0.,
        Material::Decal(Decal {
            opacity: 0.5,
            bias: 0.01,
            grazing: true,
            ground: false,
            edge: None,
        }),
    );
    decal.texture = Some(Image::from(
        Texture::from_rgba(4, 1, texels.concat()).unwrap(),
    ));
    let piece = |data: MeshData| Piece {
        levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
        impostor: None,
        pose: None,
    };
    let turned = |angle: f32| {
        let (s, c) = angle.sin_cos();
        let mut m = math::IDENTITY;
        (m[0], m[2], m[8], m[10]) = (c, -s, s, c);
        m
    };
    let mut target = Offscreen::new(160, 160).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![piece(ground()), piece(decal)]);

    // The later row lies on top: half its colour over what is under it.
    let left = [-1.5, 0., 0.];
    let mut first_over = |rows: &[f32]| {
        let piece_of: Vec<u32> = std::iter::once(0).chain(rows.iter().map(|_| 1)).collect();
        let transforms: Vec<_> = std::iter::once(math::IDENTITY)
            .chain(rows.iter().map(|&a| turned(a)))
            .collect();
        renderer.set_placements(&piece_of, &transforms);
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        target.pixel(&pixels, camera.project(left, target.bounds(), world))
    };
    let pi = std::f32::consts::PI;
    // Unturned, the left edge samples texel 0 (red); half a turn, texel 3
    // (yellow): green shows how much of the yellow is on top.
    let yellow_on_top = first_over(&[0., pi]);
    let red_on_top = first_over(&[pi, 0.]);
    assert!(
        yellow_on_top[1] > red_on_top[1] + 40,
        "yellow {yellow_on_top:?}, red {red_on_top:?}"
    );

    // Thousands across many workgroups, each turned its own way: every
    // cull lists them in row order, so every redraw is the same frame.
    let angles: Vec<f32> = (0..4096).map(|i| i as f32 * 2.399_963).collect();
    let piece_of: Vec<u32> = std::iter::once(0).chain(angles.iter().map(|_| 1)).collect();
    let transforms: Vec<_> = std::iter::once(math::IDENTITY)
        .chain(angles.iter().map(|&a| turned(a)))
        .collect();
    renderer.set_placements(&piece_of, &transforms);
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    let mut away = camera;
    away.yaw += 0.3;
    // A camera move culls again; back, it culls once more.
    let mut redraw = || {
        target.aim(&renderer, away, world);
        target.frame(&renderer).unwrap();
        target.aim(&renderer, camera, world);
        target.frame(&renderer).unwrap()
    };
    let first = redraw();
    for n in 1..24 {
        let pixels = redraw();
        let differ = (first.chunks(4).zip(pixels.chunks(4)))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differ, 0, "redraw {n}: {differ} pixels differ");
    }
}

#[test]
fn world_tiling_takes_texture_uvs_from_world_position() {
    // Red on the left half of the texture, green on the right. The quad's
    // own UVs put red at x = -1; tiled at 4 m, u = x / 4 wraps red to x = 1.
    let texture = Image::from(
        Texture::from_rgba(2, 1, [[255, 0, 0, 255], [0, 255, 0, 255]].concat()).unwrap(),
    );
    let world = Extent {
        center: [0.; 3],
        size: 4.,
    };
    let camera = Camera::default();
    let mut target = Offscreen::new(160, 160).unwrap();
    for (tiling, left) in [(None, [255, 0, 0]), (Some(4.), [0, 255, 0])] {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            ..Default::default()
        });
        let mut data = quad(2., 0., Material::Lit);
        data.texture = Some(texture.clone());
        data.ignore_alpha = true;
        data.world_tiling = tiling.map(|scale| WorldTiling {
            scale,
            diagonal: false,
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(data).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&[0], &[math::IDENTITY]);
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at =
            |x: f32| target.pixel(&pixels, camera.project([x, 0., 0.], target.bounds(), world));
        let right = [left[1], left[0], 0];
        near(at(-1.), left, 3);
        near(at(1.), right, 3);
    }
}

#[test]
fn projected_decals_drape_over_the_terrain_and_fade_at_their_edges() {
    // 33² samples over 16 units, raised to 2 on x >= 1.5. A 6 m box decal
    // placed half a metre up, under the block's top, lies on the ground and
    // the block alike; its grid is 5 whole cells, so it fades out between
    // 2.5 and 2.7 m from its centre.
    let mut field = flat_field(33);
    let map = Arc::new(HeightMap::new(&field).unwrap());
    let block = Rect {
        x0: 19,
        z0: 0,
        x1: 33,
        z1: 33,
    };
    raise(&mut field, &map, block, 2.);
    let terrain = Arc::new(Terrain {
        heights: map.clone(),
        colors: Arc::new(ControlMap::new(33, 33, [40, 40, 40, 255].repeat(33 * 33)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Flat,
        texture: None,
    });
    let world = Extent {
        center: [0.; 3],
        size: 16.,
    };
    let camera = Camera::default();
    let mut target = Offscreen::new(320, 320).unwrap();
    let mut grid = sheet(0.5, 0., 24, 0x0000ff);
    grid.normals = vec![[0., 1., 0.]; grid.positions.len()];
    grid.material = Material::Decal(Decal {
        opacity: 1.,
        bias: 0.2,
        grazing: true,
        ground: true,
        edge: Some(DecalEdge {
            extents: [3., 3.],
            fade: EdgeFade::Grid {
                distance: 0.2,
                height: None,
            },
        }),
    });
    grid.terrain_adapt = Some(TerrainAdapt::Drape);
    let mesh = Arc::new(Mesh::new(grid).unwrap());
    let mut place = math::translate([0., 0.5, 0.]);
    place[0] = 6.;
    place[10] = 6.;
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        terrain: Some(terrain.clone()),
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(mesh.clone(), 0.)],
        impostor: None,
        pose: None,
    }]);
    // Not allowed to bend, as a rigid prop: draping ignores that.
    renderer.set_placements(&[0], &[adapting(place, false, false, 0.)]);
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    for p in [[-1., 0., 0.], [0., 0., 1.], [2.2, 2., 0.]] {
        let [r, g, b] = at(p);
        assert!(b > 150 && r < 60 && g < 60, "{p:?}: {r} {g} {b}");
    }
    // Past the fade: the terrain shows.
    near(at([-2.9, 0., 0.]), [40, 40, 40], 3);
    near(at([0., 0., 2.9]), [40, 40, 40], 3);
}

// Decals that lie on the ground draw right after it and before every model,
// as Anno 1800 orders its frame (Terrain, Decals, Streets, Decals, Models),
// so a model just over the ground inside a decal's box shows whole however
// far the decal moves toward the viewer: here a rail 0.1 m up under a dirt
// decal pushed 0.8 m, seen straight down. The rail placed (lit, which the
// visibility buffer takes) and as a scene batch.
#[test]
fn models_draw_over_ground_decals_however_near_the_ground() {
    let map = Arc::new(HeightMap::new(&flat_field(33)).unwrap());
    let terrain = Arc::new(Terrain {
        heights: map,
        colors: Arc::new(ControlMap::new(33, 33, [40, 40, 40, 255].repeat(33 * 33)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Flat,
        texture: None,
    });
    let world = Extent {
        center: [0.; 3],
        size: 16.,
    };
    let camera = Camera::default();
    let mut dirt = sheet(0.5, 0., 12, 0x0000ff);
    dirt.normals = vec![[0., 1., 0.]; dirt.positions.len()];
    dirt.material = Material::Decal(Decal {
        opacity: 1.,
        bias: 0.8,
        grazing: false,
        ground: true,
        edge: None,
    });
    dirt.terrain_adapt = Some(TerrainAdapt::Drape);
    let dirt = Arc::new(Mesh::new(dirt).unwrap());
    let mut box_ = math::IDENTITY;
    box_[0] = 6.;
    box_[10] = 6.;
    let rail = || {
        let mut q = quad(1., 0.1, Material::Lit);
        for p in &mut q.positions {
            p[0] *= 2.;
            p[2] *= 0.25;
        }
        q.colors = vec![0xff0000; 4];
        q
    };
    let mut target = Offscreen::new(160, 160).unwrap();
    for batch in [false, true] {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            terrain: Some(terrain.clone()),
            batches: batch
                .then(|| single(rail(), math::IDENTITY))
                .into_iter()
                .collect(),
            light: neutral(),
            ..Default::default()
        });
        let piece = |mesh: Arc<Mesh>| Piece {
            levels: vec![(mesh, 0.)],
            impostor: None,
            pose: None,
        };
        let mut pieces = vec![piece(dirt.clone())];
        let mut rows = vec![adapting(box_, false, false, 0.)];
        if !batch {
            pieces.push(piece(Arc::new(Mesh::new(rail()).unwrap())));
            rows.push(math::IDENTITY);
        }
        let indices: Vec<u32> = (0..pieces.len() as u32).collect();
        renderer.set_pieces(pieces);
        renderer.set_placements(&indices, &rows);
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
        let [r, g, b] = at([1., 0.1, 0.]);
        assert!(
            r > 150 && g < 60 && b < 60,
            "batch {batch}: the rail under the decal is {r} {g} {b}"
        );
        // Beside the rail the decal covers the ground, past its box
        // the ground shows.
        let [r, g, b] = at([0., 0., 2.]);
        assert!(b > 150 && r < 60, "batch {batch}: decal {r} {g} {b}");
        near(at([0., 0., 5.]), [40, 40, 40], 3);
    }
}

/// A plugged pass for the plug-point tests: covers the whole view in
/// `color` at reversed depth `depth`, or draws nothing.
struct Fill {
    stage: Stage,
    replaces: bool,
    color: Option<[f32; 3]>,
    depth: f32,
    pipeline: std::sync::Mutex<Option<wgpu::RenderPipeline>>,
}

impl Fill {
    fn new(stage: Stage, replaces: bool, color: Option<[f32; 3]>, depth: f32) -> Arc<Self> {
        Arc::new(Self {
            stage,
            replaces,
            color,
            depth,
            pipeline: Default::default(),
        })
    }
}

impl Pass for Fill {
    fn stage(&self) -> Stage {
        self.stage
    }

    fn replaces(&self) -> bool {
        self.replaces
    }

    fn prepare(&self, frame: &mut Prepare<'_>) -> bool {
        let Some([r, g, b]) = self.color else {
            return true;
        };
        let mut pipeline = self.pipeline.lock().unwrap();
        if pipeline.is_some() {
            return true;
        }
        let opaque = self.stage == Stage::Opaque;
        let fragment = if opaque {
            "@fragment fn fs_fill() -> Opaque {
                return Opaque(vec4<f32>(COLOR, 1.0), 0u, pack_rgb9e5(COLOR));
            }"
        } else {
            "@fragment fn fs_fill() -> @location(0) vec4<f32> { return vec4<f32>(COLOR, 1.0); }"
        };
        let source = format!(
            "{FRAME_WGSL}
            const COLOR = vec3<f32>({r:?}, {g:?}, {b:?});
            @vertex fn vs_fill(@builtin(vertex_index) v: u32) -> @builtin(position) vec4<f32> {{
                let p = vec2<f32>(f32((v << 1u) & 2u), f32(v & 2u)) * 2.0 - 1.0;
                return vec4<f32>(p, {:?}, 1.0);
            }}
            {fragment}",
            self.depth
        );
        let d = frame.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test fill"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test fill"),
            bind_group_layouts: &[Some(frame.frame_layout)],
            immediate_size: 0,
        });
        let targets: Vec<Option<wgpu::ColorTargetState>> = if opaque {
            OPAQUE_TARGETS.iter().map(|&f| Some(f.into())).collect()
        } else {
            vec![Some(HDR_FORMAT.into())]
        };
        *pipeline = Some(d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test fill"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_fill"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_fill"),
                targets: &targets,
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(opaque),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        }));
        true
    }

    fn record(&self, _: &Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(pipeline) = &*self.pipeline.lock().unwrap() {
            pass.set_pipeline(pipeline);
            pass.draw(0..3, 0..1);
        }
    }
}

// A plugged Sky-stage pass draws over the kit's backdrop, or in its place
// when it replaces it; the backdrop's sky still lights the scene either way.
#[test]
fn a_plugged_sky_pass_draws_over_or_instead_of_the_backdrop() {
    let gradient = Sky {
        zenith: [0., 0., 1.],
        horizon: [1., 0., 0.],
        sea: None,
        radiance: None,
        sun_disc: [0.; 3],
        ambient: None,
    };
    let world = Extent {
        center: [0.; 3],
        size: 6.,
    };
    let camera = Camera {
        tilt: 1.45,
        fov: 0.9,
        ..Camera::perspective()
    };
    let top = |passes: Vec<Arc<dyn Pass>>| {
        let mut target = Offscreen::new(64, 64).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light: neutral(),
            sky: Some(gradient.clone()),
            passes,
            ..Default::default()
        });
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        target.pixel(&pixels, point(px(32.), px(1.)))
    };
    let backdrop = top(Vec::new());
    assert!(
        backdrop[0] > 0 || backdrop[2] > 0,
        "the kit's sky: {backdrop:?}"
    );
    assert_eq!(top(vec![Fill::new(Stage::Sky, false, None, 0.)]), backdrop);
    assert_eq!(top(vec![Fill::new(Stage::Sky, true, None, 0.)]), [0; 3]);
    let green = Some([0., 1., 0.]);
    assert_eq!(
        top(vec![Fill::new(Stage::Sky, true, green, 0.)]),
        [0, 255, 0]
    );
    // Over the backdrop, a plugged sky covers it as well.
    assert_eq!(
        top(vec![Fill::new(Stage::Sky, false, green, 0.)]),
        [0, 255, 0]
    );
}

// A plugged Opaque-stage pass shares the opaque depth with the kit's work:
// behind the terrain it shows only past it; replacing, it draws where the
// terrain mesh would have and picks nothing.
#[test]
fn a_plugged_ground_pass_shares_depth_or_draws_instead_of_the_terrain() {
    let side = 33;
    let field = flat_field(side);
    let half = (side - 1) as f32 * 0.25;
    let terrain = Arc::new(Terrain {
        heights: Arc::new(HeightMap::new(&field).unwrap()),
        colors: Arc::new(
            ControlMap::new(side, side, [255, 0, 0, 255].repeat(side * side)).unwrap(),
        ),
        color_gain: 1.,
        tints: false,
        material: Material::Lit,
        texture: None,
    });
    let world = Extent {
        center: [0.; 3],
        size: half * 4.,
    };
    let camera = Camera::default();
    let draw = |passes: Vec<Arc<dyn Pass>>| {
        let mut target = Offscreen::new(128, 128).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            terrain: Some(terrain.clone()),
            light: neutral(),
            passes,
            ..Default::default()
        });
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let bounds = target.bounds();
        let ground = camera.project([0.; 3], bounds, world);
        let picked = target.pick(&renderer, ground).unwrap();
        // The view is twice the terrain's size: its corner is past it.
        let corner = target.pixel(&pixels, point(px(1.), px(1.)));
        (target.pixel(&pixels, ground), corner, picked.is_some())
    };
    let green = Some([0., 1., 0.]);
    assert_eq!(draw(Vec::new()), ([255, 0, 0], [0; 3], false));
    // Far behind the ground: the terrain stays in front of it.
    let behind = draw(vec![Fill::new(Stage::Opaque, false, green, 1e-6)]);
    assert_eq!((behind.0, behind.1), ([255, 0, 0], [0, 255, 0]));
    // Replacing: no terrain mesh, the plugged ground everywhere, unpicked.
    assert_eq!(
        draw(vec![Fill::new(Stage::Opaque, true, green, 1e-6)]),
        ([0, 255, 0], [0, 255, 0], false)
    );
    // One still loading leaves the kit's terrain in place.
    struct Loading;
    impl Pass for Loading {
        fn stage(&self) -> Stage {
            Stage::Opaque
        }
        fn replaces(&self) -> bool {
            true
        }
        fn prepare(&self, _: &mut Prepare<'_>) -> bool {
            false
        }
        fn record(&self, _: &Record<'_>, _: &mut wgpu::RenderPass<'_>) {}
    }
    let mut target = Offscreen::new(128, 128).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        terrain: Some(terrain.clone()),
        light: neutral(),
        passes: vec![Arc::new(Loading)],
        ..Default::default()
    });
    target.aim(&renderer, camera, world);
    let ground = camera.project([0.; 3], target.bounds(), world);
    // The pass loading keeps the renderer pending, so `settle` would wait
    // for good: draw until the scene is up, which a loaded machine's upload
    // budget can take several frames for.
    let started = std::time::Instant::now();
    let mut pixels = target.frame(&renderer).unwrap();
    while target.pixel(&pixels, ground) == [0; 3] && started.elapsed().as_secs() < 10 {
        pixels = target.frame(&renderer).unwrap();
    }
    assert_eq!(target.pixel(&pixels, ground), [255, 0, 0]);
}

/// A plugged pass that only casts: a square of side 2 at height `HEIGHT`,
/// centred where `at` says, its version bumped by whoever moves it. One
/// that replaces stands in for the kit's ground and draws none.
struct Square {
    replaces: bool,
    at: std::sync::Mutex<[f32; 2]>,
    version: std::sync::atomic::AtomicU64,
    gpu: std::sync::Mutex<Option<(wgpu::RenderPipeline, wgpu::Buffer)>>,
}

impl Square {
    const HEIGHT: f32 = 4.;

    fn move_to(&self, at: [f32; 2]) {
        *self.at.lock().unwrap() = at;
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Pass for Square {
    fn stage(&self) -> Stage {
        Stage::Opaque
    }

    fn replaces(&self) -> bool {
        self.replaces
    }

    fn prepare(&self, frame: &mut Prepare<'_>) -> bool {
        let mut gpu = self.gpu.lock().unwrap();
        if gpu.is_some() {
            return true;
        }
        let source = format!(
            "{FRAME_WGSL}
            @vertex fn vs_square(@location(0) world: vec3<f32>) -> @builtin(position) vec4<f32> {{
                return globals.clip * vec4<f32>(world, 1.0);
            }}"
        );
        let d = frame.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test square"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test square"),
            bind_group_layouts: &[Some(frame.frame_layout)],
            immediate_size: 0,
        });
        let corners = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test square"),
            size: 4 * 12,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test square shadow"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_square"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                })],
                compilation_options: Default::default(),
            },
            fragment: None,
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: SHADOW_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        *gpu = Some((pipeline, corners));
        true
    }

    fn record(&self, _: &Record<'_>, _: &mut wgpu::RenderPass<'_>) {}

    fn shadow(&self) -> Option<Caster> {
        let [x, z] = *self.at.lock().unwrap();
        Some(Caster {
            version: self.version.load(std::sync::atomic::Ordering::Relaxed),
            bounds: [
                [x - 1., Self::HEIGHT, z - 1.],
                [x + 1., Self::HEIGHT, z + 1.],
            ],
        })
    }

    fn record_shadow(&self, frame: &Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let [x, z] = *self.at.lock().unwrap();
        let y = Self::HEIGHT;
        let corners = [
            [x - 1., y, z - 1.],
            [x + 1., y, z - 1.],
            [x - 1., y, z + 1.],
            [x + 1., y, z + 1.],
        ];
        if let Some((pipeline, buffer)) = &*self.gpu.lock().unwrap() {
            // Lands before this frame's commands run.
            frame
                .queue
                .write_buffer(buffer, 0, bytemuck::cast_slice(&corners));
            pass.set_pipeline(pipeline);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..4, 0..1);
        }
    }
}

// A plugged pass casts into the sun's shadow map: the ground under it
// darkens, the map is not drawn again while its version holds, and moving
// it (a new version) moves the shadow.
#[test]
fn a_plugged_caster_shadows_the_ground_and_redraws_only_when_it_changes() {
    // Sun up and toward +X: a caster 4 up shadows the ground 2 toward -X.
    let light = Light {
        sun: [1., 2., 0.],
        sun_color: [0.7; 3],
        sky: [0.3; 3],
        ground: [0.3; 3],
        shadows: Some(Shadows {
            resolution: 1024,
            softness: 1.,
            ..Default::default()
        }),
        ..Default::default()
    };
    let square = Arc::new(Square {
        replaces: false,
        at: std::sync::Mutex::new([0., -4.]),
        version: Default::default(),
        gpu: Default::default(),
    });
    let mut target = Offscreen::new(320, 320).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(quad(10., 0., Material::Lit), math::IDENTITY)],
        light,
        passes: vec![square.clone()],
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
    let at = |t: &Offscreen, pixels: &[u8], p: Vec3| {
        t.pixel(pixels, camera.project(p, t.bounds(), world))
    };
    // Ambient 0.3 in shadow; 0.3 + 0.7 * cos(26.6°) = 0.93 in the sun.
    near(at(&target, &pixels, [-2., 0., -4.]), [77; 3], 6);
    near(at(&target, &pixels, [-2., 0., 4.]), [237; 3], 6);
    for _ in 0..3 {
        target.frame(&renderer).unwrap();
        assert!(!renderer.drawn().shadow, "unchanged casters keep the map");
    }
    square.move_to([0., 4.]);
    let pixels = target.frame(&renderer).unwrap();
    assert!(renderer.drawn().shadow, "a new version draws the map again");
    near(at(&target, &pixels, [-2., 0., 4.]), [77; 3], 6);
    near(at(&target, &pixels, [-2., 0., -4.]), [237; 3], 6);
}

// The kit's ground is a pass the scene lists: a replacing ground that casts
// takes its place, so the terrain's shadow goes and the replacement's
// shows; `GroundDraw::Data` lists none, so the terrain draws nowhere.
#[test]
fn a_replacing_ground_takes_the_terrains_place_and_shadow() {
    // Flat at 0 but for a block 4 high over x 0..4, z -8..-2, wide enough
    // for the terrain's coarse shadow stride (2 m): with the sun up and
    // toward +X it shadows the ground from x -2 to 2 beside it.
    let side = 33;
    let mut field = flat_field(side);
    for z in 0..=12 {
        for x in 16..=24 {
            field.heights[z * side + x] = 4.;
        }
    }
    let terrain = Arc::new(Terrain {
        heights: Arc::new(HeightMap::new(&field).unwrap()),
        colors: Arc::new(ControlMap::new(side, side, [255; 4].repeat(side * side)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Lit,
        texture: None,
    });
    let light = Light {
        sun: [1., 2., 0.],
        sun_color: [0.7; 3],
        sky: [0.3; 3],
        ground: [0.3; 3],
        shadows: Some(Shadows {
            resolution: 1024,
            softness: 1.,
            ..Default::default()
        }),
        ..Default::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 22.,
    };
    let camera = Camera {
        fov: 0.9,
        ..Default::default()
    };
    // A white receiver just over the flat ground, under the wall's top.
    let draw = |passes: Vec<Arc<dyn Pass>>, ground: GroundDraw| {
        let mut target = Offscreen::new(320, 320).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches: vec![single(quad(10., 0.01, Material::Lit), math::IDENTITY)],
            terrain: Some(terrain.clone()),
            light,
            passes,
            ground,
            ..Default::default()
        });
        // A placed block well away from the spots read: its shadow draw
        // runs before the ground's, leaving its own pipeline bound.
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(cube(0xffffff, Material::Lit)), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&[0], &[math::translate([6., 2., 6.])]);
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
        (
            at([-1., 0.01, -5.]),
            at([-2., 0.01, 4.]),
            renderer.drawn().triangles,
        )
    };
    let square = || -> Arc<dyn Pass> {
        Arc::new(Square {
            replaces: true,
            at: std::sync::Mutex::new([0., 4.]),
            version: Default::default(),
            gpu: Default::default(),
        })
    };
    // Ambient 0.3 in shadow; 0.3 + 0.7 * cos(26.6°) = 0.93 in the sun.
    let (wall, beside, triangles) = draw(Vec::new(), GroundDraw::Kit);
    near(wall, [77; 3], 6);
    near(beside, [237; 3], 6);
    assert!(triangles > 2, "the kit's terrain draws");
    let (wall, under_square, triangles) = draw(vec![square()], GroundDraw::Kit);
    near(wall, [237; 3], 6);
    near(under_square, [77; 3], 6);
    assert_eq!(triangles, 2, "only the receiver: no terrain");
    // Without the kit's ground nothing draws or casts the terrain.
    let (wall, beside, triangles) = draw(Vec::new(), GroundDraw::Data);
    near(wall, [237; 3], 6);
    near(beside, [237; 3], 6);
    assert_eq!(triangles, 2);
}

// Cloud shadows shade the sun's light where the map's alpha is cloud and
// leave it where the map is clear; the sky's light is untouched.
#[test]
fn cloud_shadows_shade_the_sun_where_the_map_is_cloud() {
    // 2x1: cloud on the left texel, clear on the right; one repeat spans
    // 20 world units, so the ground's left half is under cloud.
    let map = Arc::new(ControlMap::new(2, 1, vec![0, 0, 0, 255, 0, 0, 0, 0]).unwrap());
    let layer = CloudLayer {
        scale: 1. / 20.,
        offset: [-0.5, 0.],
        velocity: [0.; 2],
    };
    let light = Light {
        sun: [0., 1., 0.],
        sun_color: [0.6; 3],
        sky: [0.2; 3],
        ground: [0.2; 3],
        ..Default::default()
    };
    let draw = |clouds: Option<CloudShadows>| {
        let mut target = Offscreen::new(200, 200).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches: vec![single(quad(10., 0., Material::Lit), math::IDENTITY)],
            light,
            cloud_shadows: clouds,
            ..Default::default()
        });
        let world = Extent {
            center: [0.; 3],
            size: 22.,
        };
        let camera = Camera::default();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at =
            |x: f32| target.pixel(&pixels, camera.project([x, 0., 0.], target.bounds(), world));
        (at(-5.), at(5.))
    };
    let (left, right) = draw(Some(CloudShadows {
        map,
        layers: [layer; 2],
        contrast: 1.,
        power: 1.,
    }));
    let (plain_left, plain_right) = draw(None);
    near(right, plain_right, 2);
    assert!(
        u32::from(left[0]) + 20 < u32::from(plain_left[0]),
        "under cloud {left:?}, clear {plain_left:?}"
    );
}

/// An Overlay pass drawing nothing that hands the frame a 2x1 cloud shade:
/// dark on the left texel, lit on the right, over a 16-unit square.
struct Shade(std::sync::Mutex<Option<CloudShade>>);

impl Pass for Shade {
    fn stage(&self) -> Stage {
        Stage::Overlay
    }

    fn prepare(&self, frame: &mut Prepare<'_>) -> bool {
        let mut shade = self.0.lock().unwrap();
        if shade.is_none() {
            let texture = frame.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("test cloud shade"),
                size: wgpu::Extent3d {
                    width: 2,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            frame.queue.write_texture(
                texture.as_image_copy(),
                &[0, 255],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(2),
                    rows_per_image: None,
                },
                texture.size(),
            );
            *shade = Some(CloudShade {
                view: texture.create_view(&Default::default()),
                origin: [-8., -8.],
                size: 16.,
                version: 0,
            });
        }
        true
    }

    fn record(&self, _frame: &Record<'_>, _pass: &mut wgpu::RenderPass<'_>) {}

    fn cloud_shade(&self) -> Option<CloudShade> {
        self.0.lock().unwrap().clone()
    }
}

// A pass's cloud shade lights what lies in its square, overriding the
// scene's cloud map there, and hands back to the map outside it.
#[test]
fn a_cloud_shade_overrides_the_cloud_map_inside_its_square() {
    // The map is cloud everywhere.
    let map = Arc::new(ControlMap::new(1, 1, vec![0, 0, 0, 255]).unwrap());
    let clouds = CloudShadows {
        map,
        layers: [CloudLayer {
            scale: 1. / 20.,
            offset: [0.; 2],
            velocity: [0.; 2],
        }; 2],
        contrast: 1.,
        power: 1.,
    };
    let light = Light {
        sun: [0., 1., 0.],
        sun_color: [0.6; 3],
        sky: [0.2; 3],
        ground: [0.2; 3],
        ..Default::default()
    };
    let draw = |clouds: Option<CloudShadows>, shade: bool| {
        let mut target = Offscreen::new(200, 200).unwrap();
        let renderer = Renderer::default();
        let passes: Vec<Arc<dyn Pass>> = if shade {
            vec![Arc::new(Shade(Default::default()))]
        } else {
            Vec::new()
        };
        renderer.set_scene(&Scene {
            batches: vec![single(quad(10., 0., Material::Lit), math::IDENTITY)],
            light,
            cloud_shadows: clouds,
            passes,
            ..Default::default()
        });
        let world = Extent {
            center: [0.; 3],
            size: 22.,
        };
        let camera = Camera::default();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let at =
            |x: f32| target.pixel(&pixels, camera.project([x, 0., 0.], target.bounds(), world));
        (at(-4.), at(4.), at(9.5))
    };
    let (dark, lit, outside) = draw(Some(clouds), true);
    let (plain, _, plain_outside) = draw(None, false);
    near(lit, plain, 2);
    assert!(
        u32::from(dark[0]) + 20 < u32::from(plain[0]),
        "under the shade's cloud {dark:?}, clear {plain:?}"
    );
    assert!(
        u32::from(outside[0]) + 20 < u32::from(plain_outside[0]),
        "outside the square, under the map {outside:?}, clear {plain_outside:?}"
    );
}

/// A Water-stage pass for the reflection test: a mirror plane at y = 0
/// across `half` world units each way, showing what `ssr_trace` finds along
/// the reflected view, black where it finds nothing.
struct Mirror {
    half: f32,
    steps: u32,
    state: std::sync::Mutex<Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout, wgpu::Buffer)>>,
}

impl Pass for Mirror {
    fn stage(&self) -> Stage {
        Stage::Water
    }

    fn prepare(&self, frame: &mut Prepare<'_>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.is_some() {
            return true;
        }
        let source = format!(
            "{FRAME_WGSL}{SSR_WGSL}
            @group(1) @binding(0) var scene: texture_2d<u32>;
            @group(1) @binding(1) var<uniform> ssr: Ssr;
            struct Out {{ @builtin(position) clip: vec4<f32>, @location(0) world: vec3<f32> }}
            @vertex fn vs(@builtin(vertex_index) v: u32) -> Out {{
                let c = array<vec2<f32>, 6>(vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
                    vec2(-1.0, -1.0), vec2(1.0, 1.0), vec2(-1.0, 1.0));
                let p = vec3<f32>(c[v].x * {h:?}, 0.0, c[v].y * {h:?});
                return Out(globals.clip * vec4<f32>(p, 1.0), p);
            }}
            @fragment fn fs(in: Out) -> @location(0) vec4<f32> {{
                let view = normalize(in.world - globals.eye.xyz);
                let r = ssr_trace(scene, in.world, reflect(view, vec3<f32>(0.0, 1.0, 0.0)), ssr, vec2<f32>(0.0));
                return vec4<f32>(r.rgb * r.a, 1.0);
            }}",
            h = self.half
        );
        let d = frame.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test mirror"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty,
            count: None,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("test mirror"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(
                    1,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
            ],
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test mirror"),
            bind_group_layouts: &[Some(frame.frame_layout), Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test mirror"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(HDR_FORMAT.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let params = Ssr {
            steps: self.steps,
            reach: 20.,
            thickness: 1.,
            edge: 0.05,
        }
        .params();
        use wgpu::util::DeviceExt;
        let buffer = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("test mirror"),
            contents: bytemuck::cast_slice(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        *state = Some((pipeline, layout, buffer));
        true
    }

    fn record(&self, frame: &Record<'_>, pass: &mut wgpu::RenderPass<'_>) {
        let state = self.state.lock().unwrap();
        let Some((pipeline, layout, buffer)) = &*state else {
            return;
        };
        let group = frame.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test mirror"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(frame.scene),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buffer.as_entire_binding(),
                },
            ],
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(1, &group, &[]);
        pass.draw(0..6, 0..1);
    }
}

// SSR_WGSL: a mirror drawn after the opaque scene shows a red wall standing
// on it, reflected from the opaque copy, where its reflected rays meet the
// wall; beside the wall the rays meet nothing and the mirror stays dark.
// With no steps (reflections off) it finds nothing anywhere.
#[test]
fn screen_space_reflections_find_the_opaque_scene() {
    reflect(48);
    reflect(0);
}

fn reflect(steps: u32) {
    let mut target = Offscreen::new(160, 120).unwrap();
    // A red wall across x -1..1 standing at z = -2, 3 units high.
    let wall = MeshData {
        positions: vec![[-1., 0., -2.], [1., 0., -2.], [1., 3., -2.], [-1., 3., -2.]],
        normals: vec![[0., 0., 1.]; 4],
        colors: vec![0xff2020; 4],
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
        material: Material::Lit,
        ..Default::default()
    };
    let wall = Batch::new(
        Arc::new(Mesh::new(wall).unwrap()),
        Arc::new(Instances {
            matrices: vec![math::IDENTITY],
            ids: vec![Some(1)],
        }),
    );
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![wall],
        light: neutral(),
        passes: vec![Arc::new(Mirror {
            half: 6.,
            steps,
            state: Default::default(),
        })],
        ..Default::default()
    });
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let camera = Camera {
        tilt: 1.0,
        fov: 0.9,
        ..Camera::perspective()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    // The wall faces the camera at z = -2 if the camera looks down -z;
    // otherwise mirror the test points.
    let toward = if camera.project([0., 1., -2.], target.bounds(), world).y
        < camera.project([0., 1., 2.], target.bounds(), world).y
    {
        1.
    } else {
        -1.
    };
    let [r, g, b] = at([0., 1., -2. * toward]);
    assert!(r > g + 40 && r > b + 40, "the wall is red: {r} {g} {b}");
    let [r, g, b] = at([0., 0., -1.2 * toward]);
    if steps == 0 {
        assert!(r < 30 && g < 30 && b < 30, "no reflection: {r} {g} {b}");
        return;
    }
    assert!(r > g + 25 && r > b + 25, "its reflection: {r} {g} {b}");
    let [r, g, b] = at([4.5, 0., -1.2 * toward]);
    assert!(r < 30 && g < 30 && b < 30, "nothing beside it: {r} {g} {b}");
}

// Placements cast into the sun's shadow map at full detail and as
// impostors. Impostors face the sun there: facing the
// view's eye, a low view looking across the sun turned them edge-on to it
// and a tree's shadow thinned to a line.
#[test]
fn placements_and_their_impostors_shadow_the_ground() {
    for impostor in [false, true] {
        for yaw in [0., std::f32::consts::FRAC_PI_2] {
            let renderer = Renderer::default();
            renderer.set_scene(&Scene {
                batches: vec![single(quad(10., 0., Material::Lit), math::IDENTITY)],
                light: Light {
                    sun: [1., 2., 0.],
                    sun_color: [0.7; 3],
                    sky: [0.3; 3],
                    ground: [0.3; 3],
                    shadows: Some(Shadows {
                        resolution: 1024,
                        softness: 1.,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            });
            renderer.set_pieces(vec![Piece {
                levels: vec![(Arc::new(cube(0xffffff, Material::Lit)), 0.)],
                // Below a billion pixels per metre: always the impostor.
                impostor: impostor.then_some(1e9),
                pose: None,
            }]);
            // A unit cube centred 4 up shadows the ground 2 toward -X,
            // half a unit to each side in Z.
            renderer.set_placements(&[0], &[math::translate([0., 4., 0.])]);
            let mut target = Offscreen::new(320, 320).unwrap();
            let world = Extent {
                center: [0.; 3],
                size: 22.,
            };
            let camera = Camera {
                fov: 0.9,
                tilt: 1.25,
                yaw,
                ..Default::default()
            };
            target.aim(&renderer, camera, world);
            target.settle(&renderer).unwrap();
            // The bake lands a frame after the arena, the map the frame after.
            target.frame(&renderer).unwrap();
            let pixels = target.frame(&renderer).unwrap();
            let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
            let case = format!("impostor {impostor}, yaw {yaw}");
            for z in [-0.45, 0., 0.45] {
                let shadowed = at([-2., 0., z]);
                assert!(
                    shadowed[0] < 140,
                    "{case}: {shadowed:?} at z {z} in the sun"
                );
            }
            near(at([-2., 0., 3.]), [237; 3], 6);
        }
    }
}

// A scene's map spread over a wide scene loses a thin caster's shadow; the
// view's cascades keep it sharp near the camera, and moving the camera
// draws them again.
#[test]
fn cascades_keep_thin_shadows_near_the_camera() {
    let shade = |cascades: u32, nudge: f32| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches: vec![
                single(quad(400., 0., Material::Lit), math::IDENTITY),
                // A 0.2-unit post 2 up: its shadow lands 1 toward -X.
                single(
                    cube(0xffffff, Material::Lit).data().clone(),
                    [
                        0.2, 0., 0., 0., 0., 0.2, 0., 0., 0., 0., 0.2, 0., 0., 2., 0., 1.,
                    ],
                ),
            ],
            light: Light {
                sun: [1., 2., 0.],
                sun_color: [0.7; 3],
                sky: [0.3; 3],
                ground: [0.3; 3],
                shadows: Some(Shadows {
                    resolution: 256,
                    softness: 1.,
                    cascades,
                    cascade_resolution: 1024,
                    reach: 100.,
                }),
                ..Default::default()
            },
            ..Default::default()
        });
        let mut target = Offscreen::new(320, 320).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 6.,
        };
        let camera = Camera {
            fov: 0.9,
            tilt: 0.6,
            yaw: nudge,
            ..Default::default()
        };
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        target.pixel(
            &pixels,
            camera.project([-1., 0., 0.], target.bounds(), world),
        )
    };
    let blurred = shade(0, 0.);
    let sharp = shade(3, 0.);
    assert!(sharp[0] < 120, "cascaded: {sharp:?}");
    assert!(
        blurred[0] > sharp[0] + 60,
        "scene map {blurred:?} vs cascades {sharp:?}"
    );
    assert!(shade(3, 0.3)[0] < 120, "a turned camera's cascades");
}

// A turning view may keep its cascades a size step coarser than it wants
// (Device::lights); once it rests it draws them again and shades as a view
// that never moved.
#[test]
fn turned_views_rest_with_the_cascades_a_still_view_draws() {
    let scene = Scene {
        batches: vec![
            single(quad(400., 0., Material::Lit), math::IDENTITY),
            single(
                cube(0xffffff, Material::Lit).data().clone(),
                [
                    1., 0., 0., 0., 0., 3., 0., 0., 0., 0., 1., 0., 4., 1.5, 2., 1.,
                ],
            ),
            single(
                cube(0xffffff, Material::Lit).data().clone(),
                [
                    2., 0., 0., 0., 0., 1., 0., 0., 0., 0., 2., 0., -6., 0.5, -3., 1.,
                ],
            ),
        ],
        light: Light {
            sun: [1., 2., 0.5],
            sun_color: [0.7; 3],
            sky: [0.3; 3],
            ground: [0.3; 3],
            shadows: Some(Shadows {
                resolution: 512,
                softness: 1.,
                cascades: 3,
                cascade_resolution: 512,
                reach: 60.,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 30.,
    };
    let camera = |i: usize| Camera {
        fov: 0.9,
        tilt: 1.0,
        yaw: 0.2 * (i as f32 * 0.3).sin(),
        ..Default::default()
    };
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let mut target = Offscreen::new(320, 240).unwrap();
    target.aim(&renderer, camera(0), world);
    target.settle(&renderer).unwrap();
    let mut redrawn = 0;
    for i in 1..=40 {
        target.aim(&renderer, camera(i), world);
        target.frame(&renderer).unwrap();
        redrawn += renderer.drawn().cascades;
    }
    assert!(redrawn > 0, "the turn redrew no cascade: it tests nothing");
    let rested = target.settle(&renderer).unwrap();

    let fresh = Renderer::default();
    fresh.set_scene(&scene);
    let mut other = Offscreen::new(320, 240).unwrap();
    other.aim(&fresh, camera(40), world);
    let still = other.settle(&fresh).unwrap();
    let (off, most) = difference(&rested, &still);
    assert_eq!(
        off, 0,
        "rested view differs from a still one by up to {most}"
    );
}

// Seen from far off, a small radius covers under a pixel and finds
// nothing; a floor on its screen size, as HBAO+ in Anno keeps one, grows
// it back so the block's foot still darkens.
#[test]
fn occlusion_keeps_a_least_screen_radius_far_away() {
    let foot = |min_pixels| {
        let mut target = Offscreen::new(320, 320).unwrap();
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            batches: vec![
                single(quad(60., 0., Material::Lit), math::IDENTITY),
                single(
                    cube(0xffffff, Material::Lit).data().clone(),
                    math::translate([0., 0.5, 0.]),
                ),
            ],
            light: neutral(),
            occlusion: Some(Occlusion {
                radius: 0.02,
                strength: 1.,
                min_pixels,
                ..Default::default()
            }),
            ..Default::default()
        });
        let world = Extent {
            center: [0.; 3],
            size: 100.,
        };
        let camera = Camera {
            fov: 0.9,
            ..Camera::perspective()
        };
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        target.pixel(
            &pixels,
            camera.project([0.75, 0., 0.], target.bounds(), world),
        )[0]
    };
    assert!(foot(0.) > 245, "no floor: {}", foot(0.));
    assert!(foot(8.) < 225, "8 px floor: {}", foot(8.));
}

/// A 64 m flat terrain under the checker ground (`checker`), the west half
/// the checker: its scene, the paint map and the half-width.
fn paged_ground() -> (Scene, Arc<ControlMap>, f32) {
    paged_ground_over(&flat_field(129))
}

/// paged_ground over `field`, 129 samples a side.
fn paged_ground_over(field: &Heightfield) -> (Scene, Arc<ControlMap>, f32) {
    paged_ground_with(field, checker::Relief::None)
}

/// paged_ground over `field`, with the checker's `relief`.
fn paged_ground_with(
    field: &Heightfield,
    relief: checker::Relief,
) -> (Scene, Arc<ControlMap>, f32) {
    let side = field.grid.width;
    let half = (side - 1) as f32 * field.grid.spacing / 2.;
    // Paint texels of a quarter metre, as an island's control maps; the
    // east half painted solid.
    let n = 256;
    let paint = (0..n * n).flat_map(|i| {
        if i % n < n / 2 {
            [0; 4]
        } else {
            [255, 0, 0, 0]
        }
    });
    let paint = Arc::new(ControlMap::new(n, n, paint.collect()).unwrap());
    let terrain = Arc::new(Terrain {
        heights: Arc::new(HeightMap::new(field).unwrap()),
        colors: Arc::new(ControlMap::new(side, side, [255; 4].repeat(side * side)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Lit,
        texture: None,
    });
    let scene = Scene {
        terrain: Some(terrain),
        light: neutral(),
        ground: GroundDraw::Surface(checker::factory(paint.clone(), relief)),
        ..Default::default()
    };
    (scene, paint, half)
}

/// The kit's own test ground, two layers over the terrain's grid: west of
/// its paint an 8 by 8 checker of 2 by 2 texel squares, 12 m a repeat and
/// bilinear between texel centres as a layer without mips reads; solid
/// blue where the paint map's red is up, filtered between paint texels.
/// Paint strokes reach the pages as edits.
mod checker {
    use super::*;
    use crate::gpu::terrain::{Ground, GroundFactory, GroundPrepare};
    use std::collections::VecDeque;

    /// What the checker declares of relief.
    #[derive(Clone, Copy, PartialEq)]
    pub(super) enum Relief {
        None,
        /// Raised and sunk everywhere, so every tile is rough.
        Bumps,
        /// Declared but raising nothing, as 1800's moderate set ships its
        /// height maps with no displacement ranges.
        Flat,
    }

    const WGSL: &str = r#"
@group(3) @binding(0) var t_paint: texture_2d<f32>;
fn checker_texel(i: vec2<i32>) -> vec3<f32> {
    let t = ((i % 8) + 8) % 8;
    if (t.x / 2 + t.y / 2) % 2 == 0 { return vec3<f32>(220.0, 180.0, 90.0) / 255.0; }
    return vec3<f32>(40.0, 90.0, 30.0) / 255.0;
}
fn checker(world: vec3<f32>) -> vec3<f32> {
    let t = world.xz / 12.0 * 8.0 - 0.5;
    let b = vec2<i32>(floor(t));
    let f = t - floor(t);
    let top = mix(checker_texel(b), checker_texel(b + vec2<i32>(1, 0)), f.x);
    let bottom = mix(checker_texel(b + vec2<i32>(0, 1)), checker_texel(b + vec2<i32>(1, 1)), f.x);
    return mix(top, bottom, f.y);
}
fn ground_displacement(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> f32 {
    if !BUMPS { return 0.0; }
    return 0.4 * sin(world.x * 3.1) * sin(world.z * 2.3);
}
fn ground(uv: vec2<f32>, world: vec3<f32>, n: vec3<f32>, dw: array<vec3<f32>, 2>) -> GroundSample {
    let painted = textureSampleLevel(t_paint, s_albedo, uv, 0.0).r;
    let color = mix(checker(world), vec3<f32>(60.0, 60.0, 200.0) / 255.0, painted);
    return GroundSample(vec4<f32>(decode(color), 1.0), n, 0.0, ground_displacement(uv, world, n, dw), 0.0);
}
"#;

    fn wgsl(bumps: bool) -> &'static str {
        static FLAT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        static BUMPY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let made = |on: bool| format!("const BUMPS: bool = {on};\n{WGSL}");
        if bumps {
            BUMPY.get_or_init(|| made(true))
        } else {
            FLAT.get_or_init(|| made(false))
        }
    }

    struct Checker {
        relief: Relief,
        grid: crate::heightfield::Grid,
        paint: ResidentControl,
        layout: wgpu::BindGroupLayout,
        group: wgpu::BindGroup,
        filled: bool,
        serial: u64,
        /// The last strokes in world X/Z; older serials take the grid.
        log: VecDeque<[f32; 4]>,
    }

    pub(super) fn factory(paint: Arc<ControlMap>, relief: Relief) -> GroundFactory {
        Arc::new(move |init| {
            let d = init.device;
            let paint = ResidentControl::new(d, "test checker paint", paint.clone(), Kept::Rgba);
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("test checker"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            });
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("test checker"),
                layout: &layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&paint.view),
                }],
            });
            Box::new(Checker {
                relief,
                grid: init.grid,
                paint,
                layout,
                group,
                filled: false,
                serial: 0,
                log: VecDeque::new(),
            })
        })
    }

    impl Ground for Checker {
        fn wgsl(&self) -> &str {
            wgsl(self.relief == Relief::Bumps)
        }
        fn layout(&self) -> &wgpu::BindGroupLayout {
            &self.layout
        }
        fn bind_group(&self) -> Option<&wgpu::BindGroup> {
            self.filled.then_some(&self.group)
        }
        fn prepare(&mut self, frame: &mut GroundPrepare<'_>) -> bool {
            let queue = frame.queue().clone();
            if !self.paint.fill(&queue, frame.budget) {
                return false;
            }
            self.filled = true;
            if let Some(r) = self.paint.sync(&queue) {
                // Texels are filtered: one more each way.
                let [w, h] = self.paint.source.size().map(|v| v as f32);
                let [ex, ez] = self.grid.size();
                let [ox, oz] = self.grid.origin;
                self.serial += 1;
                if self.log.len() == 16 {
                    self.log.pop_front();
                }
                self.log.push_back([
                    ox + (r.x0 as f32 - 1.) / w * ex,
                    oz + (r.z0 as f32 - 1.) / h * ez,
                    ox + (r.x1 as f32 + 1.) / w * ex,
                    oz + (r.z1 as f32 + 1.) / h * ez,
                ]);
            }
            true
        }
        fn edits(&self, since: u64, out: &mut Vec<[f32; 4]>) -> u64 {
            let count = self.serial.saturating_sub(since) as usize;
            if count > self.log.len() {
                let [x, z] = self.grid.origin;
                let [w, h] = self.grid.size();
                out.push([x, z, x + w, z + h]);
            } else {
                out.extend(self.log.iter().skip(self.log.len() - count));
            }
            self.serial
        }
        fn relief_bound(&self) -> f32 {
            if self.relief == Relief::Bumps {
                0.4
            } else {
                0.
            }
        }
        fn has_relief(&self) -> bool {
            self.relief != Relief::None
        }
        fn rough(&self, _area: [f32; 4]) -> bool {
            self.relief == Relief::Bumps
        }
    }
}

#[test]
fn baked_ground_pages_match_live_splat_shading() {
    let camera = Camera {
        tilt: 0.9,
        ..Camera::default()
    };
    let (pages, _) = paged_matches_live(camera, 320, 240, false, [2., 0.5]);
    assert!(pages.wanted > 0 && pages.baked == pages.wanted, "{pages:?}");
}

// Block compressed albedo pages: the checker, three colors in hard edges,
// is BC3's worst case; real ground's grain hides its error.
#[test]
fn compressed_ground_pages_stay_near_live_splat_shading() {
    let camera = Camera {
        tilt: 0.9,
        ..Camera::default()
    };
    let (pages, _) = paged_matches_live(camera, 320, 240, true, [3.5, 2.]);
    assert!(pages.wanted > 0 && pages.baked == pages.wanted, "{pages:?}");
}

// Pages fill sheets row by row (terrain.rs PAGE_SHEET_SIDE): a view
// wanting more than a sheet row's worth still shades as live ground does,
// and the pool takes what its texels need. As 384 one-page array layers
// NVIDIA's driver padded it to 357 MiB.
#[test]
fn ground_pages_past_a_sheets_first_row_match_live_shading() {
    let camera = Camera {
        tilt: 1.1,
        zoom: 8.,
        fov: 1.1,
        ..Camera::default()
    };
    let (pages, report) = paged_matches_live(camera, 640, 480, true, [3.5, 2.]);
    assert!(
        pages.wanted > 16 && pages.baked == pages.wanted,
        "{pages:?}"
    );
    // Tests share the device: each pool's own textures, not the sum, and
    // others keep RGBA8 albedo, so the smallest is this one's. Albedo is
    // BC3 where the device has it. The checker has neither normal maps nor
    // relief, so its pool keeps no normals (paged_matches_live checks).
    let bc = bc_supported(Offscreen::new(8, 8).unwrap().device());
    if let Some(report) = report {
        let smallest = (report.allocations.iter())
            .filter(|a| a.name == "editor ground pages")
            .map(|a| a.size)
            .min();
        let most = if bc { 45 } else { 180 };
        assert!(
            smallest.is_some_and(|b| b < most << 20),
            "{smallest:?} bytes of albedo pages"
        );
    }
}

// The page pool grows to what its views use and keeps the pages it baked:
// a small view, then a closer, bigger one wanting more pages than the pool
// starts with, drawn in turn. Neither bakes the other's pages again, and
// the small view draws the same bytes as before the pool grew under it:
// what a page draws must not depend on the pool's size, or a view kept
// open draws other pixels than a fresh one with the same pages.
#[test]
fn ground_page_pool_grows_for_two_views_and_keeps_their_pages() {
    let (scene, _, half) = paged_ground();
    let world = Extent {
        center: [0.; 3],
        size: half * 2.,
    };
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let sheets = || {
        let state = renderer.lock();
        let pages = state.device.as_ref().and_then(|d| d.ground_pages.as_ref());
        pages.map_or(0, |p| p.sheets)
    };
    let counts = |slot: usize| renderer.lock().slots[slot].drawn.pages;
    let mut small = Offscreen::new(320, 240).unwrap();
    // Wanting more pages than the pool's first sheet holds.
    let mut big = Offscreen::new(2560, 1920).unwrap();
    let near = Camera {
        tilt: 0.9,
        ..Camera::default()
    };
    let close = Camera {
        tilt: 1.1,
        zoom: 8.,
        fov: 1.1,
        ..Camera::default()
    };
    small.aim_in(&renderer, 0, near, world);
    let before = small.settle(&renderer).unwrap();
    let first = sheets();
    assert!(counts(0).wanted > 0 && counts(0).baked == counts(0).wanted);
    big.aim_in(&renderer, 1, close, world);
    for _ in 0..200 {
        big.frame_in(&renderer, 1).unwrap();
        small.frame_in(&renderer, 0).unwrap();
        let [a, b] = [counts(0), counts(1)];
        if a.baked == a.wanted && b.baked == b.wanted {
            break;
        }
    }
    let grown = sheets();
    assert!(
        grown > first,
        "{} pages wanted, pool of {first} sheets",
        counts(1).wanted
    );
    for _ in 0..3 {
        big.frame_in(&renderer, 1).unwrap();
        let again = small.frame_in(&renderer, 0).unwrap();
        let [a, b] = [counts(0), counts(1)];
        assert_eq!((a.new, b.new), (0, 0), "pages baked again: {a:?} {b:?}");
        assert_eq!(a.baked, a.wanted, "{a:?}");
        let off = (before.iter().zip(&again)).filter(|(a, b)| a != b).count();
        assert!(
            off == 0,
            "{off} bytes of the small view changed as the pool grew"
        );
    }
    assert_eq!(sheets(), grown);
}

/// Draws paged_ground through `camera` into a `width`x`height` target,
/// then the same ground shaded live at every pixel (`Detail::live_ground`),
/// and checks the two agree inside the ground's edges: by `most`, the mean
/// difference and the percentage of pixels off by more than 24. Pages
/// are block compressed with `bc` (where the device can sample it); the
/// bake's own placement shows best without. The paged frame's page
/// counts, and the device's memory just after it.
fn paged_matches_live(
    camera: Camera,
    width: usize,
    height: usize,
    bc: bool,
    most: [f64; 2],
) -> (super::PageCounts, Option<wgpu::AllocatorReport>) {
    let mut target = Offscreen::new(width as u32, height as u32).unwrap();
    let (scene, _, half) = paged_ground();
    let world = Extent {
        center: [0.; 3],
        size: half * 2.,
    };
    let renderer = Renderer::default();
    if !bc {
        renderer.lock().assets.bc = Some(false);
    }
    renderer.set_scene(&scene);
    target.aim(&renderer, camera, world);
    let baked = target.settle(&renderer).unwrap();
    let pages = renderer.drawn().pages;
    let report = target.device().generate_allocator_report();
    let normals = (renderer.lock().device.as_ref())
        .and_then(|d| d.ground_pages.as_ref())
        .map(|p| p.normal_view().is_some());
    assert_eq!(
        normals,
        Some(false),
        "a pool without normal maps or relief keeps normals"
    );
    renderer.set_detail(Detail {
        live_ground: true,
        ..Default::default()
    });
    let live = target.settle(&renderer).unwrap();
    assert_eq!(
        renderer.drawn().pages.baked,
        0,
        "live ground bakes no pages"
    );
    // Inside the ground's edges: the terrain's skirts show past them.
    let (mut sum, mut off, mut pixels) = (0u64, 0, 0);
    for y in height * 7 / 24..height * 17 / 24 {
        for x in width / 4..width * 3 / 4 {
            let i = (y * width + x) * 4;
            let worst = (0..3)
                .map(|c| baked[i + c].abs_diff(live[i + c]))
                .max()
                .unwrap();
            sum += u64::from(worst);
            off += usize::from(worst > 24);
            pixels += 1;
        }
    }
    let mean = sum as f64 / pixels as f64;
    let share = off as f64 * 100. / pixels as f64;
    eprintln!("pages against live: mean {mean:.2}, {share:.2}% off by more than 24");
    assert!(mean < most[0], "mean difference {mean}");
    assert!(
        share < most[1],
        "{off} of {pixels} pixels differ by more than 24"
    );
    (pages, report)
}

// Relief tiles across the view's edge draw only their chunks in view
// (gpu::CHUNK_GROUND): the same pixels as whole tiles.
#[test]
fn ground_drawn_by_chunk_matches_whole_tiles() {
    let side = 129;
    let mut field = flat_field(side);
    let grid = field.grid;
    for (i, h) in field.heights.iter_mut().enumerate() {
        let (x, z) = (
            (i % side) as f32 * grid.spacing,
            (i / side) as f32 * grid.spacing,
        );
        *h = (x * 0.3).sin() * 2. + (z * 0.2).cos();
    }
    // Bumps everywhere: rough, so near tiles take strides under a sample.
    let (mut scene, _, _) = paged_ground_with(&field, checker::Relief::Bumps);
    scene.light = Light {
        sun: [0.4, 0.8, 0.3],
        ..neutral()
    };
    let world = Extent {
        center: [0.; 3],
        size: 64.,
    };
    // Close and turned: the nearest tiles take strides under a sample and
    // reach past the view's sides and bottom.
    let camera = Camera {
        zoom: 5.,
        tilt: 1.0,
        yaw: 0.5,
        ..Camera::default()
    };
    let chunked = |on: bool| CHUNK_GROUND.store(on, std::sync::atomic::Ordering::Relaxed);
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    target.aim(&renderer, camera, world);
    chunked(true);
    let by_chunk = target.settle(&renderer).unwrap();
    let fine = renderer.drawn().triangles;
    chunked(false);
    let whole = target.settle(&renderer).unwrap();
    chunked(true);
    // Four tiles at one sample would be 32k triangles.
    assert!(
        fine > 100_000,
        "{fine} triangles: no tile finer than a sample"
    );
    let (off, most) = difference(&by_chunk, &whole);
    assert_eq!(off, 0, "chunks differ from whole tiles by up to {most}");
}

// Paged ground draws what its pages shade, then the rest
// (gpu::SPLIT_GROUND): the same pixels as one draw, over steep ground and
// what lies behind it, and again once a height edit has made more of it
// steep.
#[test]
fn ground_drawn_in_two_matches_one_draw() {
    let mut field = flat_field(129);
    // A ridge along Z west of the middle: flanks at normal y 0.45.
    let grid = field.grid;
    for (i, h) in field.heights.iter_mut().enumerate() {
        let x = grid.origin[0] + (i % 129) as f32 * grid.spacing;
        *h = (8. - 2. * (x + 8.).abs()).max(0.);
    }
    let (mut scene, _, half) = paged_ground_over(&field);
    scene.light = Light {
        sun: [0.4, 0.8, 0.3],
        ..neutral()
    };
    let mut target = Offscreen::new(320, 240).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: half * 2.,
    };
    let camera = Camera {
        tilt: 1.0,
        yaw: 0.4,
        ..Camera::default()
    };
    let renderer = Renderer::default();
    let split = |on: bool| SPLIT_GROUND.store(on, std::sync::atomic::Ordering::Relaxed);
    let both = |target: &mut Offscreen| {
        split(true);
        renderer.set_scene(&scene);
        target.aim(&renderer, camera, world);
        let two = target.settle(&renderer).unwrap();
        let pages = renderer.drawn().pages;
        assert!(pages.wanted > 0 && pages.baked == pages.wanted, "{pages:?}");
        split(false);
        renderer.set_scene(&scene);
        let one = target.settle(&renderer).unwrap();
        split(true);
        (two, one)
    };
    let same = |(two, one): (Vec<u8>, Vec<u8>)| {
        let worst = two
            .iter()
            .zip(&one)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 2, "two draws differ from one by {worst}");
    };
    same(both(&mut target));
    // A block raised 6 m east of the middle: cliffs where it was flat.
    let terrain = scene.terrain.as_ref().unwrap();
    let rect = crate::heightfield::Rect {
        x0: 80,
        z0: 40,
        x1: 100,
        z1: 70,
    };
    raise(&mut field, &terrain.heights, rect, 6.);
    same(both(&mut target));
}

// Ground up to normal y 0.6 bakes when it has no relief: its pages,
// texels stretched up the slope and side projections squeezed into X/Z,
// still match the ground shaded live on the same slope.
#[test]
fn steep_ground_pages_match_live_shading() {
    let mut target = Offscreen::new(320, 240).unwrap();
    // Normal y 0.65: under the old 0.7, over page_flat's 0.6.
    let slope = (1. / 0.65f32.powi(2) - 1.).sqrt();
    let mut field = flat_field(129);
    let grid = field.grid;
    for (i, h) in field.heights.iter_mut().enumerate() {
        *h = slope * (grid.origin[0] + (i % 129) as f32 * grid.spacing);
    }
    let (scene, _, half) = paged_ground_over(&field);
    let world = Extent {
        center: [0.; 3],
        size: half * 2.,
    };
    let camera = Camera {
        tilt: 0.6,
        yaw: -0.5,
        ..Camera::default()
    };
    let renderer = Renderer::default();
    // The bake's stretch shows best on uncompressed pages.
    renderer.lock().assets.bc = Some(false);
    renderer.set_scene(&scene);
    target.aim(&renderer, camera, world);
    let baked = target.settle(&renderer).unwrap();
    let pages = renderer.drawn().pages;
    assert!(pages.wanted > 0 && pages.baked == pages.wanted, "{pages:?}");
    renderer.set_detail(Detail {
        live_ground: true,
        ..Default::default()
    });
    let live = target.settle(&renderer).unwrap();
    // Over the checker, three pixels in from where either stops: the
    // terrain's skirts show past its edges, and the solid half matches
    // anyway.
    let checker = |x: usize, y: usize| {
        let i = (y * 320 + x) * 4;
        let solid = live[i].abs_diff(60) < 30 && live[i + 2].abs_diff(200) < 30;
        live[i + 3] == 255 && baked[i + 3] == 255 && !solid
    };
    let (mut sum, mut off, mut pixels) = (0u64, 0, 0);
    for y in 3..237 {
        for x in 3..317 {
            let i = (y * 320 + x) * 4;
            if ![(0, 0), (3, 0), (0, 3)]
                .iter()
                .all(|&(dx, dy)| checker(x - dx, y - dy) && checker(x + dx, y + dy))
            {
                continue;
            }
            let worst = (0..3)
                .map(|c| baked[i + c].abs_diff(live[i + c]))
                .max()
                .unwrap();
            sum += u64::from(worst);
            off += usize::from(worst > 24);
            pixels += 1;
        }
    }
    assert!(pixels > 15_000, "{pixels} checker pixels");
    let mean = sum as f64 / pixels as f64;
    assert!(mean < 2., "mean difference {mean}");
    assert!(
        off * 200 < pixels,
        "{off} of {pixels} pixels differ by more than 24"
    );
}

#[test]
fn painting_rebakes_only_the_ground_pages_it_touches() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let (scene, paint, half) = paged_ground();
    // Close over the west half, where 8 m pages meet.
    let world = Extent {
        center: [-16., 0., -16.],
        size: half * 2.,
    };
    let camera = Camera {
        zoom: Camera::default().zoom * 8.,
        ..Camera::default()
    };
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    let before = renderer.drawn().pages;
    assert!(
        before.baked == before.wanted && before.wanted > 2,
        "{before:?}"
    );
    // Nothing changed: nothing rebakes.
    target.frame(&renderer).unwrap();
    assert_eq!(renderer.drawn().pages.new, 0);
    // Paint a metre at (-12, -12) solid, inside one page.
    let rect = Rect {
        x0: 78,
        z0: 78,
        x1: 82,
        z1: 82,
    };
    paint.write(rect, &[255, 0, 0, 0].repeat(16));
    let pixels = target.frame(&renderer).unwrap();
    let after = renderer.drawn().pages;
    assert!(after.new > 0 && after.new < after.wanted, "{after:?}");
    assert_eq!(after.baked, after.wanted);
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, target.bounds(), world));
    // The painted corner shows the solid blue from its rebaked page; the
    // rest of the west half keeps its checker.
    near(at([-12., 0., -12.]), [60, 60, 200], 3);
    assert_ne!(at([-20., 0., -20.]), [60, 60, 200]);
}

// A splat draws each layer where its control channel weighs: red on the
// quad's left half, green on its right, as a placement. The
// mesh's mid-grey albedo tints the blend by its red doubled, which leaves
// it as it is; drawn without the splat the quad would be that grey.
#[test]
fn splat_layers_draw_where_their_control_channels_weigh() {
    let control: Vec<u8> = (0..8)
        .flat_map(|x| {
            if x < 4 {
                [255, 0, 0, 0]
            } else {
                [0, 255, 0, 0]
            }
        })
        .collect();
    let layer = |rgba: [u8; 4]| MeshSplatLayer {
        albedo: Texture::solid(rgba).into(),
        normal: None,
        tiling: 3.,
    };
    let mut data = quad(1., 0., Material::Lit);
    data.texture = Some(Texture::solid([128, 128, 128, 255]).into());
    data.ignore_alpha = true;
    data.splat = Some(Arc::new(MeshSplat {
        control: Texture::from_rgba(8, 1, control).unwrap().into(),
        layers: [
            Some(layer([200, 40, 40, 255])),
            Some(layer([40, 40, 200, 255])),
            None,
            None,
        ],
        sharpness: 8.,
        alpha_floor: 0.1,
        fallback: [0.3; 3],
        tint: Some(2.),
        mix_normals: false,
        scale_invariant: true,
        world_tiling: false,
    }));
    let mesh = Arc::new(Mesh::new(data).unwrap());
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(vec![Piece {
        levels: vec![(mesh.clone(), 0.)],
        impostor: None,
        pose: None,
    }]);
    renderer.set_placements(&[0], &[math::IDENTITY]);
    let mut target = Offscreen::new(320, 240).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 4.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 2.,
        tilt: 0.3,
        ..Camera::perspective()
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let bounds = target.bounds();
    let at = |p: Vec3| target.pixel(&pixels, camera.project(p, bounds, world));
    near(at([-0.7, 0., 0.]), [201, 40, 40], 3);
    near(at([0.7, 0., 0.]), [40, 40, 201], 3);
}

// A world-tiled splat's layer runs on in world X/Z, not on each mesh's
// UVs, and its normals lean in the world, not with the mesh: two quads
// set three metres apart and turned differently show the layer's two
// texels where the world says, and its leaning normal lights the same way
// on both.
#[test]
fn world_tiled_splat_layers_follow_the_world() {
    // Red then blue along U, two metres a repeat (tiling 0.5): texel
    // centres at world x = 0.5 (red) and 1.5 (blue), and so on every 2 m.
    let albedo = Texture::from_rgba(2, 1, vec![200, 40, 40, 255, 40, 40, 200, 255]).unwrap();
    // Leaning toward +U: in the world, +X.
    let lean = Texture::from_rgba(1, 1, vec![218, 128, 0, 0]).unwrap();
    let mut data = quad(1., 0., Material::Lit);
    data.texture = Some(Texture::solid([128, 128, 128, 255]).into());
    data.normal_map = Some(Texture::solid([128, 128, 255, 0]).into());
    data.ignore_alpha = true;
    data.splat = Some(Arc::new(MeshSplat {
        control: Texture::solid([255, 0, 0, 0]).into(),
        layers: [
            Some(MeshSplatLayer {
                albedo: albedo.into(),
                normal: Some(lean.into()),
                tiling: 0.5,
            }),
            None,
            None,
            None,
        ],
        sharpness: 1.,
        alpha_floor: 0.1,
        fallback: [0.3; 3],
        tint: None,
        mix_normals: false,
        scale_invariant: false,
        world_tiling: true,
    }));
    let mesh = Arc::new(Mesh::new(data).unwrap());
    // A quarter turn about Y: X goes to -Z, Z to X.
    let turned = |at: Vec3| {
        let mut m = math::translate(at);
        m[..12].copy_from_slice(&[0., 0., -1., 0., 0., 1., 0., 0., 1., 0., 0., 0.]);
        m
    };
    let world = Extent {
        center: [0.5, 0., 0.],
        size: 6.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.5,
        tilt: 0.,
        ..Camera::perspective()
    };
    let draw = |light: Light| {
        let renderer = Renderer::default();
        renderer.set_scene(&Scene {
            light,
            ..Default::default()
        });
        renderer.set_pieces(vec![Piece {
            levels: vec![(mesh.clone(), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(
            &[0, 0],
            &[math::translate([-1., 0., 0.]), turned([2., 0., 0.])],
        );
        let mut target = Offscreen::new(480, 240).unwrap();
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        let bounds = target.bounds();
        [-1.5, -0.5, 1.5, 2.5]
            .map(|x| target.pixel(&pixels, camera.project([x, 0., 0.], bounds, world)))
    };
    let [a, b, c, d] = draw(neutral());
    near(a, [200, 40, 40], 4);
    near(b, [40, 40, 200], 4);
    near(c, [40, 40, 200], 4);
    near(d, [200, 40, 40], 4);
    // Lit from the side, the lean shows on both quads alike, turned or
    // not; mesh-tangent normals would lean the turned one along Z.
    let side = |x: f32| Light {
        sun: [x, 1., 0.],
        sun_color: [1.; 3],
        sky: [0.1; 3],
        ground: [0.1; 3],
        ..Default::default()
    };
    // How bright each quad's first point is (red on one, blue on the
    // other), under a sun from +X and one from -X.
    let lit = |l: [[u8; 3]; 4]| [l[0][0].max(l[0][2]), l[2][0].max(l[2][2])];
    let (e, w) = (lit(draw(side(1.))), lit(draw(side(-1.))));
    assert!(
        e[0] != w[0] && e[1] != w[1],
        "no lean, east {e:?}, west {w:?}"
    );
    assert_eq!(
        e[0] > w[0],
        e[1] > w[1],
        "the turned quad leans apart, east {e:?}, west {w:?}"
    );
}

// --- Kept opaque layers -------------------------------------------------------

/// `HELD_CLOCK` is the process's: tests that hold it take turns.
static CLOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A shadowed, occluded, cloud-shaded scene for the kept-layer tests: lit
/// ground and a cube on it, two placed cubes, sun shadows, ambient
/// occlusion, and cloud shadows drifting at `drift` map units a second.
fn still_scene(drift: f32, sun: Vec3) -> Scene {
    let map = Arc::new(ControlMap::new(2, 1, vec![0, 0, 0, 255, 0, 0, 0, 0]).unwrap());
    let layer = CloudLayer {
        scale: 1. / 7.,
        offset: [0.1, 0.],
        velocity: [drift, 0.],
    };
    Scene {
        batches: vec![
            single(quad(10., 0., Material::Lit), math::IDENTITY),
            single(
                cube(0xd0a040, Material::Lit).data().clone(),
                math::translate([0., 0.5, 0.]),
            ),
        ],
        light: Light {
            sun,
            sun_color: [0.6; 3],
            sky: [0.3; 3],
            ground: [0.3; 3],
            shadows: Some(Shadows::default()),
            fog: None,
        },
        occlusion: Some(Occlusion::default()),
        cloud_shadows: Some(CloudShadows {
            map,
            layers: [layer; 2],
            contrast: 1.,
            power: 1.,
        }),
        ..Default::default()
    }
}

struct Still {
    renderer: Renderer,
    target: Offscreen,
    camera: Camera,
    world: Extent,
    scene: Scene,
}

impl Still {
    fn new(drift: f32) -> Self {
        let renderer = Renderer::default();
        let scene = still_scene(drift, [0.4, 1., 0.3]);
        renderer.set_scene(&scene);
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(cube(0x40c060, Material::Lit)), 0.)],
            impostor: None,
            pose: None,
        }]);
        let rows = [
            math::translate([3., 0.5, 1.]),
            math::translate([-3., 0.5, -2.]),
        ];
        renderer.set_placements(&[0, 0], &rows);
        let mut target = Offscreen::new(240, 180).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 16.,
        };
        let camera = Camera {
            fov: 0.9,
            zoom: 1.5,
            ..Camera::perspective()
        };
        target.aim(&renderer, camera, world);
        target.settle(&renderer).unwrap();
        Self {
            renderer,
            target,
            camera,
            world,
            scene,
        }
    }

    fn frame(&mut self) -> (Vec<u8>, Reuse) {
        let pixels = self.target.frame(&self.renderer).unwrap();
        (pixels, self.renderer.drawn().reuse)
    }

    /// Frames until one keeps the whole of its layers; at most four.
    fn rest(&mut self) -> Vec<u8> {
        for _ in 0..4 {
            let (pixels, reuse) = self.frame();
            if reuse == Reuse::Whole {
                return pixels;
            }
        }
        panic!("the view never kept its layers");
    }

    /// The same scene again, as a new one: everything draws.
    fn redrawn(&mut self) -> Vec<u8> {
        self.renderer.set_scene(&self.scene);
        let (pixels, reuse) = self.frame();
        assert_eq!(reuse, Reuse::Nothing);
        pixels
    }
}

// A view at rest keeps its opaque layers after two frames under one key,
// and what it then draws is what a full redraw draws, byte for byte; picks
// still read the kept handles.
#[test]
fn a_still_view_keeps_its_layers_and_draws_what_a_full_frame_does() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut still = Still::new(0.);
    let kept = still.rest();
    assert_eq!(still.frame().1, Reuse::Whole);
    let full = still.redrawn();
    assert!(kept == full, "kept layers drew a different image");
    still.rest();

    let at = |p: Vec3, s: &Still| s.camera.project(p, s.target.bounds(), s.world);
    let center = at([0., 0.9, 0.], &still);
    let hit = still.target.pick(&still.renderer, center).unwrap().unwrap();
    assert_eq!(still.renderer.drawn().reuse, Reuse::Whole);
    assert_eq!(hit.id, Some(1));
    assert!((hit.position[1] - 1.).abs() < 0.05, "{:?}", hit.position);
    let placed = at([3., 1., 1.], &still);
    let hit = still.target.pick(&still.renderer, placed).unwrap().unwrap();
    assert_eq!((hit.placement, hit.id), (Some(0), None));
}

// A cloud shade drawn again where it lies (its square unmoved) shades the
// layers again: its version is part of the key.
#[test]
fn a_cloud_shade_redrawn_in_place_shades_the_layers_again() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut still = Still::new(0.);
    let shade = Arc::new(Shade(Default::default()));
    still.scene.passes = vec![shade.clone()];
    still.renderer.set_scene(&still.scene);
    still.rest();
    if let Some(s) = shade.0.lock().unwrap().as_mut() {
        s.version += 1;
    }
    assert_ne!(still.frame().1, Reuse::Whole, "shade redrawn");
    still.rest();
}

// The camera, the render scale, an edit, the sun and a new scene each draw
// the layers again, and the view keeps them anew once it rests.
#[test]
fn camera_edits_sun_and_scene_draw_the_layers_again() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut still = Still::new(0.);
    let rested = still.rest();

    let mut turned = still.camera;
    turned.yaw += 0.01;
    still.target.aim(&still.renderer, turned, still.world);
    let (pixels, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Nothing, "camera");
    assert!(pixels != rested);
    still.target.aim(&still.renderer, still.camera, still.world);
    assert_eq!(still.frame().1, Reuse::Nothing, "camera back");
    assert!(still.rest() == rested);

    still.renderer.set_render_scale(0.5);
    assert_eq!(still.frame().1, Reuse::Nothing, "render scale");
    still.rest();
    still.renderer.set_render_scale(1.);
    assert_eq!(still.frame().1, Reuse::Nothing, "render scale back");
    still.rest();

    // A placement moved: the cube leaves its pixel.
    let at = still
        .camera
        .project([3., 1., 1.], still.target.bounds(), still.world);
    let pixels = still.rest();
    let before = still.target.pixel(&pixels, at);
    still
        .renderer
        .move_placements(0, &[math::translate([3., 0.5, 4.])]);
    let (pixels, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Nothing, "edit");
    assert_ne!(still.target.pixel(&pixels, at), before);
    still.rest();

    still.scene = still_scene(0., [-0.6, 0.8, 0.1]);
    still.renderer.set_scene(&still.scene);
    let (pixels, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Nothing, "sun");
    assert!(pixels != rested);
    // The cascades follow the sun over a few frames.
    let rested = still.rest();
    assert!(still.redrawn() == rested);
}

// Cloud shadows drifting over a still view change only its lights: the
// frame keeps depth, handles and culling and shades again, and draws what
// a full frame at the same clock draws.
#[test]
fn drifting_cloud_shadows_shade_kept_geometry_as_a_full_frame_does() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let hold = |t: f32| HELD_CLOCK.store(t.to_bits(), std::sync::atomic::Ordering::Relaxed);
    // The visibility buffer resolves over kept geometry with nothing drawn.
    hold(1.);
    let mut still = Still::new(0.5);
    let rested = still.rest();
    hold(2.);
    let (drifted, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Geometry);
    assert!(drifted != rested, "the clouds moved");
    let full = still.redrawn();
    assert!(drifted == full, "kept geometry shaded a different image");
    hold(f32::NAN);
}

// --- Detail ------------------------------------------------------------------

// The default detail is the renderer as it was: setting it draws the same
// image, byte for byte.
#[test]
fn default_detail_draws_what_no_detail_does() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    HELD_CLOCK.store(1f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    let mut still = Still::new(0.5);
    let before = still.redrawn();
    still.renderer.set_detail(Detail::default());
    assert!(still.redrawn() == before);
    HELD_CLOCK.store(f32::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

// Stepped cloud shadows hold still within a step, so a still view keeps
// its layers whole there, and move on at the next; off, they shade
// nothing, the view keeps everything and stops asking for frames.
#[test]
fn stepped_cloud_shadows_hold_between_steps_and_off_shades_nothing() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let hold = |t: f32| HELD_CLOCK.store(t.to_bits(), std::sync::atomic::Ordering::Relaxed);
    hold(1.);
    let mut still = Still::new(0.5);
    let smooth = still.rest();
    hold(1.1);
    assert_eq!(still.frame().1, Reuse::Geometry, "smooth drifts each frame");
    assert!(still.renderer.wants_frames());

    still.renderer.set_detail(Detail {
        cloud_shadows: CloudMotion::Stepped,
        ..Default::default()
    });
    hold(1.);
    let stepped = still.rest();
    assert!(stepped == smooth, "a whole step draws as smooth does");
    hold(1.2);
    let (pixels, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Whole, "within the step");
    assert!(pixels == stepped);
    hold(1.3);
    let (pixels, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Geometry, "the next step");
    assert!(pixels != stepped);

    still.renderer.set_detail(Detail {
        cloud_shadows: CloudMotion::Off,
        ..Default::default()
    });
    let off = still.rest();
    assert!(off != smooth, "the clouds' shade is gone");
    hold(2.);
    assert_eq!(still.frame().1, Reuse::Whole, "nothing drifts");
    assert!(!still.renderer.wants_frames());
    // As a scene without cloud shadows, and a pass's shade is not used.
    let shade = Arc::new(Shade(Default::default()));
    still.scene.cloud_shadows = None;
    still.scene.passes = vec![shade];
    assert!(still.redrawn() == off);
    hold(f32::NAN);
}

// A placement spanning fewer pixels than `min_pixels` is not drawn nor
// picked; a larger one still is, and the threshold redraws the culling.
#[test]
fn min_pixels_drops_small_placements_and_keeps_large_ones() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    HELD_CLOCK.store(1f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    let mut still = Still::new(0.);
    let scaled = |p: Vec3, s: f32| {
        let mut m = math::translate(p);
        (m[0], m[5], m[10]) = (s, s, s);
        m
    };
    // A 2 m cube and a 0.2 m one, both on the ground in view.
    let (big, small) = (([3., 1., 1.], 2.), ([-3., 0.1, -2.], 0.2));
    still
        .renderer
        .set_placements(&[0, 0], &[scaled(big.0, big.1), scaled(small.0, small.1)]);
    let all = still.rest();
    let at = |p: Vec3, s: &Still| s.camera.project(p, s.target.bounds(), s.world);
    let (big_at, small_at) = (at(big.0, &still), at(small.0, &still));
    let small_hit = still.target.pick(&still.renderer, small_at).unwrap();
    assert_eq!(small_hit.and_then(|h| h.placement), Some(1));

    still.renderer.set_detail(Detail {
        min_pixels: 12.,
        ..Default::default()
    });
    let (culled, reuse) = still.frame();
    assert_eq!(reuse, Reuse::Nothing, "the threshold is in the key");
    assert_ne!(
        still.target.pixel(&culled, small_at),
        still.target.pixel(&all, small_at)
    );
    assert_eq!(
        still.target.pixel(&culled, big_at),
        still.target.pixel(&all, big_at)
    );
    still.rest();
    let hit = still.target.pick(&still.renderer, small_at).unwrap();
    assert_ne!(hit.and_then(|h| h.placement), Some(1));
    let hit = still.target.pick(&still.renderer, big_at).unwrap();
    assert_eq!(hit.and_then(|h| h.placement), Some(0));
    HELD_CLOCK.store(f32::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

// Cascades of any size settle: a still view stops drawing them again and
// keeps its layers whole.
#[test]
fn cascades_of_any_size_let_a_still_view_rest() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    HELD_CLOCK.store(1f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    for (resolution, cascades, cascade_resolution, reach) in [
        (2048, 3, 1024, 300.),
        (2048, 2, 1024, 200.),
        (4096, 3, 2048, 500.),
        (2048, 3, 1024, 20.),
    ] {
        let mut still = Still::new(0.);
        still.scene.light.shadows = Some(Shadows {
            resolution,
            softness: 1.5,
            cascades,
            cascade_resolution,
            reach,
        });
        still.renderer.set_scene(&still.scene);
        still.rest();
    }
    HELD_CLOCK.store(f32::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

// --- Lit placements ----------------------------------------------------------

/// How far two RGBA frames differ: pixels off by more than 2 levels in any
/// channel, and the largest difference.
fn difference(a: &[u8], b: &[u8]) -> (usize, u8) {
    let mut off = 0;
    let mut most = 0;
    for (p, q) in a.chunks(4).zip(b.chunks(4)) {
        let d = (0..3).map(|c| p[c].abs_diff(q[c])).max().unwrap();
        off += usize::from(d > 2);
        most = most.max(d);
    }
    (off, most)
}

/// With VISBUF_DUMP=dir, writes `pixels` there as `<name>.ppm`, for looking
/// at what a comparison compared.
fn dump(name: &str, pixels: &[u8], width: u32, height: u32) {
    let Ok(dir) = std::env::var("VISBUF_DUMP") else {
        return;
    };
    let mut out = format!("P6 {width} {height} 255\n").into_bytes();
    out.extend(pixels.chunks(4).flat_map(|p| [p[0], p[1], p[2]]));
    std::fs::write(std::path::Path::new(&dir).join(format!("{name}.ppm")), out).unwrap();
}

/// A sloped terrain with lit placements on it: a mip-coloured textured sheet seen at a slant (texture LOD), cutout
/// cards, a thinning bush half thinned, and a sheet bent onto the slope.
/// Returns the renderer, camera, world and each placement's centre.
fn paths_scene() -> (Renderer, Camera, Extent, Vec<Vec3>) {
    let mut field = flat_field(33);
    let map = Arc::new(HeightMap::new(&field).unwrap());
    let ramp = Rect {
        x0: 20,
        z0: 0,
        x1: 33,
        z1: 33,
    };
    raise(&mut field, &map, ramp, 1.5);
    let terrain = Arc::new(Terrain {
        heights: map,
        colors: Arc::new(ControlMap::new(33, 33, [90, 80, 60, 255].repeat(33 * 33)).unwrap()),
        color_gain: 1.,
        tints: false,
        material: Material::Lit,
        texture: None,
    });
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        terrain: Some(terrain),
        light: Light {
            sun: [0.4, 1., 0.3],
            sun_color: [0.7; 3],
            sky: [0.35; 3],
            ground: [0.25; 3],
            shadows: Some(Shadows::default()),
            fog: None,
        },
        ..Default::default()
    });
    // Each mip its own colour: a footprint picked differently shows.
    let mips = || {
        let colors = [
            [220, 40, 40],
            [40, 200, 40],
            [40, 60, 220],
            [220, 200, 40],
            [200, 40, 200],
            [40, 200, 200],
            [250, 250, 250],
        ];
        compressed(Block::Bc1, 64, 64, 7, move |l| bc1(colors[l as usize]))
    };
    let mut floor = sheet(3., 0.05, 12, 0xffffff);
    floor.material = Material::Lit;
    floor.normals = vec![[0., 1., 0.]; floor.positions.len()];
    floor.uvs = floor
        .positions
        .iter()
        .map(|p| [p[0] * 2., p[2] * 2.])
        .collect();
    floor.texture = Some(mips());
    // Cards: upright quads whose texture is solid on its right half.
    let mut card = MeshData {
        positions: vec![
            [-0.6, 0., 0.],
            [0.6, 0., 0.],
            [0.6, 1.2, 0.],
            [-0.6, 1.2, 0.],
        ],
        uvs: vec![[0., 1.], [1., 1.], [1., 0.], [0., 0.]],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: Material::Lit,
        ..Default::default()
    };
    card.texture = Some(Image::from(
        Texture::from_rgba(2, 1, vec![0, 0, 0, 0, 40, 200, 60, 255]).unwrap(),
    ));
    // A bush of eight ranked leaves, thinned to about half at this size.
    let mut bush = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    let mut parts = Vec::new();
    for k in 0..8 {
        let a = k as f32 * 0.785;
        let (x, z) = (a.cos() * 0.5, a.sin() * 0.5);
        let base = bush.positions.len() as u32;
        bush.positions.extend(
            [
                [-0.5, 0., 0.],
                [0.5, 0., 0.],
                [0.5, 1., 0.3],
                [-0.5, 1., 0.3],
            ]
            .map(|p: Vec3| [p[0] + x, p[1] + 0.2, p[2] + z]),
        );
        bush.indices.extend([0, 1, 2, 0, 2, 3].map(|i| base + i));
        bush.colors.extend([0xff30a050; 4]);
        parts.extend([[x, 0.5, z, k as f32 / 8.]; 4]);
    }
    bush.thinning = Some(Thinning {
        parts,
        full: 60.,
        least: 0.2,
    });
    let mut bent = sheet(1.5, 0.3, 16, 0xffd0a060);
    bent.material = Material::Lit;
    bent.terrain_adapt = Some(TerrainAdapt::Relative);
    let mesh = |m: MeshData| (Arc::new(Mesh::new(m).unwrap()), 0.);
    renderer.set_pieces(vec![
        Piece {
            levels: vec![mesh(floor)],
            impostor: None,
            pose: None,
        },
        Piece {
            levels: vec![mesh(card)],
            impostor: None,
            pose: None,
        },
        Piece {
            levels: vec![mesh(bush)],
            impostor: None,
            pose: None,
        },
        Piece {
            levels: vec![mesh(bent)],
            impostor: None,
            pose: None,
        },
    ]);
    let origins: [Vec3; 3] = [[-3., 0., 2.], [-1., 0., -1.], [1., 0., 0.5]];
    let mut rows: Vec<Mat4> = origins.iter().map(|&c| math::translate(c)).collect();
    // Turned 0.6 about y.
    let (sin, cos) = 0.6f32.sin_cos();
    (rows[1][0], rows[1][2], rows[1][8], rows[1][10]) = (cos, -sin, sin, cos);
    // Where each shows: the card on its solid half.
    let centres = vec![
        [-3., 0.05, 2.],
        [-1. + 0.3 * cos, 0.6, -1. - 0.3 * sin],
        [1., 0.5, 0.5],
        [1., 0.3, -1.],
    ];
    // Across the ramp's foot: half on the low ground, half bent up it.
    rows.push(adapting(math::translate([2., 0., -1.]), true, false, 0.));
    renderer.set_placements(&[0, 1, 2, 3], &rows);
    let world = Extent {
        center: [0.; 3],
        size: 14.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.5,
        tilt: 0.85,
        yaw: 0.4,
        ..Camera::perspective()
    };
    (renderer, camera, world, centres)
}

// Lit placements, textured, cut out, thinned and bent onto the terrain,
// pick the placement drawn at each one's centre.
#[test]
fn lit_placements_pick_what_they_draw() {
    let (renderer, camera, world, centres) = paths_scene();
    let mut target = Offscreen::new(320, 240).unwrap();
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    let picks: Vec<Option<u32>> = (centres.iter())
        .map(|&c| {
            let at = camera.project(c, target.bounds(), world);
            target
                .pick(&renderer, at)
                .unwrap()
                .and_then(|h| h.placement)
        })
        .collect();
    assert_eq!(picks, [Some(0), Some(1), Some(2), Some(3)]);
}

// The first frame after the camera turns onto placements the last frame
// did not see: the occlusion cull's first phase draws none of them, so the
// second phase must reveal all (the visibility buffer draws them into the
// buffer and resolves both phases once). That frame shows what the settled
// one does.
#[test]
fn placements_revealed_by_the_second_phase_draw_in_the_frame_they_appear() {
    let (renderer, camera, world, _) = paths_scene();
    let mut target = Offscreen::new(320, 240).unwrap();
    // Looking at ground far off: the cull sees none of the placements.
    let away = Camera {
        center: Some([60., 0., 60.]),
        ..camera
    };
    target.aim(&renderer, away, world);
    target.settle(&renderer).unwrap();
    target.aim(&renderer, camera, world);
    let first = target.frame(&renderer).unwrap();
    let settled = target.settle(&renderer).unwrap();
    dump("revealed_first", &first, 320, 240);
    dump("revealed_settled", &settled, 320, 240);
    let (off, most) = difference(&settled, &first);
    eprintln!("{off} pixels off by more than 2, at most {most}");
    assert!(
        off <= 320 * 240 / 200,
        "the first frame differs from the settled one on {off} pixels, by up to {most}"
    );
}

// Triangles a pixel or smaller, many to a vertex as in real meshes, each its
// own colour: the resolve finds the triangle the rasteriser drew, so pixels
// match the same mesh drawn as a plain batch.
#[test]
fn the_visibility_buffer_finds_sub_pixel_triangles() {
    // A 160x160 grid, two triangles a cell, flat-coloured per triangle by
    // giving each triangle its own provoking colour through unshared
    // corners would hide the sharing; instead colour vertices in a hash.
    let n = 160u32;
    let mut data = sheet(4., 0., n, 0);
    data.material = Material::Lit;
    data.normals = vec![[0., 1., 0.]; data.positions.len()];
    let mut h = 0x9e37_79b9u32;
    data.colors = (0..data.positions.len())
        .map(|_| {
            h ^= h << 13;
            h ^= h >> 17;
            h ^= h << 5;
            0xff00_0000 | (h & 0x00ff_ffff)
        })
        .collect();
    let mesh = Arc::new(Mesh::new(data).unwrap());
    let mut images = Vec::new();
    for placed in [false, true] {
        let renderer = Renderer::default();
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
                levels: vec![(mesh.clone(), 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&[0], &[math::IDENTITY]);
        }
        let mut target = Offscreen::new(240, 180).unwrap();
        let world = Extent {
            center: [0.; 3],
            size: 8.,
        };
        let camera = Camera {
            fov: 0.9,
            zoom: 3.,
            tilt: 0.9,
            ..Camera::perspective()
        };
        target.aim(&renderer, camera, world);
        let pixels = target.settle(&renderer).unwrap();
        dump(&format!("tiny_placed_{placed}"), &pixels, 240, 180);
        images.push(pixels);
    }
    let (off, most) = difference(&images[0], &images[1]);
    eprintln!("tiny triangles: {off} pixels off by more than 2, at most {most}");
    assert!(
        off <= 240 * 180 / 500 && most <= 16,
        "{off} pixels differ, by up to {most}"
    );
}

/// Renders lit `pieces` at `rows` under neutral light, with the camera
/// tilted over them.
fn tiny_render(pieces: &[Arc<Mesh>], rows: &[(u32, Mat4)], camera: Camera) -> Vec<u8> {
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        light: neutral(),
        ..Default::default()
    });
    renderer.set_pieces(
        (pieces.iter())
            .map(|m| Piece {
                levels: vec![(m.clone(), 0.)],
                impostor: None,
                pose: None,
            })
            .collect(),
    );
    let (piece_of, transforms): (Vec<u32>, Vec<Mat4>) = rows.iter().copied().unzip();
    renderer.set_placements(&piece_of, &transforms);
    let mut target = Offscreen::new(240, 180).unwrap();
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap()
}

/// A `n`x`n` sheet `half` each way, every triangle its own colour: no two
/// share a vertex, so whichever triangle a pixel shows names itself.
fn confetti(n: u32, half: f32) -> MeshData {
    let mut data = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    let mut h = 0x2545_f491u32;
    let mut next = || {
        h ^= h << 13;
        h ^= h >> 17;
        h ^= h << 5;
        0xff00_0000 | (h & 0x00ff_ffff) | 0x40_4040
    };
    let at = |k: u32| -half + 2. * half * k as f32 / n as f32;
    for z in 0..n {
        for x in 0..n {
            let c = [
                [at(x), at(z)],
                [at(x + 1), at(z)],
                [at(x), at(z + 1)],
                [at(x + 1), at(z + 1)],
            ];
            for tri in [[0, 2, 1], [1, 2, 3]] {
                let color = next();
                for k in tri {
                    data.positions.push([c[k][0], 0., c[k][1]]);
                    data.colors.push(color);
                    data.normals.push([0., 1., 0.]);
                }
                let base = data.positions.len() as u32 - 3;
                data.indices.extend([base, base + 1, base + 2]);
            }
        }
    }
    data
}

fn tilted() -> Camera {
    Camera {
        fov: 0.9,
        zoom: 3.,
        tilt: 0.9,
        ..Camera::perspective()
    }
}

// The exposure settled from a rect of the HDR target is the one a count of
// every second texel each way gives: the rect's odd size and origin leave
// partial histogram cells at its edges, and texels nothing drew (alpha 0)
// are not counted.
#[test]
fn auto_exposure_counts_every_second_texel_of_the_rect() {
    let off = Offscreen::new(8, 8).unwrap();
    let gpu = &off.gpu;
    let size = [300u32, 140];
    let rect = [7u32, 3, 283, 131];
    let auto = AutoExposure {
        log_min: -8.,
        log_max: 8.,
        low: 0.1,
        count: 0.8,
        target: 0.18,
        min: 1e-4,
        max: 1e4,
        tolerance: 0.,
        speed_up: 1.,
        speed_down: 1.,
    };
    // Grey at a bin's centre, so the GPU's log2 cannot round it into the
    // next; bins spread over the texture, a few texels uncovered.
    let delta = (auto.log_max - auto.log_min) / 64.;
    let texel = |x: u32, y: u32| {
        let h = (x * 7 + y * 13 + (x * y) % 11) % 64;
        let l = (auto.log_min + (h as f32 + 0.5) * delta).exp2();
        let a = if (x + 3 * y) % 17 == 0 { 0. } else { 1. };
        ([l, l, l, a], h)
    };
    let texels: Vec<u16> = (0..size[1])
        .flat_map(|y| (0..size[0]).map(move |x| (x, y)))
        .flat_map(|(x, y)| texel(x, y).0.map(half))
        .collect();
    let (hdr, view) = texture_2d(
        gpu,
        "test hdr",
        size,
        HDR_FORMAT,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    gpu.queue.write_texture(
        hdr.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size[0] * 8),
            rows_per_image: None,
        },
        hdr.size(),
    );
    // csCalculateExposure over the same samples.
    let mut bins = [0u32; 64];
    for y in (0..rect[3]).step_by(2) {
        for x in (0..rect[2]).step_by(2) {
            let (c, h) = texel(rect[0] + x, rect[1] + y);
            if c[3] > 0. {
                bins[h as usize] += 1;
            }
        }
    }
    let total: u32 = bins.iter().sum();
    let (mut skip, mut keep) = (
        (total as f32 * auto.low).round() as u32,
        (total as f32 * auto.count).round() as u32,
    );
    let (mut sum, mut used) = (0f64, 0u32);
    for (i, &n) in bins.iter().enumerate() {
        let dropped = n.min(skip);
        skip -= dropped;
        let n = (n - dropped).min(keep);
        keep -= n;
        used += n;
        sum += f64::from(auto.log_min + (i as f32 + 0.5) * delta) * f64::from(n);
    }
    let expected = auto.target / ((sum / f64::from(used)) as f32).exp2();

    let pipelines = exposure::Pipelines::new(&gpu.device);
    let mut exposure = exposure::Exposure::new(gpu);
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    exposure.record(
        gpu,
        &pipelines,
        &mut encoder,
        &view,
        rect,
        Some(&auto),
        None,
    );
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(&exposure.value, 0, &readback, 0, 16);
    gpu.queue.submit([encoder.finish()]);
    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .unwrap();
    let got = bytemuck::cast_slice::<u8, f32>(&slice.get_mapped_range().unwrap())[0];
    assert!(
        (got / expected - 1.).abs() < 1e-4,
        "exposure {got}, counted {expected}"
    );
}

// Ground pages and impostor views are kept block compressed (bc.rs): the
// encoder's blocks, decoded as a sampler would, stay within BC3's and
// BC5's usual error of what was baked, and flat blocks come back exact.
#[test]
fn block_compression_keeps_baked_images_close() {
    let off = Offscreen::new(8, 8).unwrap();
    let gpu = &off.gpu;
    let side = 64u32;
    // Smooth color with grain, as ground is, a flat corner, and alpha as
    // independent as gloss.
    let texel = |x: u32, y: u32| -> [u8; 4] {
        if x < 8 && y < 8 {
            return [90, 140, 60, 200];
        }
        let grain = ((x * 37 + y * 91 + x * y * 7) % 23) as f32 - 11.;
        let r = 60. + 2. * x as f32 + grain;
        let g = 80. + 1.5 * y as f32 + 0.5 * grain;
        let b = 40. + (x + y) as f32 * 0.8;
        let a = 128. + 100. * ((x as f32 * 0.3).sin() * (y as f32 * 0.2).cos());
        [r, g, b, a].map(|v| v.clamp(0., 255.) as u8)
    };
    let texels: Vec<u8> = (0..side)
        .flat_map(|y| (0..side).flat_map(move |x| texel(x, y)))
        .collect();
    let (texture, _) = texture_2d(
        gpu,
        "test bc source",
        [side; 2],
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    gpu.queue.write_texture(
        texture.as_image_copy(),
        &texels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(side * 4),
            rows_per_image: None,
        },
        texture.size(),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let job = |kind| bc::Job {
        origin: [0, 0],
        layer: 0,
        mip: 0,
        blocks: [side / 4; 2],
        kind,
    };
    let jobs = [job(bc::Kind::Rgba), job(bc::Kind::Rg)];
    let bytes: u64 = jobs.iter().map(bc::Job::bytes).sum();
    let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC;
    let out = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage,
        mapped_at_creation: false,
    });
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let encoder_bc = bc::Encoder::new(&gpu.device);
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    let offsets = encoder_bc.encode(&gpu.device, &mut encoder, &view, &jobs, &out, 0, None);
    encoder.copy_buffer_to_buffer(&out, 0, &readback, 0, bytes);
    gpu.queue.submit([encoder.finish()]);
    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .unwrap();
    let data = slice.get_mapped_range().unwrap().to_vec();
    for ((job, &offset), (block, channels)) in jobs.iter().zip(&offsets).zip([
        (crate::texture::Block::Bc3, [0, 1, 2, 3]),
        (crate::texture::Block::Bc5, [0, 1, 4, 4]),
    ]) {
        // Rows are 256 bytes, as packed as the decoder wants them.
        assert_eq!(job.pitch(), side / 4 * 16);
        let bytes = &data[offset as usize..(offset + job.bytes()) as usize];
        let decoded = crate::texture::Texture::decode(block, side, side, bytes).unwrap();
        let (mut sum, mut worst, mut n) = (0f64, 0u8, 0usize);
        for (i, (got, want)) in decoded.rgba.chunks(4).zip(texels.chunks(4)).enumerate() {
            let (x, y) = (i as u32 % side, i as u32 / side);
            for (c, &from) in channels.iter().enumerate() {
                if from == 4 {
                    continue;
                }
                let e = got[c].abs_diff(want[from]);
                if x < 8 && y < 8 {
                    assert!(e <= 1, "{block:?} flat block off by {e}");
                }
                sum += f64::from(e);
                worst = worst.max(e);
                n += 1;
            }
        }
        let mean = sum / n as f64;
        eprintln!("{block:?} from {channels:?}: mean {mean:.2}, worst {worst}");
        assert!(
            mean < 2.5 && worst < 40,
            "{block:?}: mean {mean}, worst {worst}"
        );
    }
}

// A scene with no placements draws and settles, sun shadows
// on: pieces with no rows, no pieces at all, and rows taken away after a
// frame drew some (the editor hiding its props and objects). Bound with
// fewer bytes than one element, the shadow cull's transforms failed wgpu's
// validation; and with no rows no cull answered the views' texture wants,
// so the scene never settled.
#[test]
fn a_scene_without_placements_draws_on_every_tier() {
    let world = Extent {
        center: [0.; 3],
        size: 8.,
    };
    let light = Light {
        sun: [1., 2., 0.],
        sun_color: [0.7; 3],
        sky: [0.3; 3],
        ground: [0.3; 3],
        shadows: Some(Shadows {
            resolution: 256,
            ..Default::default()
        }),
        ..Default::default()
    };
    let camera = Camera::default();
    let mesh = Arc::new(Mesh::new(cube(0xffffff, Material::Lit).data().clone()).unwrap());
    let piece = || Piece {
        levels: vec![(mesh.clone(), 0.)],
        impostor: None,
        pose: None,
    };
    let mut target = Offscreen::new(64, 64).unwrap();
    let renderer = Renderer::default();
    renderer.set_scene(&Scene {
        batches: vec![single(quad(4., 0., Material::Lit), math::IDENTITY)],
        light: light.clone(),
        ..Default::default()
    });
    target.aim(&renderer, camera, world);
    renderer.set_pieces(vec![piece()]);
    renderer.set_placements(&[], &[]);
    let empty = target.settle(&renderer).unwrap();
    renderer.set_placements(&[0], &[math::IDENTITY]);
    let drawn = target.settle(&renderer).unwrap();
    assert_ne!(empty, drawn, "the cube draws");
    renderer.set_placements(&[], &[]);
    let again = target.settle(&renderer).unwrap();
    assert_eq!(empty, again, "rows taken away draw nothing");
    renderer.set_pieces(Vec::new());
    assert_eq!(target.settle(&renderer).unwrap(), empty, "no pieces");
}
