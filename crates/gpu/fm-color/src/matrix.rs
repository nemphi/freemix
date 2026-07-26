use core::fmt;

use fm_types::{ColorPrimaries, MatrixCoefficients, SignalRange};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Rgb {
    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    #[must_use]
    pub fn map(self, operation: impl Fn(f32) -> f32) -> Self {
        Self::new(operation(self.r), operation(self.g), operation(self.b))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Yuv {
    pub y: f32,
    pub u: f32,
    pub v: f32,
}

impl Yuv {
    #[must_use]
    pub const fn new(y: f32, u: f32, v: f32) -> Self {
        Self { y, u, v }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MatrixError {
    UnsupportedMatrix(MatrixCoefficients),
    NonFiniteSample,
}

impl fmt::Display for MatrixError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedMatrix(matrix) => {
                write!(formatter, "matrix {matrix:?} cannot represent YUV")
            }
            Self::NonFiniteSample => formatter.write_str("color sample must be finite"),
        }
    }
}

impl std::error::Error for MatrixError {}

/// Converts a normalized YUV signal to transfer-encoded RGB.
///
/// # Errors
///
/// Returns an error for non-finite samples or the identity matrix, which does
/// not define YUV coefficients.
pub fn yuv_to_rgb(
    yuv: Yuv,
    matrix: MatrixCoefficients,
    range: SignalRange,
) -> Result<Rgb, MatrixError> {
    finite_yuv(yuv)?;
    let (kr, kb) = luma_coefficients(matrix)?;
    let y = match range {
        SignalRange::Full => yuv.y,
        SignalRange::Limited => (yuv.y - 16.0 / 255.0) * 255.0 / 219.0,
    };
    let (cb, cr) = match range {
        SignalRange::Full => (yuv.u - 0.5, yuv.v - 0.5),
        SignalRange::Limited => (
            (yuv.u - 128.0 / 255.0) * 255.0 / 224.0,
            (yuv.v - 128.0 / 255.0) * 255.0 / 224.0,
        ),
    };
    let kg = 1.0 - kr - kb;
    Ok(Rgb::new(
        y + 2.0 * (1.0 - kr) * cr,
        y - 2.0 * kb * (1.0 - kb) * cb / kg - 2.0 * kr * (1.0 - kr) * cr / kg,
        y + 2.0 * (1.0 - kb) * cb,
    ))
}

/// Converts transfer-encoded RGB to a normalized YUV signal.
///
/// # Errors
///
/// Returns an error for non-finite samples or the identity matrix, which does
/// not define YUV coefficients.
pub fn rgb_to_yuv(
    rgb: Rgb,
    matrix: MatrixCoefficients,
    range: SignalRange,
) -> Result<Yuv, MatrixError> {
    finite_rgb(rgb)?;
    let (kr, kb) = luma_coefficients(matrix)?;
    let kg = 1.0 - kr - kb;
    let y = kr * rgb.r + kg * rgb.g + kb * rgb.b;
    let cb = (rgb.b - y) / (2.0 * (1.0 - kb));
    let cr = (rgb.r - y) / (2.0 * (1.0 - kr));
    Ok(match range {
        SignalRange::Full => Yuv::new(y, cb + 0.5, cr + 0.5),
        SignalRange::Limited => Yuv::new(
            (16.0 + 219.0 * y) / 255.0,
            (128.0 + 224.0 * cb) / 255.0,
            (128.0 + 224.0 * cr) / 255.0,
        ),
    })
}

/// Expands a normalized RGB signal from its declared code range.
///
/// # Errors
///
/// Returns [`MatrixError::NonFiniteSample`] for any non-finite component.
pub fn decode_rgb_range(rgb: Rgb, range: SignalRange) -> Result<Rgb, MatrixError> {
    finite_rgb(rgb)?;
    Ok(match range {
        SignalRange::Full => rgb,
        SignalRange::Limited => rgb.map(|value| (value * 255.0 - 16.0) / 219.0),
    })
}

/// Compresses normalized RGB into its declared code range.
///
/// # Errors
///
/// Returns [`MatrixError::NonFiniteSample`] for any non-finite component.
pub fn encode_rgb_range(rgb: Rgb, range: SignalRange) -> Result<Rgb, MatrixError> {
    finite_rgb(rgb)?;
    Ok(match range {
        SignalRange::Full => rgb,
        SignalRange::Limited => rgb.map(|value| (16.0 + 219.0 * value) / 255.0),
    })
}

/// Converts linear RGB between D65 color-primary definitions via XYZ.
///
/// # Errors
///
/// Returns [`MatrixError::NonFiniteSample`] for any non-finite component.
pub fn convert_primaries(
    rgb: Rgb,
    source: ColorPrimaries,
    destination: ColorPrimaries,
) -> Result<Rgb, MatrixError> {
    finite_rgb(rgb)?;
    if source == destination {
        return Ok(rgb);
    }
    let xyz = multiply(rgb_to_xyz(source), rgb);
    Ok(multiply(xyz_to_rgb(destination), xyz))
}

fn luma_coefficients(matrix: MatrixCoefficients) -> Result<(f32, f32), MatrixError> {
    match matrix {
        MatrixCoefficients::Identity => Err(MatrixError::UnsupportedMatrix(matrix)),
        MatrixCoefficients::Bt601 => Ok((0.299, 0.114)),
        MatrixCoefficients::Bt709 => Ok((0.2126, 0.0722)),
        MatrixCoefficients::Bt2020NonConstant => Ok((0.2627, 0.0593)),
    }
}

type Matrix3 = [[f32; 3]; 3];

fn rgb_to_xyz(primaries: ColorPrimaries) -> Matrix3 {
    match primaries {
        ColorPrimaries::Bt601 => [
            [0.393_589_1, 0.365_249_7, 0.191_631_3],
            [0.212_413_2, 0.701_043_7, 0.086_543_2],
            [0.018_742_3, 0.111_931_3, 0.958_156_3],
        ],
        ColorPrimaries::Bt709 => [
            [0.412_390_8, 0.357_584_33, 0.180_480_8],
            [0.212_639, 0.715_168_65, 0.072_192_32],
            [0.019_330_82, 0.119_194_78, 0.950_532_14],
        ],
        ColorPrimaries::Bt2020 => [
            [0.636_958_06, 0.144_616_9, 0.168_880_98],
            [0.262_700_2, 0.677_998_07, 0.059_301_72],
            [0.0, 0.028_072_69, 1.060_985_1],
        ],
        ColorPrimaries::DisplayP3 => [
            [0.486_570_95, 0.265_667_7, 0.198_217_29],
            [0.228_974_57, 0.691_738_55, 0.079_286_91],
            [0.0, 0.045_113_38, 1.043_944_4],
        ],
    }
}

fn xyz_to_rgb(primaries: ColorPrimaries) -> Matrix3 {
    match primaries {
        ColorPrimaries::Bt601 => [
            [3.505_396, -1.739_489_4, -0.543_964],
            [-1.069_072_2, 1.977_824_4, 0.035_172_2],
            [0.056_32, -0.197_022_6, 1.050_202_6],
        ],
        ColorPrimaries::Bt709 => [
            [3.240_97, -1.537_383_2, -0.498_610_76],
            [-0.969_243_65, 1.875_967_5, 0.041_555_06],
            [0.055_630_08, -0.203_976_96, 1.056_971_5],
        ],
        ColorPrimaries::Bt2020 => [
            [1.716_651_2, -0.355_670_78, -0.253_366_3],
            [-0.666_684_3, 1.616_481_2, 0.015_768_55],
            [0.017_639_86, -0.042_770_61, 0.942_103_15],
        ],
        ColorPrimaries::DisplayP3 => [
            [2.493_497, -0.931_383_6, -0.402_710_8],
            [-0.829_489, 1.762_664, 0.023_624_69],
            [0.035_845_83, -0.076_172_39, 0.956_884_5],
        ],
    }
}

fn multiply(matrix: Matrix3, rgb: Rgb) -> Rgb {
    Rgb::new(
        matrix[0][0] * rgb.r + matrix[0][1] * rgb.g + matrix[0][2] * rgb.b,
        matrix[1][0] * rgb.r + matrix[1][1] * rgb.g + matrix[1][2] * rgb.b,
        matrix[2][0] * rgb.r + matrix[2][1] * rgb.g + matrix[2][2] * rgb.b,
    )
}

fn finite_rgb(rgb: Rgb) -> Result<(), MatrixError> {
    if rgb.r.is_finite() && rgb.g.is_finite() && rgb.b.is_finite() {
        Ok(())
    } else {
        Err(MatrixError::NonFiniteSample)
    }
}

fn finite_yuv(yuv: Yuv) -> Result<(), MatrixError> {
    if yuv.y.is_finite() && yuv.u.is_finite() && yuv.v.is_finite() {
        Ok(())
    } else {
        Err(MatrixError::NonFiniteSample)
    }
}
