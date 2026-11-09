//! Column-major, double-precision GL transforms and bounded matrix stacks.

use crate::Error;

/// Column-major 4×4 matrix; index is `column * 4 + row`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4(pub [f64; 16]);

impl Default for Mat4 {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mat4 {
    /// Identity transform.
    pub const IDENTITY: Self =
        Self([1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);

    /// `self * rhs`, for GL's post-multiplied current matrix.
    pub fn post_mul(self, rhs: Self) -> Self {
        let mut out = [0.0; 16];
        for column in 0..4 {
            for row in 0..4 {
                out[column * 4 + row] =
                    (0..4).map(|k| self.0[k * 4 + row] * rhs.0[column * 4 + k]).sum();
            }
        }
        Self(out)
    }

    /// Transform a homogeneous column vector without perspective division.
    pub fn transform(self, v: [f64; 4]) -> [f64; 4] {
        std::array::from_fn(|row| (0..4).map(|k| self.0[k * 4 + row] * v[k]).sum())
    }

    /// Translation, matching `glTranslate`.
    pub fn translation(x: f64, y: f64, z: f64) -> Self {
        let mut m = Self::IDENTITY;
        m.0[12..15].copy_from_slice(&[x, y, z]);
        m
    }

    /// Nonuniform scale, matching `glScale`.
    pub fn scaling(x: f64, y: f64, z: f64) -> Self {
        let mut m = Self::IDENTITY;
        m.0[0] = x;
        m.0[5] = y;
        m.0[10] = z;
        m
    }

    /// Axis-angle rotation in degrees; a zero-length axis leaves the matrix unchanged.
    pub fn rotation(degrees: f64, axis: [f64; 3]) -> Self {
        let length = axis[0].hypot(axis[1]).hypot(axis[2]);
        if length == 0.0 {
            return Self::IDENTITY;
        }
        let [x, y, z] = axis.map(|a| a / length);
        let (s, c) = degrees.to_radians().sin_cos();
        let t = 1.0 - c;
        Self([
            t * x * x + c,
            t * x * y + s * z,
            t * x * z - s * y,
            0.0,
            t * x * y - s * z,
            t * y * y + c,
            t * y * z + s * x,
            0.0,
            t * x * z + s * y,
            t * y * z - s * x,
            t * z * z + c,
            0.0,
            0.0,
            0.0,
            0.0,
            1.0,
        ])
    }

    /// Orthographic projection with GL's -1..1 clip-space depth (not Vulkan depth).
    pub fn ortho(l: f64, r: f64, b: f64, t: f64, n: f64, f: f64) -> Result<Self, Error> {
        if ![l, r, b, t, n, f].iter().all(|x| x.is_finite()) || l == r || b == t || n == f {
            return Err(Error::InvalidValue);
        }
        Ok(Self([
            2.0 / (r - l),
            0.0,
            0.0,
            0.0,
            0.0,
            2.0 / (t - b),
            0.0,
            0.0,
            0.0,
            0.0,
            -2.0 / (f - n),
            0.0,
            -(r + l) / (r - l),
            -(t + b) / (t - b),
            -(f + n) / (f - n),
            1.0,
        ]))
    }

    /// Perspective frustum with positive, unequal near/far distances.
    pub fn frustum(l: f64, r: f64, b: f64, t: f64, n: f64, f: f64) -> Result<Self, Error> {
        if ![l, r, b, t, n, f].iter().all(|x| x.is_finite())
            || l == r
            || b == t
            || n == f
            || n <= 0.0
            || f <= 0.0
        {
            return Err(Error::InvalidValue);
        }
        Ok(Self([
            2.0 * n / (r - l),
            0.0,
            0.0,
            0.0,
            0.0,
            2.0 * n / (t - b),
            0.0,
            0.0,
            (r + l) / (r - l),
            (t + b) / (t - b),
            -(f + n) / (f - n),
            -1.0,
            0.0,
            0.0,
            -2.0 * f * n / (f - n),
            0.0,
        ]))
    }

    /// Inverse-transpose of the upper-left 3×3, column-major, for normal transforms.
    /// Singular or non-finite matrices return `None`; no fabricated inverse is used.
    pub fn normal_matrix(self) -> Option<[f64; 9]> {
        let a = [self.0[0], self.0[1], self.0[2]];
        let b = [self.0[4], self.0[5], self.0[6]];
        let c = [self.0[8], self.0[9], self.0[10]];
        let cross = |u: [f64; 3], v: [f64; 3]| {
            [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]]
        };
        let bc = cross(b, c);
        let ca = cross(c, a);
        let ab = cross(a, b);
        let det = a[0] * bc[0] + a[1] * bc[1] + a[2] * bc[2];
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let out = [bc[0], bc[1], bc[2], ca[0], ca[1], ca[2], ab[0], ab[1], ab[2]].map(|x| x / det);
        out.iter().all(|x| x.is_finite()).then_some(out)
    }
}

/// A stack with a permanent base element and a caller-chosen advertised depth.
/// Storage is reserved at creation; push/pop and all transforms allocate nothing.
#[derive(Debug)]
pub struct MatrixStack {
    entries: Vec<Mat4>,
    limit: usize,
}

impl MatrixStack {
    /// Create an identity stack; use depth >=32 for modelview, >=2 for projection/texture.
    pub fn new(limit: usize) -> Result<Self, Error> {
        if limit == 0 {
            return Err(Error::InvalidValue);
        }
        let mut entries = Vec::new();
        entries.try_reserve_exact(limit).map_err(|_| Error::Capacity)?;
        entries.push(Mat4::IDENTITY);
        Ok(Self { entries, limit })
    }

