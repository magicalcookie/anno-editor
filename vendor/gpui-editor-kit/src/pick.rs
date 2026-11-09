// Ray casting against triangle meshes through a Morton-ordered bounding volume tree.
//
// Rays stay in f64. Game worlds put geometry tens of kilometers from the
// origin, where f32 ray parameters lose whole centimeters.
use crate::math::{Mat4, Vec3};

#[derive(Clone, Copy, Debug)]
pub struct Ray {
    pub origin: [f64; 3],
    pub direction: [f64; 3],
}

impl Ray {
    /// Transforms the ray by `m`, usually an instance's inverse world matrix.
    /// The direction is not renormalized, so a parameter `t` names the same
    /// point on both rays and hit distances compare across instances.
    pub fn local(self, m: &Mat4) -> Self {
        Self {
            origin: std::array::from_fn(|a| {
                self.origin[0] * f64::from(m[a])
                    + self.origin[1] * f64::from(m[4 + a])
                    + self.origin[2] * f64::from(m[8 + a])
                    + f64::from(m[12 + a])
            }),
            direction: std::array::from_fn(|a| {
                self.direction[0] * f64::from(m[a])
                    + self.direction[1] * f64::from(m[4 + a])
                    + self.direction[2] * f64::from(m[8 + a])
            }),
        }
    }

    /// Entry and exit parameters through an axis-aligned box, clipped to `0..limit`.
    pub fn range(self, bounds: [Vec3; 2], limit: f64) -> Option<(f64, f64)> {
        let mut near = 0f64;
        let mut far = limit;
        for (axis, &direction) in self.direction.iter().enumerate() {
            if direction.abs() < 1e-14 {
                if self.origin[axis] < f64::from(bounds[0][axis])
                    || self.origin[axis] > f64::from(bounds[1][axis])
                {
                    return None;
                }
            } else {
                let a = (f64::from(bounds[0][axis]) - self.origin[axis]) / direction;
                let b = (f64::from(bounds[1][axis]) - self.origin[axis]) / direction;
                near = near.max(a.min(b));
                far = far.min(a.max(b));
                if near > far {
                    return None;
                }
            }
        }
        Some((near, far))
    }

    /// Möller-Trumbore. Returns the ray parameter and barycentric weights of
    /// the three corners. Both faces hit; editor geometry is often single-sided.
    pub fn triangle(self, p: [Vec3; 3]) -> Option<(f64, [f32; 3])> {
        let sub = |a: [f64; 3], b: [f64; 3]| std::array::from_fn::<_, 3, _>(|i| a[i] - b[i]);
        let cross = |a: [f64; 3], b: [f64; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let dot = |a: [f64; 3], b: [f64; 3]| a.into_iter().zip(b).map(|(a, b)| a * b).sum::<f64>();
        let a = p[0].map(f64::from);
        let e1 = sub(p[1].map(f64::from), a);
        let e2 = sub(p[2].map(f64::from), a);
        let h = cross(self.direction, e2);
        let determinant = dot(e1, h);
        if determinant.abs() < 1e-12 {
            return None;
        }
        let s = sub(self.origin, a);
        let u = dot(s, h) / determinant;
        let q = cross(s, e1);
        let v = dot(self.direction, q) / determinant;
        let t = dot(e2, q) / determinant;
        (u >= -1e-6 && v >= -1e-6 && u + v <= 1. + 1e-6 && t >= 0.)
            .then_some((t, [(1. - u - v) as f32, u as f32, v as f32]))
    }

    pub fn point(self, t: f64) -> Vec3 {
        std::array::from_fn(|a| (self.origin[a] + self.direction[a] * t) as f32)
    }
}

/// A triangle hit in the mesh's own coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub distance: f64,
    pub face: usize,
    pub weights: [f32; 3],
    pub point: Vec3,
}

fn grow(bounds: &mut [Vec3; 2], other: &[Vec3; 2]) {
    for axis in 0..3 {
        bounds[0][axis] = bounds[0][axis].min(other[0][axis]);
        bounds[1][axis] = bounds[1][axis].max(other[1][axis]);
    }
}

#[derive(Clone)]
struct Node {
    bounds: [Vec3; 2],
    children: Option<[usize; 2]>,
    faces: std::ops::Range<usize>,
}

/// Bounding volume tree over one indexed triangle list. Build it lazily on
/// first pick; a 1M-triangle mesh takes tens of milliseconds.
#[derive(Clone, Default)]
pub struct PickTree {
    nodes: Vec<Node>,
    faces: Vec<usize>,
}

// Leaves hold up to this many triangles. Smaller leaves deepen the tree
// without making rays faster on typical game meshes.
const LEAF: usize = 8;

/// A position inside a packed or interleaved host vertex stream. Picking reads
/// it in place instead of allocating a second position buffer per mesh.
pub trait Position {
    fn position(&self) -> Vec3;
}

impl Position for Vec3 {
    fn position(&self) -> Vec3 {
        *self
    }
}

