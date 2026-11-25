// Poses a skinned model with one of its animations and draws it, to check
// the skeleton and animation decode by eye: one row per time (the bind pose,
// then the animation at 0, 1/4, 1/2 and 3/4 of its duration), the side view
// (model Z to the right) and the front view (model X to the right) side by
// side. Vertices are grey points, bones red lines, joints black dots.
//
//   cargo run -p anno-formats --example anim_pose -- [<mesh.rdm> <anim.rdm>] [<out.png>]
//
// Paths are inside the archives of ANNO117_DIR (else ANNO1800_DIR). The
// default is 117's big dog walking, written to anim_pose.png in the
// system's temp folder.

use std::io::Write;

use anno_formats::rdm::{Animation, JointPose, NO_PARENT, Rdm};
use anno_formats::vfs::{Install, install_dir};
use anyhow::{Context, Result};

const PANEL: usize = 360;
const DOG: &str = "data/base/graphics/library/wildlife/dog";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mesh, anim, out) = match args.as_slice() {
        [mesh, anim, rest @ ..] => (mesh.clone(), anim.clone(), rest.first().cloned()),
        rest => (
            format!("{DOG}/rdm/dog-big_01_lod0.rdm"),
            format!("{DOG}/anim/dog-big_walk_01.rdm"),
            rest.first().cloned(),
        ),
    };
    let out = out.unwrap_or_else(|| {
        std::env::temp_dir()
            .join("anim_pose.png")
            .to_string_lossy()
            .into_owned()
    });
    let dir = install_dir("ANNO117_DIR")
        .or_else(|| install_dir("ANNO1800_DIR"))
        .context("set ANNO117_DIR or ANNO1800_DIR")?;
    let install = Install::open(dir, &["data"])?;
    let m = Rdm::parse(&install.read(&mesh)?).with_context(|| mesh.clone())?;
    let a = Animation::parse(&install.read(&anim)?).with_context(|| anim.clone())?;
    let s = m.skeleton.as_ref().context("the mesh has no skeleton")?;
    println!(
        "{mesh}: {} vertices, {} joints in {} parts; {anim}: {} s, {} tracks",
        m.positions.len(),
        s.len(),
        s.part_ends.len(),
        a.duration,
        a.tracks()
    );

    // One scale for every panel: the bind pose's box, padded for motion.
    let (lo, hi) = m.bounds;
    let size = (0..3).map(|k| hi[k] - lo[k]).fold(0f32, f32::max) * 1.4;
    let center = [0, 1, 2].map(|k| (lo[k] + hi[k]) / 2.);
    let mut image = Image::new(2 * PANEL, 5 * PANEL);
    let mut pose = vec![JointPose::IDENTITY; s.len()];
    let mut skin = vec![[[0f32; 4]; 4]; s.len()];
    let mut skinned = vec![[0f32; 3]; m.positions.len()];
    for row in 0..5 {
        let t = (row as f32 - 1.) / 4. * a.duration;
        match row {
            0 => pose.copy_from_slice(&s.bind_local),
            _ => a.sample(t, s, &mut pose),
        }
        s.to_model(&mut pose);
        s.skinning_matrices(&pose, &mut skin);
        for (v, out) in skinned.iter_mut().enumerate() {
            *out = skin_vertex(&skin, m.positions[v], m.joints[v], m.weights[v]);
        }
        let label = match row {
            0 => "bind".to_string(),
            _ => format!("t={t:.2}s"),
        };
        println!("row {row}: {label}");
        // Side: model +Z to the right; front: model +X to the right.
        for (col, axis) in [(0, 2), (1, 0)] {
            let origin = (col * PANEL, row * PANEL);
            let to_px = |p: [f32; 3]| {
                let x = (p[axis] - center[axis]) / size + 0.5;
                let y = 0.5 - (p[1] - center[1]) / size;
                (
                    origin.0 as f32 + x * PANEL as f32,
                    origin.1 as f32 + y * PANEL as f32,
                )
            };
            image.frame(origin, [200, 200, 200]);
            for p in &skinned {
                let (x, y) = to_px(*p);
                image.dot(x, y, 0, [150, 150, 150]);
            }
            for j in 0..s.len() {
                let (x, y) = to_px(pose[j].translation);
                if s.parents[j] != NO_PARENT {
                    let (px, py) = to_px(pose[s.parents[j] as usize].translation);
                    image.line((px, py), (x, y), [220, 30, 30]);
                }
                image.dot(x, y, 1, [0, 0, 0]);
            }
        }
    }
    image.write_png(&out)?;
    println!("wrote {out}");
    Ok(())
}

/// The vertex at `p` moved by its weighted joints' skinning matrices.
fn skin_vertex(skin: &[[[f32; 4]; 4]], p: [f32; 3], joints: [u8; 4], weights: [u8; 4]) -> [f32; 3] {
    let sum: f32 = weights.iter().map(|&w| w as f32).sum();
    if sum == 0. {
        return p;
    }
    let mut out = [0f32; 3];
    for k in 0..4 {
        let m = &skin[joints[k] as usize];
        let w = weights[k] as f32 / sum;
        for r in 0..3 {
            out[r] += w * (m[0][r] * p[0] + m[1][r] * p[1] + m[2][r] * p[2] + m[3][r]);
        }
    }
    out
}

/// An RGB canvas with just enough drawing for a wireframe.
struct Image {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
}

impl Image {
    fn new(width: usize, height: usize) -> Image {
        Image {
            width,
            height,
            rgb: vec![255; width * height * 3],
        }
    }

    fn set(&mut self, x: i64, y: i64, color: [u8; 3]) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            let at = (y as usize * self.width + x as usize) * 3;
            self.rgb[at..at + 3].copy_from_slice(&color);
        }
    }

    fn dot(&mut self, x: f32, y: f32, radius: i64, color: [u8; 3]) {
        let (x, y) = (x.round() as i64, y.round() as i64);
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                self.set(x + dx, y + dy, color);
            }
        }
    }

    fn line(&mut self, a: (f32, f32), b: (f32, f32), color: [u8; 3]) {
        let steps = (b.0 - a.0).abs().max((b.1 - a.1).abs()).ceil().max(1.) as usize;
        for i in 0..=steps {
            let f = i as f32 / steps as f32;
            self.dot(a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f, 0, color);
        }
    }

    fn frame(&mut self, (x0, y0): (usize, usize), color: [u8; 3]) {
        for i in 0..PANEL as i64 {
            let (x0, y0) = (x0 as i64, y0 as i64);
            self.set(x0 + i, y0, color);
            self.set(x0, y0 + i, color);
        }
    }

    /// 8-bit RGB PNG: unfiltered scanlines, zlib-compressed into one IDAT.
    fn write_png(&self, path: &str) -> Result<()> {
        let mut raw = Vec::with_capacity((self.width * 3 + 1) * self.height);
        for row in self.rgb.chunks_exact(self.width * 3) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&raw)?;
        let mut ihdr = Vec::new();
        ihdr.extend((self.width as u32).to_be_bytes());
        ihdr.extend((self.height as u32).to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in [
            (b"IHDR", ihdr),
            (b"IDAT", z.finish()?),
            (b"IEND", Vec::new()),
        ] {
            png.extend((data.len() as u32).to_be_bytes());
            let mut crc = crc32fast::Hasher::new();
            crc.update(kind);
            crc.update(&data);
            png.extend(kind);
            png.extend(&data);
            png.extend(crc.finalize().to_be_bytes());
        }
        std::fs::write(path, png).with_context(|| path.to_string())
    }
}
