// GPUI's path rendering as vendor/zed patches it: paths rasterize into an
// intermediate covering what they cover, not the whole frame, and land
// back in place.
#![cfg(target_os = "linux")]
use gpui::{
    Bounds, ContentMask, DevicePixels, Path, PlatformHeadlessRenderer, Point, ScaledPixels, Scene,
    Size, linear_color_stop, linear_gradient,
};
use gpui_wgpu::WgpuHeadlessRenderer;

const FRAME: Size<DevicePixels> = Size {
    width: DevicePixels(600),
    height: DevicePixels(400),
};

/// A `width` x `height` rectangle at `x`, `y` as a filled path of two
/// triangles, clipped to the frame.
fn rect(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: impl Into<gpui::Background>,
) -> Path<ScaledPixels> {
    let at = |x: f32, y: f32| Point {
        x: x.into(),
        y: y.into(),
    };
    let mut path = Path::new(at(x, y));
    path.line_to(at(x + width, y));
    path.line_to(at(x + width, y + height));
    path.line_to(at(x, y + height));
    path.content_mask = ContentMask {
        bounds: Bounds {
            origin: at(0., 0.),
            size: Size {
                width: 600f32.into(),
                height: 400f32.into(),
            },
        },
    };
    path.color = color.into();
    path.scale(1.)
}

fn draw(renderer: &mut WgpuHeadlessRenderer, paths: Vec<Path<ScaledPixels>>) -> image::RgbaImage {
    let mut scene = Scene::default();
    for path in paths {
        scene.insert_primitive(path);
    }
    scene.finish();
    renderer.render_scene_to_image(&scene, FRAME).unwrap()
}

fn rgb(image: &image::RgbaImage, x: u32, y: u32) -> [u8; 3] {
    let [r, g, b, _] = image.get_pixel(x, y).0;
    [r, g, b]
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 3)
}

#[test]
fn paths_land_where_they_were_drawn() {
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    let (red, black) = ([255, 0, 0], [0, 0, 0]);
    // Away from the frame's corner, as a viewport's brush outline.
    let image = draw(&mut renderer, vec![rect(400., 250., 60., 40., gpui::red())]);
    for (x, y) in [(401, 251), (430, 270), (458, 288)] {
        assert!(
            near(rgb(&image, x, y), red),
            "({x}, {y}): {:?}",
            rgb(&image, x, y)
        );
    }
    for (x, y) in [(398, 270), (462, 270), (430, 292), (5, 5)] {
        assert!(
            near(rgb(&image, x, y), black),
            "({x}, {y}): {:?}",
            rgb(&image, x, y)
        );
    }
    // Two paths far apart, one past the frame's edge.
    let image = draw(
        &mut renderer,
        vec![
            rect(10., 10., 20., 20., gpui::red()),
            rect(500., 300., 200., 200., gpui::red()),
        ],
    );
    for (x, y) in [(20, 20), (599, 399), (501, 301)] {
        assert!(
            near(rgb(&image, x, y), red),
            "({x}, {y}): {:?}",
            rgb(&image, x, y)
        );
    }
    for (x, y) in [(300, 200), (498, 350), (32, 20)] {
        assert!(
            near(rgb(&image, x, y), black),
            "({x}, {y}): {:?}",
            rgb(&image, x, y)
        );
    }
    // A frame without paths after them.
    let image = draw(&mut renderer, Vec::new());
    assert!(image.pixels().all(|p| p.0 == [0, 0, 0, 255]));
}

// A gradient fill follows the path's own bounds wherever the
// intermediate sits in the frame.
#[test]
fn path_gradients_follow_the_path() {
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    let gradient = linear_gradient(
        90.,
        linear_color_stop(gpui::red(), 0.),
        linear_color_stop(gpui::blue(), 1.),
    );
    let image = draw(&mut renderer, vec![rect(300., 200., 200., 100., gradient)]);
    let ([lr, _, lb], [rr, _, rb]) = (rgb(&image, 304, 250), rgb(&image, 496, 250));
    assert!(lr > 200 && lb < 40, "left edge {:?}", rgb(&image, 304, 250));
    assert!(
        rb > 200 && rr < 40,
        "right edge {:?}",
        rgb(&image, 496, 250)
    );
}

// Icons drawn as paths in every corner of the frame, as the editor's tab
// and toolbar glyphs are.
#[test]
fn scattered_paths_land_in_place() {
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    let corners = [
        (2., 3.),
        (571., 5.),
        (7., 377.),
        (580., 381.),
        (290.5, 190.25),
    ];
    let paths = corners
        .iter()
        .map(|&(x, y)| rect(x, y, 15., 12., gpui::red()))
        .collect();
    let image = draw(&mut renderer, paths);
    for &(x, y) in &corners {
        let (cx, cy) = (x as u32 + 7, y as u32 + 6);
        assert!(
            near(rgb(&image, cx, cy), [255, 0, 0]),
            "({cx}, {cy}): {:?}",
            rgb(&image, cx, cy)
        );
        let (ox, oy) = (x as u32 + 18, y as u32 + 6);
        assert!(
            near(rgb(&image, ox, oy), [0, 0, 0]),
            "({ox}, {oy}): {:?}",
            rgb(&image, ox, oy)
        );
    }
}

// More large paths than the intermediate holds at once: they draw in
// turns, each over the ones before it.
#[test]
fn overlapping_paths_draw_in_order_past_the_intermediate() {
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    let colors = [
        gpui::red(),
        gpui::blue(),
        gpui::green(),
        gpui::red(),
        gpui::blue(),
    ];
    let paths = (colors.iter().enumerate())
        .map(|(i, &c)| rect(40. * i as f32, 20. * i as f32, 300., 300., c))
        .collect();
    let image = draw(&mut renderer, paths);
    let green = rgb(&image, 101, 51);
    let expect = |x: u32, y: u32, want: [u8; 3]| {
        assert!(
            near(rgb(&image, x, y), want),
            "({x}, {y}): {:?}",
            rgb(&image, x, y)
        );
    };
    // Each square's own corner, then where the last one covers the rest.
    expect(5, 5, [255, 0, 0]);
    expect(45, 25, [0, 0, 255]);
    expect(85, 45, green);
    expect(125, 65, [255, 0, 0]);
    expect(200, 200, [0, 0, 255]);
    expect(455, 375, [0, 0, 255]);
    expect(465, 375, [0, 0, 0]);
    assert!(
        green[1] > 100 && green[0] < 20 && green[2] < 20,
        "{green:?}"
    );
}
