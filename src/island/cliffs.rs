// Auto cliffs: dresses steep ground with the region's cliff pieces and big
// rocks. Placement follows the 117 cliff census and a matching survey of
// 1800's model families.
use super::doc;
use anno_core::game::{CliffClass, CliffSet, IslandRules};
use anno_formats::{model::Model, vfs::Install};
use anyhow::{Result, ensure};
use gpui_editor_kit::{
    heightfield::Heightfield,
    math::{self, Mat4, Vec3},
};
use std::collections::HashMap;

/// Ground steeper than this is a face. Shipped islands have 17–35 % of
/// their land over it, and only there does rock coverage pass ~35 %.
pub const STEEP: f32 = 35.;
/// Candidates must stand this high above sea: retail cliffs dip into the
/// surf, but a piece rooted below sea level reads as a reef in the editor.
const MIN_GROUND: f32 = 0.5;
/// Candidate spacing, metres. Faces are found on this lattice; the spacing
/// rule decides what is kept.
const STEP: f32 = 2.;

/// `region`'s cliff set in `rules` (`IslandRules::cliffs`), matched ignoring
/// case; the game's catch-all entry (`""`) for other regions, if it has one.
fn cliff_set(rules: &IslandRules, region: &str) -> Option<&'static CliffSet> {
    let cliffs = rules.cliffs;
    (cliffs.iter())
        .find(|(r, _)| r.eq_ignore_ascii_case(region))
        .or_else(|| cliffs.iter().find(|(r, _)| r.is_empty()))
        .map(|&(_, set)| set)
}

/// A model the tool places, with its cfg box (`MeshCenter ± MeshExtent`).
#[derive(Clone, Debug)]
pub struct Shape {
    pub cfg: String,
    pub class: CliffClass,
    weight: u32,
    pub center: Vec3,
    pub extent: Vec3,
}

/// The selected game's regional cliff and rock models.
pub struct Kit {
    pub shapes: Vec<Shape>,
    cliff_relief: f32,
    spacing: f32,
}

impl Kit {
    pub fn load(rules: &IslandRules, install: &Install, region: &str) -> Result<Kit> {
        let Some(set) = cliff_set(rules, region) else {
            anyhow::bail!("no cliff set for {region}");
        };
        let mut shapes = Vec::new();
        for &(cfg, class, weight) in set.pieces {
            match Model::load(install, cfg) {
                Ok(m) => shapes.push(Shape {
                    cfg: cfg.to_owned(),
                    class,
                    weight,
                    center: m.mesh_center,
                    extent: m.mesh_extent,
                }),
                Err(e) => eprintln!("auto cliffs: {cfg}: {e:#}"),
            }
        }
        ensure!(
            !shapes.is_empty(),
            "no {region} cliff models in the install"
        );
        Ok(Kit {
            shapes,
            cliff_relief: set.cliff_relief,
            spacing: set.spacing,
        })
    }

    /// Weighted pick within `class`, falling back to any shape.
    fn pick(&self, class: CliffClass, roll: f32) -> usize {
        let of = |c: Option<CliffClass>| {
            self.shapes
                .iter()
                .enumerate()
                .filter(move |(_, s)| c.is_none_or(|c| s.class == c))
        };
        let mut options: Vec<_> = of(Some(class)).collect();
        if options.is_empty() {
            options = of(None).collect();
        }
        let total: u32 = options.iter().map(|(_, s)| s.weight).sum();
        let mut left = roll * total as f32;
        for (i, s) in &options {
            left -= s.weight as f32;
            if left < 0. {
                return *i;
            }
        }
        options.last().unwrap().0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// 1 keeps the retail spacing; 2 packs pieces twice as close.
    pub density: f32,
    /// Remove earlier auto cliffs before placing.
    pub replace: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            density: 1.,
            replace: true,
        }
    }
}

