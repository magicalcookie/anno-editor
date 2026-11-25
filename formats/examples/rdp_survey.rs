// Counts over every shipped `.rdp` of 117 and 1800: materials, flags,
// flipbooks, lines, sizes and the files whose particles are not grouped by
// material. With ANNO2070_DIR set, also how many `.efc` 2070 ships and the
// head of one. The numbers in docs/particle-format.md come from here.
//
//   ANNO117_DIR=… ANNO1800_DIR=… cargo run --release -p anno-formats --example rdp_survey

use std::collections::BTreeMap;

use anno_formats::rdp::Rdp;
use anno_formats::vfs::{FileId, Install};

fn main() {
    for var in ["ANNO117_DIR", "ANNO1800_DIR"] {
        let Some(dir) = std::env::var_os(var) else {
            continue;
        };
        let install = Install::open(dir, &["data"]).unwrap();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut add = |k: String| *counts.entry(k).or_default() += 1;
        let (mut files, mut most_particles, mut most_ticks) = (0, (0, String::new()), 0);
        let mut unsorted = Vec::new();
        for i in 0..install.file_count() {
            let id = FileId(i as u32);
            if !install.file_name(id).ends_with(".rdp") {
                continue;
            }
            let path = install.file_path(id);
            let rdp = Rdp::parse(&install.read_file(id).unwrap()).unwrap();
            files += 1;
            if rdp.starts.len() > most_particles.0 {
                most_particles = (rdp.starts.len(), path.clone());
            }
            most_ticks = most_ticks.max(rdp.positions.len());
            add(format!("file: materials {}", rdp.materials.len()));
            add(format!("file: looped {}", rdp.looped));
            add(format!("file: invincible {}", rdp.invincible));
            add(format!("file: lighting {}", rdp.lighting));
            add(format!("file: self occlusion {}", rdp.self_occlusion));
            add(format!("file: do sort {}", rdp.do_sort));
            add(format!("file: atlas {}", !rdp.texture_path.is_empty()));
            add(format!(
                "file: occlusion ticks {}",
                !rdp.occlusion.is_empty()
            ));
            add(format!("file: zero extent {}", rdp.extents.contains(&0.)));
            add(format!(
                "file: frames {}",
                match rdp.frame_count {
                    0..=63 => "<64",
                    64..=127 => "64..127",
                    128..=255 => "128..255",
                    _ => "256",
                }
            ));
            let wraps = (0..rdp.starts.len()).any(|p| rdp.ends[p] < rdp.starts[p]);
            add(format!("file: particles across the seam {wraps}"));
            if !rdp.particle_materials.is_sorted() {
                unsorted.push(path.clone());
            }
            for m in &rdp.materials {
                add(format!("material: view aligned {}", m.view_aligned));
                add(format!("material: line type {}", m.line_type));
                add(format!(
                    "material: movie fps {}",
                    match m.movie_fps {
                        -1. => "-1 (atlas)",
                        0. => "0 (once a life)",
                        v if v < 0. => "other negative",
                        _ => "> 0 (looping)",
                    }
                ));
                for (flag, on) in [
                    ("additive", m.additive),
                    ("refractive", m.refractive),
                    ("wave", m.wave_particle),
                    ("churn", m.churn_particle),
                    ("opaque with depth", m.opaque_with_depth),
                    ("complex lighting", m.complex_lighting),
                    ("distance fade", m.distance_fade),
                    ("terrain adapt", m.terrain_adapt),
                    ("no view angle fade", m.disable_view_angle_fade),
                    ("color boost", m.color_boost != 0),
                    ("center offset", m.center_offset != [0., 0.]),
                    ("crop", m.crop != [0., 0., 1., 1.]),
                    ("movie", !m.movie_tex_path.is_empty()),
                    ("motion vectors", !m.movie_motion_path.is_empty()),
                    ("normal flipbook", !m.movie_norm_path.is_empty()),
                    ("six-way lightmaps", !m.lightmap_rtbk_path.is_empty()),
                    ("albedo ramp", !m.albedo_ramp_path.is_empty()),
                    ("emissive ramp", !m.emissive_ramp_path.is_empty()),
                ] {
                    if on {
                        add(format!("material: {flag}"));
                    }
                }
            }
        }
        println!("{var}: {files} effects");
        for (k, n) in &counts {
            println!("  {n:5} {k}");
        }
        println!(
            "  most particles {} ({}), most ticks {most_ticks}",
            most_particles.0, most_particles.1
        );
        println!("  particles not grouped by material: {}", unsorted.len());
        for path in &unsorted {
            println!("    {path}");
        }
    }
    if let Some(dir) = std::env::var_os("ANNO2070_DIR") {
        let families = ["data", "eng", "patch", "addon", "addon_eng", "exedata"];
        let install = Install::open(dir, &families).unwrap();
        let efc: Vec<FileId> = (0..install.file_count())
            .map(|i| FileId(i as u32))
            .filter(|&id| install.file_name(id).ends_with(".efc"))
            .collect();
        println!("ANNO2070_DIR: {} .efc", efc.len());
        if let Some(&id) = efc.first() {
            let bytes = install.read_file(id).unwrap();
            let head = &bytes[..bytes.len().min(240)];
            println!(
                "  {}: {:?}",
                install.file_path(id),
                String::from_utf8_lossy(head)
            );
        }
    }
}
