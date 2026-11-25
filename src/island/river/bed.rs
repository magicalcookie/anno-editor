// Owns how a river's bed is generated along its curve, shaped as shipped
// rivers' beds are: a shallow sandy channel whose deepest line swings to the
// outside of bends (leaving a point bar inside), pools and riffles along it,
// low banks easing into the ground, and the ground materials each part
// takes. Measured on five roman pool islands' rivers (median, narrow and
// wide): centre 0.27 m under the water, waterline at about half the width,
// the curve's edge 0.35-0.5 m above it, banks 0.9 m up four metres out;
// `beach_04` under three quarters of the bed, `beach_05` along the edges,
// `cliff_limestone` on steep banks, `river_pebbles_01` down the middle of
// wide rivers.
use super::Sample;
use crate::island::doc::Materials;
use crate::terrain::Palette;
use anno_core::game::GroundNames;
use gpui_editor_kit::heightfield::Heightfield;
use std::f32::consts::TAU;

/// The bed generator's settings, the River tool's Bed fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bed {
    /// Metres under the surface along the deepest line.
    pub depth: f32,
    /// Metres the bank tops out above the surface.
    pub bank: f32,
    /// Metres past the curve's edge the bank takes to rise to `bank`.
    pub reach: f32,
    /// 0..1: how far bends push the deepest line toward the outer bank and
    /// deepen it there.
    pub bends: f32,
    /// 0..1: pools and riffles along the river, and ripples in the sand.
    pub pools: f32,
}

impl Default for Bed {
    fn default() -> Self {
        Self {
            depth: 0.3,
            bank: 0.9,
            reach: 4.,
            bends: 0.7,
            pools: 0.5,
        }
    }
}

/// The curve's edge stands this share of `bank` above the water: shipped
/// edges sit 0.35-0.5 m up under 0.9 m banks.
const EDGE_SHARE: f32 = 0.45;
/// Past the bank's reach the ground is cut no steeper than this (27°),
/// and not at all farther than `CUT` metres out.
const CUT_SLOPE: f32 = 0.5;
const CUT: f32 = 20.;
/// Side of the buckets segments are filed in for nearest lookups, metres.
const BUCKET: f32 = 8.;

/// The stretches of a river an edit changed, by distance along it: a
/// re-carve rewrites those and blends into the ground around them over
/// `feather` metres, leaving the rest of the bed as it was.
#[derive(Clone, Debug, Default)]
pub(super) struct Stretches {
    pub core: Vec<(f32, f32)>,
    pub feather: f32,
}

impl Stretches {
    /// 1 inside a stretch, easing to 0 `feather` metres past its ends.
    pub fn weight(&self, along: f32) -> f32 {
        self.core
            .iter()
            .map(|&(a, b)| {
                let gap = (a - along).max(along - b).max(0.);
                1. - smoothstep(0., self.feather.max(1e-3), gap)
            })
            .fold(0., f32::max)
    }

    pub fn is_empty(&self) -> bool {
        self.core.is_empty()
    }
}

/// The river's centre line, filed for finding the nearest stretch: per
/// sample its distance along the river and how sharply it bends there.
pub(super) struct Channel<'a> {
    samples: &'a [Sample],
    along: Vec<f32>,
    /// Signed curvature times half the width: positive turns toward the
    /// left normal (`-dir.z, dir.x`), so the outer bank is the other side.
    bend: Vec<f32>,
    /// Segment `k` (samples `k`, `k + 1`) filed by bucket, compressed rows.
    origin: [f32; 2],
    size: [usize; 2],
    starts: Vec<u32>,
    segments: Vec<u32>,
}

/// Where a point lies relative to the river.
#[derive(Clone, Copy, Debug)]
pub(super) struct Hit {
    /// Signed distance across, positive on the left normal's side.
    pub across: f32,
    pub half: f32,
    pub surface: f32,
    pub along: f32,
    pub bend: f32,
    /// The left normal there, unit length: `p - normal * across` is on the
    /// centre line.
    pub normal: [f32; 2],
}

