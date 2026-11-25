// Steps the cloth of shipped cfgs for a few seconds under wind and writes
// orthographic renders: one PNG per cfg, a column per moment (0, 0.5, 1,
// 2 and 4 s), the front (X right, Y up) above the side (Z right, Y up).
// Cloth is shaded by its normal; the rest pose is drawn in grey beneath.
//
//   ANNO117_DIR=… cargo run --release -p anno-formats --example cloth_sim -- \
//       [OUT_DIR] [CFG …]

use std::io::Write;

use anno_formats::cloth::{ClothMesh, ClothState, clothes, colliders};
use anno_formats::model::Transform;
use anno_formats::rdm::Rdm;
use anno_formats::vfs::Install;
use anno_formats::xml::Doc;

const DEFAULT_CFGS: &[&str] = &[
    "data/base/graphics/library/props/decoration/banner/banner_01.cfg",
    "data/base/graphics/library/modular_level_kits/caeso_island/sunsail_shape01.cfg",
    "data/base/graphics/celtic/units/ships/ship_01/ship_01.cfg",
    "data/base/graphics/library/props/decoration/cloth/cloth_line_01.cfg",
];
const MOMENTS: [f32; 5] = [0., 0.5, 1., 2., 4.];
const FRAME: f32 = 1. / 30.;
const VIEW: usize = 220;
// The ambient preset's WindStrength in 76 of 117's 101 presets, blowing
// mostly toward +Z.
const WIND: [f32; 3] = [0.5, 0., 0.99];

fn main() -> anyhow::Result<()> {
    let dir = std::env::var_os("ANNO117_DIR").expect("ANNO117_DIR");
    let install = Install::open(dir, &["data"])?;
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| {
        std::env::temp_dir()
            .join("anno-cloth")
            .to_string_lossy()
            .into_owned()
    });
    let mut cfgs: Vec<String> = args.collect();
    if cfgs.is_empty() {
        cfgs = DEFAULT_CFGS.iter().map(|s| s.to_string()).collect();
    }
    for cfg in &cfgs {
        let doc = Doc::parse(&install.read(cfg)?);
        let all = colliders(&doc);
        // Each cloth of the cfg with its own mesh and state.
        let mut sims = Vec::new();
        for c in clothes(&doc) {
            let Ok(bytes) = install.read(&c.mesh) else {
                eprintln!("{cfg}: {} does not ship", c.mesh);
                continue;
            };
            let mesh = ClothMesh::build(&Rdm::parse(&bytes)?);
            let state = ClothState::new(&mesh, &c, &all);
            let rest = state.positions().to_vec();
            println!(
                "{cfg}: {} {} particles, {} pinned, {} edges, {} rope segments, gravity {}, wind {}",
                c.name,
                mesh.particle_count(),
                mesh.pinned().len(),
                mesh.edges().len(),
                mesh.lines.len(),
                c.gravity,
                c.wind_strength
            );
            sims.push((c.transform, mesh, rest, state));
        }
        if sims.is_empty() {
            continue;
        }
        // Frame the rest pose with room to swing.
        let rest: Vec<[f32; 3]> = (sims.iter())
            .flat_map(|(t, _, rest, _)| rest.iter().map(move |p| t.apply(*p)))
            .collect();
        let (lo, hi) = bounds(&rest);
        let size = (0..3).map(|k| hi[k] - lo[k]).fold(0f32, f32::max) * 1.6 + 0.05;
        let center = [0, 1, 2].map(|k| 0.5 * (lo[k] + hi[k]));

        let (w, h) = (VIEW * MOMENTS.len(), VIEW * 2);
        let mut image = Image::new(w, h);
        let mut t = 0.;
        for (column, &moment) in MOMENTS.iter().enumerate() {
            while t + 1e-4 < moment {
                for (_, mesh, _, state) in &mut sims {
                    state.step(mesh, FRAME, WIND);
                }
                t += FRAME;
            }
            for (row, axes) in [[0, 1, 2], [2, 1, 0]].into_iter().enumerate() {
                let view = View {
                    x0: column * VIEW,
                    y0: row * VIEW,
                    axes,
                    center,
                    size,
                };
                image.depth.fill(f32::INFINITY);
                for (at, mesh, rest, state) in &sims {
                    // The rest pose first, flat grey, then the cloth.
                    draw(&mut image, &view, at, mesh, rest, None);
                    draw(
                        &mut image,
                        &view,
                        at,
                        mesh,
                        state.positions(),
                        Some(state.normals()),
                    );
                }
            }
        }
        let stem = cfg.rsplit('/').next().unwrap().trim_end_matches(".cfg");
        let path = format!("{out}/cloth_{stem}.png");
        image.write_png(&path)?;
        println!("wrote {path}");
    }
    Ok(())
}

struct View {
    x0: usize,
    y0: usize,
    /// Model axes on screen right, up and depth.
    axes: [usize; 3],
    center: [f32; 3],
    size: f32,
}

impl View {
    fn project(&self, p: [f32; 3]) -> [f32; 3] {
        let s = VIEW as f32 / self.size;
        let [a, b, c] = self.axes;
        [
            self.x0 as f32 + VIEW as f32 / 2. + (p[a] - self.center[a]) * s,
            self.y0 as f32 + VIEW as f32 / 2. - (p[b] - self.center[b]) * s,
            -(p[c] - self.center[c]),
        ]
    }
}

