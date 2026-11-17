//! Row-vector 4x4 matrices with the same conventions as System.Numerics (v' = v * M, translation in row 3),
//! uploaded as-is to HLSL `row_major` constant buffers.

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4(pub [[f32; 4]; 4]);

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4([[1., 0., 0., 0.], [0., 1., 0., 0.], [0., 0., 1., 0.], [0., 0., 0., 1.]]);

    pub fn scale(x: f32, y: f32) -> Mat4 {
        let mut m = Self::IDENTITY;
        m.0[0][0] = x;
        m.0[1][1] = y;
        m
    }
    pub fn translate(x: f32, y: f32) -> Mat4 {
        let mut m = Self::IDENTITY;
        m.0[3][0] = x;
        m.0[3][1] = y;
        m
    }
    pub fn rotate_z(radians: f32) -> Mat4 {
        let (s, c) = radians.sin_cos();
        Mat4([[c, s, 0., 0.], [-s, c, 0., 0.], [0., 0., 1., 0.], [0., 0., 0., 1.]])
    }

    /// 2D point transform (z = 0, w = 1).
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.0;
        (x * m[0][0] + y * m[1][0] + m[3][0], x * m[0][1] + y * m[1][1] + m[3][1])
    }

    /// Inverse of a 2D affine matrix (scale/rotate/translate in the XY plane).
    pub fn invert_2d(&self) -> Option<Mat4> {
        let m = &self.0;
        let (a, b, c, d) = (m[0][0], m[0][1], m[1][0], m[1][1]);
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return None;
        }
        let (ia, ib, ic, id) = (d / det, -b / det, -c / det, a / det);
        let (tx, ty) = (m[3][0], m[3][1]);
        let mut r = Self::IDENTITY;
        r.0[0][0] = ia;
        r.0[0][1] = ib;
        r.0[1][0] = ic;
        r.0[1][1] = id;
        r.0[3][0] = -(tx * ia + ty * ic);
        r.0[3][1] = -(tx * ib + ty * id);
        Some(r)
    }
}

impl std::ops::Mul for Mat4 {
    type Output = Mat4;
    fn mul(self, o: Mat4) -> Mat4 {
        let mut r = [[0f32; 4]; 4];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..4).map(|k| self.0[i][k] * o.0[k][j]).sum();
            }
        }
        Mat4(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inverse_roundtrip() {
        let m = Mat4::scale(200., 100.) * Mat4::rotate_z(0.7) * Mat4::translate(30., 40.);
        let inv = m.invert_2d().unwrap();
        let (x, y) = m.apply(0.25, 0.75);
        let (u, v) = inv.apply(x, y);
        assert!((u - 0.25).abs() < 1e-4 && (v - 0.75).abs() < 1e-4);
    }
}