/// One planned piece: `shape` indexes [`Kit::shapes`].
#[derive(Clone, Debug)]
pub struct Placement {
    pub shape: usize,
    pub position: Vec3,
    /// Stored (x, y, z, w) quaternion.
    pub rotation: [f32; 4],
    pub scale: f32,
}

/// Something already on the ground that new pieces keep their distance
/// from: horizontal centre and footprint width.
#[derive(Clone, Copy, Debug)]
pub struct Obstacle {
    pub at: [f32; 2],
    pub width: f32,
}

impl Obstacle {
    /// The footprint of `shape` placed by `m` (a [`doc::trs`] matrix).
    pub fn of(shape: &Shape, m: &Mat4) -> Obstacle {
        let col = |i: usize| [m[i * 4], m[i * 4 + 1], m[i * 4 + 2]];
        let c = shape.center;
        Obstacle {
            at: [
                m[12] + col(0)[0] * c[0] + col(2)[0] * c[2],
                m[14] + col(0)[2] * c[0] + col(2)[2] * c[2],
            ],
            width: width(shape, math::length(col(0))),
        }
    }
}

/// Cheap reproducible noise in [0, 1) per lattice point and channel.
fn roll(x: i32, z: i32, salt: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x9e37_79b1)
        ^ (z as u32).wrapping_mul(0x85eb_ca77)
        ^ salt.wrapping_mul(0xc2b2_ae3d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    (h >> 8) as f32 / (1 << 24) as f32
}

fn gradient(field: &Heightfield, x: f32, z: f32) -> [f32; 2] {
    let h = |x: f32, z: f32| field.height([x, z]);
    [
        (h(x + 1., z) - h(x - 1., z)) / 2.,
        (h(x, z + 1.) - h(x, z - 1.)) / 2.,
    ]
}

fn slope_deg(g: [f32; 2]) -> f32 {
    g[0].hypot(g[1]).atan().to_degrees()
}

/// Footprint width of a placed shape: the larger horizontal box side.
pub fn width(shape: &Shape, scale: f32) -> f32 {
    shape.extent[0].max(shape.extent[2]) * 2. * scale
}