    /// Current number of matrices, including the base.
    pub fn depth(&self) -> usize {
        self.entries.len()
    }

    /// A copy of the current matrix.
    pub fn current(&self) -> Mat4 {
        self.entries[self.entries.len() - 1]
    }

    /// Replace the current matrix, matching `glLoadMatrix`.
    pub fn load(&mut self, matrix: Mat4) {
        let last = self.entries.len() - 1;
        self.entries[last] = matrix;
    }

    /// Post-multiply the current matrix, matching `glMultMatrix`.
    pub fn multiply(&mut self, matrix: Mat4) {
        self.load(self.current().post_mul(matrix));
    }

    /// Duplicate the current matrix, or report overflow without modifying the stack.
    pub fn push(&mut self) -> Result<(), Error> {
        if self.depth() == self.limit {
            return Err(Error::StackOverflow);
        }
        self.entries.push(self.current());
        Ok(())
    }

    /// Restore the previous matrix, or report underflow without modifying the stack.
    pub fn pop(&mut self) -> Result<(), Error> {
        if self.depth() == 1 {
            return Err(Error::StackUnderflow);
        }
        self.entries.pop();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[f64], b: &[f64]) {
        assert_eq!(a.len(), b.len());
        for (a, b) in a.iter().zip(b) {
            assert!((a - b).abs() < 1e-10, "{a} != {b}");
        }
    }

    #[test]
    fn column_major_translation() {
        assert_eq!(
            Mat4::translation(2.0, 3.0, 4.0).transform([1.0, 2.0, 3.0, 1.0]),
            [3.0, 5.0, 7.0, 1.0]
        );
        assert_eq!(
            Mat4::translation(2.0, 3.0, 4.0).transform([1.0, 2.0, 3.0, 0.0]),
            [1.0, 2.0, 3.0, 0.0]
        );
    }

    #[test]
    fn gl_post_multiplication_order() {
        let m = Mat4::translation(1.0, 2.0, 3.0).post_mul(Mat4::scaling(2.0, 3.0, 4.0));
        assert_eq!(m.transform([1.0, 1.0, 1.0, 1.0]), [3.0, 5.0, 7.0, 1.0]);
    }

    #[test]
    fn arbitrary_matrix_identities() {
        let m = Mat4(std::array::from_fn(|i| i as f64 - 7.0));
        assert_eq!(m.post_mul(Mat4::IDENTITY), m);
        assert_eq!(Mat4::IDENTITY.post_mul(m), m);
    }