impl<'a> Channel<'a> {
    /// `reach`: farthest past the curve's edge anything is asked.
    pub fn new(samples: &'a [Sample], reach: f32) -> Option<Self> {
        if samples.len() < 2 {
            return None;
        }
        let n = samples.len();
        let mut along = vec![0f32; n];
        for k in 1..n {
            let (a, b) = (samples[k - 1].p, samples[k].p);
            along[k] = along[k - 1] + (b[0] - a[0]).hypot(b[2] - a[2]);
        }
        let angle: Vec<f32> = samples.iter().map(|s| s.dir[1].atan2(s.dir[0])).collect();
        let raw: Vec<f32> = (0..n)
            .map(|k| {
                let (a, b) = (k.saturating_sub(1), (k + 1).min(n - 1));
                let turn = (angle[b] - angle[a] + TAU / 2.).rem_euclid(TAU) - TAU / 2.;
                turn / (along[b] - along[a]).max(1e-3)
            })
            .collect();
        // Averaged over about a width either side: a bend shapes the bed
        // over its whole arc, not at one sample.
        let bend = (0..n)
            .map(|k| {
                let window = samples[k].w.max(4.);
                let (mut sum, mut count) = (0., 0.);
                let mut j = k;
                while j > 0 && along[k] - along[j - 1] <= window {
                    j -= 1;
                }
                while j < n && along[j] - along[k] <= window {
                    sum += raw[j];
                    count += 1.;
                    j += 1;
                }
                sum / count * samples[k].w / 2.
            })
            .collect();
        let pad = samples.iter().map(|s| s.w / 2.).fold(0., f32::max) + reach;
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for s in samples {
            lo = [lo[0].min(s.p[0] - pad), lo[1].min(s.p[2] - pad)];
            hi = [hi[0].max(s.p[0] + pad), hi[1].max(s.p[2] + pad)];
        }
        let size = [0, 1].map(|a| ((hi[a] - lo[a]) / BUCKET).ceil() as usize + 1);
        let bucket = |v: f32, a: usize| (((v - lo[a]) / BUCKET).max(0.) as usize).min(size[a] - 1);
        // Two passes: count per bucket, then fill.
        let mut counts = vec![0u32; size[0] * size[1] + 1];
        let each = |fill: &mut dyn FnMut(usize, u32)| {
            for k in 0..n - 1 {
                let (a, b) = (samples[k].p, samples[k + 1].p);
                let r = samples[k].w.max(samples[k + 1].w) / 2. + reach;
                for z in bucket(a[2].min(b[2]) - r, 1)..=bucket(a[2].max(b[2]) + r, 1) {
                    for x in bucket(a[0].min(b[0]) - r, 0)..=bucket(a[0].max(b[0]) + r, 0) {
                        fill(z * size[0] + x, k as u32);
                    }
                }
            }
        };
        each(&mut |i, _| counts[i + 1] += 1);
        for i in 1..counts.len() {
            counts[i] += counts[i - 1];
        }
        let starts = counts.clone();
        let mut segments = vec![0u32; *counts.last().unwrap_or(&0) as usize];
        let mut next = counts;
        each(&mut |i, k| {
            segments[next[i] as usize] = k;
            next[i] += 1;
        });
        Some(Self {
            samples,
            along,
            bend,
            origin: lo,
            size,
            starts,
            segments,
        })
    }

    /// The nearest stretch of the river to `p`, if one was filed there.
    pub fn at(&self, p: [f32; 2]) -> Option<Hit> {
        let b = [0, 1].map(|a| (p[a] - self.origin[a]) / BUCKET);
        if b[0] < 0. || b[1] < 0. || b[0] as usize >= self.size[0] || b[1] as usize >= self.size[1]
        {
            return None;
        }
        let i = b[1] as usize * self.size[0] + b[0] as usize;
        let mut best: Option<(f32, Hit)> = None;
        for &k in &self.segments[self.starts[i] as usize..self.starts[i + 1] as usize] {
            let k = k as usize;
            let (a, c) = (&self.samples[k], &self.samples[k + 1]);
            let (dx, dz) = (c.p[0] - a.p[0], c.p[2] - a.p[2]);
            let l2 = dx * dx + dz * dz;
            let t = if l2 > 0. {
                (((p[0] - a.p[0]) * dx + (p[1] - a.p[2]) * dz) / l2).clamp(0., 1.)
            } else {
                0.
            };
            let (ox, oz) = (p[0] - a.p[0] - t * dx, p[1] - a.p[2] - t * dz);
            let d = ox.hypot(oz);
            if best.as_ref().is_some_and(|(bd, _)| *bd <= d) {
                continue;
            }
            let l = l2.sqrt().max(1e-6);
            let side = (-dz * ox + dx * oz) / l;
            let lerp = |u: f32, v: f32| u + (v - u) * t;
            best = Some((
                d,
                Hit {
                    across: if side < 0. { -d } else { d },
                    half: lerp(a.w, c.w) / 2.,
                    surface: lerp(a.p[1], c.p[1]),
                    along: lerp(self.along[k], self.along[k + 1]),
                    bend: lerp(self.bend[k], self.bend[k + 1]),
                    normal: [-dz / l, dx / l],
                },
            ));
        }
        best.map(|(_, h)| h)
    }
}

