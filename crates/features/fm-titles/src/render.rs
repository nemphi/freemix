use crate::{
    Alignment, AssetCatalog, Bounds, Color, Element, ElementKind, FieldValue, HorizontalAlignment,
    TitleScene, VerticalAlignment, animation::evaluate_tracks, evaluate_clock,
    evaluate_ticker_position, validate_scene,
};
use core::fmt;
use fm_video::{FrameError, ImageFrame};

pub const REFERENCE_RENDERER_LIMITATIONS: &[&str] = &[
    "text uses fixed deterministic block glyphs without shaping, kerning, bidi, or font rasterization",
    "image elements render placeholders; image assets are reported but never decoded",
    "layout and animation use integer CPU arithmetic and omit production GPU effects",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderReport {
    pub missing_fonts: Vec<crate::MissingFont>,
    pub missing_images: Vec<crate::MissingImage>,
    pub limitations: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderOutput {
    pub frame: ImageFrame,
    pub report: RenderReport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderError {
    Frame(FrameError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<FrameError> for RenderError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReferenceRenderer;

impl ReferenceRenderer {
    /// Renders a premultiplied RGBA8 frame in deterministic z/index order.
    ///
    /// # Errors
    ///
    /// Returns an [`fm_video::FrameError`] if the output layout is invalid or
    /// exceeds the bounded frame allocation limit.
    pub fn render(
        self,
        scene: &TitleScene,
        scene_time_ms: u64,
        assets: &AssetCatalog,
    ) -> Result<RenderOutput, RenderError> {
        let stride = usize::try_from(scene.width())
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(FrameError::LayoutOverflow)?;
        let length = stride
            .checked_mul(usize::try_from(scene.height()).map_err(|_| FrameError::LayoutOverflow)?)
            .ok_or(FrameError::LayoutOverflow)?;
        if length > ImageFrame::MAX_BUFFER_BYTES {
            return Err(FrameError::BufferTooLarge {
                required: length,
                maximum: ImageFrame::MAX_BUFFER_BYTES,
            }
            .into());
        }
        let mut pixels = vec![0; length];
        fill_canvas(
            &mut pixels,
            scene.width(),
            scene.height(),
            scene.background(),
        );

        let mut elements: Vec<(usize, &Element)> = scene.elements().iter().enumerate().collect();
        elements.sort_by_key(|(index, element)| (element.z_index, *index));
        for (_, element) in elements {
            if element.visible {
                draw_element(
                    &mut pixels,
                    scene.width(),
                    scene.height(),
                    scene,
                    element,
                    scene_time_ms,
                );
            }
        }

        let validation = validate_scene(scene, assets);
        Ok(RenderOutput {
            frame: ImageFrame::new(scene.width(), scene.height(), stride, pixels)?,
            report: RenderReport {
                missing_fonts: validation.missing_fonts,
                missing_images: validation.missing_images,
                limitations: REFERENCE_RENDERER_LIMITATIONS,
            },
        })
    }
}

fn fill_canvas(pixels: &mut [u8], width: u32, height: u32, color: Color) {
    let color = premultiply(color, 255);
    for y in 0..height {
        for x in 0..width {
            set_raw(pixels, width, x, y, color);
        }
    }
}

fn draw_element(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    scene: &TitleScene,
    element: &Element,
    time_ms: u64,
) {
    let evaluated = evaluate_tracks(
        element.bounds,
        element.style.opacity,
        &element.animations,
        time_ms,
    );
    if evaluated.bounds.width == 0 || evaluated.bounds.height == 0 {
        return;
    }
    let fill = element
        .color_field
        .and_then(|field| scene.field_value(field))
        .and_then(|value| match value {
            FieldValue::Color(color) => Some(*color),
            _ => None,
        })
        .unwrap_or(element.style.fill);

    match element.kind {
        ElementKind::Rectangle => draw_rect(
            pixels,
            width,
            height,
            evaluated.bounds,
            fill,
            evaluated.opacity,
        ),
        ElementKind::ImagePlaceholder { .. } => draw_placeholder(
            pixels,
            width,
            height,
            evaluated.bounds,
            element
                .style
                .background
                .unwrap_or(Color::new(64, 64, 64, 255)),
            fill,
            evaluated.opacity,
        ),
        ElementKind::Text { field } => {
            if let Some(text) = scene.field_value(field).and_then(FieldValue::display_text) {
                draw_text(
                    pixels,
                    width,
                    height,
                    evaluated.bounds,
                    &text,
                    element,
                    fill,
                    evaluated.opacity,
                    None,
                );
            }
        }
        ElementKind::Clock(spec) => draw_text(
            pixels,
            width,
            height,
            evaluated.bounds,
            &evaluate_clock(spec, time_ms),
            element,
            fill,
            evaluated.opacity,
            None,
        ),
        ElementKind::Ticker(spec) => {
            if let Some(text) = scene
                .field_value(spec.field)
                .and_then(FieldValue::display_text)
            {
                let content_width = text_width(&text, font_size(element));
                let relative_x =
                    evaluate_ticker_position(spec, time_ms, evaluated.bounds.width, content_width);
                draw_text(
                    pixels,
                    width,
                    height,
                    evaluated.bounds,
                    &text,
                    element,
                    fill,
                    evaluated.opacity,
                    Some(relative_x),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    bounds: Bounds,
    text: &str,
    element: &Element,
    color: Color,
    opacity: u8,
    forced_relative_x: Option<i64>,
) {
    if let Some(background) = element.style.background {
        draw_rect(pixels, width, height, bounds, background, opacity);
    }

    let size = font_size(element);
    let glyph_width = (size / 2).max(1);
    let spacing = 1_u32;
    let line_height = size.saturating_add(1);
    let lines: Vec<&str> = text.lines().collect();
    let lines = if lines.is_empty() { vec![""] } else { lines };
    let block_height = line_height.saturating_mul(u32::try_from(lines.len()).unwrap_or(u32::MAX));
    let relative_y = aligned_offset(
        bounds.height,
        block_height,
        element.style.alignment.vertical,
    );

    for (line_index, line) in lines.iter().enumerate() {
        let line_width = text_width(line, size);
        let relative_x = forced_relative_x.unwrap_or_else(|| {
            aligned_offset_horizontal(bounds.width, line_width, element.style.alignment)
        });
        let y = i64::from(bounds.y)
            + relative_y
            + i64::try_from(line_index).unwrap_or(i64::MAX) * i64::from(line_height);
        for (character_index, character) in line.chars().enumerate() {
            if !character.is_whitespace() {
                let x = i64::from(bounds.x)
                    + relative_x
                    + i64::try_from(character_index).unwrap_or(i64::MAX)
                        * i64::from(glyph_width.saturating_add(spacing));
                draw_clipped_glyph(
                    pixels,
                    width,
                    height,
                    bounds,
                    x,
                    y,
                    glyph_width,
                    size,
                    color,
                    opacity,
                );
            }
        }
    }
}

fn font_size(element: &Element) -> u32 {
    element
        .style
        .font
        .as_ref()
        .map_or(8, |font| font.size_px)
        .max(1)
}

fn text_width(text: &str, size: u32) -> u32 {
    let count = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
    if count == 0 {
        return 0;
    }
    let glyph_width = (size / 2).max(1);
    count
        .saturating_mul(glyph_width.saturating_add(1))
        .saturating_sub(1)
}

fn aligned_offset_horizontal(viewport: u32, content: u32, alignment: Alignment) -> i64 {
    match alignment.horizontal {
        HorizontalAlignment::Start => 0,
        HorizontalAlignment::Center => (i64::from(viewport) - i64::from(content)) / 2,
        HorizontalAlignment::End => i64::from(viewport) - i64::from(content),
    }
}

fn aligned_offset(viewport: u32, content: u32, alignment: VerticalAlignment) -> i64 {
    match alignment {
        VerticalAlignment::Start => 0,
        VerticalAlignment::Center => (i64::from(viewport) - i64::from(content)) / 2,
        VerticalAlignment::End => i64::from(viewport) - i64::from(content),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_clipped_glyph(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    clip: Bounds,
    x: i64,
    y: i64,
    glyph_width: u32,
    glyph_height: u32,
    color: Color,
    opacity: u8,
) {
    for glyph_y in 0..glyph_height {
        for glyph_x in 0..glyph_width {
            let destination_x = x + i64::from(glyph_x);
            let destination_y = y + i64::from(glyph_y);
            if in_bounds(destination_x, destination_y, clip) {
                blend_at(
                    pixels,
                    width,
                    height,
                    destination_x,
                    destination_y,
                    color,
                    opacity,
                );
            }
        }
    }
}

fn draw_rect(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    bounds: Bounds,
    color: Color,
    opacity: u8,
) {
    for local_y in 0..bounds.height {
        for local_x in 0..bounds.width {
            blend_at(
                pixels,
                width,
                height,
                i64::from(bounds.x) + i64::from(local_x),
                i64::from(bounds.y) + i64::from(local_y),
                color,
                opacity,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_placeholder(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    bounds: Bounds,
    background: Color,
    mark: Color,
    opacity: u8,
) {
    draw_rect(pixels, width, height, bounds, background, opacity);
    let last_x = bounds.width.saturating_sub(1);
    let last_y = bounds.height.saturating_sub(1);
    for y in 0..bounds.height {
        let diagonal = if bounds.height <= 1 {
            0
        } else {
            u64::from(y) * u64::from(last_x) / u64::from(last_y.max(1))
        };
        for x in [
            u32::try_from(diagonal).unwrap_or(last_x),
            last_x.saturating_sub(u32::try_from(diagonal).unwrap_or(last_x)),
        ] {
            blend_at(
                pixels,
                width,
                height,
                i64::from(bounds.x) + i64::from(x),
                i64::from(bounds.y) + i64::from(y),
                mark,
                opacity,
            );
        }
    }
}

fn in_bounds(x: i64, y: i64, bounds: Bounds) -> bool {
    x >= i64::from(bounds.x)
        && y >= i64::from(bounds.y)
        && x < i64::from(bounds.x) + i64::from(bounds.width)
        && y < i64::from(bounds.y) + i64::from(bounds.height)
}

fn blend_at(pixels: &mut [u8], width: u32, height: u32, x: i64, y: i64, color: Color, opacity: u8) {
    let Ok(x) = u32::try_from(x) else { return };
    let Ok(y) = u32::try_from(y) else { return };
    if x >= width || y >= height {
        return;
    }
    let Some(offset) = pixel_offset(width, x, y) else {
        return;
    };
    let source = premultiply(color, opacity);
    let inverse = u8::MAX - source[3];
    for channel in 0..3 {
        pixels[offset + channel] =
            source[channel].saturating_add(multiply_u8(pixels[offset + channel], inverse));
    }
    pixels[offset + 3] = source[3].saturating_add(multiply_u8(pixels[offset + 3], inverse));
}

fn set_raw(pixels: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 4]) {
    if let Some(offset) = pixel_offset(width, x, y) {
        pixels[offset..offset + 4].copy_from_slice(&color);
    }
}

fn pixel_offset(width: u32, x: u32, y: u32) -> Option<usize> {
    usize::try_from(y)
        .ok()?
        .checked_mul(usize::try_from(width).ok()?)?
        .checked_add(usize::try_from(x).ok()?)?
        .checked_mul(4)
}

fn premultiply(color: Color, opacity: u8) -> [u8; 4] {
    let alpha = multiply_u8(color.a, opacity);
    [
        multiply_u8(color.r, alpha),
        multiply_u8(color.g, alpha),
        multiply_u8(color.b, alpha),
        alpha,
    ]
}

fn multiply_u8(left: u8, right: u8) -> u8 {
    let product = u16::from(left) * u16::from(right) + 127;
    u8::try_from(product / 255).unwrap_or(u8::MAX)
}
