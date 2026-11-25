// Models laid along work areas' paths (1800's mine rails and river
// channels, `WorkAreaPath/AreaPathConfig`), bent the way the engine bends
// models along Bezier paths. Path rules from Anno 117's
// BezierPath class (build 26043928; the 1800 engine shares it):
//
// - Tangents are rebuilt from the points (`sub_14227E690`): an inner point's
//   handles run along `next - prev`, `i` scaled to `TangentScale` times the
//   chord before it and `o` to the chord after it, each no longer than
//   `|next - prev|`; an end point's `o` is `TangentScale * (next - p)` and
//   `i = -o`. `TangentScale` defaults to 1/6 (117's paths); 1800's work
//   areas store 0.3.
// - The curve is sampled by halving each cubic (de Casteljau) while all three
//   legs of its control polygon are longer than `SegmentMinimum` (0.1 m)
//   and its end directions differ from the chord by `SegmentThreshold`
//   (0.001) or more (`sub_14227FA80`); the control points of the pieces are
//   the samples, distance along the path is 3D (`sub_14227EBB0`).
// - Along the path, position is linear between samples and direction
//   blends from one sample leg to the next (`sub_14227F480`); what follows
//   the path turns about up only, its up stays world up (`sub_142D3D900`,
//   and 117's spline shader `BezierSplineWorldMatrix`). So a bent rail's
//   track follows the curve while its legs stay plumb.
//
// How many copies of the model the engine lays along a path is in 1800's
// renderer, which we have not read: here `round(length / model length)`
// copies stretched to fit.
use anno_core::island::objects::PathPoint;
use gpui_editor_kit::{
    gpu::{Mesh, MeshData},
    math::{self, Mat4, Vec3},
};
use std::sync::Arc;

/// `SegmentMinimum` and `SegmentThreshold` defaults; work-area records
/// store neither.
const SEGMENT_MINIMUM: f32 = 0.1;
const SEGMENT_THRESHOLD: f32 = 0.001;
/// Halvings per span at most, against degenerate control points.
const MAX_DEPTH: u32 = 16;

/// A path sampled as the engine samples it, with the 3D distance to each
/// sample.
pub struct Samples {
    at: Vec<Vec3>,
    run: Vec<f32>,
}

impl Samples {
    /// Samples of `path`'s cubic spans (`P0, P0 + o, P1 + i, P1`).
    pub fn new(path: &[PathPoint]) -> Samples {
        let mut at: Vec<Vec3> = Vec::new();
        let push = |p: Vec3, at: &mut Vec<Vec3>| {
            if at.last() != Some(&p) {
                at.push(p);
            }
        };
        if let [only] = path {
            push(only.p, &mut at);
        }
        for w in path.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            let c = [
                a.p,
                math::add(a.p, a.o.unwrap_or_default()),
                math::add(b.p, b.i.unwrap_or_default()),
                b.p,
            ];
            split(c, 0, &mut |q| {
                for p in q {
                    push(p, &mut at);
                }
            });
        }
        let mut run = Vec::with_capacity(at.len());
        for (k, &p) in at.iter().enumerate() {
            let before = k
                .checked_sub(1)
                .map_or(0., |j| run[j] + math::length(math::sub(p, at[j])));
            run.push(before);
        }
        Samples { at, run }
    }

    pub fn length(&self) -> f32 {
        self.run.last().copied().unwrap_or(0.)
    }

    /// Position and horizontal unit direction at distance `d` along.
    pub fn frame(&self, d: f32) -> (Vec3, [f32; 2]) {
        let n = self.at.len();
        if n < 2 {
            return (self.at.first().copied().unwrap_or_default(), [1., 0.]);
        }
        let k = (self.run.partition_point(|&r| r <= d).max(1) - 1).min(n - 2);
        let (r0, r1) = (self.run[k], self.run[k + 1]);
        let t = if r1 > r0 {
            ((d - r0) / (r1 - r0)).clamp(0., 1.)
        } else {
            0.
        };
        let p = std::array::from_fn(|x| self.at[k][x] + (self.at[k + 1][x] - self.at[k][x]) * t);
        let leg = |k: usize| {
            let k = k.min(n - 2);
            let d = math::sub(self.at[k + 1], self.at[k]);
            [d[0], d[2]]
        };
        // Blends into the next leg's direction across this one.
        let (a, b) = (leg(k), leg(k + 1));
        let mut dir = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        let l = dir[0].hypot(dir[1]);
        if l < 1e-6 {
            // Straight up or down: the nearest leg that runs somewhere.
            let any = (0..n - 1)
                .map(leg)
                .find(|d| d[0].hypot(d[1]) > 1e-6)
                .unwrap_or([1., 0.]);
            dir = any;
        }
        let l = dir[0].hypot(dir[1]);
        (p, [dir[0] / l, dir[1] / l])
    }
}

