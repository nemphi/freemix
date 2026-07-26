//! Deterministic CPU reference color transforms.
//!
//! The working representation is premultiplied, linear-light BT.2020 RGB.
//! SDR linear value `1.0` denotes reference white. PQ transfer helpers expose
//! normalized absolute luminance (`1.0` is 10,000 cd/m2); the frame pipeline
//! converts that representation to reference-white-relative working light.

mod lut;
mod matrix;
#[cfg(feature = "native-wgpu")]
mod native;
mod pipeline;
mod tone_map;
mod transfer;

pub use fm_types::AlphaMode;
pub use lut::{Lut1D, Lut3D, LutError};
pub use matrix::{
    MatrixError, Rgb, Yuv, convert_primaries, decode_rgb_range, encode_rgb_range, rgb_to_yuv,
    yuv_to_rgb,
};
#[cfg(feature = "native-wgpu")]
pub use native::{
    NativeImportError, NativeImportNormalizer, NativeSdrOutputTransform,
    NativeSdrOutputTransformError, NativeWorkingFrame,
};
pub use pipeline::{
    ColorError, ColorPipeline, ConvertedImage, DecodedFrame, LinearFrame, LinearRgba,
    working_color_metadata, working_video_frame_metadata,
};
pub use tone_map::{ToneMapError, ToneMapPolicy, tone_map_rgb};
pub use transfer::{
    TransferError, bt709_from_linear, bt709_to_linear, decode_transfer, encode_transfer,
    hlg_from_linear, hlg_to_linear, pq_from_linear, pq_to_linear, srgb_from_linear, srgb_to_linear,
};
