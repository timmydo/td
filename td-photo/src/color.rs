//! Linear colour math in `f32`: 3x3 matrices, dcraw's camera-to-sRGB
//! construction from an XYZ-to-camera matrix, the daylight multipliers that
//! construction yields, and the sRGB transfer through a 65536-entry table.
//! Nothing here reads a file, the environment or a clock.

use std::fmt;

pub type Matrix = [[f32; 3]; 3];

/// sRGB (D65) primaries to XYZ, the `xyz_rgb` dcraw uses.
pub const RGB_TO_XYZ: Matrix = [
    [0.412_453, 0.357_580, 0.180_423],
    [0.212_671, 0.715_160, 0.072_169],
    [0.019_334, 0.119_193, 0.950_227],
];

/// Rec. 709 luminance weights of linear sRGB.
pub const LUMA: [f32; 3] = [0.212_671, 0.715_160, 0.072_169];

/// Middle grey in linear scene light, the tone curves' fixed point.
pub const MIDDLE_GREY: f32 = 0.1845;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The camera matrix has no inverse.
    Singular,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Singular => f.write_str("camera colour matrix is singular"),
        }
    }
}

impl std::error::Error for Error {}

pub fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [[0.0f32; 3]; 3];
    for (row_out, row_a) in out.iter_mut().zip(a.iter()) {
        for (j, cell) in row_out.iter_mut().enumerate() {
            *cell = row_a
                .iter()
                .zip(b.iter())
                .map(|(a_ik, b_row)| a_ik * b_row.get(j).copied().unwrap_or(0.0))
                .sum();
        }
    }
    out
}

pub fn apply(m: &Matrix, v: [f32; 3]) -> [f32; 3] {
    let dot = |row: &[f32; 3]| row[0] * v[0] + row[1] * v[1] + row[2] * v[2];
    [dot(&m[0]), dot(&m[1]), dot(&m[2])]
}

/// The inverse by cofactors, or `None` when the determinant vanishes.
pub fn invert(m: &Matrix) -> Option<Matrix> {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let co_a = e * i - f * h;
    let co_b = -(d * i - f * g);
    let co_c = d * h - e * g;
    let det = a * co_a + b * co_b + c * co_c;
    if det.abs() < 1e-12 || !det.is_finite() {
        return None;
    }
    let inv = 1.0 / det;
    Some([
        [co_a * inv, -(b * i - c * h) * inv, (b * f - c * e) * inv],
        [co_b * inv, (a * i - c * g) * inv, -(a * f - c * d) * inv],
        [co_c * inv, -(a * h - b * g) * inv, (a * e - b * d) * inv],
    ])
}

/// A body's colour: the matrix from white-balanced camera space to linear
/// sRGB, and the multipliers that balance daylight, scaled to green.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraColor {
    pub rgb_cam: Matrix,
    pub daylight: [f32; 3],
}

/// dcraw's `cam_xyz_coeff`: `cam_rgb = cam_xyz * xyz_rgb`, each row
/// normalized to sum to one so a balanced camera white is display white,
/// inverted; the reciprocals of the row sums are the daylight multipliers.
pub fn camera_color(xyz_to_cam: &[[i32; 3]; 3]) -> Result<CameraColor, Error> {
    let mut cam_xyz = [[0.0f32; 3]; 3];
    for (row_out, row_in) in cam_xyz.iter_mut().zip(xyz_to_cam.iter()) {
        for (cell, value) in row_out.iter_mut().zip(row_in.iter()) {
            *cell = *value as f32 / 10000.0;
        }
    }
    let mut cam_rgb = multiply(&cam_xyz, &RGB_TO_XYZ);
    let mut pre_mul = [0.0f32; 3];
    for (row, mul) in cam_rgb.iter_mut().zip(pre_mul.iter_mut()) {
        let sum: f32 = row.iter().sum();
        if sum.abs() < 1e-9 || !sum.is_finite() {
            return Err(Error::Singular);
        }
        for cell in row.iter_mut() {
            *cell /= sum;
        }
        *mul = 1.0 / sum;
    }
    let rgb_cam = invert(&cam_rgb).ok_or(Error::Singular)?;
    let green = pre_mul[1];
    if green.abs() < 1e-9 {
        return Err(Error::Singular);
    }
    let daylight = [pre_mul[0] / green, 1.0, pre_mul[2] / green];
    Ok(CameraColor { rgb_cam, daylight })
}

/// The sRGB piecewise transfer of one linear value in `0..=1`.
pub fn srgb_encode(linear: f32) -> f32 {
    let x = linear.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// The transfer as a table from 16-bit linear to 8-bit encoded, built once.
#[derive(Clone, Debug)]
pub struct Transfer {
    table: Vec<u8>,
}

impl Transfer {
    pub fn srgb() -> Self {
        let table = (0..=u16::MAX)
            .map(|i| (srgb_encode(f32::from(i) / 65535.0) * 255.0 + 0.5) as u8)
            .collect();
        Self { table }
    }

    /// Encodes a linear value; anything outside `0..=1` clips.
    #[inline]
    pub fn encode(&self, linear: f32) -> u8 {
        let index = (linear.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize;
        self.table.get(index).copied().unwrap_or(u8::MAX)
    }
}

impl Default for Transfer {
    fn default() -> Self {
        Self::srgb()
    }
}
