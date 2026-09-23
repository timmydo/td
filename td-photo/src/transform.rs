//! The AV1 transforms the AVIF export codes with: the integer inverse DCT
//! and ADST the specification fixes (AV1 bitstream specification, section
//! 7.13), which every decoder reproduces bit for bit, the forward
//! butterflies libaom pairs them with, the quantizer steps, and the
//! coefficient scans. Square sizes of 4 to 32 points only: that is every
//! transform the encoder's block ladder reaches. The butterflies are
//! tables of stages, each output one operation over the previous stage,
//! extracted from libaom 3.9.1's `av1_inv_txfm1d.c` and
//! `av1_fwd_txfm1d.c`, which `butterfly!` expands to straight code; the
//! tests hold them to a floating-point reference.

use std::convert::TryFrom;

/// A square transform's size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Size {
    S4,
    S8,
    S16,
    S32,
}

impl Size {
    /// Points along a side.
    pub fn points(self) -> usize {
        match self {
            Size::S4 => 4,
            Size::S8 => 8,
            Size::S16 => 16,
            Size::S32 => 32,
        }
    }

    /// The spec's `TX_4X4`..`TX_32X32` index, its context and its
    /// quantizer band.
    pub fn index(self) -> usize {
        match self {
            Size::S4 => 0,
            Size::S8 => 1,
            Size::S16 => 2,
            Size::S32 => 3,
        }
    }

    /// The spec's `Transform_Row_Shift`.
    fn row_shift(self) -> u32 {
        match self {
            Size::S4 => 0,
            Size::S8 => 1,
            Size::S16 | Size::S32 => 2,
        }
    }

    /// libaom's forward column shift: the rounding that keeps the
    /// forward pair's output at the scale the inverse expects.
    fn forward_column_shift(self) -> u32 {
        match self {
            Size::S4 => 0,
            Size::S8 => 1,
            Size::S16 => 2,
            Size::S32 => 4,
        }
    }

    /// The spec's `dqDenom` shift: a 32x32 dequantizes to half.
    pub fn dequant_shift(self) -> u32 {
        match self {
            Size::S32 => 1,
            _ => 0,
        }
    }

    /// The default scan of this size.
    pub fn scan(self) -> &'static [u16] {
        match self {
            Size::S4 => &SCAN_4,
            Size::S8 => &SCAN_8,
            Size::S16 => &SCAN_16,
            Size::S32 => &SCAN_32,
        }
    }
}

/// The 1D kernel a direction uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kernel {
    Dct,
    Adst,
}

/// A 2D transform type, the spec's `DCT_DCT`, `ADST_DCT`, `DCT_ADST` and
/// `ADST_ADST`: the vertical kernel then the horizontal one. A 32x32 is
/// DCT both ways whatever the mode asks (`TX_SET_DCTONLY`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TxType {
    DctDct,
    AdstDct,
    DctAdst,
    AdstAdst,
}

impl TxType {
    pub fn vertical(self) -> Kernel {
        match self {
            TxType::DctDct | TxType::DctAdst => Kernel::Dct,
            TxType::AdstDct | TxType::AdstAdst => Kernel::Adst,
        }
    }

    pub fn horizontal(self) -> Kernel {
        match self {
            TxType::DctDct | TxType::AdstDct => Kernel::Dct,
            TxType::DctAdst | TxType::AdstAdst => Kernel::Adst,
        }
    }

    /// The symbol of the spec's `Tx_Type_Intra_Inv_Set2`: `IDTX` is 0.
    pub fn symbol(self) -> usize {
        match self {
            TxType::DctDct => 1,
            TxType::AdstAdst => 2,
            TxType::AdstDct => 3,
            TxType::DctAdst => 4,
        }
    }
}

/// `cos(i * pi / 128)` at twelve bits: the inverse transforms' constants.
const COSPI12: [i32; 64] = [
    4096, 4095, 4091, 4085, 4076, 4065, 4052, 4036, 4017, 3996, 3973, 3948, 3920, 3889, 3857, 3822,
    3784, 3745, 3703, 3659, 3612, 3564, 3513, 3461, 3406, 3349, 3290, 3229, 3166, 3102, 3035, 2967,
    2896, 2824, 2751, 2675, 2598, 2520, 2440, 2359, 2276, 2191, 2106, 2019, 1931, 1842, 1751, 1660,
    1567, 1474, 1380, 1285, 1189, 1092, 995, 897, 799, 700, 601, 501, 401, 301, 201, 101,
];

/// The same at thirteen bits: the forward transforms' constants.
const COSPI13: [i32; 64] = [
    8192, 8190, 8182, 8170, 8153, 8130, 8103, 8071, 8035, 7993, 7946, 7895, 7839, 7779, 7713, 7643,
    7568, 7489, 7405, 7317, 7225, 7128, 7027, 6921, 6811, 6698, 6580, 6458, 6333, 6203, 6070, 5933,
    5793, 5649, 5501, 5351, 5197, 5040, 4880, 4717, 4551, 4383, 4212, 4038, 3862, 3683, 3503, 3320,
    3135, 2948, 2760, 2570, 2378, 2185, 1990, 1795, 1598, 1401, 1202, 1003, 803, 603, 402, 201,
];

/// The ADST4's `sqrt(2) * sin(i * pi / 9) * 2 / 3` at twelve bits, the
/// first two summing to the fourth (spec `SINPI_1_9`..`SINPI_4_9`).
const SINPI12: [i32; 5] = [0, 1321, 2482, 3344, 3803];
/// The same at thirteen bits.
const SINPI13: [i32; 5] = [0, 2642, 4964, 6689, 7606];

const INVERSE_BITS: u32 = 12;
const FORWARD_BITS: u32 = 13;

/// `Round2(x, n)`.
fn round2(x: i64, n: u32) -> i32 {
    if n == 0 {
        return x as i32;
    }
    ((x + (1i64 << (n - 1))) >> n) as i32
}

/// The spec's sixteen-bit intermediate clamp for eight-bit video.
fn clamp16(x: i32) -> i32 {
    x.clamp(-32768, 32767)
}

fn cosine(table: &[i32; 64], index: i8) -> i32 {
    let c = table
        .get(usize::from(index.unsigned_abs()))
        .copied()
        .unwrap_or(0);
    if index < 0 {
        -c
    } else {
        c
    }
}

/// One output of a butterfly stage over the previous stage's values
/// `$v`: `C(i)` is `x[i]`, `N(i)` is `-x[i]`, `A(i, j)` and `S(i, j)` are
/// `x[i] + x[j]` and `x[i] - x[j]` through `$clamp` (the inverse's
/// sixteen-bit clamp, the forward's `keep`), and `B(a, i, b, j)` is
/// `Round2(cos(a) * x[i] + cos(b) * x[j], bits)`, a negative index for a
/// negated cosine (spec `B()`). The value indices are literals into a
/// fixed array, so a wrong one is a compile error, not a silent zero.
macro_rules! butterfly_op {
    ($v:ident, $t:ident, $bits:ident, $clamp:ident, C($i:literal)) => {
        $v[$i]
    };
    ($v:ident, $t:ident, $bits:ident, $clamp:ident, N($i:literal)) => {
        -$v[$i]
    };
    ($v:ident, $t:ident, $bits:ident, $clamp:ident, A($i:literal, $j:literal)) => {
        $clamp($v[$i] + $v[$j])
    };
    ($v:ident, $t:ident, $bits:ident, $clamp:ident, S($i:literal, $j:literal)) => {
        $clamp($v[$i] - $v[$j])
    };
    (
        $v:ident,
        $t:ident,
        $bits:ident,
        $clamp:ident,
        B($a:literal, $i:literal, $b:literal, $j:literal)
    ) => {
        round2(
            i64::from(cosine(&$t, $a)) * i64::from($v[$i])
                + i64::from(cosine(&$t, $b)) * i64::from($v[$j]),
            $bits,
        )
    };
}