impl Bed {
    /// How strongly the bend at `h` shapes the bed, 0..1.
    fn bendiness(&self, h: &Hit) -> f32 {
        (h.bend.abs() * 2.5).min(1.) * self.bends
    }

    /// Where the deepest line runs across, shifted toward the outer bank.
    fn thalweg(&self, h: &Hit) -> f32 {
        -h.bend.signum() * self.bendiness(h) * 0.6 * h.half
    }

    /// How far the bank takes to rise on `h`'s side: longer on the inside
    /// of a bend (the point bar), shorter on the outside.
    fn reach_at(&self, h: &Hit) -> f32 {
        let inner = h.across.signum() == h.bend.signum();
        let k = self.bendiness(h);
        self.reach * if inner { 1. + k } else { 1. - 0.4 * k }
    }

    /// The bed's height inside the curve's width at `p`: deepest along the
    /// thalweg, deeper in pools (at bends and every seven widths), rising
    /// to the edge as shipped profiles do (`u^1.6`, waterline near half
    /// the width), with ripples in the sand.
    fn channel(&self, h: &Hit, p: [f32; 2]) -> f32 {
        let edge = self.bank * EDGE_SHARE;
        let o = self.thalweg(h);
        let span = if h.across >= o {
            h.half - o
        } else {
            h.half + o
        };
        let u = ((h.across - o).abs() / span.max(1e-3)).min(1.);
        let wave = (TAU * h.along / (7. * (2. * h.half).max(2.))).sin();
        let pool = (1. + self.pools * (0.6 * self.bendiness(h) + 0.35 * wave)).max(0.3);
        let depth = self.depth * pool;
        let ripple = self.pools * 0.05 * (noise([p[0] * 0.8, p[1] * 0.8]) - 0.5);
        h.surface - depth + (depth + edge) * u.powf(1.6) + ripple * (1. - u)
    }

    /// Lowest and highest the ground may be `x` metres past the edge: the
    /// bank rises from the edge to `bank` over its reach (raising ground
    /// lower than that near the water, so the water's edge never shows),
    /// then cuts no steeper than [`CUT_SLOPE`].
    fn bank_limits(&self, h: &Hit, x: f32) -> (f32, f32) {
        let edge = self.bank * EDGE_SHARE;
        let reach = self.reach_at(h).max(0.5);
        let floor = h.surface + edge - (x / reach) * (edge + 1.);
        let ceiling = if x <= reach {
            h.surface + edge + (self.bank - edge) * smoothstep(0., reach, x)
        } else {
            let y = x - reach;
            h.surface + self.bank + CUT_SLOPE * y * y / (y + reach)
        };
        (floor, ceiling.max(floor))
    }

    /// The farthest past the edge the bed changes the ground.
    fn extent(&self) -> f32 {
        self.reach * 2. + CUT
    }

    /// Shapes the ground along the river, or along `only` its changed
    /// stretches: the channel set to its profile, banks held between
    /// their limits. Carving the same river again changes nothing.
    pub(super) fn carve(
        &self,
        field: &mut Heightfield,
        samples: &[Sample],
        only: Option<&Stretches>,
    ) {
        let Some(channel) = Channel::new(samples, self.extent()) else {
            return;
        };
        for (i, p) in heights_near(field, samples, self.extent()) {
            let Some(h) = channel.at(p) else { continue };
            let w = only.map_or(1., |o| o.weight(h.along));
            if w <= 0. {
                continue;
            }
            let past = h.across.abs() - h.half;
            let old = field.heights[i];
            let new = if past <= 0. {
                self.channel(&h, p)
            } else if past <= self.reach_at(&h) + CUT {
                let (floor, ceiling) = self.bank_limits(&h, past);
                old.clamp(floor, ceiling)
            } else {
                old
            };
            field.heights[i] = old + (new - old) * w;
        }
    }

