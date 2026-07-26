use crate::Rgba8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorRange {
    Full,
    Limited,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Yuv8 {
    pub y: u8,
    pub u: u8,
    pub v: u8,
}

impl Yuv8 {
    #[must_use]
    pub const fn new(y: u8, u: u8, v: u8) -> Self {
        Self { y, u, v }
    }
}

/// Converts an RGB pixel to deterministic 8-bit YUV using fixed-point
/// broadcast coefficients. Alpha is ignored.
#[must_use]
pub fn rgb_to_yuv(pixel: Rgba8, matrix: ColorMatrix, range: ColorRange) -> Yuv8 {
    let red = i32::from(pixel.r);
    let green = i32::from(pixel.g);
    let blue = i32::from(pixel.b);
    let (luma, chroma_blue, chroma_red) = match (matrix, range) {
        (ColorMatrix::Bt601, ColorRange::Full) => (
            (77 * red + 150 * green + 29 * blue + 128) >> 8,
            (128 * 256 - 43 * red - 85 * green + 128 * blue + 128) >> 8,
            (128 * 256 + 128 * red - 107 * green - 21 * blue + 128) >> 8,
        ),
        (ColorMatrix::Bt709, ColorRange::Full) => (
            (54 * red + 183 * green + 19 * blue + 128) >> 8,
            (128 * 256 - 29 * red - 99 * green + 128 * blue + 128) >> 8,
            (128 * 256 + 128 * red - 116 * green - 12 * blue + 128) >> 8,
        ),
        (ColorMatrix::Bt601, ColorRange::Limited) => (
            16 + ((66 * red + 129 * green + 25 * blue + 128) >> 8),
            128 + ((-38 * red - 74 * green + 112 * blue + 128) >> 8),
            128 + ((112 * red - 94 * green - 18 * blue + 128) >> 8),
        ),
        (ColorMatrix::Bt709, ColorRange::Limited) => (
            16 + ((47 * red + 157 * green + 16 * blue + 128) >> 8),
            128 + ((-26 * red - 86 * green + 112 * blue + 128) >> 8),
            128 + ((112 * red - 102 * green - 10 * blue + 128) >> 8),
        ),
    };
    Yuv8::new(clamp_u8(luma), clamp_u8(chroma_blue), clamp_u8(chroma_red))
}

/// Converts deterministic 8-bit YUV to opaque RGB using fixed-point broadcast
/// coefficients. Out-of-gamut values are clipped to the RGB byte range.
#[must_use]
pub fn yuv_to_rgb(pixel: Yuv8, matrix: ColorMatrix, range: ColorRange) -> Rgba8 {
    let luma = i32::from(pixel.y);
    let chroma_blue = i32::from(pixel.u) - 128;
    let chroma_red = i32::from(pixel.v) - 128;
    let (red, green, blue) = match (matrix, range) {
        (ColorMatrix::Bt601, ColorRange::Full) => (
            luma + rounded_shift(359 * chroma_red),
            luma + rounded_shift(-88 * chroma_blue - 183 * chroma_red),
            luma + rounded_shift(454 * chroma_blue),
        ),
        (ColorMatrix::Bt709, ColorRange::Full) => (
            luma + rounded_shift(403 * chroma_red),
            luma + rounded_shift(-48 * chroma_blue - 120 * chroma_red),
            luma + rounded_shift(475 * chroma_blue),
        ),
        (ColorMatrix::Bt601, ColorRange::Limited) => {
            let scaled_luma = 298 * (luma - 16);
            (
                rounded_shift(scaled_luma + 409 * chroma_red),
                rounded_shift(scaled_luma - 100 * chroma_blue - 208 * chroma_red),
                rounded_shift(scaled_luma + 516 * chroma_blue),
            )
        }
        (ColorMatrix::Bt709, ColorRange::Limited) => {
            let scaled_luma = 298 * (luma - 16);
            (
                rounded_shift(scaled_luma + 459 * chroma_red),
                rounded_shift(scaled_luma - 55 * chroma_blue - 136 * chroma_red),
                rounded_shift(scaled_luma + 541 * chroma_blue),
            )
        }
    };
    Rgba8::new(clamp_u8(red), clamp_u8(green), clamp_u8(blue), u8::MAX)
}

fn rounded_shift(value: i32) -> i32 {
    if value >= 0 {
        (value + 128) >> 8
    } else {
        -((-value + 128) >> 8)
    }
}

fn clamp_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(if value < 0 { 0 } else { u8::MAX })
}
