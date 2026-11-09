// Owns regular height grids, the sculpt brushes that edit them, brush
// footprints for painting any per-cell layer, and tiled geometry for drawing.
//
// Grid space: cell (x, z) sits at `origin + (x, z) * spacing` in world X/Z;
// heights are world Y. Edits report the dirty cell rectangle so hosts rebuild
// only the tiles a stroke touched.
use crate::math::Vec3;
use crate::pick::Ray;

#[cfg(feature = "import")]
pub mod files;

/// Cells per tile side. 64 keeps a rebuilt tile around 8k triangles, cheap
/// enough to redo on every dab of a drag.
pub const TILE: usize = 64;

/// Placement of a sample grid in the world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    pub width: usize,
    pub depth: usize,
    pub spacing: f32,
    /// World X/Z of sample (0, 0).
    pub origin: [f32; 2],
}

/// Half-open cell rectangle `[x0, x1) x [z0, z1)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x0: usize,
    pub z0: usize,
    pub x1: usize,
    pub z1: usize,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.z0 >= self.z1
    }

    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Rect {
            x0: self.x0.min(other.x0),
            z0: self.z0.min(other.z0),
            x1: self.x1.max(other.x1),
            z1: self.z1.max(other.z1),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Falloff {
    /// Sharp peak: weight falls off quadratically from the inner radius.
    Spiky,
    Linear,
    /// Smoothstep between the inner and outer radius.
    #[default]
    Smooth,
    /// Rounded dome: full for longer, steep near the rim.
    Bulge,
    /// Full strength everywhere inside the radius.
    Constant,
}

impl Falloff {
    pub const ALL: [Self; 5] = [
        Self::Spiky,
        Self::Linear,
        Self::Smooth,
        Self::Bulge,
        Self::Constant,
    ];
}

/// How Raise and Lower accumulate within one stroke.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Every dab adds `strength`; going over the same spot keeps building.
    #[default]
    Standard,
    /// A stroke moves ground at most `strength` from where it was when the
    /// stroke began, however often it passes. Needs the stroke baseline.
    Absolute,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brush {
    /// Outer radius, world units.
    pub radius: f32,
    /// Fraction of `radius` (0..1) painted at full weight before falloff.
    pub inner: f32,
    /// Raise/Lower: world units per dab at the center (Standard) or per
    /// stroke (Absolute). Smooth, Flatten, Fill, Scrape and Noise: blend
    /// amount per dab, 0..1.
    pub strength: f32,
    pub falloff: Falloff,
    pub mode: Mode,
}

impl Default for Brush {
    fn default() -> Self {
        Self {
            radius: 16.,
            inner: 0.,
            strength: 0.5,
            falloff: Falloff::Smooth,
            mode: Mode::Standard,
        }
    }
}

impl Brush {
    /// Weight 0..1 at `distance` from the center.
    pub fn weight(&self, distance: f32) -> f32 {
        if self.radius <= 0. || distance >= self.radius {
            return 0.;
        }
        let inner = self.inner.clamp(0., 0.999);
        let r = distance / self.radius;
        if r <= inner {
            return 1.;
        }
        // 1 at the inner radius, 0 at the rim.
        let t = 1. - (r - inner) / (1. - inner);
        match self.falloff {
            Falloff::Spiky => t * t,
            Falloff::Linear => t,
            Falloff::Smooth => t * t * (3. - 2. * t),
            Falloff::Bulge => 1. - (1. - t) * (1. - t),
            Falloff::Constant => 1.,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sculpt {
    Raise,
    Lower,
    /// 3x3 box blur, read from the surface before the dab.
    Smooth,
    /// Pull toward a height, usually sampled where the stroke started.
    Flatten(f32),
    /// Raise only ground below the height toward it (fills hollows).
    Fill(f32),
    /// Lower only ground above the height toward it (scrapes bumps off).
    Scrape(f32),
    /// Add value noise with this feature size in world units.
    Noise(f32),
}

impl Grid {
    pub fn len(&self) -> usize {
        self.width * self.depth
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn index(&self, x: usize, z: usize) -> usize {
        z * self.width + x
    }

    /// World X/Z to fractional cell coordinates.
    pub fn cell(&self, p: [f32; 2]) -> [f32; 2] {
        [
            (p[0] - self.origin[0]) / self.spacing,
            (p[1] - self.origin[1]) / self.spacing,
        ]
    }

    pub fn world(&self, x: usize, z: usize) -> [f32; 2] {
        [
            self.origin[0] + x as f32 * self.spacing,
            self.origin[1] + z as f32 * self.spacing,
        ]
    }

    /// World X/Z covered by the samples.
    pub fn size(&self) -> [f32; 2] {
        [
            self.width.saturating_sub(1) as f32 * self.spacing,
            self.depth.saturating_sub(1) as f32 * self.spacing,
        ]
    }

    /// Cells a circle can touch, clipped to the grid.
    pub fn footprint(&self, center: [f32; 2], radius: f32) -> Rect {
        let [cx, cz] = self.cell(center);
        let r = radius / self.spacing;
        let clip = |v: f32, n: usize| (v.max(0.) as usize).min(n);
        let rect = Rect {
            x0: clip((cx - r).floor(), self.width),
            z0: clip((cz - r).floor(), self.depth),
            x1: clip((cx + r).ceil() + 1., self.width),
            z1: clip((cz + r).ceil() + 1., self.depth),
        };
        if rect.is_empty() {
            Rect::default()
        } else {
            rect
        }
    }

    /// `(cell index, weight)` for every cell under the brush with a non-zero
    /// weight. Use it to paint layers that are not heights.
    pub fn cells(&self, center: [f32; 2], brush: &Brush) -> impl Iterator<Item = (usize, f32)> {
        let rect = self.footprint(center, brush.radius);
        let brush = *brush;
        (rect.z0..rect.z1).flat_map(move |z| {
            (rect.x0..rect.x1).filter_map(move |x| {
                let [wx, wz] = self.world(x, z);
                let weight = brush.weight((wx - center[0]).hypot(wz - center[1]));
                (weight > 0.).then_some((self.index(x, z), weight))
            })
        })
    }

    /// Tiles overlapping a cell rectangle, as `(tile x, tile z)`. A sample on
    /// a tile edge belongs to both tiles, so the rectangle grows by one.
    pub fn tiles(&self, rect: Rect) -> Vec<(usize, usize)> {
        if rect.is_empty() {
            return Vec::new();
        }
        let [tw, td] = self.tile_count();
        let x0 = rect.x0.saturating_sub(1) / TILE;
        let z0 = rect.z0.saturating_sub(1) / TILE;
        let x1 = (rect.x1 / TILE + 1).min(tw);
        let z1 = (rect.z1 / TILE + 1).min(td);
        (z0..z1)
            .flat_map(|z| (x0..x1).map(move |x| (x, z)))
            .collect()
    }

    pub fn tile_count(&self) -> [usize; 2] {
        [
            self.width.saturating_sub(1).div_ceil(TILE).max(1),
            self.depth.saturating_sub(1).div_ceil(TILE).max(1),
        ]
    }
}

/// Triangles for one tile. `samples[i]` is the grid index of vertex `i`, so
/// hosts can color vertices from their own layers. UVs span the whole grid.
#[derive(Clone, Debug, Default)]
pub struct TileGeometry {
    pub positions: Vec<Vec3>,
    pub uvs: Vec<[f32; 2]>,
    pub samples: Vec<usize>,
    pub indices: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Heightfield {
    pub grid: Grid,
    pub heights: Vec<f32>,
    /// Sculpting clamps to this range; game formats rarely store floats.
    pub limits: [f32; 2],
}

impl Heightfield {
    pub fn new(grid: Grid, heights: Vec<f32>) -> Self {
        assert_eq!(heights.len(), grid.len(), "one height per grid sample");
        Self {
            grid,
            heights,
            limits: [f32::MIN, f32::MAX],
        }
    }

    pub fn at(&self, x: usize, z: usize) -> f32 {
        self.heights[self.grid.index(x, z)]
    }

    /// Bilinear height at world X/Z, clamped to the grid edge.
    pub fn height(&self, p: [f32; 2]) -> f32 {
        let g = &self.grid;
        let [cx, cz] = g.cell(p);
        let cx = cx.clamp(0., (g.width - 1) as f32);
        let cz = cz.clamp(0., (g.depth - 1) as f32);
        let x0 = cx.floor() as usize;
        let z0 = cz.floor() as usize;
        let x1 = (x0 + 1).min(g.width - 1);
        let z1 = (z0 + 1).min(g.depth - 1);
        let (u, v) = (cx - x0 as f32, cz - z0 as f32);
        let top = self.at(x0, z0) * (1. - u) + self.at(x1, z0) * u;
        let bottom = self.at(x0, z1) * (1. - u) + self.at(x1, z1) * u;
        top * (1. - v) + bottom * v
    }

    /// World X/Z bounds `[x0, z0, x1, z1]` of the samples above `level`
    /// (the land above the sea), or `None` when none are. Cameras frame
    /// this instead of the whole grid, whose margin is mostly water.
    pub fn bounds_above(&self, level: f32) -> Option<[f32; 4]> {
        let g = &self.grid;
        let (mut x0, mut z0, mut x1, mut z1) = (usize::MAX, usize::MAX, 0, 0);
        for z in 0..g.depth {
            for x in 0..g.width {
                if self.heights[g.index(x, z)] > level {
                    (x0, z0, x1, z1) = (x0.min(x), z0.min(z), x1.max(x), z1.max(z));
                }
            }
        }
        (x0 <= x1).then(|| {
            let [a, b] = g.world(x0, z0);
            let [c, d] = g.world(x1, z1);
            [a, b, c, d]
        })
    }

    pub fn range(&self) -> [f32; 2] {
        self.heights
            .iter()
            .fold([f32::MAX, f32::MIN], |[lo, hi], &h| [lo.min(h), hi.max(h)])
    }

    /// One dab. Returns the cells that changed, empty when the brush misses.
    pub fn sculpt(&mut self, center: [f32; 2], brush: &Brush, op: Sculpt) -> Rect {
        self.sculpt_from(center, brush, op, None)
    }

    /// One dab with the heights from when the stroke began, which
    /// [`Mode::Absolute`] measures its cap against. Without a baseline,
    /// Absolute behaves like Standard.
    pub fn sculpt_from(
        &mut self,
        center: [f32; 2],
        brush: &Brush,
        op: Sculpt,
        baseline: Option<&[f32]>,
    ) -> Rect {
        let rect = self.grid.footprint(center, brush.radius);
        if rect.is_empty() {
            return rect;
        }
        // Smooth reads a copy so the sweep order cannot bias the result.
        let source = if op == Sculpt::Smooth {
            let x0 = rect.x0.saturating_sub(1);
            let x1 = (rect.x1 + 1).min(self.grid.width);
            let z0 = rect.z0.saturating_sub(1);
            let z1 = (rect.z1 + 1).min(self.grid.depth);
            let mut copy = Vec::with_capacity((x1 - x0) * (z1 - z0));
            for z in z0..z1 {
                let row = self.grid.index(x0, z);
                copy.extend_from_slice(&self.heights[row..row + x1 - x0]);
            }
            Some((x0, z0, x1 - x0, copy))
        } else {
            None
        };
        let mut dirty = Rect::default();
        for z in rect.z0..rect.z1 {
            for x in rect.x0..rect.x1 {
                let [wx, wz] = self.grid.world(x, z);
                let weight = brush.weight((wx - center[0]).hypot(wz - center[1]));
                if weight <= 0. {
                    continue;
                }
                let index = self.grid.index(x, z);
                let old = self.heights[index];
                let blend = (brush.strength * weight).clamp(0., 1.);
                // Absolute caps the stroke's total change per cell.
                let base = baseline
                    .filter(|_| brush.mode == Mode::Absolute)
                    .and_then(|b| b.get(index).copied());
                // Absolute moves a fifth of the remaining way per dab so a
                // stroke eases into its cap instead of snapping.
                let step = brush.strength * weight;
                let new = match op {
                    Sculpt::Raise => match base {
                        Some(b) => old.max(old + (b + step - old) * 0.2).min(b + step).max(old),
                        None => old + step,
                    },
                    Sculpt::Lower => match base {
                        Some(b) => old
                            .min(old - (old - (b - step)) * 0.2)
                            .max(b - step)
                            .min(old),
                        None => old - step,
                    },
                    Sculpt::Flatten(level) => old + (level - old) * blend,
                    Sculpt::Fill(level) => old.max(old + (level - old) * blend),
                    Sculpt::Scrape(level) => old.min(old + (level - old) * blend),
                    Sculpt::Noise(size) => {
                        let n = value_noise(wx / size.max(1e-3), wz / size.max(1e-3));
                        old + (n * 2. - 1.) * size * 0.25 * blend
                    }
                    Sculpt::Smooth => {
                        let (sx, sz, stride, copy) = source.as_ref().expect("smooth copies");
                        let mut sum = 0.;
                        let mut count = 0.;
                        for nz in z.saturating_sub(1)..=(z + 1).min(self.grid.depth - 1) {
                            for nx in x.saturating_sub(1)..=(x + 1).min(self.grid.width - 1) {
                                sum += copy[(nz - sz) * stride + nx - sx];
                                count += 1.;
                            }
                        }
                        old + (sum / count - old) * blend
                    }
                }
                .clamp(self.limits[0], self.limits[1]);
                if new != old {
                    self.heights[index] = new;
                    dirty = dirty.union(Rect {
                        x0: x,
                        z0: z,
                        x1: x + 1,
                        z1: z + 1,
                    });
                }
            }
        }
        dirty
    }

    /// First surface hit along a ray. Marches half a cell at a time inside the
    /// height range, then bisects; fine for brushes, not for thin spikes.
    pub fn raycast(&self, ray: Ray) -> Option<Vec3> {
        let g = &self.grid;
        if g.width < 2 || g.depth < 2 {
            return None;
        }
        let [w, d] = g.size();
        let o = g.origin;
        // Clip to the X/Z footprint only. The cell walk below skips cells the
        // ray passes over, so it needs no height range (scanning one costs a
        // read of every sample per cast, ~1.5 ms at 1025²).
        let (near, far) = ray.range(
            [[o[0], f32::MIN, o[1]], [o[0] + w, f32::MAX, o[1] + d]],
            f64::MAX,
        )?;
        let above = |t: f64| {
            let p = ray.point(t);
            p[1] - self.height([p[0], p[2]])
        };
        if above(near) <= 0. {
            return Some(ray.point(near));
        }
        // Walk the cells under the ray (2D DDA). A cell can only be hit
        // where the ray dips to its highest corner: bilinear patches peak at
        // a corner. Along the ray a cell's patch is quadratic in t, so those
        // cells are solved exactly.
        let s = f64::from(g.spacing);
        let (cols, rows) = (g.width as isize - 1, g.depth as isize - 1);
        let origin = [f64::from(o[0]), f64::from(o[1])];
        let dir = [ray.direction[0], ray.direction[2]];
        let start = [ray.origin[0], ray.origin[2]];
        let at = |t: f64, a: usize| start[a] + dir[a] * t - origin[a];
        let mut cell = [0, 1].map(|a| {
            let limit = if a == 0 { cols } else { rows };
            ((at(near, a) / s).floor() as isize).clamp(0, limit - 1)
        });
        let step = dir.map(|v| if v > 0. { 1 } else { -1 });
        let delta = dir.map(|v| if v == 0. { f64::INFINITY } else { s / v.abs() });
        let mut next = [0, 1].map(|a| {
            if dir[a] == 0. {
                return f64::INFINITY;
            }
            let edge = (cell[a] + isize::from(dir[a] > 0.)) as f64 * s + origin[a];
            (edge - start[a]) / dir[a]
        });
        let (oy, dy) = (ray.origin[1], ray.direction[1]);
        let mut t0 = near;
        loop {
            let t1 = next[0].min(next[1]).min(far);
            let (x, z) = (cell[0] as usize, cell[1] as usize);
            let h = [
                self.at(x, z),
                self.at(x + 1, z),
                self.at(x, z + 1),
                self.at(x + 1, z + 1),
            ]
            .map(f64::from);
            let peak = h[0].max(h[1]).max(h[2]).max(h[3]);
            if (oy + dy * t0).min(oy + dy * t1) <= peak {
                // Cell-local u, v at t0 and their rates; y minus the patch
                // h00 + b u + c v + e u v is A τ² + B τ + C for τ = t - t0.
                let (u0, v0) = (at(t0, 0) / s - x as f64, at(t0, 1) / s - z as f64);
                let (du, dv) = (dir[0] / s, dir[1] / s);
                let (b, c, e) = (h[1] - h[0], h[2] - h[0], h[3] - h[1] - h[2] + h[0]);
                let qa = -e * du * dv;
                let qb = dy - b * du - c * dv - e * (u0 * dv + v0 * du);
                let qc = oy + dy * t0 - (h[0] + b * u0 + c * v0 + e * u0 * v0);
                let span = t1 - t0;
                if let Some(tau) = first_root(qa, qb, qc, span) {
                    return Some(ray.point(t0 + tau));
                }
            }
            if t1 >= far {
                return None;
            }
            let a = usize::from(next[1] < next[0]);
            cell[a] += step[a];
            let limit = if a == 0 { cols } else { rows };
            if cell[a] < 0 || cell[a] >= limit {
                return None;
            }
            next[a] += delta[a];
            t0 = t1;
        }
    }

    /// Geometry for tile `(tx, tz)`, taking every `stride`-th sample. All
    /// tiles must use the same stride or their shared edges crack.
    pub fn tile(&self, tx: usize, tz: usize, stride: usize) -> TileGeometry {
        let g = &self.grid;
        let stride = stride.max(1);
        let axis = |t: usize, n: usize| {
            let start = t * TILE;
            let end = ((t + 1) * TILE).min(n - 1);
            let mut v: Vec<usize> = (start..end).step_by(stride).collect();
            v.push(end);
            v
        };
        let xs = axis(tx, g.width);
        let zs = axis(tz, g.depth);
        let [w, d] = g.size();
        let mut out = TileGeometry::default();
        for &z in &zs {
            for &x in &xs {
                let [wx, wz] = g.world(x, z);
                out.positions.push([wx, self.at(x, z), wz]);
                out.uvs.push([
                    (wx - g.origin[0]) / w.max(1e-6),
                    (wz - g.origin[1]) / d.max(1e-6),
                ]);
                out.samples.push(g.index(x, z));
            }
        }
        let row = xs.len() as u32;
        for z in 0..zs.len() as u32 - 1 {
            for x in 0..row - 1 {
                let a = z * row + x;
                let (b, c, e) = (a + 1, a + row, a + row + 1);
                out.indices.extend([a, c, b, b, c, e]);
            }
        }
        out
    }
}

/// Emits evenly spaced dab centers along a drag so fast strokes do not leave
/// gaps and slow ones do not pile up.
#[derive(Clone, Debug, Default)]
pub struct Stroke {
    last: Option<[f32; 2]>,
}

impl Stroke {
    /// Dabs between the previous point and `to`, spaced `spacing` apart.
    /// The first call always yields `to`.
    pub fn to(&mut self, to: [f32; 2], spacing: f32) -> Vec<[f32; 2]> {
        let Some(from) = self.last else {
            self.last = Some(to);
            return vec![to];
        };
        let length = (to[0] - from[0]).hypot(to[1] - from[1]);
        let spacing = spacing.max(1e-3);
        if length < spacing {
            return Vec::new();
        }
        let steps = (length / spacing).floor() as usize;
        let dabs: Vec<[f32; 2]> = (1..=steps)
            .map(|i| {
                let t = i as f32 * spacing / length;
                [
                    from[0] + (to[0] - from[0]) * t,
                    from[1] + (to[1] - from[1]) * t,
                ]
            })
            .collect();
        self.last = dabs.last().copied();
        dabs
    }
}

/// Smooth 0..1 value noise on integer lattice points.
fn value_noise(x: f32, z: f32) -> f32 {
    fn hash(x: i32, z: i32) -> f32 {
        let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (z as u32).wrapping_mul(0xd816_3841);
        h = (h ^ (h >> 13)).wrapping_mul(0x85eb_ca6b);
        (h ^ (h >> 16)) as f32 / u32::MAX as f32
    }
    let (ix, iz) = (x.floor() as i32, z.floor() as i32);
    let (fx, fz) = (x - x.floor(), z - z.floor());
    let (u, v) = (fx * fx * (3. - 2. * fx), fz * fz * (3. - 2. * fz));
    let top = hash(ix, iz) * (1. - u) + hash(ix + 1, iz) * u;
    let bottom = hash(ix, iz + 1) * (1. - u) + hash(ix + 1, iz + 1) * u;
    top * (1. - v) + bottom * v
}

/// The smallest τ in `[0, span]` where `a τ² + b τ + c`, positive at 0,
/// reaches zero.
fn first_root(a: f64, b: f64, c: f64, span: f64) -> Option<f64> {
    if c <= 0. {
        return Some(0.);
    }
    let roots = if a.abs() < 1e-12 {
        [(b < 0.).then(|| -c / b), None]
    } else {
        let disc = b * b - 4. * a * c;
        if disc < 0. {
            return None;
        }
        // The stable form: no cancellation when b dominates.
        let q = -0.5 * (b + b.signum() * disc.sqrt());
        [
            (q != 0.).then(|| q / a),
            Some(c / q).filter(|r| r.is_finite()),
        ]
    };
    roots
        .into_iter()
        .flatten()
        .filter(|&r| (0. ..=span).contains(&r))
        .min_by(f64::total_cmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(width: usize, h: f32) -> Heightfield {
        let grid = Grid {
            width,
            depth: width,
            spacing: 1.,
            origin: [0., 0.],
        };
        Heightfield::new(grid, vec![h; width * width])
    }

    #[test]
    fn land_bounds_cover_only_samples_above_the_level() {
        let mut field = flat(8, -1.);
        assert_eq!(field.bounds_above(0.), None);
        let (i, j) = (field.grid.index(2, 3), field.grid.index(5, 4));
        field.heights[i] = 2.;
        field.heights[j] = 1.;
        assert_eq!(field.bounds_above(0.), Some([2., 3., 5., 4.]));
    }

    #[test]
    fn raise_peaks_at_center_and_reports_its_footprint() {
        let mut field = flat(33, 0.);
        let brush = Brush {
            radius: 4.,
            strength: 2.,
            ..Brush::default()
        };
        let dirty = field.sculpt([16., 16.], &brush, Sculpt::Raise);
        assert_eq!(field.at(16, 16), 2.);
        assert!(field.at(18, 16) > 0. && field.at(18, 16) < 2.);
        assert_eq!(field.at(20, 16), 0., "rim has zero weight");
        assert_eq!(field.at(0, 0), 0.);
        assert_eq!(
            dirty,
            Rect {
                x0: 13,
                z0: 13,
                x1: 20,
                z1: 20
            }
        );
        let lowered = field.sculpt([16., 16.], &brush, Sculpt::Lower);
        assert_eq!(lowered, dirty);
        assert!(field.heights.iter().all(|&h| h.abs() < 1e-6));
    }

    #[test]
    fn flatten_smooth_and_limits() {
        let mut field = flat(17, 0.);
        field.heights[field.grid.index(8, 8)] = 9.;
        let brush = Brush {
            radius: 3.,
            strength: 1.,
            falloff: Falloff::Constant,
            ..Brush::default()
        };
        field.sculpt([8., 8.], &brush, Sculpt::Smooth);
        assert_eq!(field.at(8, 8), 1., "box blur of one spike");
        assert_eq!(field.at(7, 8), 1.);
        field.sculpt([8., 8.], &brush, Sculpt::Flatten(5.));
        assert_eq!(field.at(8, 8), 5.);
        field.limits = [-1., 3.];
        field.sculpt([8., 8.], &brush, Sculpt::Raise);
        assert_eq!(field.at(8, 8), 3.);
        let missed = field.sculpt([-50., -50.], &brush, Sculpt::Raise);
        assert!(missed.is_empty());
    }

    #[test]
    fn layer_cells_follow_brush_weights() {
        let field = flat(9, 0.);
        let brush = Brush {
            radius: 2.,
            strength: 1.,
            falloff: Falloff::Linear,
            ..Brush::default()
        };
        let cells: Vec<_> = field.grid.cells([4., 4.], &brush).collect();
        assert!(cells.contains(&(field.grid.index(4, 4), 1.)));
        assert!(cells.iter().all(|&(_, w)| w > 0. && w <= 1.));
        assert_eq!(
            cells.len(),
            9,
            "radius 2 linear touches the 3x3 plus none on the rim"
        );
    }

    #[test]
    fn tiles_share_edges_and_cover_the_grid() {
        let field = flat(TILE * 2 + 1, 1.);
        assert_eq!(field.grid.tile_count(), [2, 2]);
        let a = field.tile(0, 0, 1);
        let b = field.tile(1, 0, 1);
        assert_eq!(a.positions.len(), (TILE + 1) * (TILE + 1));
        assert_eq!(a.indices.len(), TILE * TILE * 6);
        let edge_a: Vec<_> = a.positions.iter().filter(|p| p[0] == TILE as f32).collect();
        let edge_b: Vec<_> = b.positions.iter().filter(|p| p[0] == TILE as f32).collect();
        assert_eq!(edge_a, edge_b);
        let coarse = field.tile(1, 1, 16);
        assert_eq!(coarse.positions.len(), 25);
        assert_eq!(coarse.uvs.last(), Some(&[1., 1.]));
        let touched = field.grid.tiles(Rect {
            x0: TILE,
            z0: 3,
            x1: TILE + 1,
            z1: 4,
        });
        assert_eq!(
            touched,
            vec![(0, 0), (1, 0)],
            "edge sample dirties both tiles"
        );
    }

    #[test]
    fn raycast_matches_a_dense_march_on_rough_terrain() {
        let grid = Grid {
            width: 65,
            depth: 65,
            spacing: 0.5,
            origin: [-10., 4.],
        };
        // Ridges and pits at every scale, including single-sample spikes.
        let heights = (0..grid.len())
            .map(|i| {
                let (x, z) = ((i % 65) as f32, (i / 65) as f32);
                (x * 0.37).sin() * 3. + (z * 1.9).cos() * 1.5 + if i % 97 == 0 { 6. } else { 0. }
            })
            .collect();
        let field = Heightfield::new(grid, heights);
        let dense = |ray: Ray| {
            let mut t = 0.;
            while t < 400. {
                let p = ray.point(t);
                let inside = (-10. ..=22.).contains(&p[0]) && (4. ..=36.).contains(&p[2]);
                if inside && p[1] <= field.height([p[0], p[2]]) {
                    return Some(p);
                }
                t += 0.002;
            }
            None
        };
        let mut checked = 0;
        for i in 0..400 {
            let f = f64::from(i);
            let ray = Ray {
                origin: [
                    -20. + (f * 0.13) % 50.,
                    12. + (f * 0.7) % 10.,
                    (f * 0.31) % 45.,
                ],
                direction: crate::math::normalize([
                    ((f * 1.3).sin()) as f32,
                    -0.2 - ((f * 0.9).cos().abs() as f32),
                    ((f * 0.7).cos()) as f32,
                ])
                .map(|d| d.map(f64::from))
                .unwrap(),
            };
            let (got, want) = (field.raycast(ray), dense(ray));
            assert_eq!(
                got.is_some(),
                want.is_some(),
                "ray {i}: {got:?} vs {want:?}"
            );
            if let (Some(g), Some(w)) = (got, want) {
                let gap = (0..3).map(|a| (g[a] - w[a]).abs()).fold(0., f32::max);
                assert!(gap < 0.05, "ray {i}: {g:?} vs {w:?}");
                checked += 1;
            }
        }
        assert!(checked > 100, "only {checked} rays hit");
    }

    #[test]
    fn raycast_finds_the_surface_from_above() {
        let mut field = flat(65, 2.);
        field.sculpt(
            [32., 32.],
            &Brush {
                radius: 10.,
                strength: 6.,
                ..Brush::default()
            },
            Sculpt::Raise,
        );
        let down = Ray {
            origin: [32., 100., 32.],
            direction: [0., -1., 0.],
        };
        let hit = field.raycast(down).unwrap();
        assert!((hit[1] - 8.).abs() < 0.01, "{hit:?}");
        let slant = Ray {
            origin: [0., 50., 10.],
            direction: [0.7, -0.7, 0.],
        };
        let gap = field
            .raycast(slant)
            .map(|p| p[1] - field.height([p[0], p[2]]));
        assert!(gap.unwrap().abs() < 0.01, "{gap:?}");
        let miss = Ray {
            origin: [0., 50., 10.],
            direction: [0., 1., 0.],
        };
        assert!(field.raycast(miss).is_none());
    }

    #[test]
    fn falloffs_and_inner_radius_shape_the_weight() {
        let mut b = Brush {
            radius: 10.,
            inner: 0.5,
            ..Brush::default()
        };
        assert_eq!(b.weight(4.), 1., "inside the inner radius");
        assert_eq!(b.weight(10.), 0.);
        let at = |b: &Brush| b.weight(7.5);
        let mut shapes = Vec::new();
        for f in Falloff::ALL {
            b.falloff = f;
            shapes.push(at(&b));
        }
        // Halfway between inner and rim: spiky < linear = smooth < bulge < flat.
        assert_eq!(shapes, [0.25, 0.5, 0.5, 0.75, 1.]);
    }

    #[test]
    fn absolute_strokes_cap_at_strength_above_the_baseline() {
        let mut field = flat(17, 0.);
        let base = field.heights.clone();
        let brush = Brush {
            radius: 3.,
            strength: 2.,
            falloff: Falloff::Constant,
            mode: Mode::Absolute,
            ..Brush::default()
        };
        for _ in 0..200 {
            field.sculpt_from([8., 8.], &brush, Sculpt::Raise, Some(&base));
        }
        assert!((field.at(8, 8) - 2.).abs() < 1e-3, "{}", field.at(8, 8));
        // Standard keeps building.
        let standard = Brush {
            mode: Mode::Standard,
            ..brush
        };
        field.sculpt_from([8., 8.], &standard, Sculpt::Raise, Some(&base));
        assert!((field.at(8, 8) - 4.).abs() < 1e-3);
        // Fill only raises, Scrape only lowers.
        field.sculpt([8., 8.], &standard, Sculpt::Fill(1.));
        assert!((field.at(8, 8) - 4.).abs() < 1e-3);
        field.sculpt(
            [8., 8.],
            &Brush {
                strength: 1.,
                ..standard
            },
            Sculpt::Scrape(1.),
        );
        assert!((field.at(8, 8) - 1.).abs() < 1e-3);
    }

    #[test]
    fn strokes_space_dabs_evenly() {
        let mut stroke = Stroke::default();
        assert_eq!(stroke.to([0., 0.], 2.), vec![[0., 0.]]);
        assert!(stroke.to([1., 0.], 2.).is_empty());
        assert_eq!(stroke.to([5., 0.], 2.), vec![[2., 0.], [4., 0.]]);
        assert_eq!(stroke.to([6., 0.], 2.), vec![[6., 0.]]);
    }
}