/// A butterfly kernel from its table of stages, each a row of the
/// stage's outputs over the previous one's values: expanded to straight
/// code over fixed arrays, the table the one source of the transform.
macro_rules! butterfly {
    (
        $(#[$doc:meta])*
        fn $name:ident($n:literal, $t:ident, $bits:ident, $clamp:ident)
        [$([$($op:ident($($arg:literal),+)),+ $(,)?]),+ $(,)?]
    ) => {
        $(#[$doc])*
        fn $name(x: &mut [i32; $n]) {
            let v = *x;
            $(
                let v: [i32; $n] = [$(butterfly_op!(v, $t, $bits, $clamp, $op($($arg),+))),+];
            )+
            *x = v;
        }
    };
}

/// The forward kernels' `A` and `S`: no clamp.
fn keep(x: i32) -> i32 {
    x
}

/// Runs a kernel over `x` when it is the kernel's length; a slice of
/// another length is left as it is.
fn run<const N: usize>(x: &mut [i32], kernel: impl FnOnce(&mut [i32; N])) {
    if let Ok(x) = <&mut [i32; N]>::try_from(x) {
        kernel(x);
    }
}

/// The spec's inverse ADST4 (7.13.2.6): no intermediate clamps.
fn iadst4(x: &mut [i32]) {
    let Ok(x) = <&mut [i32; 4]>::try_from(x) else {
        return;
    };
    let s = &SINPI12;
    let (x0, x1, x2, x3) = (
        i64::from(x[0]),
        i64::from(x[1]),
        i64::from(x[2]),
        i64::from(x[3]),
    );
    let (s1, s2, s3, s4) = (
        i64::from(s[1]),
        i64::from(s[2]),
        i64::from(s[3]),
        i64::from(s[4]),
    );
    let mut a0 = s1 * x0;
    let mut a1 = s2 * x0;
    let a2 = s3 * x1;
    let a3 = s4 * x2;
    let a4 = s1 * x2;
    let a5 = s2 * x3;
    let a6 = s4 * x3;
    let b7 = (x0 - x2) + x3;
    a0 += a3;
    a1 -= a4;
    let a3 = a2;
    let a2 = s3 * b7;
    a0 += a5;
    a1 -= a6;
    let y0 = a0 + a3;
    let y1 = a1 + a3;
    let y2 = a2;
    let y3 = a0 + a1 - a3;
    x[0] = round2(y0, INVERSE_BITS);
    x[1] = round2(y1, INVERSE_BITS);
    x[2] = round2(y2, INVERSE_BITS);
    x[3] = round2(y3, INVERSE_BITS);
}

/// libaom's forward ADST4, the inverse's pair.
fn fadst4(x: &mut [i32]) {
    let Ok(x) = <&mut [i32; 4]>::try_from(x) else {
        return;
    };
    let s = &SINPI13;
    let (x0, x1, x2, x3) = (
        i64::from(x[0]),
        i64::from(x[1]),
        i64::from(x[2]),
        i64::from(x[3]),
    );
    let (s1, s2, s3, s4) = (
        i64::from(s[1]),
        i64::from(s[2]),
        i64::from(s[3]),
        i64::from(s[4]),
    );
    let a0 = s1 * x0;
    let a1 = s4 * x0;
    let a2 = s2 * x1;
    let a3 = s1 * x1;
    let a4 = s3 * x2;
    let a5 = s4 * x3;
    let a6 = s2 * x3;
    let a7 = x0 + x1 - x3;
    let mut y0 = a0 + a2;
    let y1 = s3 * a7;
    let mut y2 = a1 - a3;
    let y3 = a4;
    y0 += a5;
    y2 += a6;
    let b0 = y0 + y3;
    let b1 = y1;
    let b2 = y2 - y3;
    let b3 = y2 - y0 + y3;
    x[0] = round2(b0, FORWARD_BITS);
    x[1] = round2(b1, FORWARD_BITS);
    x[2] = round2(b2, FORWARD_BITS);
    x[3] = round2(b3, FORWARD_BITS);
}

/// One 1D inverse kernel over `x`, whose length picks the size.
fn inverse_1d(kernel: Kernel, x: &mut [i32]) {
    match (kernel, x.len()) {
        (Kernel::Dct, 4) => run(x, idct4),
        (Kernel::Dct, 8) => run(x, idct8),
        (Kernel::Dct, 16) => run(x, idct16),
        (Kernel::Dct, 32) => run(x, idct32),
        (Kernel::Adst, 4) => iadst4(x),
        (Kernel::Adst, 8) => run(x, iadst8),
        (Kernel::Adst, 16) => run(x, iadst16),
        _ => {}
    }
}

/// One 1D forward kernel over `x`.
fn forward_1d(kernel: Kernel, x: &mut [i32]) {
    match (kernel, x.len()) {
        (Kernel::Dct, 4) => run(x, fdct4),
        (Kernel::Dct, 8) => run(x, fdct8),
        (Kernel::Dct, 16) => run(x, fdct16),
        (Kernel::Dct, 32) => run(x, fdct32),
        (Kernel::Adst, 4) => fadst4(x),
        (Kernel::Adst, 8) => run(x, fadst8),
        (Kernel::Adst, 16) => run(x, fadst16),
        _ => {}
    }
}

/// The spec's 2D inverse transform (7.13.3) of `coeffs`, row-major with
/// the row the vertical frequency, into `residual`, row-major pixels'
/// worth of difference: rows first with their shift, then columns with
/// the final `Round2(.., 4)`, sixteen-bit clamps at each entry. Slices
/// of another length than the size squared are left untouched.
pub fn inverse(size: Size, tx: TxType, coeffs: &[i32], residual: &mut [i32]) {
    let n = size.points();
    if coeffs.len() != n * n || residual.len() != n * n {
        return;
    }
    let mut column = [0i32; 32];
    let Some(column) = column.get_mut(..n) else {
        return;
    };
    for (row, out) in coeffs.chunks_exact(n).zip(residual.chunks_exact_mut(n)) {
        for (o, c) in out.iter_mut().zip(row) {
            *o = clamp16(*c);
        }
        inverse_1d(tx.horizontal(), out);
        for o in out.iter_mut() {
            *o = round2(i64::from(*o), size.row_shift());
        }
    }
    for j in 0..n {
        for (c, row) in column.iter_mut().zip(residual.chunks_exact(n)) {
            *c = clamp16(row.get(j).copied().unwrap_or(0));
        }
        inverse_1d(tx.vertical(), column);
        for (c, row) in column.iter().zip(residual.chunks_exact_mut(n)) {
            if let Some(o) = row.get_mut(j) {
                *o = round2(i64::from(*c), 4);
            }
        }
    }
}

/// libaom's 2D forward transform of `residual`, the scale the inverse
/// undoes: columns first over the residual doubled twice, the column
/// shift, then rows. Slices of another length are left untouched.
pub fn forward(size: Size, tx: TxType, residual: &[i32], coeffs: &mut [i32]) {
    let n = size.points();
    if coeffs.len() != n * n || residual.len() != n * n {
        return;
    }
    let mut column = [0i32; 32];
    let Some(column) = column.get_mut(..n) else {
        return;
    };
    for j in 0..n {
        for (c, row) in column.iter_mut().zip(residual.chunks_exact(n)) {
            *c = row.get(j).copied().unwrap_or(0) << 2;
        }
        forward_1d(tx.vertical(), column);
        for (c, row) in column.iter().zip(coeffs.chunks_exact_mut(n)) {
            if let Some(o) = row.get_mut(j) {
                *o = round2(i64::from(*c), size.forward_column_shift());
            }
        }
    }
    for row in coeffs.chunks_exact_mut(n) {
        forward_1d(tx.horizontal(), row);
    }
}

/// The DC quantizer step of a `qindex`.
pub fn dc_q(qindex: u8) -> i32 {
    DC_Q.get(usize::from(qindex)).copied().map_or(4, i32::from)
}

/// The AC quantizer step of a `qindex`.
pub fn ac_q(qindex: u8) -> i32 {
    AC_Q.get(usize::from(qindex)).copied().map_or(4, i32::from)
}
butterfly! {
    /// The inverse DCT of 4 points, stage by stage (spec 7.13.2.3).
    fn idct4(4, COSPI12, INVERSE_BITS, clamp16) [
        [
            C(0), C(2), C(1), C(3),
        ],
        [
            B(32, 0, 32, 1), B(32, 0, -32, 1), B(48, 2, -16, 3), B(16, 2, 48, 3),
        ],
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3),
        ],
    ]
}

