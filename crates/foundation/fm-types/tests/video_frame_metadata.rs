use fm_types::{
    AlphaMode, ChromaLocation, ColorMetadata, ColorPrimaries, MatrixCoefficients, PixelFormat,
    SignalRange, TransferFunction, VideoFrameMetadata, VideoFrameMetadataError,
};

fn color(matrix: MatrixCoefficients, chroma_location: ChromaLocation) -> ColorMetadata {
    ColorMetadata {
        primaries: ColorPrimaries::DisplayP3,
        transfer: TransferFunction::Pq,
        matrix,
        range: SignalRange::Limited,
        chroma_location,
    }
}

#[test]
fn every_rgb_format_requires_identity_matrix_and_alpha() {
    for pixel_format in [
        PixelFormat::Rgba8,
        PixelFormat::Bgra8,
        PixelFormat::Rgba16Float,
    ] {
        for alpha_mode in [AlphaMode::Straight, AlphaMode::Premultiplied] {
            let metadata = VideoFrameMetadata::new(
                color(MatrixCoefficients::Identity, ChromaLocation::TopLeft),
                Some(alpha_mode),
            );
            assert_eq!(metadata.validate_for(pixel_format), Ok(()));
            assert_eq!(metadata.alpha_mode(), Some(alpha_mode));
            assert_eq!(metadata.color().chroma_location, ChromaLocation::TopLeft);
        }

        assert_eq!(
            VideoFrameMetadata::new(
                color(MatrixCoefficients::Bt709, ChromaLocation::Center),
                Some(AlphaMode::Straight),
            )
            .validate_for(pixel_format),
            Err(VideoFrameMetadataError::RgbMatrixMustBeIdentity {
                pixel_format,
                matrix: MatrixCoefficients::Bt709,
            })
        );
        assert_eq!(
            VideoFrameMetadata::new(
                color(MatrixCoefficients::Identity, ChromaLocation::Left),
                None,
            )
            .validate_for(pixel_format),
            Err(VideoFrameMetadataError::RgbAlphaModeRequired { pixel_format })
        );
    }
}

#[test]
fn every_yuv_format_requires_non_identity_matrix_and_no_alpha() {
    for pixel_format in [PixelFormat::Nv12, PixelFormat::P010, PixelFormat::Yuv422] {
        for matrix in [
            MatrixCoefficients::Bt601,
            MatrixCoefficients::Bt709,
            MatrixCoefficients::Bt2020NonConstant,
        ] {
            let metadata = VideoFrameMetadata::new(color(matrix, ChromaLocation::Left), None);
            assert_eq!(metadata.validate_for(pixel_format), Ok(()));
            assert_eq!(metadata.color().matrix, matrix);
            assert_eq!(metadata.alpha_mode(), None);
        }

        assert_eq!(
            VideoFrameMetadata::new(
                color(MatrixCoefficients::Identity, ChromaLocation::Center),
                None,
            )
            .validate_for(pixel_format),
            Err(VideoFrameMetadataError::YuvMatrixMustNotBeIdentity { pixel_format })
        );
        assert_eq!(
            VideoFrameMetadata::new(
                color(MatrixCoefficients::Bt709, ChromaLocation::Center),
                Some(AlphaMode::Premultiplied),
            )
            .validate_for(pixel_format),
            Err(VideoFrameMetadataError::YuvAlphaModeNotAllowed {
                pixel_format,
                alpha_mode: AlphaMode::Premultiplied,
            })
        );
    }
}