/// Plans pieces along every face steeper than [`STEEP`].
///
/// Shipped placement, as measured: walls turn local −Z downhill (≈90 %
/// within ±45°) and lean back into the slope by about a third of it; big
/// rocks are oriented loosely. Origins sit ~18 % of the model height under
/// the ground at the origin. Pieces overlap into one face; spacing is tuned
/// so a face ends up with the retail share under rock.
pub fn plan(
    field: &Heightfield,
    kit: &Kit,
    settings: &Settings,
    obstacles: &[Obstacle],
) -> Vec<Placement> {
    let size = field.grid.size();
    let (nx, nz) = ((size[0] / STEP) as i32, (size[1] / STEP) as i32);
    // (lattice x, z, relief): steep and above sea.
    let mut faces = Vec::new();
    for iz in 1..nz {
        for ix in 1..nx {
            let (x, z) = (ix as f32 * STEP, iz as f32 * STEP);
            if field.height([x, z]) < MIN_GROUND {
                continue;
            }
            let g = gradient(field, x, z);
            if slope_deg(g) <= STEEP {
                continue;
            }
            let up = math::scale(g, 1. / g[0].hypot(g[1]));
            let relief = field.height([x + up[0] * 8., z + up[1] * 8.])
                - field.height([x - up[0] * 8., z - up[1] * 8.]);
            faces.push((ix, iz, relief));
        }
    }
    // Tallest faces first so walls claim them before rocks fill the rest.
    faces.sort_by(|a, b| b.2.total_cmp(&a.2).then((a.1, a.0).cmp(&(b.1, b.0))));

    // Spatial buckets of placed and existing footprints.
    const BUCKET: f32 = 16.;
    let key = |p: [f32; 2]| ((p[0] / BUCKET) as i32, (p[1] / BUCKET) as i32);
    let mut buckets: HashMap<(i32, i32), Vec<Obstacle>> = HashMap::new();
    let mut widest = 0f32;
    for o in obstacles {
        buckets.entry(key(o.at)).or_default().push(*o);
        widest = widest.max(o.width);
    }
    let density = settings.density.clamp(0.25, 4.);
    let mut out = Vec::new();
    for (ix, iz, relief) in faces {
        let (x, z) = (ix as f32 * STEP, iz as f32 * STEP);
        let class = if relief >= kit.cliff_relief {
            CliffClass::Cliff
        } else {
            CliffClass::Boulder
        };
        let index = kit.pick(class, roll(ix, iz, 1));
        let shape = &kit.shapes[index];
        let wall = shape.class == CliffClass::Cliff;
        // Retail scale p10–p90: walls 0.55–1.25 (wide `_b` pieces ~0.8),
        // big rocks 0.9–1.3.
        let scale = if wall {
            0.7 + 0.5 * roll(ix, iz, 2)
        } else {
            0.85 + 0.45 * roll(ix, iz, 2)
        };
        let g = gradient(field, x, z);
        let slope = slope_deg(g);
        let up = math::scale(g, 1. / g[0].hypot(g[1]));
        // Local +Z uphill, jittered: walls ±15°, rocks ±60°.
        let jitter = (roll(ix, iz, 3) - 0.5) * if wall { 30f32 } else { 120. }.to_radians();
        let yaw = up[0].atan2(up[1]) + jitter;
        let zh = [yaw.sin(), 0., yaw.cos()];
        let lean = (slope * if wall { 0.35 } else { 0.2 })
            .min(30.)
            .to_radians();
        let yaxis = [zh[0] * lean.sin(), lean.cos(), zh[2] * lean.sin()];
        let zaxis = [zh[0] * lean.cos(), -lean.sin(), zh[2] * lean.cos()];
        let xaxis = math::cross(yaxis, zaxis);

        // Box centre on the ground plane decides spacing.
        let c = shape.center;
        let centre = [
            x + (xaxis[0] * c[0] + zaxis[0] * c[2]) * scale,
            z + (xaxis[2] * c[0] + zaxis[2] * c[2]) * scale,
        ];
        let w = width(shape, scale);
        // Retail neighbours stand 0.4 (walls) and 0.6 (rocks) widths
        // apart, but that counts loose pebbles and ragged faces. On a clean
        // sculpted face these gaps give the retail coverage (~58 % of
        // steep ground on the unit test's 45° face; shipped 33–80 %).
        let gap = if wall { 1.1 } else { 1.3 } * kit.spacing / density;
        let reach = ((w.max(widest) * gap) / BUCKET).ceil() as i32 + 1;
        let (bx, bz) = key(centre);
        let crowded = (-reach..=reach).any(|dz| {
            (-reach..=reach).any(|dx| {
                buckets.get(&(bx + dx, bz + dz)).is_some_and(|list| {
                    list.iter().any(|o| {
                        let d = (o.at[0] - centre[0]).hypot(o.at[1] - centre[1]);
                        d < gap * (w + o.width) / 2.
                    })
                })
            })
        });
        if crowded {
            continue;
        }

        let ground = field.height([x, z]);
        let height = shape.extent[1] * 2. * scale;
        let mut y = ground - 0.18 * height;
        // No floating: the box bottom goes under the lowest ground of its
        // footprint. The origin is near the downhill edge, so the front
        // corners matter most.
        let (e, bottom) = (shape.extent, (c[1] - shape.extent[1]) * scale);
        let mut low = ground;
        for (sx, sz) in [(-1., -1.), (1., -1.), (-1., 1.), (1., 1.), (0., -1.)] {
            let (lx, lz) = ((c[0] + sx * e[0]) * scale, (c[2] + sz * e[2]) * scale);
            let p = [
                x + xaxis[0] * lx + zaxis[0] * lz,
                z + xaxis[2] * lx + zaxis[2] * lz,
            ];
            low = low.min(field.height(p));
        }
        y = y.min(low - bottom * lean.cos() - 0.3);

        let m: Mat4 = [
            xaxis[0], xaxis[1], xaxis[2], 0., yaxis[0], yaxis[1], yaxis[2], 0., zaxis[0], zaxis[1],
            zaxis[2], 0., x, y, z, 1.,
        ];
        let (_, rotation, _) = doc::decompose(&m);
        let o = Obstacle {
            at: centre,
            width: w,
        };
        buckets.entry(key(centre)).or_default().push(o);
        widest = widest.max(w);
        out.push(Placement {
            shape: index,
            position: [x, y, z],
            rotation,
            scale,
        });
    }
    out
}