butterfly! {
    /// The inverse DCT of 8 points, stage by stage (spec 7.13.2.3).
    fn idct8(8, COSPI12, INVERSE_BITS, clamp16) [
        [
            C(0), C(4), C(2), C(6), C(1), C(5), C(3), C(7),
        ],
        [
            C(0), C(1), C(2), C(3), B(56, 4, -8, 7), B(24, 5, -40, 6),
            B(40, 5, 24, 6), B(8, 4, 56, 7),
        ],
        [
            B(32, 0, 32, 1), B(32, 0, -32, 1), B(48, 2, -16, 3), B(16, 2, 48, 3),
            A(4, 5), S(4, 5), S(7, 6), A(6, 7),
        ],
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3), C(4), B(-32, 5, 32, 6),
            B(32, 5, 32, 6), C(7),
        ],
        [
            A(0, 7), A(1, 6), A(2, 5), A(3, 4), S(3, 4), S(2, 5), S(1, 6), S(0, 7),
        ],
    ]
}

butterfly! {
    /// The inverse DCT of 16 points, stage by stage (spec 7.13.2.3).
    fn idct16(16, COSPI12, INVERSE_BITS, clamp16) [
        [
            C(0), C(8), C(4), C(12), C(2), C(10), C(6), C(14), C(1), C(9), C(5),
            C(13), C(3), C(11), C(7), C(15),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), B(60, 8, -4, 15),
            B(28, 9, -36, 14), B(44, 10, -20, 13), B(12, 11, -52, 12),
            B(52, 11, 12, 12), B(20, 10, 44, 13), B(36, 9, 28, 14), B(4, 8, 60, 15),
        ],
        [
            C(0), C(1), C(2), C(3), B(56, 4, -8, 7), B(24, 5, -40, 6),
            B(40, 5, 24, 6), B(8, 4, 56, 7), A(8, 9), S(8, 9), S(11, 10), A(10, 11),
            A(12, 13), S(12, 13), S(15, 14), A(14, 15),
        ],
        [
            B(32, 0, 32, 1), B(32, 0, -32, 1), B(48, 2, -16, 3), B(16, 2, 48, 3),
            A(4, 5), S(4, 5), S(7, 6), A(6, 7), C(8), B(-16, 9, 48, 14),
            B(-48, 10, -16, 13), C(11), C(12), B(-16, 10, 48, 13), B(48, 9, 16, 14),
            C(15),
        ],
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3), C(4), B(-32, 5, 32, 6),
            B(32, 5, 32, 6), C(7), A(8, 11), A(9, 10), S(9, 10), S(8, 11),
            S(15, 12), S(14, 13), A(13, 14), A(12, 15),
        ],
        [
            A(0, 7), A(1, 6), A(2, 5), A(3, 4), S(3, 4), S(2, 5), S(1, 6), S(0, 7),
            C(8), C(9), B(-32, 10, 32, 13), B(-32, 11, 32, 12), B(32, 11, 32, 12),
            B(32, 10, 32, 13), C(14), C(15),
        ],
        [
            A(0, 15), A(1, 14), A(2, 13), A(3, 12), A(4, 11), A(5, 10), A(6, 9),
            A(7, 8), S(7, 8), S(6, 9), S(5, 10), S(4, 11), S(3, 12), S(2, 13),
            S(1, 14), S(0, 15),
        ],
    ]
}

butterfly! {
    /// The inverse DCT of 32 points, stage by stage (spec 7.13.2.3).
    fn idct32(32, COSPI12, INVERSE_BITS, clamp16) [
        [
            C(0), C(16), C(8), C(24), C(4), C(20), C(12), C(28), C(2), C(18), C(10),
            C(26), C(6), C(22), C(14), C(30), C(1), C(17), C(9), C(25), C(5), C(21),
            C(13), C(29), C(3), C(19), C(11), C(27), C(7), C(23), C(15), C(31),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), C(8), C(9), C(10),
            C(11), C(12), C(13), C(14), C(15), B(62, 16, -2, 31),
            B(30, 17, -34, 30), B(46, 18, -18, 29), B(14, 19, -50, 28),
            B(54, 20, -10, 27), B(22, 21, -42, 26), B(38, 22, -26, 25),
            B(6, 23, -58, 24), B(58, 23, 6, 24), B(26, 22, 38, 25),
            B(42, 21, 22, 26), B(10, 20, 54, 27), B(50, 19, 14, 28),
            B(18, 18, 46, 29), B(34, 17, 30, 30), B(2, 16, 62, 31),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), B(60, 8, -4, 15),
            B(28, 9, -36, 14), B(44, 10, -20, 13), B(12, 11, -52, 12),
            B(52, 11, 12, 12), B(20, 10, 44, 13), B(36, 9, 28, 14), B(4, 8, 60, 15),
            A(16, 17), S(16, 17), S(19, 18), A(18, 19), A(20, 21), S(20, 21),
            S(23, 22), A(22, 23), A(24, 25), S(24, 25), S(27, 26), A(26, 27),
            A(28, 29), S(28, 29), S(31, 30), A(30, 31),
        ],
        [
            C(0), C(1), C(2), C(3), B(56, 4, -8, 7), B(24, 5, -40, 6),
            B(40, 5, 24, 6), B(8, 4, 56, 7), A(8, 9), S(8, 9), S(11, 10), A(10, 11),
            A(12, 13), S(12, 13), S(15, 14), A(14, 15), C(16), B(-8, 17, 56, 30),
            B(-56, 18, -8, 29), C(19), C(20), B(-40, 21, 24, 26),
            B(-24, 22, -40, 25), C(23), C(24), B(-40, 22, 24, 25),
            B(24, 21, 40, 26), C(27), C(28), B(-8, 18, 56, 29), B(56, 17, 8, 30),
            C(31),
        ],
        [
            B(32, 0, 32, 1), B(32, 0, -32, 1), B(48, 2, -16, 3), B(16, 2, 48, 3),
            A(4, 5), S(4, 5), S(7, 6), A(6, 7), C(8), B(-16, 9, 48, 14),
            B(-48, 10, -16, 13), C(11), C(12), B(-16, 10, 48, 13), B(48, 9, 16, 14),
            C(15), A(16, 19), A(17, 18), S(17, 18), S(16, 19), S(23, 20), S(22, 21),
            A(21, 22), A(20, 23), A(24, 27), A(25, 26), S(25, 26), S(24, 27),
            S(31, 28), S(30, 29), A(29, 30), A(28, 31),
        ],
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3), C(4), B(-32, 5, 32, 6),
            B(32, 5, 32, 6), C(7), A(8, 11), A(9, 10), S(9, 10), S(8, 11),
            S(15, 12), S(14, 13), A(13, 14), A(12, 15), C(16), C(17),
            B(-16, 18, 48, 29), B(-16, 19, 48, 28), B(-48, 20, -16, 27),
            B(-48, 21, -16, 26), C(22), C(23), C(24), C(25), B(-16, 21, 48, 26),
            B(-16, 20, 48, 27), B(48, 19, 16, 28), B(48, 18, 16, 29), C(30), C(31),
        ],
        [
            A(0, 7), A(1, 6), A(2, 5), A(3, 4), S(3, 4), S(2, 5), S(1, 6), S(0, 7),
            C(8), C(9), B(-32, 10, 32, 13), B(-32, 11, 32, 12), B(32, 11, 32, 12),
            B(32, 10, 32, 13), C(14), C(15), A(16, 23), A(17, 22), A(18, 21),
            A(19, 20), S(19, 20), S(18, 21), S(17, 22), S(16, 23), S(31, 24),
            S(30, 25), S(29, 26), S(28, 27), A(27, 28), A(26, 29), A(25, 30),
            A(24, 31),
        ],
        [
            A(0, 15), A(1, 14), A(2, 13), A(3, 12), A(4, 11), A(5, 10), A(6, 9),
            A(7, 8), S(7, 8), S(6, 9), S(5, 10), S(4, 11), S(3, 12), S(2, 13),
            S(1, 14), S(0, 15), C(16), C(17), C(18), C(19), B(-32, 20, 32, 27),
            B(-32, 21, 32, 26), B(-32, 22, 32, 25), B(-32, 23, 32, 24),
            B(32, 23, 32, 24), B(32, 22, 32, 25), B(32, 21, 32, 26),
            B(32, 20, 32, 27), C(28), C(29), C(30), C(31),
        ],
        [
            A(0, 31), A(1, 30), A(2, 29), A(3, 28), A(4, 27), A(5, 26), A(6, 25),
            A(7, 24), A(8, 23), A(9, 22), A(10, 21), A(11, 20), A(12, 19),
            A(13, 18), A(14, 17), A(15, 16), S(15, 16), S(14, 17), S(13, 18),
            S(12, 19), S(11, 20), S(10, 21), S(9, 22), S(8, 23), S(7, 24), S(6, 25),
            S(5, 26), S(4, 27), S(3, 28), S(2, 29), S(1, 30), S(0, 31),
        ],
    ]
}

