// The a7m ground's behaviour: layers, palettes, relief, grit, gloss, tint
// and validation, drawn over a terrain as the island draws it, and the
// frames the kit pinned before the splat moved here.
use super::*;
use gpui_editor_kit::{
    camera::{Camera, Extent},
    gpu::{
        Detail, GroundTint, HeightMap, Light, Material, Offscreen, Renderer, Scene, Shadows,
        Terrain, Tone,
    },
    heightfield::Heightfield,
    math::Vec3,
    texture::{Block, Compressed, Texture},
};

/// Light that leaves albedo as it is, so colours check exactly.
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

/// A 4x4 texture of one colour.
fn plain(rgba: [u8; 4]) -> Image {
    Image::from(Texture::from_rgba(4, 4, rgba.repeat(16)).unwrap())
}

/// A 4x2 control map, its left two columns `left` and right two `right`.
fn halves(left: [u8; 4], right: [u8; 4]) -> Arc<ControlMap> {
    let row = [left, left, right, right].concat();
    Arc::new(ControlMap::new(4, 2, row.repeat(2)).unwrap())
}

/// One layer everywhere, in `layers`' first.
fn one_layer(layers: Vec<Image>) -> Splat {
    let one = |rgba: [u8; 4]| Arc::new(ControlMap::new(4, 2, rgba.repeat(8)).unwrap());
    Splat {
        tiling: vec![1.; layers.len()],
        layers,
        indices: vec![one([0; 4])],
        weights: vec![one([255, 0, 0, 0])],
        ..Default::default()
    }
}

/// `splat` as the ground of a flat terrain from -`half` to `half` each
/// way, its control maps spanning it as they span an island's grid.
fn flat_scene(splat: &Arc<Splat>, half: f32, light: Light) -> Scene {
    let side = 33;
    let grid = Grid {
        width: side,
        depth: side,
        spacing: 2. * half / (side - 1) as f32,
        origin: [-half; 2],
    };
    let field = Heightfield::new(grid, vec![0.; side * side]);
    Scene {
        terrain: Some(Arc::new(Terrain {
            heights: Arc::new(HeightMap::new(&field).unwrap()),
            colors: Arc::new(ControlMap::new(side, side, [255; 4].repeat(side * side)).unwrap()),
            color_gain: 1.,
            tints: false,
            material: Material::Lit,
            texture: None,
        })),
        light,
        ground: ground(splat),
        ..Default::default()
    }
}

/// A renderer shading the ground live at every pixel: no page between the
/// splat and the pixels these tests read.
fn live() -> Renderer {
    let renderer = Renderer::default();
    renderer.set_detail(Detail {
        live_ground: true,
        ..Default::default()
    });
    renderer
}