/// Share of steep land (1 m cells over [`STEEP`], above sea) whose ground
/// point lies inside one of the boxes: `matrix` places a `(center,
/// extent)` model box. The census measures shipped islands the same way.
pub fn coverage(field: &Heightfield, boxes: &[(Mat4, Vec3, Vec3)]) -> f32 {
    let size = field.grid.size();
    let (w, d) = (size[0] as usize, size[1] as usize);
    let mut covered = vec![false; w * d];
    for (m, c, e) in boxes {
        let t = math::translation(m);
        let cols = [0, 1, 2].map(|i| [m[i * 4], m[i * 4 + 1], m[i * 4 + 2]]);
        let reach = ((c[0].abs() + e[0]).hypot(c[2].abs() + e[2])
            * cols.iter().map(|c| math::length(*c)).fold(0., f32::max))
        .ceil() as i32
            + 1;
        for dz in -reach..=reach {
            for dx in -reach..=reach {
                let (cx, cz) = (t[0].floor() as i32 + dx, t[2].floor() as i32 + dz);
                if cx < 0 || cz < 0 || cx as usize >= w || cz as usize >= d {
                    continue;
                }
                let p = [cx as f32 + 0.5, 0., cz as f32 + 0.5];
                let p = [p[0], field.height([p[0], p[2]]), p[2]];
                let v = [p[0] - t[0], p[1] - t[1], p[2] - t[2]];
                // Local coordinate along column i: (col · v) / |col|².
                let inside = (0..3).all(|i| {
                    let l2 = math::dot(cols[i], cols[i]);
                    let local = math::dot(cols[i], v) / l2;
                    (local - c[i]).abs() <= e[i] + 0.5 / l2.sqrt()
                });
                if inside {
                    covered[cz as usize * w + cx as usize] = true;
                }
            }
        }
    }
    let (mut steep, mut hit) = (0usize, 0usize);
    for z in 0..d {
        for x in 0..w {
            let (px, pz) = (x as f32 + 0.5, z as f32 + 0.5);
            if field.height([px, pz]) <= MIN_GROUND || slope_deg(gradient(field, px, pz)) <= STEEP {
                continue;
            }
            steep += 1;
            hit += usize::from(covered[z * w + x]);
        }
    }
    hit as f32 / steep.max(1) as f32
}

/// Whether a prop cfg is a cliff piece or rock (the census's rule).
pub fn is_rock(cfg: &str) -> bool {
    let p = cfg.to_ascii_lowercase().replace('\\', "/");
    (p.contains("cliff") || p.contains("/rocks/")) && !p.contains("decal")
}

/// Ground under cliff pieces is painted only this steep: the pieces stand
/// on faces over 35°, and their footprints reach onto the plateau above and
/// the beach below, which keep their own ground.
pub const PAINT_SLOPE: f32 = 25.;
/// How far past a piece's footprint the paint reaches, metres.
pub const PAINT_MARGIN: f32 = 1.5;

/// The palette material for ground under cliffs: what retail puts on
/// steep faces (`cliff_limestone_01` roman, `celtic_cliffs_05` celtic),
/// else coastal rock. Same names as the World Machine template's cliff
/// and rock channels.
pub fn ground_material(rules: &IslandRules, materials: &[(u8, String)]) -> Option<u8> {
    let channels = super::worldmachine::template::channel_materials(&rules.ground_names, materials);
    channels[0][0].or(channels[0][2])
}

