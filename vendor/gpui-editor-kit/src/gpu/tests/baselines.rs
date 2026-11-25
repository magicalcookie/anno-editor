// Image digests of the lit placements scene (`paths_scene`), recorded on
// trunk for the re-architecture tracks to keep equal: what it drew, byte for
// byte, and what the picks named; and a scene of pixel-sized triangles.
//
// Frames depend on the GPU and its driver, so each golden belongs to one
// adapter, and the test skips with a note on any other. To record or
// regenerate this machine's after an intended change:
//
//   KIT_BLESS=1 cargo test -p gpui-editor-kit lit_placements_render_the_recorded_frames -- --nocapture
//
// and paste the printed entry into `FRAMES`, replacing this adapter's.
// VISBUF_DUMP=dir writes each frame there, to see where a digest differs.
use super::*;

/// Per adapter and driver (`adapter()`), the recorded `key = value` lines.
const FRAMES: &[(&str, &str)] = &[(
    "NVIDIA GeForce RTX 4080 SUPER (Vulkan), NVIDIA 620.70.06",
    r#"
paths frame = len 307200 fnv 472b1b9bf50a44c4
paths picks = [Some(0), Some(1), Some(2), Some(3)]
confetti frame = len 172800 fnv eb891091e638fbae
"#,
)];

/// FNV-1a, 64 bit: the same on every Rust version, easy to recompute.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn digest(pixels: &[u8]) -> String {
    format!("len {} fnv {:016x}", pixels.len(), fnv(pixels))
}

/// The offscreen device's adapter, name and driver, as `FRAMES` keys it.
fn adapter(device: &wgpu::Device) -> String {
    let info = device.adapter_info();
    format!(
        "{} ({:?}), {} {}",
        info.name, info.backend, info.driver, info.driver_info
    )
}

