use fm_video::{
    ColorMatrix, ColorRange, CompositeError, CropError, CropRect, ImageFrame, Layer, Rgba8,
    Rotation, Transform, Yuv8, apply_opacity_premultiplied, compose_layers, crop,
    premultiply_alpha, rgb_to_yuv, scale_nearest, transform_nearest, yuv_to_rgb,
};

fn frame(width: u32, height: u32, pixels: &[Rgba8]) -> ImageFrame {
    let bytes = pixels.iter().flat_map(|pixel| pixel.to_bytes()).collect();
    ImageFrame::new(width, height, usize::try_from(width).unwrap() * 4, bytes).unwrap()
}

fn pixels(frame: &ImageFrame) -> Vec<Rgba8> {
    (0..frame.height())
        .flat_map(|y| (0..frame.width()).map(move |x| frame.pixel(x, y).unwrap()))
        .collect()
}

#[test]
fn crop_has_golden_pixels_and_validates_boundaries() {
    let source = frame(
        3,
        2,
        &[
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
            Rgba8::new(5, 0, 0, 255),
            Rgba8::new(6, 0, 0, 255),
        ],
    );

    let output = crop(&source, CropRect::new(1, 0, 2, 2)).unwrap();
    assert_eq!(
        pixels(&output),
        [
            Rgba8::new(2, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(5, 0, 0, 255),
            Rgba8::new(6, 0, 0, 255),
        ]
    );
    assert_eq!(
        crop(&source, CropRect::new(2, 0, 2, 1)),
        Err(CropError::OutOfBounds {
            frame_width: 3,
            frame_height: 2,
        })
    );
    assert_eq!(
        crop(&source, CropRect::new(u32::MAX, 0, 2, 1)),
        Err(CropError::BoundsOverflow)
    );
}

#[test]
fn nearest_scale_repeats_source_pixels_exactly() {
    let source = frame(
        2,
        2,
        &[
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
        ],
    );
    let output = scale_nearest(&source, 4, 2).unwrap();
    assert_eq!(
        pixels(&output),
        [
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
        ]
    );
}

#[test]
fn opacity_multiplies_every_premultiplied_channel() {
    let straight = frame(1, 1, &[Rgba8::new(200, 100, 50, 128)]);
    let premultiplied = premultiply_alpha(&straight).unwrap();
    assert_eq!(
        premultiplied.pixel(0, 0),
        Some(Rgba8::new(100, 50, 25, 128))
    );
    assert_eq!(
        apply_opacity_premultiplied(&premultiplied, 128)
            .unwrap()
            .pixel(0, 0),
        Some(Rgba8::new(50, 25, 13, 64))
    );

    let invalid = frame(1, 1, &[Rgba8::new(200, 0, 0, 100)]);
    assert!(matches!(
        apply_opacity_premultiplied(&invalid, 255),
        Err(CompositeError::NotPremultiplied { .. })
    ));
}

#[test]
fn composition_clips_positions_and_obeys_stable_z_order() {
    let red = frame(2, 1, &[Rgba8::new(255, 0, 0, 255); 2]);
    let green = frame(1, 1, &[Rgba8::new(0, 255, 0, 255)]);
    let blue = frame(1, 1, &[Rgba8::new(0, 0, 255, 255)]);
    let layers = [
        Layer::new(&blue, 1, 0, 20, 255),
        Layer::new(&red, -1, 0, 0, 255),
        Layer::new(&green, 1, 0, 10, 255),
    ];

    let output = compose_layers(3, 1, Rgba8::new(0, 0, 0, 0), &layers).unwrap();
    assert_eq!(
        pixels(&output),
        [
            Rgba8::new(255, 0, 0, 255),
            Rgba8::new(0, 0, 255, 255),
            Rgba8::new(0, 0, 0, 0),
        ]
    );

    let stable = compose_layers(
        1,
        1,
        Rgba8::new(0, 0, 0, 0),
        &[
            Layer::new(&green, 0, 0, 5, 255),
            Layer::new(&blue, 0, 0, 5, 255),
        ],
    )
    .unwrap();
    assert_eq!(stable.pixel(0, 0), Some(Rgba8::new(0, 0, 255, 255)));
}

#[test]
fn composition_uses_premultiplied_source_over_alpha() {
    let half_red = frame(1, 1, &[Rgba8::new(128, 0, 0, 128)]);
    let output = compose_layers(
        1,
        1,
        Rgba8::new(0, 0, 255, 255),
        &[Layer::new(&half_red, 0, 0, 0, 128)],
    )
    .unwrap();
    assert_eq!(output.pixel(0, 0), Some(Rgba8::new(64, 0, 191, 255)));
}

#[test]
fn transform_scales_rotates_translates_and_clips_at_boundaries() {
    let source = frame(
        2,
        2,
        &[
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
        ],
    );
    let rotated =
        transform_nearest(&source, 2, 2, Transform::new(0, 0, 2, 2, Rotation::Deg90)).unwrap();
    assert_eq!(
        pixels(&rotated),
        [
            Rgba8::new(3, 0, 0, 255),
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(4, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
        ]
    );

    let clipped =
        transform_nearest(&source, 2, 2, Transform::new(-1, 1, 4, 2, Rotation::Deg0)).unwrap();
    assert_eq!(
        pixels(&clipped),
        [
            Rgba8::new(0, 0, 0, 0),
            Rgba8::new(0, 0, 0, 0),
            Rgba8::new(1, 0, 0, 255),
            Rgba8::new(2, 0, 0, 255),
        ]
    );
}

#[test]
fn color_conversion_matches_bt601_bt709_full_and_limited_vectors() {
    let vectors = [
        (
            ColorMatrix::Bt601,
            ColorRange::Full,
            Rgba8::new(255, 0, 0, 255),
            Yuv8::new(77, 85, 255),
        ),
        (
            ColorMatrix::Bt709,
            ColorRange::Full,
            Rgba8::new(0, 255, 0, 255),
            Yuv8::new(182, 29, 12),
        ),
        (
            ColorMatrix::Bt601,
            ColorRange::Limited,
            Rgba8::new(0, 0, 255, 255),
            Yuv8::new(41, 240, 110),
        ),
        (
            ColorMatrix::Bt709,
            ColorRange::Limited,
            Rgba8::new(255, 255, 255, 255),
            Yuv8::new(235, 128, 128),
        ),
    ];

    for (matrix, range, rgb, expected_yuv) in vectors {
        let yuv = rgb_to_yuv(rgb, matrix, range);
        assert_eq!(yuv, expected_yuv);
        let round_trip = yuv_to_rgb(yuv, matrix, range);
        assert_channel_close(round_trip.r, rgb.r, 2);
        assert_channel_close(round_trip.g, rgb.g, 2);
        assert_channel_close(round_trip.b, rgb.b, 2);
    }

    assert_eq!(
        yuv_to_rgb(
            Yuv8::new(16, 128, 128),
            ColorMatrix::Bt709,
            ColorRange::Limited,
        ),
        Rgba8::new(0, 0, 0, 255)
    );
}

fn assert_channel_close(actual: u8, expected: u8, tolerance: u8) {
    assert!(
        actual.abs_diff(expected) <= tolerance,
        "channel {actual} differs from {expected} by more than {tolerance}"
    );
}