/// Halves cubic `c` until it is short or flat enough, handing each piece's
/// control points to `emit` in order.
fn split(c: [Vec3; 4], depth: u32, emit: &mut impl FnMut([Vec3; 4])) {
    let unit = |v: Vec3| math::normalize(v).unwrap_or([0.; 3]);
    let chord = unit(math::sub(c[3], c[0]));
    let flat =
        |q: Vec3| (1. - math::dot(chord, unit(math::sub(q, c[0])))).abs() < SEGMENT_THRESHOLD;
    let long = (0..3).all(|k| math::length(math::sub(c[k + 1], c[k])) > SEGMENT_MINIMUM);
    if depth >= MAX_DEPTH || !long || (flat(c[1]) && flat(c[2])) {
        return emit(c);
    }
    let mid = |a: Vec3, b: Vec3| -> Vec3 { std::array::from_fn(|k| (a[k] + b[k]) / 2.) };
    let (ab, bc, cd) = (mid(c[0], c[1]), mid(c[1], c[2]), mid(c[2], c[3]));
    let (abc, bcd) = (mid(ab, bc), mid(bc, cd));
    let m = mid(abc, bcd);
    split([c[0], ab, abc, m], depth + 1, emit);
    split([m, bcd, cd, c[3]], depth + 1, emit);
}

/// The rail model's pieces, geometry in model space (each piece's own
/// transform applied), and the model's extent along x, its length axis.
pub struct Template {
    pieces: Vec<MeshData>,
    x: [f32; 2],
}

impl Template {
    /// From a loaded model's pieces: their local transforms and geometry.
    pub fn new(pieces: impl IntoIterator<Item = (Mat4, MeshData)>) -> Option<Template> {
        let mut x = [f32::MAX, f32::MIN];
        let pieces: Vec<MeshData> = (pieces.into_iter())
            .map(|(local, mut data)| {
                for p in &mut data.positions {
                    *p = math::transform(&local, *p);
                    x = [x[0].min(p[0]), x[1].max(p[0])];
                }
                let turn = |n: Vec3| {
                    let v = std::array::from_fn(|r| {
                        local[r] * n[0] + local[4 + r] * n[1] + local[8 + r] * n[2]
                    });
                    math::normalize(v).unwrap_or([0., 1., 0.])
                };
                for n in &mut data.normals {
                    *n = turn(*n);
                }
                data
            })
            .collect();
        (x[1] > x[0] && !pieces.is_empty()).then_some(Template { pieces, x })
    }

    /// The model laid along `path`, one mesh per piece.
    pub fn bend(&self, path: &[PathPoint]) -> Vec<Arc<Mesh>> {
        let samples = Samples::new(path);
        let total = samples.length();
        let model = self.x[1] - self.x[0];
        if total < 0.1 {
            return Vec::new();
        }
        let copies = ((total / model).round() as usize).max(1);
        let stretch = total / (copies as f32 * model);
        self.pieces
            .iter()
            .filter_map(|piece| {
                let mut out = MeshData {
                    positions: Vec::with_capacity(piece.positions.len() * copies),
                    indices: Vec::with_capacity(piece.indices.len() * copies),
                    ..piece.clone()
                };
                out.positions.clear();
                out.normals.clear();
                out.uvs.clear();
                out.colors.clear();
                out.indices.clear();
                for copy in 0..copies {
                    let base = out.positions.len() as u32;
                    for (k, v) in piece.positions.iter().enumerate() {
                        let along = (copy as f32 * model + v[0] - self.x[0]) * stretch;
                        let (p, f) = samples.frame(along);
                        // Forward f, side (-f.z, f.x), up +y.
                        let side = [-f[1], f[0]];
                        out.positions.push([
                            p[0] + side[0] * v[2],
                            p[1] + v[1],
                            p[2] + side[1] * v[2],
                        ]);
                        if let Some(n) = piece.normals.get(k) {
                            let w = [
                                f[0] * n[0] + side[0] * n[2],
                                n[1],
                                f[1] * n[0] + side[1] * n[2],
                            ];
                            out.normals.push(math::normalize(w).unwrap_or([0., 1., 0.]));
                        }
                    }
                    out.uvs.extend(&piece.uvs);
                    out.colors.extend(&piece.colors);
                    out.indices.extend(piece.indices.iter().map(|i| i + base));
                }
                Mesh::new(out).ok().map(Arc::new)
            })
            .collect()
    }
}