/// `scene` drawn live from the default camera over a `size` metre world:
/// the colour at a world point.
fn shot(scene: &Scene, size: f32) -> impl Fn(Vec3) -> [u8; 3] + use<> {
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = live();
    renderer.set_scene(scene);
    let camera = Camera::default();
    let world = Extent {
        center: [0.; 3],
        size,
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    move |p| target.pixel(&pixels, camera.project(p, target.bounds(), world))
}

fn digest(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf29ce484222325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn solid(rgba: [u8; 4]) -> Image {
    Arc::new(Texture::from_rgba(8, 8, rgba.repeat(64)).unwrap()).into()
}

/// Two layers, a checker and a solid blue, the west half of `side`²
/// control texels the checker.
fn west_checker(side: usize) -> Splat {
    // 8x8 texels of light and dark squares, so blur and misplacement show.
    let checker: Vec<u8> = (0..64)
        .flat_map(|i| {
            if (i % 8 / 2 + i / 8 / 2) % 2 == 0 {
                [220, 180, 90, 255]
            } else {
                [40, 90, 30, 255]
            }
        })
        .collect();
    let control = |texels: Vec<u8>| Arc::new(ControlMap::new(side, side, texels).unwrap());
    let west = (0..side * side).flat_map(|i| {
        if i % side < side / 2 {
            [0; 4]
        } else {
            [1, 0, 0, 0]
        }
    });
    Splat {
        layers: vec![
            Arc::new(Texture::from_rgba(8, 8, checker).unwrap()).into(),
            solid([60, 60, 200, 255]),
        ],
        tiling: vec![12., 5.],
        indices: vec![control(west.collect())],
        weights: vec![control([255, 0, 0, 0].repeat(side * side))],
        ..Default::default()
    }
}

/// 129² samples half a metre apart, centred on the origin.
fn flat_field() -> Heightfield {
    let grid = Grid {
        width: 129,
        depth: 129,
        spacing: 0.5,
        origin: [-32.; 2],
    };
    Heightfield::new(grid, vec![0.; 129 * 129])
}

// The frames the kit pinned for its own splat ground before the splat
// moved here (gpui-editor-kit gpu/tests/ground.rs ground_frame_digests,
// re-blessed for its page sheets): this ground, drawn over a terrain
// in the island's own material, baked into pages, after a sculpt with
// shadows on, and live at every pixel (`Detail::live_ground`), plain,
// with normal layers and with relief. The kit's live frame drew the
// splat on a mesh quad, which no longer exists; the relief frames are
// the ones the shell's kit-against-island test checked equal.
#[test]
fn the_ground_draws_the_kits_pinned_frames() {
    let mut target = Offscreen::new(320, 240).unwrap();
    let camera = Camera {
        tilt: 0.9,
        zoom: 5.,
        ..Default::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 64.,
    };
    for variant in ["plain", "normals", "relief"] {
        let mut field = flat_field();
        let mut splat = west_checker(256);
        if variant == "normals" {
            splat.normals = vec![solid([210, 145, 210, 255]); splat.layers.len()];
        }
        if variant == "relief" {
            splat.relief = Some(Relief {
                heights: vec![solid([255; 4]), solid([0, 0, 0, 255])],
                ranges: vec![[0., 0.6, 0.], [0., 0.2, 0.]],
                on: true,
            });
        }
        let splat = Arc::new(splat);
        let heights = Arc::new(HeightMap::new(&field).unwrap());
        let scene = Scene {
            terrain: Some(Arc::new(Terrain {
                heights: heights.clone(),
                colors: Arc::new(ControlMap::new(129, 129, [255; 4].repeat(129 * 129)).unwrap()),
                color_gain: 1.,
                tints: false,
                material: Material::Lit,
                texture: None,
            })),
            light: Light {
                sun: [1., 1., 0.],
                shadows: Some(Shadows::default()),
                ..Light::default()
            },
            ground: ground(&splat),
            ..Default::default()
        };
        let live = {
            let renderer = Renderer::default();
            renderer.set_detail(Detail {
                live_ground: true,
                ..Default::default()
            });
            renderer.set_scene(&scene);
            target.aim(&renderer, camera, world);
            let live = digest(&target.settle(&renderer).unwrap());
            assert_eq!(renderer.drawn().pages.baked, 0, "{variant}: no pages live");
            live
        };
        let renderer = Renderer::default();
        target.aim(&renderer, camera, world);
        renderer.set_scene(&scene);
        let paged = digest(&target.settle(&renderer).unwrap());
        let pages = renderer.drawn().pages;
        assert!(
            pages.baked > 0 && pages.baked == pages.wanted,
            "{variant}: {pages:?}"
        );
        // A ridge across the checker's edge, casting west.
        let rect = Rect {
            x0: 47,
            z0: 57,
            x1: 56,
            z1: 74,
        };
        for z in rect.z0..rect.z1 {
            for x in rect.x0..rect.x1 {
                field.heights[z * 129 + x] = 7.;
            }
        }
        heights.update(rect, &field.heights);
        let sculpted = digest(&target.settle(&renderer).unwrap());
        assert_ne!(paged, sculpted);
        eprintln!("{variant} paged={paged:016x} live={live:016x} sculpted={sculpted:016x}");
        // Raw frames include driver-specific filtering and BC rounding.
        let info = target.device().adapter_info();
        if info.name == "NVIDIA GeForce RTX 4080 SUPER" && info.driver_info == "620.70.06" {
            let expected = match variant {
                "plain" => [0xde07652b851d69d0, 0xf0bca2e3ab4af330, 0xfaa4e75d03e4432b],
                "normals" => [0x3ce9d8a109b2e990, 0xe2d4a6edcfee20a1, 0x49f474c5eb17a0d2],
                _ => [0x87631655a80268b4, 0x0f9fc9eeb9d1b350, 0x770f4de7f3fffee2],
            };
            assert_eq!([paged, live, sculpted], expected, "{variant}");
        }
    }
}

// Grey ground under a three-texel tint: dark (Y 0.35: (0.7)^2.2 = x0.456),
// neutral (0.5 grey, x1), bright (Y 0.75: x2.44, so albedo clamps at 1),
// decoded per pixel as the game's terrain shader does.
#[test]
fn the_ground_takes_the_tint_map_per_pixel() {
    let splat = Arc::new(one_layer(vec![plain([200, 200, 200, 255])]));
    let texel = |y: u8| [128, y, 128, 255];
    let tint = [texel(89), texel(128), texel(191)].concat();
    let mut scene = flat_scene(&splat, 3., neutral());
    // Three texels over x -3..3, one 2 m column each.
    scene.ground_tint = Some(GroundTint {
        map: Arc::new(ControlMap::new(3, 1, tint).unwrap()),
        origin: [-3., -3.],
        size: [6., 6.],
    });
    let at = shot(&scene, 7.);
    let (dark, neutral, bright) = (at([-2., 0., 0.])[1], at([0.; 3])[1], at([2., 0., 0.])[1]);
    assert!(dark.abs_diff(91) <= 3, "dark {dark}");
    assert!(neutral.abs_diff(202) <= 3, "neutral {neutral}");
    assert_eq!(bright, 255);
}

// Red ground, green grit, and a mask up on its east half only: the grit
// covers flat ground there and leaves the west as it was. The mask spans
// the terrain from its grid's origin.
#[test]
fn grit_covers_flat_ground_where_its_mask_is_up() {
    let mask = [
        [0, 0, 0, 255],
        [0, 0, 0, 255],
        [255, 0, 0, 255],
        [255, 0, 0, 255],
    ]
    .concat();
    let splat = Arc::new(Splat {
        grit: Some(Grit {
            texture: plain([0, 255, 0, 255]),
            mask: Arc::new(ControlMap::new(4, 2, mask.repeat(2)).unwrap()),
            metres: 4.,
            tiling: 1.,
            sharpness: 0.,
            slope: 0.2,
        }),
        ..one_layer(vec![plain([255, 0, 0, 255])])
    });
    let at = shot(&flat_scene(&splat, 2., neutral()), 5.);
    near(at([-1., 0., 0.]), [255, 0, 0], 3);
    near(at([1.5, 0., 0.]), [0, 255, 0], 3);
}

#[test]
fn weights_interpolate_across_palette_tiles() {
    // 16 control texels across two 8-texel palette tiles: red layer 0 under
    // everything, as shipped islands keep a base, and green layer 1 over it
    // on the east tile. Midway green covers half, with no seam.
    let row = |left: [u8; 4], right: [u8; 4]| [left.repeat(8), right.repeat(8)].concat();
    let control =
        |left, right| Arc::new(ControlMap::new(16, 2, row(left, right).repeat(2)).unwrap());
    let splat = Arc::new(Splat {
        layers: vec![plain([255, 0, 0, 255]), plain([0, 255, 0, 255])],
        tiling: vec![1., 1.],
        indices: vec![control([0, 1, 0, 0], [0, 1, 0, 0])],
        weights: vec![control([255, 0, 0, 0], [255, 255, 0, 0])],
        ..Default::default()
    });
    let at = shot(&flat_scene(&splat, 2., neutral()), 5.);
    near(at([-1., 0., 0.]), [255, 0, 0], 1);
    near(at([1., 0., 0.]), [0, 255, 0], 1);
    // Halfway from texel 7 to 8: Anno holds texel i's value at i, and the
    // 16 texels span x -2..2.
    near(at([-0.125, 0., 0.]), [128, 128, 0], 24);
    // A quarter texel past texel 7 the bicubic coverage is 0.203 (bilinear
    // gave 0.25), shown through BlendSplatSmooth as 0.107 green: 27 of 255,
    // where bilinear showed 40. The pixel lands within 0.03 texels of it.
    near(at([-0.1875, 0., 0.]), [228, 27, 0], 6);

    // Anno 1800's bake: bilinear coverage, laid over with the plain cut,
    // so the same spot shows a quarter green.
    let classic = Arc::new(Splat {
        bake: Bake::Plain,
        ..(*splat).clone()
    });
    let at = shot(&flat_scene(&classic, 2., neutral()), 5.);
    near(at([-1., 0., 0.]), [255, 0, 0], 1);
    near(at([-0.1875, 0., 0.]), [191, 64, 0], 6);
}

#[test]
fn layers_follow_index_and_weight_maps() {
    // 4x2 control texels: the west two columns all layer 0; the east two
    // all layer 1 (first pair) under half of layer 2 (second pair).
    let splat = Arc::new(Splat {
        layers: vec![
            plain([255, 0, 0, 255]),
            plain([0, 255, 0, 255]),
            plain([0, 0, 255, 255]),
        ],
        tiling: vec![1., 2., 3.],
        indices: vec![halves([0; 4], [1, 0, 0, 0]), halves([0; 4], [2, 0, 0, 0])],
        weights: vec![
            halves([255, 0, 0, 0], [255, 0, 0, 0]),
            halves([0; 4], [128, 0, 0, 0]),
        ],
        ..Default::default()
    });
    let mut target = Offscreen::new(320, 240).unwrap();
    let renderer = live();
    renderer.set_scene(&flat_scene(&splat, 2., neutral()));
    let camera = Camera::default();
    let world = Extent {
        center: [0.; 3],
        size: 5.,
    };
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    let at = |target: &Offscreen, pixels: &[u8], p: Vec3| {
        target.pixel(pixels, camera.project(p, target.bounds(), world))
    };
    near(at(&target, &pixels, [-1., 0., 0.]), [255, 0, 0], 1);
    near(at(&target, &pixels, [1., 0., 0.]), [0, 128, 128], 2);

    // Painting the east half to layer 0 shows on the next frame, through
    // the same splat and scene.
    let rect = Rect {
        x0: 2,
        z0: 0,
        x1: 4,
        z1: 2,
    };
    splat.indices[0].write(rect, &[0; 16]);
    splat.weights[0].write(rect, &[255, 0, 0, 0].repeat(4));
    splat.weights[1].write(rect, &[0; 16]);
    let pixels = target.settle(&renderer).unwrap();
    near(at(&target, &pixels, [1., 0., 0.]), [255, 0, 0], 1);
    near(at(&target, &pixels, [-1., 0., 0.]), [255, 0, 0], 1);
    // The sharpening check below shares these index maps; put them back.
    splat.indices[0].write(rect, &[1, 0, 0, 0].repeat(4));

    // With sharpness 2 and opaque images a layer covers at 3x its weight,
    // then smoothed as Anno's BlendSplatSmooth: a quarter of layer 2 over
    // layer 1 covers 0.75, shown as 0.84.
    let sharp = Arc::new(Splat {
        sharpness: vec![1., 2., 2.],
        weights: vec![
            halves([255, 0, 0, 0], [192, 0, 0, 0]),
            halves([0; 4], [64, 0, 0, 0]),
        ],
        ..(*splat).clone()
    });
    let at = shot(&flat_scene(&sharp, 2., neutral()), 5.);
    near(at([1., 0., 0.]), [0, 40, 215], 2);
}

// Compressed layers of different sizes start at the mip of the smallest
// (gpu::StreamedTexture::check holds the rule) and draw as stored; a splat
// rebuilt over the same layers (undoing paint) shares the uploaded array.
#[test]
fn compressed_layers_share_the_smallest_size() {
    let layer = |side: u32, levels: usize, rgb: [u8; 3]| -> Image {
        let [r, g, b] = rgb.map(u16::from);
        let c0 = (r >> 3) << 11 | (g >> 2) << 5 | b >> 3;
        // c0 > c1 picks four-colour mode, where index 0 is c0, opaque.
        let mut block = c0.to_le_bytes().to_vec();
        block.extend([0; 6]);
        let mips = (0..levels)
            .map(|l| {
                let (w, h) = ((side >> l).max(1), (side >> l).max(1));
                block.repeat(Block::Bc1.size(w, h) / Block::Bc1.bytes())
            })
            .collect();
        Compressed {
            block: Block::Bc1,
            width: side,
            height: side,
            mips,
            srgb: false,
        }
        .into()
    };
    let splat = Arc::new(Splat {
        // 16² starts two levels in, 8² one, to meet at 4².
        layers: vec![
            layer(16, 5, [255, 0, 0]),
            layer(8, 4, [0, 255, 0]),
            layer(4, 3, [0, 0, 255]),
        ],
        tiling: vec![1., 2., 3.],
        indices: vec![halves([0; 4], [1, 0, 0, 0]), halves([0; 4], [2, 0, 0, 0])],
        weights: vec![
            halves([255, 0, 0, 0], [255, 0, 0, 0]),
            halves([0; 4], [128, 0, 0, 0]),
        ],
        ..Default::default()
    });
    assert!(splat.validate().is_ok());
    // An 8² layer cannot reach 4² without mips.
    let bad = Splat {
        layers: vec![layer(8, 1, [255, 0, 0]), layer(4, 3, [0, 0, 255])],
        ..(*splat).clone()
    };
    assert!(bad.validate().is_err());
    let at = shot(&flat_scene(&splat, 2., neutral()), 5.);
    near(at([-1., 0., 0.]), [255, 0, 0], 1);
    near(at([1., 0., 0.]), [0, 128, 128], 2);

    let target = Offscreen::new(8, 8).unwrap();
    let rebuilt = (*splat).clone();
    let bc = gpu::bc_supported(target.device());
    let first = array(target.device(), &splat.layers, bc);
    assert!(Arc::ptr_eq(
        &first,
        &array(target.device(), &rebuilt.layers, bc)
    ));
}

// A new scene over the same terrain keeps the tiles it found rough or
// flat, and what paint since changed of them, as a fresh renderer finds
// them: the editor presents one after every stroke and ground bake.
#[test]
fn a_new_scene_over_the_same_terrain_keeps_its_relief_current() {
    let mut target = Offscreen::new(256, 256).unwrap();
    let field = flat_field();
    // 64² control texels, all layer 0: flat relief.
    let n = 64;
    let control = |rgba: [u8; 4]| Arc::new(ControlMap::new(n, n, rgba.repeat(n * n)).unwrap());
    let splat = Arc::new(Splat {
        layers: vec![plain([200, 60, 40, 255]), plain([40, 160, 60, 255])],
        tiling: vec![1., 1.],
        // Layer 1 stands a unit proud where its height map is white.
        relief: Some(Relief {
            heights: vec![plain([0, 0, 0, 255]), plain([255, 0, 0, 255])],
            ranges: vec![[0., 0., 0.], [0., 1., 0.]],
            on: true,
        }),
        indices: vec![control([0; 4])],
        weights: vec![control([255, 0, 0, 0])],
        ..Default::default()
    });
    let side = field.grid.width;
    let scene = Scene {
        terrain: Some(Arc::new(Terrain {
            heights: Arc::new(HeightMap::new(&field).unwrap()),
            colors: Arc::new(ControlMap::new(side, side, [255; 4].repeat(side * side)).unwrap()),
            color_gain: 1.,
            tints: false,
            material: Material::Lit,
            texture: None,
        })),
        light: neutral(),
        ground: ground(&splat),
        ..Default::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 64.,
    };
    // Close enough that rough ground takes strides under a sample.
    let camera = Camera {
        zoom: 4.,
        ..Camera::default()
    };
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    target.aim(&renderer, camera, world);
    target.settle(&renderer).unwrap();
    let flat = renderer.drawn().triangles;
    // Paint layer 1 inside the north-east tile (32 texels a tile), framed
    // once, then inside the south-west one with no frame between it and
    // the next scene.
    let paint = |x0: usize, z0: usize| {
        let rect = Rect {
            x0,
            z0,
            x1: x0 + 12,
            z1: z0 + 12,
        };
        splat.indices[0].write(rect, &[1, 0, 0, 0].repeat(144));
    };
    paint(44, 44);
    target.settle(&renderer).unwrap();
    let one = renderer.drawn().triangles;
    assert!(
        one > flat,
        "the paint is rough: {one} triangles, {flat} flat"
    );
    paint(8, 8);
    renderer.set_scene(&scene);
    let pixels = target.settle(&renderer).unwrap();
    let kept = renderer.drawn().triangles;
    assert!(
        kept > one,
        "and so is the second: {kept} triangles, {one} before"
    );

    let fresh = Renderer::default();
    fresh.set_scene(&scene);
    target.aim(&fresh, camera, world);
    let want = target.settle(&fresh).unwrap();
    assert_eq!(kept, fresh.drawn().triangles, "triangles as found afresh");
    let off = (pixels.iter().zip(&want))
        .filter(|(a, b)| a.abs_diff(**b) > 1)
        .count();
    assert_eq!(off, 0, "channels off from a fresh renderer's frame");
}

// Paint moves the shadow caster only within reach of a layer whose relief
// raises or sinks the ground by RELIEF_ROUGH or more: a flat layer laid
// over flat ground, or one raised by less, keeps the shadow map; the
// raised layer laid down redraws it, and so does a flat layer laid back
// over that.
#[test]
fn only_paint_near_displacing_layers_redraws_the_shadows() {
    let mut target = Offscreen::new(256, 256).unwrap();
    let field = flat_field();
    let n = 64;
    let control = |rgba: [u8; 4]| Arc::new(ControlMap::new(n, n, rgba.repeat(n * n)).unwrap());
    // Layer 1 stands a unit proud, 2 a few centimetres, as 117's sand
    // does; 0 maps to nothing.
    let splat = Arc::new(Splat {
        layers: vec![
            plain([200, 60, 40, 255]),
            plain([40, 160, 60, 255]),
            plain([40, 60, 200, 255]),
        ],
        tiling: vec![1.; 3],
        relief: Some(Relief {
            heights: vec![plain([255, 0, 0, 255]); 3],
            ranges: vec![[0., 0., 0.], [0., 1., 0.], [0., 0.06, 0.]],
            on: true,
        }),
        indices: vec![control([0; 4])],
        weights: vec![control([255, 0, 0, 0])],
        ..Default::default()
    });
    let side = field.grid.width;
    let scene = Scene {
        terrain: Some(Arc::new(Terrain {
            heights: Arc::new(HeightMap::new(&field).unwrap()),
            colors: Arc::new(ControlMap::new(side, side, [255; 4].repeat(side * side)).unwrap()),
            color_gain: 1.,
            tints: false,
            material: Material::Lit,
            texture: None,
        })),
        light: Light {
            sun: [1., 0.5, 0.2],
            sun_color: [0.8; 3],
            sky: [0.2; 3],
            ground: [0.2; 3],
            shadows: Some(Shadows {
                resolution: 1024,
                ..Default::default()
            }),
            fog: None,
        },
        ground: ground(&splat),
        ..Default::default()
    };
    let renderer = Renderer::default();
    renderer.set_scene(&scene);
    let world = Extent {
        center: [0.; 3],
        size: 64.,
    };
    target.aim(&renderer, Camera::default(), world);
    target.settle(&renderer).unwrap();
    // One texel a metre; the two spots lie 40 texels apart, past the
    // reach of a palette window round either.
    let mut paint = |x0: usize, layer: u8| {
        let rect = Rect {
            x0,
            z0: x0,
            x1: x0 + 12,
            z1: x0 + 12,
        };
        splat.indices[0].write(rect, &[layer, 0, 0, 0].repeat(144));
        target.frame(&renderer).unwrap();
        let drawn = renderer.drawn();
        target.settle(&renderer).unwrap();
        assert!(drawn.pages.new > 0, "the stroke's pages bake again");
        drawn.shadow
    };
    assert!(!paint(4, 2), "flat over flat");
    assert!(paint(44, 1), "the raised layer laid down");
    assert!(!paint(4, 0), "flat over flat again");
    assert!(paint(44, 2), "a flat layer over the raised one");
}

#[test]
fn normal_layers_bend_the_lit_normal() {
    // West layer 0 with a flat normal map, east layer 1 whose normals lean
    // toward +X, where the sun is.
    let splat = Arc::new(Splat {
        layers: vec![plain([200, 200, 200, 255]), plain([200, 200, 200, 255])],
        tiling: vec![1., 1.],
        normals: vec![plain([128, 128, 255, 255]), plain([230, 128, 180, 255])],
        indices: vec![halves([0; 4], [1, 0, 0, 0])],
        weights: vec![halves([255, 0, 0, 0], [255, 0, 0, 0])],
        ..Default::default()
    });
    let light = Light {
        sun: [1., 0.3, 0.],
        sun_color: [1.; 3],
        sky: [0.1; 3],
        ground: [0.1; 3],
        ..Default::default()
    };
    let at = shot(&flat_scene(&splat, 2., light), 5.);
    let (flat, leaning) = (at([-1., 0., 0.])[0], at([1., 0., 0.])[0]);
    assert!(leaning > flat + 40, "flat {flat}, leaning {leaning}");
}

// The same grey on both halves; the east layer's gloss map says glossy.
// With the sun overhead and the camera looking down, only it shines.
#[test]
fn gloss_layers_give_toned_ground_highlights() {
    let splat = Arc::new(Splat {
        layers: vec![plain([128, 128, 128, 255]), plain([128, 128, 128, 255])],
        tiling: vec![1., 1.],
        gloss: vec![plain([0, 0, 0, 255]), plain([255, 0, 0, 255])],
        indices: vec![halves([0; 4], [1, 0, 0, 0])],
        weights: vec![halves([255, 0, 0, 0], [255, 0, 0, 0])],
        ..Default::default()
    });
    let light = Light {
        sun: [0., 1., 0.],
        sun_color: [1.; 3],
        sky: [0.1; 3],
        ground: [0.1; 3],
        ..Default::default()
    };
    let mut scene = flat_scene(&splat, 2., light);
    scene.tone = Some(Tone::default());
    let at = shot(&scene, 5.);
    let (matte, glossy) = (at([-1., 0., 0.])[0], at([1., 0., 0.])[0]);
    assert!(glossy > matte + 30, "matte {matte}, glossy {glossy}");
}

// Control maps stay on the CPU: palettes build from windows of them, a
// band of rows a frame (PALETTE_BAND). A map taking several bands still
// shades in every band, and no copy of it stays on the GPU.
#[test]
fn palettes_build_in_bands_from_cpu_control_maps() {
    let mut target = Offscreen::new(256, 256).unwrap();
    let splat = Arc::new(west_checker(1024));
    let camera = Camera {
        tilt: 0.05,
        yaw: 0.,
        ..Camera::default()
    };
    let world = Extent {
        center: [0.; 3],
        size: 32.,
    };
    let renderer = live();
    renderer.set_scene(&flat_scene(&splat, 16., neutral()));
    target.aim(&renderer, camera, world);
    let pixels = target.settle(&renderer).unwrap();
    // The checker west, solid blue east, in the bands north and south.
    let blue = |x: usize, y: usize| {
        let i = (y * 256 + x) * 4;
        let [r, g, b] = [pixels[i], pixels[i + 1], pixels[i + 2]];
        b > r.saturating_add(40) && b > g.saturating_add(40)
    };
    for y in [70, 186] {
        assert!(!blue(70, y), "west at row {y}");
        assert!(blue(186, y), "east at row {y}");
    }
    // A whole copy of one of the maps is 4 MiB; tests share the device,
    // and other splats' windows come and go.
    if let Some(report) = target.device().generate_allocator_report() {
        let biggest = (report.allocations.iter())
            .filter(|a| a.name.starts_with("splat control"))
            .map(|a| a.size)
            .max()
            .unwrap_or(0);
        assert!(biggest < 4 << 20, "a {biggest}-byte control map stays");
    }
}

// A stroke over the whole of a 1024² splat builds its palettes over
// several frames, and the ground shows it once they are in.
#[test]
fn a_paint_edit_finishes_over_several_frames() {
    let splat = Arc::new(west_checker(1024));
    let renderer = live();
    let mut target = Offscreen::new(128, 128).unwrap();
    let camera = Camera::default();
    let world = Extent {
        center: [0.; 3],
        size: 32.,
    };
    renderer.set_scene(&flat_scene(&splat, 16., neutral()));
    let before = target.render(&renderer, camera, world).unwrap();
    splat.indices[0].write(
        Rect {
            x0: 0,
            z0: 0,
            x1: 1024,
            z1: 1024,
        },
        &[1, 0, 0, 0].repeat(1024 * 1024),
    );
    target.frame(&renderer).unwrap();
    assert!(renderer.pending());
    let after = target.settle(&renderer).unwrap();
    let at = camera.project([-6., 0., -6.], target.bounds(), world);
    assert_ne!(target.pixel(&before, at), target.pixel(&after, at));
    near(target.pixel(&after, at), [60, 60, 200], 3);
}

// 1800's moderate set ships height maps with no displacement ranges: the
// ground still says it has relief, so the kit keeps normal pages for it
// (shaded from its height), though it raises nothing.
#[test]
fn relief_with_no_displacement_still_has_relief() {
    let mut splat = west_checker(256);
    splat.relief = Some(Relief {
        heights: splat.layers.clone(),
        ranges: vec![[0.; 3]; splat.layers.len()],
        on: true,
    });
    let gpu::GroundDraw::Surface(factory) = ground(&Arc::new(splat)) else {
        panic!("a mounted ground");
    };
    let target = Offscreen::new(8, 8).unwrap();
    let mounted = factory(terrain::GroundInit {
        device: target.device(),
        grid: flat_field().grid,
    });
    assert!(mounted.has_relief());
    assert_eq!(mounted.relief_bound(), 0.);
}

#[test]
fn splats_reject_mismatched_layers_and_controls() {
    let image = |w, h| -> Image {
        Arc::new(Texture::from_rgba(w, h, vec![0; (w * h * 4) as usize]).unwrap()).into()
    };
    let map = |w: usize, h: usize| Arc::new(ControlMap::new(w, h, vec![0; w * h * 4]).unwrap());
    let good = Splat {
        layers: vec![image(4, 4), image(4, 4)],
        tiling: vec![2., 3.],
        indices: vec![map(8, 8)],
        weights: vec![map(8, 8)],
        ..Default::default()
    };
    assert!(good.validate().is_ok());
    let bad = [
        Splat {
            layers: vec![image(4, 4), image(2, 2)],
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
        // Relief takes a height map and a range for every layer.
        Splat {
            relief: Some(Relief {
                heights: vec![image(4, 4)],
                ranges: vec![[0., 1., 0.]; 2],
                on: true,
            }),
            ..good.clone()
        },
        Splat {
            relief: Some(Relief {
                heights: vec![image(4, 4), image(4, 4)],
                ranges: vec![[0., f32::NAN, 0.]; 2],
                on: true,
            }),
            ..good.clone()
        },
    ];
    let relieved = Splat {
        relief: Some(Relief {
            heights: vec![image(4, 4), image(4, 4)],
            ranges: vec![[-0.4, 0.6, 0.], [0., 0.05, 1.]],
            on: true,
        }),
        ..good.clone()
    };
    assert!(relieved.validate().is_ok());
    for splat in bad {
        assert!(splat.validate().is_err());
    }
}

// A device lost as a ground mounts: the mount panics in wgpu there, with
// the factory's lock on its device copies held. The next device mounts the
// same splat and draws it as a renderer that never lost one: a poisoned
// lock had failed every later mount, and the view never drew again.
#[test]
fn a_ground_mounting_as_its_device_is_lost_draws_on_the_next() {
    let camera = Camera::default();
    let world = Extent {
        center: [0.; 3],
        size: 7.,
    };
    // The terrain drawn whole on the doomed device under another ground,
    // so the frame after the loss gets as far as mounting the new one.
    let before = flat_scene(
        &Arc::new(one_layer(vec![plain([40, 120, 200, 255])])),
        3.,
        neutral(),
    );
    let splat = Arc::new(one_layer(vec![plain([200, 120, 40, 255])]));
    let renderer = live();
    renderer.set_scene(&before);
    let mut lost = Offscreen::on_new_device(64, 64).unwrap();
    lost.aim(&renderer, camera, world);
    lost.settle(&renderer).unwrap();
    let scene = Scene {
        ground: ground(&splat),
        ..before.clone()
    };
    renderer.set_scene(&scene);
    lost.lose();
    let mounted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lost.frame(&renderer)));
    assert!(
        mounted.is_err() || mounted.is_ok_and(|f| f.is_err()),
        "a frame on a lost device fails"
    );
    drop(lost);

    let mut target = Offscreen::new(64, 64).unwrap();
    target.aim(&renderer, camera, world);
    target.frame(&renderer).unwrap();
    let after = target.settle(&renderer).unwrap();
    let fresh = live();
    fresh.set_scene(&flat_scene(&splat, 3., neutral()));
    target.aim(&fresh, camera, world);
    let expected = target.settle(&fresh).unwrap();
    assert_eq!(digest(&after), digest(&expected));
    let center = camera.project([0.; 3], target.bounds(), world);
    assert_ne!(target.pixel(&after, center), [0; 3], "the ground drew");
}
