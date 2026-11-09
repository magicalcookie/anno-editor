// Small vector and column-major 4x4 matrix helpers on plain arrays.
//
// Matrices use the layout game formats and the GPU shaders share: columns are
// contiguous, so the translation is `m[12..15]`.

pub type Vec3 = [f32; 3];
pub type Mat4 = [f32; 16];

pub const IDENTITY: Mat4 = [
    1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
];

pub fn add<const N: usize>(a: [f32; N], b: [f32; N]) -> [f32; N] {
    std::array::from_fn(|i| a[i] + b[i])
}

pub fn sub<const N: usize>(a: [f32; N], b: [f32; N]) -> [f32; N] {
    std::array::from_fn(|i| a[i] - b[i])
}

pub fn scale<const N: usize>(a: [f32; N], v: f32) -> [f32; N] {
    a.map(|x| x * v)
}

pub fn dot<const N: usize>(a: [f32; N], b: [f32; N]) -> f32 {
    a.iter().zip(b).map(|(a, b)| a * b).sum()
}

pub fn length<const N: usize>(a: [f32; N]) -> f32 {
    dot(a, a).sqrt()
}

pub fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn normalize(v: Vec3) -> Option<Vec3> {
    let length = length(v);
    (length > 1e-6).then(|| scale(v, 1. / length))
}

pub fn translation(m: &Mat4) -> Vec3 {
    [m[12], m[13], m[14]]
}

pub fn translate(p: Vec3) -> Mat4 {
    let mut m = IDENTITY;
    m[12..15].copy_from_slice(&p);
    m
}

pub fn transform(m: &Mat4, p: Vec3) -> Vec3 {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

/// `a * b`: applies `b` first, then `a`.
pub fn multiply(a: &Mat4, b: &Mat4) -> Mat4 {
    std::array::from_fn(|i| {
        let (col, row) = (i / 4, i % 4);
        (0..4).map(|k| a[k * 4 + row] * b[col * 4 + k]).sum()
    })
}

/// Gauss-Jordan in f64. Native transforms can carry shear, reflection and
/// large translations, so neither an affine shortcut nor f32 pivots are safe.
pub fn inverse(m: &Mat4) -> Option<Mat4> {
    if !m.iter().all(|v| v.is_finite()) {
        return None;
    }
    let mut rows = [[0.0f64; 8]; 4];
    for (row, values) in rows.iter_mut().enumerate() {
        for col in 0..4 {
            values[col] = f64::from(m[row * 4 + col]);
        }
        values[row + 4] = 1.0;
    }
    for col in 0..4 {
        let pivot = (col..4).max_by(|a, b| rows[*a][col].abs().total_cmp(&rows[*b][col].abs()))?;
        if rows[pivot][col] == 0.0 {
            return None;
        }
        rows.swap(col, pivot);
        let scale = rows[col][col];
        for value in &mut rows[col] {
            *value /= scale;
        }
        let pivot_values = rows[col];
        for (row, values) in rows.iter_mut().enumerate() {
            if row == col {
                continue;
            }
            let factor = values[col];
            for (value, pivot) in values.iter_mut().zip(pivot_values) {
                *value -= factor * pivot;
            }
        }
    }
    let mut result = [0.0; 16];
    for row in 0..4 {
        for col in 0..4 {
            result[row * 4 + col] = rows[row][col + 4] as f32;
        }
    }
    result.iter().all(|v| v.is_finite()).then_some(result)
}

/// The eight corners of an axis-aligned box, bit `a` of the index selecting max on axis `a`.
pub fn corners(bounds: [Vec3; 2]) -> [Vec3; 8] {
    std::array::from_fn(|i| std::array::from_fn(|a| bounds[(i >> a) & 1][a]))
}

/// Bounds of `points`, or an inverted empty box when there are none.
pub fn bounds(points: impl IntoIterator<Item = Vec3>) -> [Vec3; 2] {
    let mut b = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
    for p in points {
        for a in 0..3 {
            b[0][a] = b[0][a].min(p[a]);
            b[1][a] = b[1][a].max(p[a]);
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_undoes_sheared_reflected_transforms() {
        let m = [
            2., 0.5, 0., 0., 0., -1., 0., 0., 0.3, 0., 4., 0., 100., -20., 5000., 1.,
        ];
        let i = inverse(&m).unwrap();
        let p = [3., -7., 11.];
        let back = transform(&i, transform(&m, p));
        assert!(back.iter().zip(p).all(|(a, b)| (a - b).abs() < 1e-3));
        let product = multiply(&m, &i);
        assert!(
            product
                .iter()
                .zip(IDENTITY)
                .all(|(a, b)| (a - b).abs() < 1e-4)
        );
        assert!(inverse(&[0.; 16]).is_none());
    }

    #[test]
    fn multiply_applies_the_right_operand_first() {
        let t = translate([1., 2., 3.]);
        let mut s = IDENTITY;
        s[0] = 2.;
        assert_eq!(transform(&multiply(&t, &s), [1., 0., 0.]), [3., 2., 3.]);
        assert_eq!(transform(&multiply(&s, &t), [1., 0., 0.]), [4., 2., 3.]);
    }
}