// paths_scene's settled frame's digest and the placement each centre
// picks; then confetti's frame. A second draw must match the first, or the
// digest would not reproduce.
#[test]
fn lit_placements_render_the_recorded_frames() {
    // Nothing in the scene runs on the clock today; held, nothing can.
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    HELD_CLOCK.store(1f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    let mut lines: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| lines.push((k.into(), v));
    let (renderer, camera, world, centres) = paths_scene();
    let mut target = Offscreen::new(320, 240).unwrap();
    let device = adapter(target.device());
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let again = target.settle(&renderer).unwrap();
    assert!(pixels == again, "a second settled frame differs");
    let picks: Vec<Option<u32>> = (centres.iter())
        .map(|&c| {
            let at = camera.project(c, target.bounds(), world);
            let hit = target.pick(&renderer, at).unwrap();
            hit.and_then(|h| h.placement)
        })
        .collect();
    put("paths frame", digest(&pixels));
    dump("baseline_paths", &pixels, 320, 240);
    put("paths picks", format!("{picks:?}"));
    // Each triangle about a pixel and its own colour: every pixel names the
    // triangle it shows.
    let mesh = Arc::new(Mesh::new(confetti(50, 1.)).unwrap());
    let rows =
        [[-1., -1.], [1., -1.], [-1., 1.], [1., 1.]].map(|[x, z]| (0, math::translate([x, 0., z])));
    let pixels = tiny_render(std::slice::from_ref(&mesh), &rows, tilted());
    let again = tiny_render(&[mesh], &rows, tilted());
    assert!(pixels == again, "confetti drew other bytes again");
    put("confetti frame", digest(&pixels));
    dump("baseline_confetti", &pixels, 240, 180);
    HELD_CLOCK.store(f32::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed);
    if std::env::var("KIT_BLESS").is_ok_and(|v| v == "1") {
        let mut text = format!("    (\n        {device:?},\n        r#\"\n");
        for (k, v) in &lines {
            text += &format!("{k} = {v}\n");
        }
        eprintln!("{text}\"#,\n    ),");
        return;
    }
    let Some((_, golden)) = FRAMES.iter().find(|(a, _)| *a == device) else {
        eprintln!(
            "skipped: no recorded frames for adapter {device:?} (recorded: {:?}); \
             KIT_BLESS=1 prints them",
            FRAMES.iter().map(|(a, _)| *a).collect::<Vec<_>>()
        );
        return;
    };
    let want: Vec<(&str, &str)> = (golden.lines())
        .filter_map(|l| l.split_once(" = "))
        .collect();
    let mut diffs = Vec::new();
    for (k, v) in &want {
        match lines.iter().find(|(h, _)| h == k) {
            Some((_, h)) if h == v => {}
            h => diffs.push(format!(
                "{k}\n      recorded {v}\n      now      {}",
                h.map_or("(missing)", |(_, h)| h.as_str())
            )),
        }
    }
    for (k, h) in &lines {
        if !want.iter().any(|(w, _)| w == k) {
            diffs.push(format!("{k}\n      recorded (missing)\n      now      {h}"));
        }
    }
    assert!(
        diffs.is_empty(),
        "frames on {device}: {} records differ from the baseline \
         (KIT_BLESS=1 prints fresh ones):\n  {}",
        diffs.len(),
        diffs.join("\n  ")
    );
}

/// Per adapter and driver, `arena_scene`'s records, as `FRAMES`: recorded
/// on `rearch` 4d99179, whose arena draws started their instances and
/// vertices at bases in the indirect arguments.
const ARENA_FRAMES: &[(&str, &str)] = &[(
    "NVIDIA GeForce RTX 4080 SUPER (Vulkan), NVIDIA 620.70.06",
    r#"
arena placed = [18, 24, 0, 0]
arena triangles = 136105
arena frame = len 307200 fnv fc44d5c9ad9de683
"#,
)];

/// Placements that take every way arena draws can share a mesh's indices
/// or a visible list, under sun shadows: a cutout card and a cube that two
/// pieces each draw, the cube also as a cut-out piece's coarse level;
/// thinning leaves two pieces share, at levels sharing their vertices; a
/// bent grid whose coarse level shares its vertices; impostor cubes near
/// and far; translucent decals of two pieces sharing one mesh, overlapping;
/// a grid past 65,536 vertices; and a flat quad.
pub(super) fn arena_scene() -> (Renderer, Camera, Extent) {
    let field = flat_field(33);
    let terrain = Arc::new(Terrain {
        heights: Arc::new(HeightMap::new(&field).unwrap()),
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
    let card = Arc::new(Mesh::new(card).unwrap());
    let block = Arc::new(cube(0xffc08040, Material::Lit));
    // 24 ranked leaves of 18 triangles each, rising round a stem.
    let mut leaves = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    let mut parts = Vec::new();
    for k in 0..24u32 {
        let a = k as f32 * 2.399_963;
        let r = 0.15 + 0.4 * k as f32 / 24.;
        let (x, y, z) = (a.cos() * r, 0.2 + 0.04 * k as f32, a.sin() * r);
        let leaf = sheet(0.2, 0., 3, 0xff30a050 + k * 0x0004_0300);
        let base = leaves.positions.len() as u32;
        (leaves.positions).extend(
            leaf.positions
                .iter()
                .map(|p| [p[0] + x, y + 0.3 * p[0], p[2] + z]),
        );
        leaves.colors.extend(&leaf.colors);
        leaves.indices.extend(leaf.indices.iter().map(|i| i + base));
        parts.extend(vec![[x, y, z, k as f32 / 24.]; leaf.positions.len()]);
    }
    leaves.thinning = Some(Thinning {
        parts,
        full: 120.,
        least: 0.2,
    });
    let (bush, bush_levels) = Mesh::compact_levels(leaves, &[0.5]).unwrap();
    assert_eq!(bush_levels.len(), 1, "the leaves simplify once");
    // compact_levels_draw_from_the_full_mesh_vertices's bent grid, smaller.
    let n = 33u32;
    let mut bent = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    for z in 0..n {
        for x in 0..n {
            let (u, v) = (x as f32 / 32., z as f32 / 32.);
            bent.positions.push([
                u * 2. - 1.,
                (u * std::f32::consts::PI).sin() * 0.6,
                v * 2. - 1.,
            ]);
            bent.colors
                .push(0xff00_0000 | ((u * 255.) as u32) << 16 | ((v * 255.) as u32) << 8 | 0x60);
        }
    }
    for z in 0..n - 1 {
        for x in 0..n - 1 {
            let i = z * n + x;
            bent.indices
                .extend([i, i + 1, i + n + 1, i, i + n + 1, i + n]);
        }
    }
    let (bent, bent_levels) = Mesh::compact_levels(bent, &[0.15]).unwrap();
    let mut boxed = cube(0xffffffff, Material::Lit).data().clone();
    boxed.texture = Some(Texture::solid([60, 90, 220, 255]).into());
    let boxed = Arc::new(Mesh::new(boxed).unwrap());
    let mut decal = quad(
        0.8,
        0.,
        Material::Decal(Decal {
            opacity: 0.6,
            bias: 0.01,
            grazing: true,
            ground: false,
            edge: None,
        }),
    );
    decal.texture = Some(Image::from(
        Texture::from_rgba(
            4,
            1,
            [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 0, 255],
            ]
            .concat(),
        )
        .unwrap(),
    ));
    let decal = Arc::new(Mesh::new(decal).unwrap());
    let side = 260u32;
    let mut wide = MeshData {
        material: Material::Lit,
        ..Default::default()
    };
    for z in 0..side {
        for x in 0..side {
            let (u, v) = (x as f32 / (side - 1) as f32, z as f32 / (side - 1) as f32);
            wide.positions
                .push([u * 2. - 1., 0.05 * (u * 40.).sin(), v * 2. - 1.]);
            wide.normals.push([0., 1., 0.]);
            wide.colors
                .push(0xff00_0000 | ((u * 255.) as u32) << 16 | ((v * 255.) as u32) << 8 | 0x40);
        }
    }
    for z in 0..side - 1 {
        for x in 0..side - 1 {
            let i = z * side + x;
            wide.indices
                .extend([i, i + side, i + 1, i + 1, i + side, i + side + 1]);
        }
    }
    let wide = Arc::new(Mesh::new(wide).unwrap());
    let flat = Arc::new(Mesh::new(quad(0.5, 0.02, Material::Flat)).unwrap());
    let one = |m: &Arc<Mesh>| Piece {
        levels: vec![(m.clone(), 0.)],
        impostor: None,
        pose: None,
    };
    let thinning = || Piece {
        levels: vec![(bush.clone(), 60.), (bush_levels[0].clone(), 0.)],
        impostor: None,
        pose: None,
    };
    renderer.set_pieces(vec![
        one(&card),
        one(&card),
        one(&block),
        Piece {
            levels: vec![(card.clone(), 1e9), (block.clone(), 0.)],
            impostor: None,
            pose: None,
        },
        thinning(),
        thinning(),
        Piece {
            levels: std::iter::once((bent.clone(), 80.))
                .chain(bent_levels.iter().map(|l| (l.clone(), 0.)))
                .collect(),
            impostor: None,
            pose: None,
        },
        Piece {
            levels: vec![(boxed.clone(), 0.)],
            impostor: Some(70.),
            pose: None,
        },
        one(&decal),
        one(&decal),
        one(&wide),
        one(&flat),
    ]);
    let turned = |at: Vec3, angle: f32, scale: f32| {
        let (s, c) = angle.sin_cos();
        let mut m = math::translate(at);
        (m[0], m[2], m[8], m[10]) = (c * scale, -s * scale, s * scale, c * scale);
        m[5] = scale;
        m
    };
    let mut rows: Vec<(u32, Mat4)> = vec![
        (0, turned([-5., 0., -4.], 0.3, 1.)),
        (0, turned([-4., 0., -4.5], 1.1, 1.)),
        (1, turned([-3., 0., -4.], 2.0, 1.)),
        (1, turned([-2., 0., -4.5], 2.7, 1.)),
        (2, turned([-5., 0.3, -2.], 0.2, 0.6)),
        (2, turned([-4., 0.3, -2.], 0.9, 0.6)),
        (3, turned([-3., 0.3, -2.], 1.6, 0.6)),
        (3, turned([-2., 0.3, -2.], 2.3, 0.6)),
        (6, turned([3., 0.6, -4.], 0.4, 0.8)),
        (6, turned([5., 0.6, 5.], 1.4, 0.8)),
        (7, turned([4., 0.5, 0.], 0.5, 1.)),
        (7, turned([5.5, 0.5, 3.], 1.5, 1.)),
        (7, turned([-5.5, 0.5, 5.5], 2.5, 1.)),
        (10, turned([2., 0.1, 2.], 0.3, 1.)),
        (11, turned([-1., 0., 0.], 0.7, 1.)),
        (11, turned([1., 0., 5.], 1.2, 1.)),
    ];
    // Bushes over the field, the two pieces alternating, from near to far.
    for k in 0..20 {
        let (x, z) = ((k % 5) as f32 * 2.6 - 5.2, (k / 5) as f32 * 3. - 4.5);
        rows.push((4 + k % 2, turned([x + 0.7, 0., z + 1.1], k as f32, 1.5)));
    }
    // Decals of the two pieces in turn, each over the last.
    for k in 0..6 {
        let at = [-0.9 + 0.35 * k as f32, 0.02, 3. + 0.1 * k as f32];
        rows.push((8 + k % 2, turned(at, k as f32 * 1.3, 1.)));
    }
    let (piece_of, transforms): (Vec<u32>, Vec<Mat4>) = rows.into_iter().unzip();
    renderer.set_placements(&piece_of, &transforms);
    let world = Extent {
        center: [0.; 3],
        size: 14.,
    };
    let camera = Camera {
        fov: 0.9,
        zoom: 1.3,
        tilt: 0.9,
        yaw: 0.4,
        ..Camera::perspective()
    };
    (renderer, camera, world)
}

// arena_scene, its draws' base vertex and first instance zero, as D3D12
// sees them without wgpu's indirect-draw checks: the frame once impostors
// are baked and two frames agree, and the placements and triangles drawn,
// as recorded when draws had both.
#[test]
fn the_arena_draws_as_recorded() {
    let _clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    HELD_CLOCK.store(1f32.to_bits(), std::sync::atomic::Ordering::Relaxed);
    let (renderer, camera, world) = arena_scene();
    let mut target = Offscreen::new(320, 240).unwrap();
    let device = adapter(target.device());
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    // The bake lands a frame after the arena, counts a frame late.
    let mut pixels = target.frame(&renderer).unwrap();
    for _ in 0..8 {
        let again = target.frame(&renderer).unwrap();
        if again == pixels {
            break;
        }
        pixels = again;
    }
    assert!(
        pixels == target.frame(&renderer).unwrap(),
        "frames still change"
    );
    {
        let state = renderer.lock();
        let placed = &state.assets.placed;
        assert!(placed.arena.is_some() && placed.atlas.is_some());
        // What D3D12 draws without wgpu's indirect-draw checks, which
        // leave both out of the shaders' indices: every draw starts at
        // vertex and instance zero.
        let args = placed.args_template();
        assert!(
            args.chunks(placed::ARGS as usize)
                .all(|a| a[3] == 0 && a[4] == 0),
            "a draw has a base vertex or first instance"
        );
    }
    let drawn = renderer.drawn();
    let lines = [
        ("arena placed", format!("{:?}", drawn.placed)),
        // Thinning's buckets show in the triangles.
        ("arena triangles", drawn.placed_triangles.to_string()),
        ("arena frame", digest(&pixels)),
    ];
    dump("arena", &pixels, 320, 240);
    HELD_CLOCK.store(f32::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed);
    if std::env::var("KIT_BLESS").is_ok_and(|v| v == "1") {
        let mut text = format!("    (\n        {device:?},\n        r#\"\n");
        for (k, v) in &lines {
            text += &format!("{k} = {v}\n");
        }
        eprintln!("{text}\"#,\n    ),");
        return;
    }
    let Some((_, golden)) = ARENA_FRAMES.iter().find(|(a, _)| *a == device) else {
        eprintln!(
            "skipped: no recorded arena frames for adapter {device:?}; KIT_BLESS=1 prints them"
        );
        return;
    };
    let want: Vec<(&str, &str)> = (golden.lines())
        .filter_map(|l| l.split_once(" = "))
        .collect();
    let now: Vec<(&str, &str)> = lines.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_eq!(
        now, want,
        "arena frames on {device} (KIT_BLESS=1 prints fresh ones)"
    );
}