butterfly! {
    /// The inverse ADST of 8 points, stage by stage (spec 7.13.2.6/7).
    fn iadst8(8, COSPI12, INVERSE_BITS, clamp16) [
        [
            C(7), C(0), C(5), C(2), C(3), C(4), C(1), C(6),
        ],
        [
            B(4, 0, 60, 1), B(60, 0, -4, 1), B(20, 2, 44, 3), B(44, 2, -20, 3),
            B(36, 4, 28, 5), B(28, 4, -36, 5), B(52, 6, 12, 7), B(12, 6, -52, 7),
        ],
        [
            A(0, 4), A(1, 5), A(2, 6), A(3, 7), S(0, 4), S(1, 5), S(2, 6), S(3, 7),
        ],
        [
            C(0), C(1), C(2), C(3), B(16, 4, 48, 5), B(48, 4, -16, 5),
            B(-48, 6, 16, 7), B(16, 6, 48, 7),
        ],
        [
            A(0, 2), A(1, 3), S(0, 2), S(1, 3), A(4, 6), A(5, 7), S(4, 6), S(5, 7),
        ],
        [
            C(0), C(1), B(32, 2, 32, 3), B(32, 2, -32, 3), C(4), C(5),
            B(32, 6, 32, 7), B(32, 6, -32, 7),
        ],
        [
            C(0), N(4), C(6), N(2), C(3), N(7), C(5), N(1),
        ],
    ]
}

butterfly! {
    /// The inverse ADST of 16 points, stage by stage (spec 7.13.2.6/7).
    fn iadst16(16, COSPI12, INVERSE_BITS, clamp16) [
        [
            C(15), C(0), C(13), C(2), C(11), C(4), C(9), C(6), C(7), C(8), C(5),
            C(10), C(3), C(12), C(1), C(14),
        ],
        [
            B(2, 0, 62, 1), B(62, 0, -2, 1), B(10, 2, 54, 3), B(54, 2, -10, 3),
            B(18, 4, 46, 5), B(46, 4, -18, 5), B(26, 6, 38, 7), B(38, 6, -26, 7),
            B(34, 8, 30, 9), B(30, 8, -34, 9), B(42, 10, 22, 11),
            B(22, 10, -42, 11), B(50, 12, 14, 13), B(14, 12, -50, 13),
            B(58, 14, 6, 15), B(6, 14, -58, 15),
        ],
        [
            A(0, 8), A(1, 9), A(2, 10), A(3, 11), A(4, 12), A(5, 13), A(6, 14),
            A(7, 15), S(0, 8), S(1, 9), S(2, 10), S(3, 11), S(4, 12), S(5, 13),
            S(6, 14), S(7, 15),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), B(8, 8, 56, 9),
            B(56, 8, -8, 9), B(40, 10, 24, 11), B(24, 10, -40, 11),
            B(-56, 12, 8, 13), B(8, 12, 56, 13), B(-24, 14, 40, 15),
            B(40, 14, 24, 15),
        ],
        [
            A(0, 4), A(1, 5), A(2, 6), A(3, 7), S(0, 4), S(1, 5), S(2, 6), S(3, 7),
            A(8, 12), A(9, 13), A(10, 14), A(11, 15), S(8, 12), S(9, 13), S(10, 14),
            S(11, 15),
        ],
        [
            C(0), C(1), C(2), C(3), B(16, 4, 48, 5), B(48, 4, -16, 5),
            B(-48, 6, 16, 7), B(16, 6, 48, 7), C(8), C(9), C(10), C(11),
            B(16, 12, 48, 13), B(48, 12, -16, 13), B(-48, 14, 16, 15),
            B(16, 14, 48, 15),
        ],
        [
            A(0, 2), A(1, 3), S(0, 2), S(1, 3), A(4, 6), A(5, 7), S(4, 6), S(5, 7),
            A(8, 10), A(9, 11), S(8, 10), S(9, 11), A(12, 14), A(13, 15), S(12, 14),
            S(13, 15),
        ],
        [
            C(0), C(1), B(32, 2, 32, 3), B(32, 2, -32, 3), C(4), C(5),
            B(32, 6, 32, 7), B(32, 6, -32, 7), C(8), C(9), B(32, 10, 32, 11),
            B(32, 10, -32, 11), C(12), C(13), B(32, 14, 32, 15), B(32, 14, -32, 15),
        ],
        [
            C(0), N(8), C(12), N(4), C(6), N(14), C(10), N(2), C(3), N(11), C(15),
            N(7), C(5), N(13), C(9), N(1),
        ],
    ]
}

butterfly! {
    /// The forward DCT of 4 points: libaom's butterflies, the inverse's transpose.
    fn fdct4(4, COSPI13, FORWARD_BITS, keep) [
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3),
        ],
        [
            B(32, 0, 32, 1), B(-32, 1, 32, 0), B(48, 2, 16, 3), B(48, 3, -16, 2),
        ],
        [
            C(0), C(2), C(1), C(3),
        ],
    ]
}

butterfly! {
    /// The forward DCT of 8 points: libaom's butterflies, the inverse's transpose.
    fn fdct8(8, COSPI13, FORWARD_BITS, keep) [
        [
            A(0, 7), A(1, 6), A(2, 5), A(3, 4), S(3, 4), S(2, 5), S(1, 6), S(0, 7),
        ],
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3), C(4), B(-32, 5, 32, 6),
            B(32, 6, 32, 5), C(7),
        ],
        [
            B(32, 0, 32, 1), B(-32, 1, 32, 0), B(48, 2, 16, 3), B(48, 3, -16, 2),
            A(4, 5), S(4, 5), S(7, 6), A(7, 6),
        ],
        [
            C(0), C(1), C(2), C(3), B(56, 4, 8, 7), B(24, 5, 40, 6),
            B(24, 6, -40, 5), B(56, 7, -8, 4),
        ],
        [
            C(0), C(4), C(2), C(6), C(1), C(5), C(3), C(7),
        ],
    ]
}

butterfly! {
    /// The forward DCT of 16 points: libaom's butterflies, the inverse's transpose.
    fn fdct16(16, COSPI13, FORWARD_BITS, keep) [
        [
            A(0, 15), A(1, 14), A(2, 13), A(3, 12), A(4, 11), A(5, 10), A(6, 9),
            A(7, 8), S(7, 8), S(6, 9), S(5, 10), S(4, 11), S(3, 12), S(2, 13),
            S(1, 14), S(0, 15),
        ],
        [
            A(0, 7), A(1, 6), A(2, 5), A(3, 4), S(3, 4), S(2, 5), S(1, 6), S(0, 7),
            C(8), C(9), B(-32, 10, 32, 13), B(-32, 11, 32, 12), B(32, 12, 32, 11),
            B(32, 13, 32, 10), C(14), C(15),
        ],
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3), C(4), B(-32, 5, 32, 6),
            B(32, 6, 32, 5), C(7), A(8, 11), A(9, 10), S(9, 10), S(8, 11),
            S(15, 12), S(14, 13), A(14, 13), A(15, 12),
        ],
        [
            B(32, 0, 32, 1), B(-32, 1, 32, 0), B(48, 2, 16, 3), B(48, 3, -16, 2),
            A(4, 5), S(4, 5), S(7, 6), A(7, 6), C(8), B(-16, 9, 48, 14),
            B(-48, 10, -16, 13), C(11), C(12), B(48, 13, -16, 10), B(16, 14, 48, 9),
            C(15),
        ],
        [
            C(0), C(1), C(2), C(3), B(56, 4, 8, 7), B(24, 5, 40, 6),
            B(24, 6, -40, 5), B(56, 7, -8, 4), A(8, 9), S(8, 9), S(11, 10),
            A(11, 10), A(12, 13), S(12, 13), S(15, 14), A(15, 14),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), B(60, 8, 4, 15),
            B(28, 9, 36, 14), B(44, 10, 20, 13), B(12, 11, 52, 12),
            B(12, 12, -52, 11), B(44, 13, -20, 10), B(28, 14, -36, 9),
            B(60, 15, -4, 8),
        ],
        [
            C(0), C(8), C(4), C(12), C(2), C(10), C(6), C(14), C(1), C(9), C(5),
            C(13), C(3), C(11), C(7), C(15),
        ],
    ]
}