    #[test]
    fn normalized_axis_rotation() {
        close(
            &Mat4::rotation(90.0, [0.0, 0.0, 20.0]).transform([1.0, 0.0, 0.0, 1.0]),
            &[0.0, 1.0, 0.0, 1.0],
        );
        assert_eq!(Mat4::rotation(90.0, [0.0; 3]), Mat4::IDENTITY);
    }

    #[test]
    fn rotation_inverse() {
        let m = Mat4::rotation(31.0, [1.0, 2.0, 3.0]);
        close(&m.post_mul(Mat4::rotation(-31.0, [1.0, 2.0, 3.0])).0, &Mat4::IDENTITY.0);
    }

    #[test]
    fn ortho_maps_clip_corners() {
        let m = Mat4::ortho(-2.0, 4.0, -3.0, 5.0, 1.0, 9.0).unwrap();
        close(&m.transform([-2.0, -3.0, -1.0, 1.0]), &[-1.0, -1.0, -1.0, 1.0]);
        close(&m.transform([4.0, 5.0, -9.0, 1.0]), &[1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn perspective_maps_near_far() {
        let m = Mat4::frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0).unwrap();
        let near = m.transform([1.0, 1.0, -1.0, 1.0]);
        let far = m.transform([0.0, 0.0, -10.0, 1.0]);
        close(&near, &[1.0, 1.0, -1.0, 1.0]);
        close(&[far[2] / far[3]], &[1.0]);
    }

    #[test]
    fn invalid_projections() {
        assert_eq!(Mat4::ortho(0.0, 0.0, 0.0, 1.0, 1.0, 10.0), Err(Error::InvalidValue));
        assert_eq!(Mat4::frustum(-1.0, 1.0, -1.0, 1.0, 0.0, 10.0), Err(Error::InvalidValue));
        assert_eq!(Mat4::frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 1.0), Err(Error::InvalidValue));
        assert_eq!(Mat4::ortho(f64::NAN, 1.0, 0.0, 1.0, 1.0, 10.0), Err(Error::InvalidValue));
    }

    #[test]
    fn reversed_planes_are_legal() {
        assert!(Mat4::ortho(2.0, -2.0, 2.0, -2.0, 10.0, 1.0).is_ok());
        assert!(Mat4::frustum(2.0, -2.0, 2.0, -2.0, 10.0, 1.0).is_ok());
    }

    #[test]
    fn normal_inverse_transpose() {
        let n = Mat4::translation(7.0, 8.0, 9.0)
            .post_mul(Mat4::scaling(2.0, 4.0, 8.0))
            .normal_matrix()
            .unwrap();
        close(&n, &[0.5, 0.0, 0.0, 0.0, 0.25, 0.0, 0.0, 0.0, 0.125]);
        let r = Mat4::rotation(33.0, [1.0, 2.0, 3.0]);
        let expected = [r.0[0], r.0[1], r.0[2], r.0[4], r.0[5], r.0[6], r.0[8], r.0[9], r.0[10]];
        close(&r.normal_matrix().unwrap(), &expected);
    }

    #[test]
    fn singular_normal_has_no_inverse() {
        assert_eq!(Mat4::scaling(1.0, 0.0, 1.0).normal_matrix(), None);
        assert_eq!(Mat4::scaling(f64::NAN, 1.0, 1.0).normal_matrix(), None);
    }

    #[test]
    fn stack_restores_exact_state() {
        let mut stack = MatrixStack::new(32).unwrap();
        stack.multiply(Mat4::translation(2.0, 3.0, 4.0));
        let saved = stack.current();
        stack.push().unwrap();
        stack.multiply(Mat4::scaling(5.0, 6.0, 7.0));
        stack.pop().unwrap();
        assert_eq!(stack.current(), saved);
        assert_eq!(stack.depth(), 1);
    }

    #[test]
    fn stack_errors_preserve_state() {
        assert!(matches!(MatrixStack::new(0), Err(Error::InvalidValue)));
        let mut stack = MatrixStack::new(2).unwrap();
        assert_eq!(stack.pop(), Err(Error::StackUnderflow));
        stack.push().unwrap();
        assert_eq!(stack.push(), Err(Error::StackOverflow));
        assert_eq!(stack.depth(), 2);
        assert_eq!(stack.current(), Mat4::IDENTITY);
    }
}