    /// Fills the bed and banks of `samples`, a river's old course, along
    /// `only` the stretches an edit moved it away from, so a river moved
    /// sideways leaves no empty trench behind. The ground from before the
    /// first carve is kept nowhere: across the old bed it goes to a
    /// straight line between the ground just past either bank, the best
    /// guess level ground allows. Carve the new course after.
    pub(super) fn fill(&self, field: &mut Heightfield, samples: &[Sample], only: &Stretches) {
        let Some(channel) = Channel::new(samples, self.reach * 2.) else {
            return;
        };
        // Read from as it was: the ground past one bank can be another
        // stretch's bank, filled earlier in the same pass.
        let before = field.clone();
        for (i, p) in heights_near(field, samples, self.reach * 2.) {
            let Some(h) = channel.at(p) else { continue };
            let w = only.weight(h.along);
            let r = h.half + self.reach_at(&h);
            if w <= 0. || h.across.abs() > r {
                continue;
            }
            let n = h.normal;
            let centre = [p[0] - n[0] * h.across, p[1] - n[1] * h.across];
            let out = r + 1.;
            let side = |s: f32| before.height([centre[0] + n[0] * s, centre[1] + n[1] * s]);
            let (left, right) = (side(out), side(-out));
            let target = right + (left - right) * (h.across + out) / (2. * out);
            let old = field.heights[i];
            field.heights[i] = old + (target - old) * w;
        }
    }

    /// Paints the bed's materials, after carving (rock goes where the
    /// carved banks are steep): sand under the water fading out over the
    /// banks, the edge's sand along the waterline, pebbles down the middle
    /// of wide rivers, rock on steep banks. Returns the painted rectangle.
    pub(super) fn paint(
        &self,
        materials: &mut Materials,
        names: &GroundNames,
        palette: &Palette,
        field: &Heightfield,
        samples: &[Sample],
        only: Option<&Stretches>,
    ) -> Option<[f32; 4]> {
        let reach = self.reach * 2.;
        let channel = Channel::new(samples, reach)?;
        let pad = samples.iter().map(|s| s.w / 2.).fold(0., f32::max) + reach;
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for s in samples {
            lo = [lo[0].min(s.p[0] - pad), lo[1].min(s.p[2] - pad)];
            hi = [hi[0].max(s.p[0] + pad), hi[1].max(s.p[2] + pad)];
        }
        let roles = Roles::of(names, palette)?;
        // The river's stretch at `p` and how much of it this paint touches.
        let at = |p: [f32; 2]| {
            let h = channel.at(p)?;
            let w = only.map_or(1., |o| o.weight(h.along));
            (w > 0.).then_some((h, w))
        };
        let sand = |p: [f32; 2]| {
            let Some((h, w)) = at(p) else { return 0. };
            let past = h.across.abs() - h.half;
            let r = self.reach_at(&h);
            let fade = 1.
                - smoothstep(
                    0.5 * r,
                    1.5 * r,
                    past + 0.6 * r * (noise(scaled(p, 0.3)) - 0.5),
                );
            0.9 * fade * w
        };
        materials.paint_with(lo, hi, roles.sand, sand);
        if let Some(edge) = roles.edge {
            let along_edge = |p: [f32; 2]| {
                let Some((h, w)) = at(p) else { return 0. };
                let past = h.across.abs() - h.half;
                let width = 0.3 * h.half + 0.4 * self.reach_at(&h);
                let near = (1. - (past - 0.2 * h.half).abs() / width.max(0.5)).max(0.);
                0.7 * w * near * smoothstep(0.35, 0.6, noise(scaled(p, 0.5)))
            };
            materials.paint_with(lo, hi, edge, along_edge);
        }
        if let Some(pebbles) = roles.pebbles {
            let middle = |p: [f32; 2]| {
                let Some((h, w)) = at(p) else { return 0. };
                let u = (h.across - self.thalweg(&h)).abs() / h.half.max(1e-3);
                let wide = smoothstep(2., 4., h.half);
                0.85 * w
                    * wide
                    * (1. - u / 0.45).max(0.)
                    * smoothstep(0.5, 0.7, noise(scaled(p, 0.35)))
            };
            materials.paint_with(lo, hi, pebbles, middle);
        }
        if let Some(rock) = roles.rock {
            let steep = |p: [f32; 2]| {
                let Some((h, w)) = at(p) else { return 0. };
                if h.across.abs() <= h.half {
                    return 0.;
                }
                let gx = field.height([p[0] + 0.5, p[1]]) - field.height([p[0] - 0.5, p[1]]);
                let gz = field.height([p[0], p[1] + 0.5]) - field.height([p[0], p[1] - 0.5]);
                w * smoothstep(0.55, 0.9, gx.hypot(gz))
            };
            materials.paint_with(lo, hi, rock, steep);
        }
        Some([lo[0], lo[1], hi[0], hi[1]])
    }
}