butterfly! {
    /// The forward DCT of 32 points: libaom's butterflies, the inverse's transpose.
    fn fdct32(32, COSPI13, FORWARD_BITS, keep) [
        [
            A(0, 31), A(1, 30), A(2, 29), A(3, 28), A(4, 27), A(5, 26), A(6, 25),
            A(7, 24), A(8, 23), A(9, 22), A(10, 21), A(11, 20), A(12, 19),
            A(13, 18), A(14, 17), A(15, 16), S(15, 16), S(14, 17), S(13, 18),
            S(12, 19), S(11, 20), S(10, 21), S(9, 22), S(8, 23), S(7, 24), S(6, 25),
            S(5, 26), S(4, 27), S(3, 28), S(2, 29), S(1, 30), S(0, 31),
        ],
        [
            A(0, 15), A(1, 14), A(2, 13), A(3, 12), A(4, 11), A(5, 10), A(6, 9),
            A(7, 8), S(7, 8), S(6, 9), S(5, 10), S(4, 11), S(3, 12), S(2, 13),
            S(1, 14), S(0, 15), C(16), C(17), C(18), C(19), B(-32, 20, 32, 27),
            B(-32, 21, 32, 26), B(-32, 22, 32, 25), B(-32, 23, 32, 24),
            B(32, 24, 32, 23), B(32, 25, 32, 22), B(32, 26, 32, 21),
            B(32, 27, 32, 20), C(28), C(29), C(30), C(31),
        ],
        [
            A(0, 7), A(1, 6), A(2, 5), A(3, 4), S(3, 4), S(2, 5), S(1, 6), S(0, 7),
            C(8), C(9), B(-32, 10, 32, 13), B(-32, 11, 32, 12), B(32, 12, 32, 11),
            B(32, 13, 32, 10), C(14), C(15), A(16, 23), A(17, 22), A(18, 21),
            A(19, 20), S(19, 20), S(18, 21), S(17, 22), S(16, 23), S(31, 24),
            S(30, 25), S(29, 26), S(28, 27), A(28, 27), A(29, 26), A(30, 25),
            A(31, 24),
        ],
        [
            A(0, 3), A(1, 2), S(1, 2), S(0, 3), C(4), B(-32, 5, 32, 6),
            B(32, 6, 32, 5), C(7), A(8, 11), A(9, 10), S(9, 10), S(8, 11),
            S(15, 12), S(14, 13), A(14, 13), A(15, 12), C(16), C(17),
            B(-16, 18, 48, 29), B(-16, 19, 48, 28), B(-48, 20, -16, 27),
            B(-48, 21, -16, 26), C(22), C(23), C(24), C(25), B(48, 26, -16, 21),
            B(48, 27, -16, 20), B(16, 28, 48, 19), B(16, 29, 48, 18), C(30), C(31),
        ],
        [
            B(32, 0, 32, 1), B(-32, 1, 32, 0), B(48, 2, 16, 3), B(48, 3, -16, 2),
            A(4, 5), S(4, 5), S(7, 6), A(7, 6), C(8), B(-16, 9, 48, 14),
            B(-48, 10, -16, 13), C(11), C(12), B(48, 13, -16, 10), B(16, 14, 48, 9),
            C(15), A(16, 19), A(17, 18), S(17, 18), S(16, 19), S(23, 20), S(22, 21),
            A(22, 21), A(23, 20), A(24, 27), A(25, 26), S(25, 26), S(24, 27),
            S(31, 28), S(30, 29), A(30, 29), A(31, 28),
        ],
        [
            C(0), C(1), C(2), C(3), B(56, 4, 8, 7), B(24, 5, 40, 6),
            B(24, 6, -40, 5), B(56, 7, -8, 4), A(8, 9), S(8, 9), S(11, 10),
            A(11, 10), A(12, 13), S(12, 13), S(15, 14), A(15, 14), C(16),
            B(-8, 17, 56, 30), B(-56, 18, -8, 29), C(19), C(20), B(-40, 21, 24, 26),
            B(-24, 22, -40, 25), C(23), C(24), B(24, 25, -40, 22),
            B(40, 26, 24, 21), C(27), C(28), B(56, 29, -8, 18), B(8, 30, 56, 17),
            C(31),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), B(60, 8, 4, 15),
            B(28, 9, 36, 14), B(44, 10, 20, 13), B(12, 11, 52, 12),
            B(12, 12, -52, 11), B(44, 13, -20, 10), B(28, 14, -36, 9),
            B(60, 15, -4, 8), A(16, 17), S(16, 17), S(19, 18), A(19, 18), A(20, 21),
            S(20, 21), S(23, 22), A(23, 22), A(24, 25), S(24, 25), S(27, 26),
            A(27, 26), A(28, 29), S(28, 29), S(31, 30), A(31, 30),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), C(8), C(9), C(10),
            C(11), C(12), C(13), C(14), C(15), B(62, 16, 2, 31), B(30, 17, 34, 30),
            B(46, 18, 18, 29), B(14, 19, 50, 28), B(54, 20, 10, 27),
            B(22, 21, 42, 26), B(38, 22, 26, 25), B(6, 23, 58, 24),
            B(6, 24, -58, 23), B(38, 25, -26, 22), B(22, 26, -42, 21),
            B(54, 27, -10, 20), B(14, 28, -50, 19), B(46, 29, -18, 18),
            B(30, 30, -34, 17), B(62, 31, -2, 16),
        ],
        [
            C(0), C(16), C(8), C(24), C(4), C(20), C(12), C(28), C(2), C(18), C(10),
            C(26), C(6), C(22), C(14), C(30), C(1), C(17), C(9), C(25), C(5), C(21),
            C(13), C(29), C(3), C(19), C(11), C(27), C(7), C(23), C(15), C(31),
        ],
    ]
}

butterfly! {
    /// The forward ADST of 8 points: libaom's butterflies.
    fn fadst8(8, COSPI13, FORWARD_BITS, keep) [
        [
            C(0), N(7), N(3), C(4), N(1), C(6), C(2), N(5),
        ],
        [
            C(0), C(1), B(32, 2, 32, 3), B(32, 2, -32, 3), C(4), C(5),
            B(32, 6, 32, 7), B(32, 6, -32, 7),
        ],
        [
            A(0, 2), A(1, 3), S(0, 2), S(1, 3), A(4, 6), A(5, 7), S(4, 6), S(5, 7),
        ],
        [
            C(0), C(1), C(2), C(3), B(16, 4, 48, 5), B(48, 4, -16, 5),
            B(-48, 6, 16, 7), B(16, 6, 48, 7),
        ],
        [
            A(0, 4), A(1, 5), A(2, 6), A(3, 7), S(0, 4), S(1, 5), S(2, 6), S(3, 7),
        ],
        [
            B(4, 0, 60, 1), B(60, 0, -4, 1), B(20, 2, 44, 3), B(44, 2, -20, 3),
            B(36, 4, 28, 5), B(28, 4, -36, 5), B(52, 6, 12, 7), B(12, 6, -52, 7),
        ],
        [
            C(1), C(6), C(3), C(4), C(5), C(2), C(7), C(0),
        ],
    ]
}

