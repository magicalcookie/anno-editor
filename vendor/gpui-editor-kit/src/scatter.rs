// The scatter brush: where to add props under a brush dab to reach a density
// with a minimum spacing, and which to thin out. The host owns the props;
// this only decides positions, variation and removals on the X/Z plane.
use crate::heightfield::Brush;
use std::f32::consts::{PI, TAU};

/// Per-dab cap on new props, so a large brush on an empty area stays
/// interactive; strokes keep filling on the next dabs.
pub const MAX_PER_DAB: usize = 48;
/// Random tries per prop wanted; most fail on spacing in dense areas.
const TRIES_PER_PROP: usize = 4;

#[derive(Clone, Debug)]
pub struct Scatter {
    /// Props per 100 m² at full brush weight.
    pub density: f32,
    /// Uniform scale varies by up to this fraction either way.
    pub scale_jitter: f32,
    seed: u64,
}

impl Default for Scatter {
    fn default() -> Self {
        Self {
            density: 4.,
            scale_jitter: 0.2,
            seed: 0x9e37_79b9_7f4a_7c15,
        }
    }
}

/// One prop to add.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// World X/Z.
    pub at: [f32; 2],
    /// Rotation about Y, radians.
    pub yaw: f32,
    /// Uniform scale multiplier.
    pub scale: f32,
    /// Index into the host's set of prop variants.
    pub choice: usize,
}

impl Scatter {
    /// xorshift64*, 0..1. Reproducibility is not a goal; speed is.
    fn random(&mut self) -> f32 {
        self.seed ^= self.seed >> 12;
        self.seed ^= self.seed << 25;
        self.seed ^= self.seed >> 27;
        (self.seed.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Minimum distance between scattered props for the density.
    pub fn spacing(&self) -> f32 {
        (100. / self.density.max(0.01)).sqrt() * 0.6
    }

    /// Props to add under a dab at `center` toward the density. `set` holds
    /// the X/Z of props already in the set; spacing is kept only from those,
    /// so grass scattered into a forest is not blocked by its trees.
    /// `accept` can veto a spot, e.g. under water. `choices` is the number
    /// of variants to pick from; with none, nothing is placed.
    pub fn fill(
        &mut self,
        center: [f32; 2],
        brush: &Brush,
        set: impl IntoIterator<Item = [f32; 2]>,
        choices: usize,
        mut accept: impl FnMut([f32; 2]) -> bool,
    ) -> Vec<Placement> {
        if choices == 0 || self.density <= 0. {
            return Vec::new();
        }
        let r = brush.radius;
        let spacing = self.spacing();
        let distance = |p: [f32; 2]| (p[0] - center[0]).hypot(p[1] - center[1]);
        let mut nearby: Vec<[f32; 2]> = set
            .into_iter()
            .filter(|p| distance(*p) < r + spacing)
            .collect();
        let ours = nearby.iter().filter(|p| distance(**p) < r).count();
        let wanted = self.density / 100. * PI * r * r;
        let missing = (wanted - ours as f32).max(0.) * brush.strength.clamp(0.05, 1.);
        let count = (missing.ceil() as usize).min(MAX_PER_DAB);
        let mut placed = Vec::with_capacity(count);
        for _ in 0..count * TRIES_PER_PROP {
            if placed.len() == count {
                break;
            }
            // sqrt spreads tries evenly over the disc's area.
            let (a, d) = (self.random() * TAU, self.random().sqrt() * r);
            let at = [center[0] + a.cos() * d, center[1] + a.sin() * d];
            if self.random() > brush.weight(d)
                || nearby
                    .iter()
                    .any(|p| (p[0] - at[0]).hypot(p[1] - at[1]) < spacing)
                || !accept(at)
            {
                continue;
            }
            let choice = ((self.random() * choices as f32) as usize).min(choices - 1);
            let yaw = self.random() * TAU;
            let scale = 1. + (self.random() * 2. - 1.) * self.scale_jitter;
            nearby.push(at);
            placed.push(Placement {
                at,
                yaw,
                scale,
                choice,
            });
        }
        placed
    }

    /// Whether to remove the prop at `at` on a thinning dab: chance is brush
    /// weight times strength, so repeated dabs clear an area gradually.
    pub fn thin(&mut self, center: [f32; 2], brush: &Brush, at: [f32; 2]) -> bool {
        let w = brush.weight((at[0] - center[0]).hypot(at[1] - center[1]));
        w > 0. && self.random() < w * brush.strength
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brush(radius: f32, strength: f32) -> Brush {
        Brush {
            radius,
            strength,
            ..Default::default()
        }
    }

    #[test]
    fn repeated_dabs_approach_the_density_and_keep_spacing() {
        let mut scatter = Scatter {
            density: 10.,
            ..Default::default()
        };
        let b = brush(10., 1.);
        let mut props: Vec<[f32; 2]> = Vec::new();
        for _ in 0..20 {
            let placed = scatter.fill([0., 0.], &b, props.iter().copied(), 3, |_| true);
            assert!(placed.len() <= MAX_PER_DAB);
            assert!(
                placed
                    .iter()
                    .all(|p| p.choice < 3 && (0.8..=1.2).contains(&p.scale))
            );
            props.extend(placed.iter().map(|p| p.at));
        }
        let wanted = 10. / 100. * PI * 100.;
        assert!(props.len() as f32 <= wanted.ceil(), "{} props", props.len());
        assert!(props.len() as f32 > wanted * 0.5, "{} props", props.len());
        for (i, a) in props.iter().enumerate() {
            assert!((a[0]).hypot(a[1]) <= 10.);
            for b in &props[i + 1..] {
                assert!((a[0] - b[0]).hypot(a[1] - b[1]) >= scatter.spacing());
            }
        }
    }

    #[test]
    fn vetoed_spots_and_empty_sets_place_nothing() {
        let mut scatter = Scatter::default();
        let b = brush(8., 1.);
        assert!(scatter.fill([0., 0.], &b, [], 0, |_| true).is_empty());
        let placed = scatter.fill([0., 0.], &b, [], 2, |at| at[0] > 0.);
        assert!(!placed.is_empty() && placed.iter().all(|p| p.at[0] > 0.));
    }

    #[test]
    fn thinning_only_touches_props_under_the_brush() {
        let mut scatter = Scatter::default();
        let b = brush(5., 1.);
        assert!(!(0..100).any(|_| scatter.thin([0., 0.], &b, [6., 0.])));
        assert!((0..100).any(|_| scatter.thin([0., 0.], &b, [1., 0.])));
    }
}