/// Height points within `pad` metres past the widest stretch of `samples`,
/// as (index, position).
fn heights_near(
    field: &Heightfield,
    samples: &[Sample],
    pad: f32,
) -> impl Iterator<Item = (usize, [f32; 2])> + use<> {
    let g = field.grid;
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    let pad = samples.iter().map(|s| s.w / 2.).fold(0., f32::max) + pad;
    for s in samples {
        lo = [lo[0].min(s.p[0] - pad), lo[1].min(s.p[2] - pad)];
        hi = [hi[0].max(s.p[0] + pad), hi[1].max(s.p[2] + pad)];
    }
    let span = move |a: usize, n: usize| {
        let first = ((lo[a] - g.origin[a]) / g.spacing).floor().max(0.) as usize;
        let last = (((hi[a] - g.origin[a]) / g.spacing).ceil().max(0.) as usize).min(n - 1);
        first..=last
    };
    let xs = span(0, g.width);
    span(1, g.depth).flat_map(move |z| {
        xs.clone().map(move |x| {
            let p = [
                g.origin[0] + x as f32 * g.spacing,
                g.origin[1] + z as f32 * g.spacing,
            ];
            (g.index(x, z), p)
        })
    })
}

/// The palette materials a bed paints: the game's names for them
/// (`GroundNames::painted`), from what shipped rivers use, with the nearest
/// the palette has otherwise.
struct Roles {
    sand: u8,
    edge: Option<u8>,
    pebbles: Option<u8>,
    rock: Option<u8>,
}

impl Roles {
    fn of(names: &GroundNames, palette: &Palette) -> Option<Self> {
        let pick = |role: &str| names.pick(names.painted(role), files(palette));
        let sand = sand(names, palette)?;
        Some(Self {
            sand,
            edge: pick("bed edge").filter(|&e| e != sand),
            pebbles: pick("bed pebbles").filter(|&p| p != sand),
            rock: pick("bed rock")
                .or_else(|| palette.rock().and_then(|m| palette_id(palette, &m.file))),
        })
    }
}

/// The bed's sand, which lake shores take too; none when the game names
/// none in the palette.
pub(super) fn sand(names: &GroundNames, palette: &Palette) -> Option<u8> {
    names.pick(names.painted("bed sand"), files(palette))
}

fn files(palette: &Palette) -> impl Iterator<Item = (u8, &str)> + Clone {
    (palette.materials.iter()).map(|(&id, m)| (id, m.file.as_str()))
}

fn palette_id(palette: &Palette, file: &str) -> Option<u8> {
    palette
        .materials
        .iter()
        .find_map(|(&id, m)| (m.file == file).then_some(id))
}