butterfly! {
    /// The forward ADST of 16 points: libaom's butterflies.
    fn fadst16(16, COSPI13, FORWARD_BITS, keep) [
        [
            C(0), N(15), N(7), C(8), N(3), C(12), C(4), N(11), N(1), C(14), C(6),
            N(9), C(2), N(13), N(5), C(10),
        ],
        [
            C(0), C(1), B(32, 2, 32, 3), B(32, 2, -32, 3), C(4), C(5),
            B(32, 6, 32, 7), B(32, 6, -32, 7), C(8), C(9), B(32, 10, 32, 11),
            B(32, 10, -32, 11), C(12), C(13), B(32, 14, 32, 15), B(32, 14, -32, 15),
        ],
        [
            A(0, 2), A(1, 3), S(0, 2), S(1, 3), A(4, 6), A(5, 7), S(4, 6), S(5, 7),
            A(8, 10), A(9, 11), S(8, 10), S(9, 11), A(12, 14), A(13, 15), S(12, 14),
            S(13, 15),
        ],
        [
            C(0), C(1), C(2), C(3), B(16, 4, 48, 5), B(48, 4, -16, 5),
            B(-48, 6, 16, 7), B(16, 6, 48, 7), C(8), C(9), C(10), C(11),
            B(16, 12, 48, 13), B(48, 12, -16, 13), B(-48, 14, 16, 15),
            B(16, 14, 48, 15),
        ],
        [
            A(0, 4), A(1, 5), A(2, 6), A(3, 7), S(0, 4), S(1, 5), S(2, 6), S(3, 7),
            A(8, 12), A(9, 13), A(10, 14), A(11, 15), S(8, 12), S(9, 13), S(10, 14),
            S(11, 15),
        ],
        [
            C(0), C(1), C(2), C(3), C(4), C(5), C(6), C(7), B(8, 8, 56, 9),
            B(56, 8, -8, 9), B(40, 10, 24, 11), B(24, 10, -40, 11),
            B(-56, 12, 8, 13), B(8, 12, 56, 13), B(-24, 14, 40, 15),
            B(40, 14, 24, 15),
        ],
        [
            A(0, 8), A(1, 9), A(2, 10), A(3, 11), A(4, 12), A(5, 13), A(6, 14),
            A(7, 15), S(0, 8), S(1, 9), S(2, 10), S(3, 11), S(4, 12), S(5, 13),
            S(6, 14), S(7, 15),
        ],
        [
            B(2, 0, 62, 1), B(62, 0, -2, 1), B(10, 2, 54, 3), B(54, 2, -10, 3),
            B(18, 4, 46, 5), B(46, 4, -18, 5), B(26, 6, 38, 7), B(38, 6, -26, 7),
            B(34, 8, 30, 9), B(30, 8, -34, 9), B(42, 10, 22, 11),
            B(22, 10, -42, 11), B(50, 12, 14, 13), B(14, 12, -50, 13),
            B(58, 14, 6, 15), B(6, 14, -58, 15),
        ],
        [
            C(1), C(14), C(3), C(12), C(5), C(10), C(7), C(8), C(9), C(6), C(11),
            C(4), C(13), C(2), C(15), C(0),
        ],
    ]
}

/// The default scan of a 4x4 transform (spec `Default_Scan_4x4`),
/// each entry `row * 4 + column`.
#[rustfmt::skip]
pub const SCAN_4: [u16; 16] = [
    0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15,
];

/// The default scan of a 8x8 transform (spec `Default_Scan_8x8`),
/// each entry `row * 8 + column`.
#[rustfmt::skip]
pub const SCAN_8: [u16; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40,
    48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61,
    54, 47, 55, 62, 63,
];

/// The default scan of a 16x16 transform (spec `Default_Scan_16x16`),
/// each entry `row * 16 + column`.
#[rustfmt::skip]
pub const SCAN_16: [u16; 256] = [
    0, 1, 16, 32, 17, 2, 3, 18, 33, 48, 64, 49, 34, 19, 4, 5, 20, 35, 50, 65,
    80, 96, 81, 66, 51, 36, 21, 6, 7, 22, 37, 52, 67, 82, 97, 112, 128, 113, 98,
    83, 68, 53, 38, 23, 8, 9, 24, 39, 54, 69, 84, 99, 114, 129, 144, 160, 145,
    130, 115, 100, 85, 70, 55, 40, 25, 10, 11, 26, 41, 56, 71, 86, 101, 116,
    131, 146, 161, 176, 192, 177, 162, 147, 132, 117, 102, 87, 72, 57, 42, 27,
    12, 13, 28, 43, 58, 73, 88, 103, 118, 133, 148, 163, 178, 193, 208, 224,
    209, 194, 179, 164, 149, 134, 119, 104, 89, 74, 59, 44, 29, 14, 15, 30, 45,
    60, 75, 90, 105, 120, 135, 150, 165, 180, 195, 210, 225, 240, 241, 226, 211,
    196, 181, 166, 151, 136, 121, 106, 91, 76, 61, 46, 31, 47, 62, 77, 92, 107,
    122, 137, 152, 167, 182, 197, 212, 227, 242, 243, 228, 213, 198, 183, 168,
    153, 138, 123, 108, 93, 78, 63, 79, 94, 109, 124, 139, 154, 169, 184, 199,
    214, 229, 244, 245, 230, 215, 200, 185, 170, 155, 140, 125, 110, 95, 111,
    126, 141, 156, 171, 186, 201, 216, 231, 246, 247, 232, 217, 202, 187, 172,
    157, 142, 127, 143, 158, 173, 188, 203, 218, 233, 248, 249, 234, 219, 204,
    189, 174, 159, 175, 190, 205, 220, 235, 250, 251, 236, 221, 206, 191, 207,
    222, 237, 252, 253, 238, 223, 239, 254, 255,
];