struct Image {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
    depth: Vec<f32>,
}

impl Image {
    fn new(w: usize, h: usize) -> Image {
        let mut rgb = vec![255; w * h * 3];
        // Thin separators between the views.
        for y in 0..h {
            for x in 0..w {
                if x % VIEW == 0 || y % VIEW == 0 {
                    rgb[(y * w + x) * 3..][..3].copy_from_slice(&[200, 200, 200]);
                }
            }
        }
        Image {
            w,
            h,
            rgb,
            depth: vec![f32::INFINITY; w * h],
        }
    }

    fn write_png(&self, path: &str) -> anyhow::Result<()> {
        let mut raw = Vec::with_capacity((self.w * 3 + 1) * self.h);
        for row in self.rgb.chunks_exact(self.w * 3) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&raw)?;
        let mut ihdr = Vec::new();
        ihdr.extend((self.w as u32).to_be_bytes());
        ihdr.extend((self.h as u32).to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in [
            (b"IHDR", ihdr),
            (b"IDAT", z.finish()?),
            (b"IEND", Vec::new()),
        ] {
            png.extend((data.len() as u32).to_be_bytes());
            let start = png.len();
            png.extend(kind);
            png.extend(&data);
            let crc = crc32fast::hash(&png[start..]);
            png.extend(crc.to_be_bytes());
        }
        std::fs::write(path, png)?;
        Ok(())
    }
}

// Rasterises the cloth's triangles (shaded by `normals`, or flat grey) and
// its rope segments, depth-tested.
fn draw(
    image: &mut Image,
    view: &View,
    at: &Transform,
    mesh: &ClothMesh,
    pos: &[[f32; 3]],
    normals: Option<&[[f32; 3]]>,
) {
    let screen: Vec<[f32; 3]> = pos.iter().map(|p| view.project(at.apply(*p))).collect();
    let light = normalize([0.4, 0.8, -0.45]);
    for t in mesh.triangles.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|i| screen[i as usize]);
        let color = match normals {
            None => [215, 215, 215],
            Some(n) => {
                let n = t.map(|i| n[i as usize]);
                let avg = normalize([0, 1, 2].map(|k| n[0][k] + n[1][k] + n[2][k]));
                let lit = 0.35 + 0.65 * dot(avg, light).abs();
                [(200. * lit) as u8, (90. * lit) as u8, (60. * lit) as u8]
            }
        };
        fill(image, view, a, b, c, color);
    }
    for &[i, j] in &mesh.lines {
        let (a, b) = (screen[i as usize], screen[j as usize]);
        let steps = ((b[0] - a[0]).abs().max((b[1] - a[1]).abs()) as usize).max(1);
        for s in 0..=steps {
            let f = s as f32 / steps as f32;
            let p = [0, 1, 2].map(|k| a[k] + (b[k] - a[k]) * f);
            plot(image, view, p, [40, 40, 40]);
        }
    }
}

fn fill(image: &mut Image, view: &View, a: [f32; 3], b: [f32; 3], c: [f32; 3], color: [u8; 3]) {
    let area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
    if area.abs() < 1e-6 {
        return;
    }
    let x0 = a[0].min(b[0]).min(c[0]).floor().max(view.x0 as f32) as usize;
    let x1 = a[0]
        .max(b[0])
        .max(c[0])
        .ceil()
        .min((view.x0 + VIEW - 1) as f32);
    let y0 = a[1].min(b[1]).min(c[1]).floor().max(view.y0 as f32) as usize;
    let y1 = a[1]
        .max(b[1])
        .max(c[1])
        .ceil()
        .min((view.y0 + VIEW - 1) as f32);
    if x1 < 0. || y1 < 0. {
        return;
    }
    for y in y0..=y1 as usize {
        for x in x0..=x1 as usize {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let w0 = ((b[0] - px) * (c[1] - py) - (b[1] - py) * (c[0] - px)) / area;
            let w1 = ((c[0] - px) * (a[1] - py) - (c[1] - py) * (a[0] - px)) / area;
            let w2 = 1. - w0 - w1;
            if w0 < 0. || w1 < 0. || w2 < 0. {
                continue;
            }
            plot(
                image,
                view,
                [px, py, w0 * a[2] + w1 * b[2] + w2 * c[2]],
                color,
            );
        }
    }
}

fn plot(image: &mut Image, view: &View, p: [f32; 3], color: [u8; 3]) {
    let (x, y) = (p[0] as isize, p[1] as isize);
    let inside = |v: isize, lo: usize| v >= lo as isize && v < (lo + VIEW) as isize;
    if !inside(x, view.x0) || !inside(y, view.y0) {
        return;
    }
    let i = y as usize * image.w + x as usize;
    if p[2] <= image.depth[i] {
        image.depth[i] = p[2];
        image.rgb[i * 3..][..3].copy_from_slice(&color);
    }
}

fn bounds(points: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for p in points {
        for k in 0..3 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    (lo, hi)
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = dot(v, v).sqrt().max(1e-12);
    v.map(|c| c / l)
}