impl PickTree {
    pub fn bounds(&self) -> Option<[Vec3; 2]> {
        self.nodes.first().map(|node| node.bounds)
    }

    pub fn build(positions: &[impl Position], indices: &[u32]) -> Self {
        let triangles = indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| crate::math::bounds(t.iter().map(|&v| positions[v as usize].position())))
            .collect::<Vec<_>>();
        if triangles.is_empty() {
            return Self::default();
        }
        let mut bounds = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
        for triangle in &triangles {
            grow(&mut bounds, triangle);
        }
        let spread = |mut x: u32| {
            x = (x | x << 16) & 0x030000ff;
            x = (x | x << 8) & 0x0300f00f;
            x = (x | x << 4) & 0x030c30c3;
            (x | x << 2) & 0x09249249
        };
        let scale: [f32; 3] =
            std::array::from_fn(|a| 1023. / (bounds[1][a] - bounds[0][a]).max(0.0001));
        let mut order = triangles
            .iter()
            .enumerate()
            .map(|(face, b)| {
                let q: [u32; 3] = std::array::from_fn(|a| {
                    (((b[0][a] + b[1][a]) * 0.5 - bounds[0][a]) * scale[a]).clamp(0., 1023.) as u32
                });
                (spread(q[0]) | spread(q[1]) << 1 | spread(q[2]) << 2, face)
            })
            .collect::<Vec<_>>();
        // Morton ordering keeps adjacent leaves spatially close. Radix sorting
        // avoids comparison sorting and rescanning vertices at every tree level.
        let mut scratch = vec![(0u32, 0usize); order.len()];
        for shift in [0, 8, 16, 24] {
            let mut offsets = [0usize; 256];
            for &(key, _) in &order {
                offsets[((key >> shift) & 255) as usize] += 1;
            }
            let mut total = 0;
            for n in &mut offsets {
                let count = *n;
                *n = total;
                total += count;
            }
            for &entry in &order {
                let bucket = ((entry.0 >> shift) & 255) as usize;
                scratch[offsets[bucket]] = entry;
                offsets[bucket] += 1;
            }
            std::mem::swap(&mut order, &mut scratch);
        }
        let mut tree = Self {
            nodes: Vec::with_capacity(triangles.len() / 2),
            faces: order.into_iter().map(|(_, face)| face).collect(),
        };
        tree.branch(&triangles, 0..tree.faces.len());
        tree
    }

    /// Updates bounds for moved vertices. When topology is unchanged, keeping
    /// the partition is far cheaper than sorting every triangle again.
    pub fn refitted(&self, positions: &[impl Position], indices: &[u32]) -> Self {
        let mut tree = self.clone();
        for i in (0..tree.nodes.len()).rev() {
            let mut bounds = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
            if let Some(children) = tree.nodes[i].children {
                for child in children {
                    grow(&mut bounds, &tree.nodes[child].bounds);
                }
            } else {
                for &face in &tree.faces[tree.nodes[i].faces.clone()] {
                    for &v in &indices[face * 3..face * 3 + 3] {
                        for (axis, &p) in positions[v as usize].position().iter().enumerate() {
                            bounds[0][axis] = bounds[0][axis].min(p);
                            bounds[1][axis] = bounds[1][axis].max(p);
                        }
                    }
                }
            }
            tree.nodes[i].bounds = bounds;
        }
        tree
    }

    fn branch(&mut self, triangles: &[[Vec3; 2]], faces: std::ops::Range<usize>) -> usize {
        let at = self.nodes.len();
        self.nodes.push(Node {
            bounds: [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]],
            children: None,
            faces: faces.clone(),
        });
        let mut bounds = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
        if faces.len() > LEAF {
            let middle = faces.start + faces.len() / 2;
            let children = [
                self.branch(triangles, faces.start..middle),
                self.branch(triangles, middle..faces.end),
            ];
            for child in children {
                grow(&mut bounds, &self.nodes[child].bounds);
            }
            self.nodes[at].children = Some(children);
        } else {
            for &face in &self.faces[faces] {
                grow(&mut bounds, &triangles[face]);
            }
        }
        self.nodes[at].bounds = bounds;
        at
    }

    /// Nearest hit closer than `limit`. `accept` can reject a candidate, for
    /// example a transparent texel or a region another mesh owns; traversal
    /// then continues to the next nearest triangle.
    pub fn hit(
        &self,
        positions: &[impl Position],
        indices: &[u32],
        ray: Ray,
        mut limit: f64,
        mut accept: impl FnMut(&Hit) -> bool,
    ) -> Option<Hit> {
        let mut pending = Vec::with_capacity(32);
        if !self.nodes.is_empty() {
            pending.push(0);
        }
        let mut result = None;
        while let Some(index) = pending.pop() {
            let node = &self.nodes[index];
            if ray.range(node.bounds, limit).is_none() {
                continue;
            }
            if let Some(children) = node.children {
                pending.extend(children);
                continue;
            }
            for &face in &self.faces[node.faces.clone()] {
                let points =
                    std::array::from_fn(|i| positions[indices[face * 3 + i] as usize].position());
                if let Some((t, weights)) = ray.triangle(points)
                    && t < limit
                {
                    let point =
                        std::array::from_fn(|a| (0..3).map(|i| points[i][a] * weights[i]).sum());
                    let hit = Hit {
                        distance: t,
                        face,
                        weights,
                        point,
                    };
                    if accept(&hit) {
                        limit = t;
                        result = Some(hit);
                    }
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(side: usize) -> (Vec<Vec3>, Vec<u32>) {
        let mut positions = vec![];
        for z in 0..=side {
            for x in 0..=side {
                positions.push([x as f32, ((x * 7 + z * 3) % 5) as f32 * 0.1, z as f32]);
            }
        }
        let mut indices = vec![];
        let row = side as u32 + 1;
        for z in 0..side as u32 {
            for x in 0..side as u32 {
                let a = z * row + x;
                indices.extend([a, a + row, a + 1, a + 1, a + row, a + row + 1]);
            }
        }
        (positions, indices)
    }

    fn brute(positions: &[impl Position], indices: &[u32], ray: Ray) -> Option<f64> {
        indices
            .as_chunks::<3>()
            .0
            .iter()
            .filter_map(|t| ray.triangle(t.map(|v| positions[v as usize].position())))
            .map(|(t, _)| t)
            .min_by(f64::total_cmp)
    }

    #[test]
    fn interleaved_host_vertices_support_build_refit_and_filtered_hits() {
        #[derive(Clone)]
        struct Vertex {
            position: Vec3,
            _attributes: [f32; 5],
        }
        impl Position for Vertex {
            fn position(&self) -> Vec3 {
                self.position
            }
        }
        let mut vertices = [
            [0., 0., 0.],
            [2., 0., 0.],
            [0., 0., 2.],
            [0., -3., 0.],
            [2., -3., 0.],
            [0., -3., 2.],
        ]
        .map(|position| Vertex {
            position,
            _attributes: [9.; 5],
        });
        let indices = [0, 1, 2, 3, 4, 5];
        let tree = PickTree::build(&vertices, &indices);
        let ray = Ray {
            origin: [0.5, 10., 0.5],
            direction: [0., -1., 0.],
        };
        let hit = tree
            .hit(&vertices, &indices, ray, 100., |hit| hit.face != 0)
            .unwrap();
        assert_eq!(hit.point, [0.5, -3., 0.5]);
        for vertex in &mut vertices {
            vertex.position[1] += 4.;
        }
        let refitted = tree.refitted(&vertices, &indices);
        assert_eq!(refitted.bounds(), Some([[0., 1., 0.], [2., 4., 2.]]));
        assert_eq!(
            refitted
                .hit(&vertices, &indices, ray, 100., |_| true)
                .unwrap()
                .point,
            [0.5, 4., 0.5]
        );
    }

    #[test]
    fn tree_matches_brute_force_and_refits_after_edits() {
        let (mut positions, indices) = grid(40);
        let tree = PickTree::build(&positions, &indices);
        for i in 0..50 {
            let ray = Ray {
                origin: [i as f64 * 0.79 + 0.13, 50., (i * 13 % 38) as f64 + 0.37],
                direction: [0.01, -1., 0.02],
            };
            let expected = brute(&positions, &indices, ray).unwrap();
            let hit = tree
                .hit(&positions, &indices, ray, f64::INFINITY, |_| true)
                .unwrap();
            assert!((hit.distance - expected).abs() < 1e-6);
        }
        for p in &mut positions {
            p[1] += p[0] * 0.5;
        }
        let refit = tree.refitted(&positions, &indices);
        let ray = Ray {
            origin: [30.5, 100., 10.5],
            direction: [0., -1., 0.],
        };
        let hit = refit
            .hit(&positions, &indices, ray, f64::INFINITY, |_| true)
            .unwrap();
        assert!((hit.distance - brute(&positions, &indices, ray).unwrap()).abs() < 1e-6);
        assert!(hit.point[1] > 15.);
    }

    #[test]
    fn rejected_hits_fall_through_to_the_next_surface() {
        let positions = vec![
            [0., 1., 0.],
            [0., 1., 1.],
            [1., 1., 0.],
            [0., 0., 0.],
            [0., 0., 1.],
            [1., 0., 0.],
        ];
        let indices = [0, 1, 2, 3, 4, 5];
        let tree = PickTree::build(&positions, &indices);
        let ray = Ray {
            origin: [0.2, 5., 0.2],
            direction: [0., -1., 0.],
        };
        let top = tree
            .hit(&positions, &indices, ray, f64::INFINITY, |_| true)
            .unwrap();
        assert_eq!(top.face, 0);
        let below = tree
            .hit(&positions, &indices, ray, f64::INFINITY, |h| h.face != 0)
            .unwrap();
        assert_eq!(below.face, 1);
        assert!(
            tree.hit(&positions, &indices, ray, 3.5, |h| h.face != 0)
                .is_none()
        );
    }
}