/// The default scan of a 32x32 transform (spec `Default_Scan_32x32`),
/// each entry `row * 32 + column`.
#[rustfmt::skip]
pub const SCAN_32: [u16; 1024] = [
    0, 1, 32, 64, 33, 2, 3, 34, 65, 96, 128, 97, 66, 35, 4, 5, 36, 67, 98, 129,
    160, 192, 161, 130, 99, 68, 37, 6, 7, 38, 69, 100, 131, 162, 193, 224, 256,
    225, 194, 163, 132, 101, 70, 39, 8, 9, 40, 71, 102, 133, 164, 195, 226, 257,
    288, 320, 289, 258, 227, 196, 165, 134, 103, 72, 41, 10, 11, 42, 73, 104,
    135, 166, 197, 228, 259, 290, 321, 352, 384, 353, 322, 291, 260, 229, 198,
    167, 136, 105, 74, 43, 12, 13, 44, 75, 106, 137, 168, 199, 230, 261, 292,
    323, 354, 385, 416, 448, 417, 386, 355, 324, 293, 262, 231, 200, 169, 138,
    107, 76, 45, 14, 15, 46, 77, 108, 139, 170, 201, 232, 263, 294, 325, 356,
    387, 418, 449, 480, 512, 481, 450, 419, 388, 357, 326, 295, 264, 233, 202,
    171, 140, 109, 78, 47, 16, 17, 48, 79, 110, 141, 172, 203, 234, 265, 296,
    327, 358, 389, 420, 451, 482, 513, 544, 576, 545, 514, 483, 452, 421, 390,
    359, 328, 297, 266, 235, 204, 173, 142, 111, 80, 49, 18, 19, 50, 81, 112,
    143, 174, 205, 236, 267, 298, 329, 360, 391, 422, 453, 484, 515, 546, 577,
    608, 640, 609, 578, 547, 516, 485, 454, 423, 392, 361, 330, 299, 268, 237,
    206, 175, 144, 113, 82, 51, 20, 21, 52, 83, 114, 145, 176, 207, 238, 269,
    300, 331, 362, 393, 424, 455, 486, 517, 548, 579, 610, 641, 672, 704, 673,
    642, 611, 580, 549, 518, 487, 456, 425, 394, 363, 332, 301, 270, 239, 208,
    177, 146, 115, 84, 53, 22, 23, 54, 85, 116, 147, 178, 209, 240, 271, 302,
    333, 364, 395, 426, 457, 488, 519, 550, 581, 612, 643, 674, 705, 736, 768,
    737, 706, 675, 644, 613, 582, 551, 520, 489, 458, 427, 396, 365, 334, 303,
    272, 241, 210, 179, 148, 117, 86, 55, 24, 25, 56, 87, 118, 149, 180, 211,
    242, 273, 304, 335, 366, 397, 428, 459, 490, 521, 552, 583, 614, 645, 676,
    707, 738, 769, 800, 832, 801, 770, 739, 708, 677, 646, 615, 584, 553, 522,
    491, 460, 429, 398, 367, 336, 305, 274, 243, 212, 181, 150, 119, 88, 57, 26,
    27, 58, 89, 120, 151, 182, 213, 244, 275, 306, 337, 368, 399, 430, 461, 492,
    523, 554, 585, 616, 647, 678, 709, 740, 771, 802, 833, 864, 896, 865, 834,
    803, 772, 741, 710, 679, 648, 617, 586, 555, 524, 493, 462, 431, 400, 369,
    338, 307, 276, 245, 214, 183, 152, 121, 90, 59, 28, 29, 60, 91, 122, 153,
    184, 215, 246, 277, 308, 339, 370, 401, 432, 463, 494, 525, 556, 587, 618,
    649, 680, 711, 742, 773, 804, 835, 866, 897, 928, 960, 929, 898, 867, 836,
    805, 774, 743, 712, 681, 650, 619, 588, 557, 526, 495, 464, 433, 402, 371,
    340, 309, 278, 247, 216, 185, 154, 123, 92, 61, 30, 31, 62, 93, 124, 155,
    186, 217, 248, 279, 310, 341, 372, 403, 434, 465, 496, 527, 558, 589, 620,
    651, 682, 713, 744, 775, 806, 837, 868, 899, 930, 961, 992, 993, 962, 931,
    900, 869, 838, 807, 776, 745, 714, 683, 652, 621, 590, 559, 528, 497, 466,
    435, 404, 373, 342, 311, 280, 249, 218, 187, 156, 125, 94, 63, 95, 126, 157,
    188, 219, 250, 281, 312, 343, 374, 405, 436, 467, 498, 529, 560, 591, 622,
    653, 684, 715, 746, 777, 808, 839, 870, 901, 932, 963, 994, 995, 964, 933,
    902, 871, 840, 809, 778, 747, 716, 685, 654, 623, 592, 561, 530, 499, 468,
    437, 406, 375, 344, 313, 282, 251, 220, 189, 158, 127, 159, 190, 221, 252,
    283, 314, 345, 376, 407, 438, 469, 500, 531, 562, 593, 624, 655, 686, 717,
    748, 779, 810, 841, 872, 903, 934, 965, 996, 997, 966, 935, 904, 873, 842,
    811, 780, 749, 718, 687, 656, 625, 594, 563, 532, 501, 470, 439, 408, 377,
    346, 315, 284, 253, 222, 191, 223, 254, 285, 316, 347, 378, 409, 440, 471,
    502, 533, 564, 595, 626, 657, 688, 719, 750, 781, 812, 843, 874, 905, 936,
    967, 998, 999, 968, 937, 906, 875, 844, 813, 782, 751, 720, 689, 658, 627,
    596, 565, 534, 503, 472, 441, 410, 379, 348, 317, 286, 255, 287, 318, 349,
    380, 411, 442, 473, 504, 535, 566, 597, 628, 659, 690, 721, 752, 783, 814,
    845, 876, 907, 938, 969, 1000, 1001, 970, 939, 908, 877, 846, 815, 784, 753,
    722, 691, 660, 629, 598, 567, 536, 505, 474, 443, 412, 381, 350, 319, 351,
    382, 413, 444, 475, 506, 537, 568, 599, 630, 661, 692, 723, 754, 785, 816,
    847, 878, 909, 940, 971, 1002, 1003, 972, 941, 910, 879, 848, 817, 786, 755,
    724, 693, 662, 631, 600, 569, 538, 507, 476, 445, 414, 383, 415, 446, 477,
    508, 539, 570, 601, 632, 663, 694, 725, 756, 787, 818, 849, 880, 911, 942,
    973, 1004, 1005, 974, 943, 912, 881, 850, 819, 788, 757, 726, 695, 664, 633,
    602, 571, 540, 509, 478, 447, 479, 510, 541, 572, 603, 634, 665, 696, 727,
    758, 789, 820, 851, 882, 913, 944, 975, 1006, 1007, 976, 945, 914, 883, 852,
    821, 790, 759, 728, 697, 666, 635, 604, 573, 542, 511, 543, 574, 605, 636,
    667, 698, 729, 760, 791, 822, 853, 884, 915, 946, 977, 1008, 1009, 978, 947,
    916, 885, 854, 823, 792, 761, 730, 699, 668, 637, 606, 575, 607, 638, 669,
    700, 731, 762, 793, 824, 855, 886, 917, 948, 979, 1010, 1011, 980, 949, 918,
    887, 856, 825, 794, 763, 732, 701, 670, 639, 671, 702, 733, 764, 795, 826,
    857, 888, 919, 950, 981, 1012, 1013, 982, 951, 920, 889, 858, 827, 796, 765,
    734, 703, 735, 766, 797, 828, 859, 890, 921, 952, 983, 1014, 1015, 984, 953,
    922, 891, 860, 829, 798, 767, 799, 830, 861, 892, 923, 954, 985, 1016, 1017,
    986, 955, 924, 893, 862, 831, 863, 894, 925, 956, 987, 1018, 1019, 988, 957,
    926, 895, 927, 958, 989, 1020, 1021, 990, 959, 991, 1022, 1023,
];

/// The DC quantizer step of every `qindex` at 8 bits (spec `Dc_Qlookup[0]`).
#[rustfmt::skip]
pub const DC_Q: [u16; 256] = [
    4, 8, 8, 9, 10, 11, 12, 12, 13, 14, 15, 16, 17, 18, 19, 19, 20, 21, 22, 23,
    24, 25, 26, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 35, 36, 37, 38, 38, 39,
    40, 41, 42, 43, 43, 44, 45, 46, 47, 48, 48, 49, 50, 51, 52, 53, 53, 54, 55,
    56, 57, 57, 58, 59, 60, 61, 62, 62, 63, 64, 65, 66, 66, 67, 68, 69, 70, 70,
    71, 72, 73, 74, 74, 75, 76, 77, 78, 78, 79, 80, 81, 81, 82, 83, 84, 85, 85,
    87, 88, 90, 92, 93, 95, 96, 98, 99, 101, 102, 104, 105, 107, 108, 110, 111,
    113, 114, 116, 117, 118, 120, 121, 123, 125, 127, 129, 131, 134, 136, 138,
    140, 142, 144, 146, 148, 150, 152, 154, 156, 158, 161, 164, 166, 169, 172,
    174, 177, 180, 182, 185, 187, 190, 192, 195, 199, 202, 205, 208, 211, 214,
    217, 220, 223, 226, 230, 233, 237, 240, 243, 247, 250, 253, 257, 261, 265,
    269, 272, 276, 280, 284, 288, 292, 296, 300, 304, 309, 313, 317, 322, 326,
    330, 335, 340, 344, 349, 354, 359, 364, 369, 374, 379, 384, 389, 395, 400,
    406, 411, 417, 423, 429, 435, 441, 447, 454, 461, 467, 475, 482, 489, 497,
    505, 513, 522, 530, 539, 549, 559, 569, 579, 590, 602, 614, 626, 640, 654,
    668, 684, 700, 717, 736, 755, 775, 796, 819, 843, 869, 896, 925, 955, 988,
    1022, 1058, 1098, 1139, 1184, 1232, 1282, 1336,
];