/// Bent path models by work area, rebuilt only where the path or the
/// model changed.
#[derive(Default, Clone)]
pub struct Rails {
    built: Vec<Built>,
}

/// One work area's bent model, and what it was bent from.
#[derive(Clone)]
struct Built {
    work: u64,
    cfg: String,
    path: Vec<PathPoint>,
    meshes: Vec<Arc<Mesh>>,
}

impl Rails {
    /// Keeps the meshes of `wanted` (work area, model cfg, its template,
    /// path) current, dropping the rest.
    pub fn update<'a>(
        &mut self,
        wanted: impl IntoIterator<Item = (u64, &'a str, &'a Template, &'a [PathPoint])>,
    ) {
        use rayon::prelude::*;
        let mut old = std::mem::take(&mut self.built);
        let wanted: Vec<_> = (wanted.into_iter())
            .map(|(id, cfg, template, path)| {
                let kept = (old.iter())
                    .position(|b| b.work == id && b.cfg == cfg && b.path.as_slice() == path);
                let meshes = kept.map(|k| old.swap_remove(k).meshes);
                (id, cfg, template, path, meshes)
            })
            .collect();
        // Bent across cores: opening an 1800 island bent its rails in
        // 68 ms on the UI thread one after another.
        self.built = gpui_editor_kit::task::interactive(|| {
            (wanted.into_par_iter())
                .map(|(id, cfg, template, path, meshes)| Built {
                    work: id,
                    cfg: cfg.to_owned(),
                    path: path.to_vec(),
                    meshes: meshes.unwrap_or_else(|| template.bend(path)),
                })
                .collect()
        });
    }

    pub fn meshes(&self) -> impl Iterator<Item = &Arc<Mesh>> {
        self.built.iter().flat_map(|b| &b.meshes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_core::island::objects::bezier_handles as handles;

    // A straight path needs no halving; a bent one is sampled under 0.1 m
    // legs wherever it curves, and runs measure 3D length.
    #[test]
    fn samples_split_curves_only() {
        let straight = handles(&[[0., 0., 0.], [10., 0., 0.]], 0.3);
        let s = Samples::new(&straight);
        assert_eq!(s.at.len(), 4, "the control points, flat");
        assert!((s.length() - 10.).abs() < 1e-4);
        let bent = handles(&[[0., 0., 0.], [5., 2., 0.], [5., 2., 5.]], 0.3);
        let s = Samples::new(&bent);
        assert!(s.at.len() > 40, "{}", s.at.len());
        let (p, f) = s.frame(s.length());
        assert!(math::length(math::sub(p, [5., 2., 5.])) < 1e-4);
        assert!(f[1] > 0.99, "heads +z at the end: {f:?}");
    }

    // Bent copies keep their verticals plumb: a post straight down from a
    // track point stays straight down, wherever the path turns or climbs.
    #[test]
    fn legs_stay_plumb() {
        let post = MeshData {
            positions: vec![[0., 0., 0.2], [0., -2., 0.2], [0.5, 0., 0.2]],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let span = MeshData {
            positions: vec![[-1., 0., 0.], [1., 0., 0.], [0., 0., 0.]],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let template = Template::new([(math::IDENTITY, post), (math::IDENTITY, span)]).unwrap();
        let path = handles(&[[0., 0., 0.], [4., 1., 0.], [6., 1.5, 3.]], 0.3);
        let meshes = template.bend(&path);
        let top = meshes[0].data().positions[0];
        let foot = meshes[0].data().positions[1];
        assert_eq!([top[0], top[2]], [foot[0], foot[2]]);
        assert!((top[1] - foot[1] - 2.).abs() < 1e-5);
    }
}