fn scaled(p: [f32; 2], k: f32) -> [f32; 2] {
    [p[0] * k, p[1] * k]
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// Value noise, 0..1, a feature per unit.
fn noise(p: [f32; 2]) -> f32 {
    let hash = |x: i32, z: i32| {
        let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (z as u32).wrapping_mul(0xd816_3841);
        h ^= h >> 13;
        h = h.wrapping_mul(0x85eb_ca6b);
        h ^= h >> 16;
        (h & 0xffff) as f32 / 65535.
    };
    let (fx, fz) = (p[0].floor(), p[1].floor());
    let (tx, tz) = (p[0] - fx, p[1] - fz);
    let (sx, sz) = (tx * tx * (3. - 2. * tx), tz * tz * (3. - 2. * tz));
    let (x, z) = (fx as i32, fz as i32);
    let top = hash(x, z) + (hash(x + 1, z) - hash(x, z)) * sx;
    let bottom = hash(x, z + 1) + (hash(x + 1, z + 1) - hash(x, z + 1)) * sx;
    top + (bottom - top) * sz
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_editor_kit::heightfield::Grid;

    fn straight(w: f32) -> Vec<Sample> {
        (0..=100)
            .map(|i| Sample {
                p: [i as f32, 5., 50.],
                w,
                dir: [1., 0.],
            })
            .collect()
    }

    fn plain(height: f32) -> Heightfield {
        let grid = Grid {
            width: 101,
            depth: 101,
            spacing: 1.,
            origin: [0., 0.],
        };
        Heightfield::new(grid, vec![height; grid.len()])
    }

    // Across a straight river on flat ground two metres above its water:
    // the bed follows the shipped profile, deepest on the centre line,
    // dry by the curve's edge, banks up to the bank height four metres
    // out and the plain untouched past the cut.
    #[test]
    fn a_straight_bed_follows_the_shipped_profile() {
        let bed = Bed {
            pools: 0.,
            ..Bed::default()
        };
        let mut field = plain(7.);
        bed.carve(&mut field, &straight(8.), None);
        let at = |z: f32| field.height([50., z]) - 5.;
        assert!((at(50.) + 0.3).abs() < 0.02, "centre {}", at(50.));
        // Shipped: waterline between 40 % and 60 % of the half width.
        let wet = (0..=40)
            .map(|i| 50. + i as f32 * 0.1)
            .take_while(|&z| at(z) < 0.)
            .last()
            .unwrap();
        let share = (wet - 50.) / 4.;
        assert!((0.4..=0.65).contains(&share), "waterline at {share}");
        assert!((at(54.) - 0.405).abs() < 0.05, "edge {}", at(54.));
        assert!((at(58.) - 0.9).abs() < 0.05, "bank top {}", at(58.));
        // Symmetric when it does not bend.
        assert!((at(47.) - at(53.)).abs() < 1e-3);
        // Far out the ground is as it was, and carving again changes nothing.
        assert_eq!(at(50. - 30.), 2.);
        let once = field.heights.clone();
        bed.carve(&mut field, &straight(8.), None);
        assert_eq!(field.heights, once);
    }

    // A bend to the left (toward +z) puts the deepest water on its right,
    // outer bank, and a shallow point bar on the inside.
    #[test]
    fn bends_push_the_deep_water_outward() {
        let r = 25.;
        let samples: Vec<Sample> = (0..=60)
            .map(|i| {
                let a = -0.6 + i as f32 / 60. * 1.2;
                let (s, c) = a.sin_cos();
                Sample {
                    p: [50. + r * s, 5., 50. + r - r * c],
                    w: 8.,
                    dir: [c, s],
                }
            })
            .collect();
        let bed = Bed {
            pools: 0.,
            ..Bed::default()
        };
        let mut field = plain(7.);
        bed.carve(&mut field, &samples, None);
        // At the apex (50, 50) the river runs +x; left is +z, the inside.
        let at = |z: f32| field.height([50., z]) - 5.;
        let deepest = (-40..=40)
            .map(|i| 50. + i as f32 * 0.1)
            .min_by(|a, b| at(*a).total_cmp(&at(*b)))
            .unwrap();
        assert!(deepest < 49.5, "deepest line at z = {deepest}");
        assert!(at(52.) > at(48.), "inner side shallower");
    }

    // The bucketed lookup finds the same nearest stretch as a scan.
    #[test]
    fn channel_finds_the_nearest_stretch() {
        let samples = straight(6.);
        let channel = Channel::new(&samples, 10.).unwrap();
        let h = channel.at([30.2, 53.]).unwrap();
        assert!((h.across - 3.).abs() < 1e-4 && (h.along - 30.2).abs() < 1e-3);
        assert!(
            channel
                .at([30., 50. - 3. - 10.5])
                .is_none_or(|h| h.across.abs() > 13.)
        );
    }
}