/// The AC quantizer step of every `qindex` at 8 bits (spec `Ac_Qlookup[0]`).
#[rustfmt::skip]
pub const AC_Q: [u16; 256] = [
    4, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
    46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64,
    65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83,
    84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101,
    102, 104, 106, 108, 110, 112, 114, 116, 118, 120, 122, 124, 126, 128, 130,
    132, 134, 136, 138, 140, 142, 144, 146, 148, 150, 152, 155, 158, 161, 164,
    167, 170, 173, 176, 179, 182, 185, 188, 191, 194, 197, 200, 203, 207, 211,
    215, 219, 223, 227, 231, 235, 239, 243, 247, 251, 255, 260, 265, 270, 275,
    280, 285, 290, 295, 300, 305, 311, 317, 323, 329, 335, 341, 347, 353, 359,
    366, 373, 380, 387, 394, 401, 408, 416, 424, 432, 440, 448, 456, 465, 474,
    483, 492, 501, 510, 520, 530, 540, 550, 560, 571, 582, 593, 604, 615, 627,
    639, 651, 663, 676, 689, 702, 715, 729, 743, 757, 771, 786, 801, 816, 832,
    848, 864, 881, 898, 915, 933, 951, 969, 988, 1007, 1026, 1046, 1066, 1087,
    1108, 1129, 1151, 1173, 1196, 1219, 1243, 1267, 1292, 1317, 1343, 1369,
    1396, 1423, 1451, 1479, 1508, 1537, 1567, 1597, 1628, 1660, 1692, 1725,
    1759, 1793, 1828,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator: no dependency, no clock.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }

        fn signed(&mut self, magnitude: i32) -> i32 {
            (self.next() % (2 * magnitude as u32 + 1)) as i32 - magnitude
        }
    }

    const SIZES: [Size; 4] = [Size::S4, Size::S8, Size::S16, Size::S32];
    const TYPES: [TxType; 4] = [
        TxType::DctDct,
        TxType::AdstDct,
        TxType::DctAdst,
        TxType::AdstAdst,
    ];

    #[test]
    fn the_cosines_are_the_rounded_reals() {
        for (i, (c12, c13)) in COSPI12.iter().zip(COSPI13.iter()).enumerate() {
            let real = (i as f64 * std::f64::consts::PI / 128.0).cos();
            assert!(((real * 4096.0).round() as i32 - c12).abs() <= 1, "{i}");
            assert!(((real * 8192.0).round() as i32 - c13).abs() <= 1, "{i}");
        }
        for (i, (s12, s13)) in SINPI12.iter().zip(SINPI13.iter()).enumerate() {
            let real = (i as f64 * std::f64::consts::PI / 9.0).sin() * 2f64.sqrt() * 2.0 / 3.0;
            assert!(((real * 4096.0).round() as i32 - s12).abs() <= 2, "{i}");
            assert!(((real * 8192.0).round() as i32 - s13).abs() <= 3, "{i}");
        }
        assert_eq!(SINPI12[1] + SINPI12[2], SINPI12[4]);
        assert_eq!(SINPI13[1] + SINPI13[2], SINPI13[4]);
    }

    /// The unnormalised DCT-III the spec's inverse DCT computes: the DC
    /// term at `1/sqrt(2)`, the others at unit gain.
    fn reference_idct(coeffs: &[i32]) -> Vec<f64> {
        let n = coeffs.len();
        (0..n)
            .map(|i| {
                coeffs
                    .iter()
                    .enumerate()
                    .map(|(k, &c)| {
                        let scale = if k == 0 { 0.5f64.sqrt() } else { 1.0 };
                        f64::from(c)
                            * scale
                            * ((2 * i + 1) as f64 * k as f64 * std::f64::consts::PI
                                / (2 * n) as f64)
                                .cos()
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn the_inverse_dct_tables_compute_the_dct() {
        let mut rng = Lcg(7);
        for size in SIZES {
            let n = size.points();
            for _ in 0..200 {
                let coeffs: Vec<i32> = (0..n).map(|_| rng.signed(2000)).collect();
                let mut x = coeffs.clone();
                inverse_1d(Kernel::Dct, &mut x);
                let want = reference_idct(&coeffs);
                for (i, (got, want)) in x.iter().zip(want.iter()).enumerate() {
                    let error = (f64::from(*got) - want).abs();
                    assert!(error <= 8.0, "{size:?} [{i}] {got} vs {want}");
                }
            }
        }
    }

    /// The columns of each inverse kernel, the responses to unit
    /// impulses: nearly orthogonal at nearly equal norms, so a wrong
    /// index or sign in a table shows.
    #[test]
    fn every_inverse_kernel_is_orthogonal() {
        for size in SIZES {
            let n = size.points();
            for kernel in [Kernel::Dct, Kernel::Adst] {
                if kernel == Kernel::Adst && size == Size::S32 {
                    continue;
                }
                let columns: Vec<Vec<f64>> = (0..n)
                    .map(|k| {
                        let mut x = vec![0i32; n];
                        x[k] = 4096;
                        inverse_1d(kernel, &mut x);
                        x.iter().map(|&v| f64::from(v) / 4096.0).collect()
                    })
                    .collect();
                for a in 0..n {
                    for b in 0..n {
                        let dot: f64 = columns[a].iter().zip(&columns[b]).map(|(x, y)| x * y).sum();
                        let want = if a == b { (n / 2) as f64 } else { 0.0 };
                        assert!(
                            (dot - want).abs() < 0.02 * n as f64,
                            "{size:?} {kernel:?} {a} {b} {dot}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_forward_and_inverse_round_trip_a_residual() {
        let mut rng = Lcg(11);
        for size in SIZES {
            let n = size.points();
            for tx in TYPES {
                if size == Size::S32 && tx != TxType::DctDct {
                    continue;
                }
                for _ in 0..20 {
                    let residual: Vec<i32> = (0..n * n).map(|_| rng.signed(255)).collect();
                    let mut coeffs = vec![0i32; n * n];
                    forward(size, tx, &residual, &mut coeffs);
                    let mut back = vec![0i32; n * n];
                    inverse(size, tx, &coeffs, &mut back);
                    for (i, (b, r)) in back.iter().zip(&residual).enumerate() {
                        assert!((b - r).abs() <= 2, "{size:?} {tx:?} [{i}] {b} vs {r}");
                    }
                }
                // The extremes stay within the sixteen-bit ranges.
                for v in [-255, 255] {
                    let residual = vec![v; n * n];
                    let mut coeffs = vec![0i32; n * n];
                    forward(size, tx, &residual, &mut coeffs);
                    let mut back = vec![0i32; n * n];
                    inverse(size, tx, &coeffs, &mut back);
                    assert!(
                        back.iter().all(|b| (b - v).abs() <= 1),
                        "{size:?} {tx:?} {v}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_flat_block_is_one_dc_coefficient_at_the_documented_scale() {
        for (size, dc) in [
            (Size::S4, 32),
            (Size::S8, 64),
            (Size::S16, 128),
            (Size::S32, 128),
        ] {
            let n = size.points();
            let residual = vec![100i32; n * n];
            let mut coeffs = vec![0i32; n * n];
            forward(size, TxType::DctDct, &residual, &mut coeffs);
            let want = dc * 100;
            assert!(
                (coeffs[0] - want).abs() * 100 <= want,
                "{size:?} {}",
                coeffs[0]
            );
            assert!(coeffs[1..].iter().all(|&c| c.abs() <= 1), "{size:?}");
        }
    }

    #[test]
    fn a_wrong_length_is_left_alone() {
        let mut x = vec![5i32; 6];
        inverse_1d(Kernel::Dct, &mut x);
        forward_1d(Kernel::Adst, &mut x);
        let mut out = vec![7i32; 6];
        inverse(Size::S4, TxType::DctDct, &x, &mut out);
        forward(Size::S4, TxType::DctDct, &x, &mut out);
        assert_eq!(out, vec![7; 6]);
        assert_eq!(x, vec![5; 6]);
    }

    #[test]
    fn the_scans_walk_the_anti_diagonals_of_a_row_major_block() {
        for size in SIZES {
            let n = size.points();
            let scan = size.scan();
            let mut seen = vec![false; n * n];
            let mut last = 0;
            for (c, &pos) in scan.iter().enumerate() {
                let pos = usize::from(pos);
                assert!(!seen[pos], "{size:?} {c}");
                seen[pos] = true;
                let diagonal = pos / n + pos % n;
                assert!(diagonal >= last, "{size:?} {c}");
                last = diagonal;
            }
            assert!(seen.iter().all(|s| *s));
            assert_eq!(scan[0], 0);
            assert_eq!(scan[1], 1, "{size:?}: the horizontal frequency first");
        }
    }

    #[test]
    fn the_quantizer_steps_are_the_specs() {
        assert_eq!(dc_q(0), 4);
        assert_eq!(ac_q(0), 4);
        assert_eq!(dc_q(255), 1336);
        assert_eq!(ac_q(255), 1828);
        for q in 1..=255u8 {
            assert!(dc_q(q) >= dc_q(q - 1));
            assert!(ac_q(q) >= ac_q(q - 1));
        }
        assert_eq!(Size::S32.dequant_shift(), 1);
        assert_eq!(Size::S16.dequant_shift(), 0);
    }
}