/// Dab centres, one per metre, inside a piece's footprint (plus the
/// margin) where the ground is steeper than [`PAINT_SLOPE`].
pub fn paint_spots(field: &Heightfield, center: [f32; 2], radius: f32) -> Vec<[f32; 2]> {
    let r = radius.ceil() as i32;
    let mut out = Vec::new();
    for dz in -r..=r {
        for dx in -r..=r {
            let (x, z) = (center[0] + dx as f32, center[1] + dz as f32);
            if (dx * dx + dz * dz) as f32 > radius * radius {
                continue;
            }
            if slope_deg(gradient(field, x, z)) > PAINT_SLOPE && field.height([x, z]) >= MIN_GROUND
            {
                out.push([x, z]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{CliffClass, Kit, STEEP, Settings, Shape, coverage, plan};
    use gpui_editor_kit::heightfield::{Grid, Heightfield};

    /// 160 m square: flat 4 m plateau, a 45° face rising to 34 m along
    /// z = 80..110, a flat top behind it.
    fn face() -> Heightfield {
        let grid = Grid {
            width: 321,
            depth: 321,
            spacing: 0.5,
            origin: [0., 0.],
        };
        let heights = (0..321 * 321)
            .map(|i| {
                let z = (i / 321) as f32 * 0.5;
                4. + (z - 80.).clamp(0., 30.)
            })
            .collect();
        Heightfield::new(grid, heights)
    }

    fn kit() -> Kit {
        let shape = |class, extent: [f32; 3], center: [f32; 3]| Shape {
            cfg: String::new(),
            class,
            weight: 1,
            center,
            extent,
        };
        Kit {
            // Box sizes of roman_cliff_01_h_noskirt and roman_rock_02_b.
            shapes: vec![
                shape(CliffClass::Cliff, [6.6, 8.1, 6.], [0., 6., 4.]),
                shape(CliffClass::Boulder, [6., 5.1, 4.], [0., 2., 1.]),
            ],
            cliff_relief: 8.,
            spacing: 1.,
        }
    }

    #[test]
    fn pieces_follow_the_face_facing_downhill() {
        let field = face();
        let kit = kit();
        let placed = plan(&field, &kit, &Settings::default(), &[]);
        assert!(!placed.is_empty());
        for p in &placed {
            let [x, y, z] = p.position;
            assert!((79. ..=111.).contains(&z), "piece off the face at z {z}");
            assert!(y < field.height([x, z]), "origin above ground");
            // Local +Z is the uphill (+z) direction, give or take jitter.
            let [qx, qy, qz, qw] = p.rotation;
            let zz = 1. - 2. * (qx * qx + qy * qy);
            let zx = 2. * (qx * qz + qy * qw);
            assert!(zz > 0.3, "piece faces sideways: z axis ({zx}, {zz})");
        }
        let boxes: Vec<_> = placed
            .iter()
            .map(|p| {
                let s = &kit.shapes[p.shape];
                (
                    super::doc::trs(p.position, p.rotation, [p.scale; 3]),
                    s.center,
                    s.extent,
                )
            })
            .collect();
        let share = coverage(&field, &boxes);
        assert!(
            (0.33..=0.8).contains(&share),
            "covers {share} over {STEEP}°"
        );
        // Twice the density packs more pieces; a rerun next to its own
        // output adds nothing.
        let dense = plan(
            &field,
            &kit,
            &Settings {
                density: 2.,
                ..Settings::default()
            },
            &[],
        );
        assert!(dense.len() > placed.len());
        let existing: Vec<_> = placed
            .iter()
            .zip(&boxes)
            .map(|(p, (m, _, _))| super::Obstacle::of(&kit.shapes[p.shape], m))
            .collect();
        let again = plan(&field, &kit, &Settings::default(), &existing);
        assert!(
            again.len() * 4 < placed.len(),
            "{} more on top of {}",
            again.len(),
            placed.len()
        );
    }
}
