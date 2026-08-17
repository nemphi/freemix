//! Font rasterization tests.
//!
//! These need a real font file. They look for `FM_TITLES_FONT`, then well
//! known system font paths, then scan the system font roots. When nothing is
//! found they skip, unless `FM_REQUIRE_FONT=1` makes absence a failure.

use fm_titles::{
    Alignment, AssetCatalog, Bounds, Color, Element, ElementId, ElementKind, FieldDefinition,
    FieldId, FieldValue, FontStyle, HorizontalAlignment, MAX_GLYPHS_PER_ELEMENT, ReferenceRenderer,
    RenderError, Style, TemplateId, TickerDirection, TickerSpec, TitleId, TitleTemplate,
    VerticalAlignment,
};
use std::fs;
use std::num::NonZeroU128;
use std::path::{Path, PathBuf};

const FAMILY: &str = "test-face";

const DIRECT_FONTS: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
];

const FONT_ROOTS: &[&str] = &[
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/Library/Fonts",
    "/System/Library/Fonts",
    "C:\\Windows\\Fonts",
];

fn font_bytes() -> Option<Vec<u8>> {
    if let Some(bytes) = discover_font() {
        return Some(bytes);
    }
    assert!(
        std::env::var("FM_REQUIRE_FONT").as_deref() != Ok("1"),
        "FM_REQUIRE_FONT=1 but no usable font file was found"
    );
    eprintln!("skipping title text rendering: no font file available");
    None
}

fn discover_font() -> Option<Vec<u8>> {
    if let Ok(path) = std::env::var("FM_TITLES_FONT") {
        return fs::read(path).ok();
    }
    for path in DIRECT_FONTS {
        if let Ok(bytes) = fs::read(path) {
            return Some(bytes);
        }
    }
    FONT_ROOTS
        .iter()
        .find_map(|root| scan_for_font(Path::new(root), 0))
}

fn scan_for_font(directory: &Path, depth: u32) -> Option<Vec<u8>> {
    if depth > 4 {
        return None;
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries.iter().filter(|path| path.is_file()) {
        let usable = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("ttf") || extension.eq_ignore_ascii_case("otf")
            });
        if usable && let Ok(bytes) = fs::read(path) {
            return Some(bytes);
        }
    }
    entries
        .iter()
        .filter(|path| path.is_dir())
        .find_map(|path| scan_for_font(path, depth + 1))
}

fn catalog() -> Option<AssetCatalog> {
    let bytes = font_bytes()?;
    Some(
        AssetCatalog::new()
            .with_font_bytes(FAMILY, bytes)
            .expect("system font parses"),
    )
}

fn field_id(value: u128) -> FieldId {
    FieldId::new(NonZeroU128::new(value).unwrap())
}

fn text_style(size_px: u32, fill: Color, alignment: Alignment) -> Style {
    Style {
        fill,
        background: None,
        opacity: 255,
        font: Some(FontStyle {
            family: FAMILY.into(),
            size_px,
        }),
        alignment,
    }
}

fn text_element(bounds: Bounds, style: Style) -> Element {
    Element {
        id: ElementId::new(NonZeroU128::new(1).unwrap()),
        name: "text".into(),
        bounds,
        z_index: 0,
        visible: true,
        style,
        color_field: None,
        kind: ElementKind::Text { field: field_id(1) },
        animations: Vec::new(),
    }
}

fn scene_of(text: &str, width: u32, height: u32, element: Element) -> TitleTemplate {
    TitleTemplate {
        id: TemplateId::new(NonZeroU128::new(1).unwrap()),
        name: "text template".into(),
        width,
        height,
        background: Color::TRANSPARENT,
        fields: vec![FieldDefinition {
            id: field_id(1),
            name: "message".into(),
            default: FieldValue::Text(text.into()),
        }],
        elements: vec![element],
    }
}

/// Bounding box of every pixel with non-zero alpha, as `(min_x, min_y, max_x, max_y)`.
fn ink_bounds(pixels: &[u8], width: u32) -> Option<(u32, u32, u32, u32)> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (index, pixel) in pixels.chunks_exact(4).enumerate() {
        if pixel[3] == 0 {
            continue;
        }
        let index = u32::try_from(index).unwrap();
        let (x, y) = (index % width, index / width);
        bounds = Some(match bounds {
            None => (x, y, x, y),
            Some((min_x, min_y, max_x, max_y)) => {
                (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
            }
        });
    }
    bounds
}

#[test]
fn identical_input_renders_byte_identical_output() {
    let Some(assets) = catalog() else { return };
    let bounds = Bounds {
        x: 4,
        y: 4,
        width: 120,
        height: 56,
    };
    let mut template = scene_of(
        "Live production\ntitle text",
        128,
        64,
        text_element(
            bounds,
            text_style(
                18,
                Color::new(240, 230, 210, 255),
                Alignment {
                    horizontal: HorizontalAlignment::Center,
                    vertical: VerticalAlignment::Center,
                },
            ),
        ),
    );
    let mut ticker = text_element(
        Bounds { y: 44, ..bounds },
        text_style(12, Color::new(0, 180, 255, 255), Alignment::default()),
    );
    ticker.id = ElementId::new(NonZeroU128::new(2).unwrap());
    ticker.z_index = 1;
    ticker.kind = ElementKind::Ticker(TickerSpec {
        field: field_id(1),
        direction: TickerDirection::Left,
        pixels_per_second: 40,
        gap_px: 8,
        starts_at_ms: 0,
    });
    template.elements.push(ticker);

    let scene = template
        .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
        .expect("valid template");
    let first = ReferenceRenderer.render(&scene, 1_234, &assets).unwrap();
    let second = ReferenceRenderer.render(&scene, 1_234, &assets).unwrap();
    // A separately instantiated scene must also rasterize identically: no
    // cached or accumulated state may leak between renders.
    let reinstantiated = template
        .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
        .expect("valid template");
    let third = ReferenceRenderer
        .render(&reinstantiated, 1_234, &assets)
        .unwrap();

    assert_eq!(first.frame.pixels(), second.frame.pixels());
    assert_eq!(first.frame.pixels(), third.frame.pixels());
    assert!(
        first.frame.pixels().iter().any(|channel| *channel != 0),
        "expected rasterized ink"
    );
    assert!(first.report.missing_fonts.is_empty());
}

#[test]
fn glyph_coverage_is_premultiplied_over_a_transparent_background() {
    let Some(assets) = catalog() else { return };
    let template = scene_of(
        "Ag",
        64,
        48,
        text_element(
            Bounds {
                x: 0,
                y: 0,
                width: 64,
                height: 48,
            },
            text_style(
                32,
                Color::new(255, 0, 0, 255),
                Alignment {
                    horizontal: HorizontalAlignment::Center,
                    vertical: VerticalAlignment::Center,
                },
            ),
        ),
    );
    let scene = template
        .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
        .expect("valid template");
    let output = ReferenceRenderer.render(&scene, 0, &assets).unwrap();

    let mut partial = 0_u32;
    let mut opaque = 0_u32;
    for pixel in output.frame.pixels().chunks_exact(4) {
        let (red, green, blue, alpha) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        // Pure red at full opacity: premultiplication must scale the colour by
        // coverage exactly as it scales alpha, and must not leave colour in
        // fully transparent pixels.
        assert_eq!(
            red, alpha,
            "red channel must equal alpha for premultiplied opaque red"
        );
        assert_eq!(
            (green, blue),
            (0, 0),
            "no colour bleed into unused channels"
        );
        match alpha {
            0 => {}
            u8::MAX => opaque += 1,
            _ => partial += 1,
        }
    }
    assert!(partial > 0, "expected anti-aliased edge coverage");
    assert!(opaque > 0, "expected fully covered interior pixels");
}

#[test]
fn text_wraps_inside_and_is_clipped_to_the_element_box() {
    let Some(assets) = catalog() else { return };
    let message = "AAAA AAAA AAAA AAAA AAAA AAAA";
    let box_bounds = Bounds {
        x: 20,
        y: 10,
        width: 120,
        height: 40,
    };
    let narrow = scene_of(
        message,
        400,
        120,
        text_element(
            box_bounds,
            text_style(20, Color::new(255, 255, 255, 255), Alignment::default()),
        ),
    )
    .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
    .expect("valid template");
    let wide = scene_of(
        message,
        400,
        120,
        text_element(
            Bounds {
                x: 0,
                y: 10,
                width: 400,
                height: 40,
            },
            text_style(20, Color::new(255, 255, 255, 255), Alignment::default()),
        ),
    )
    .instantiate(TitleId::new(NonZeroU128::new(2).unwrap()))
    .expect("valid template");

    let narrow = ReferenceRenderer.render(&narrow, 0, &assets).unwrap();
    let wide = ReferenceRenderer.render(&wide, 0, &assets).unwrap();
    let (min_x, min_y, max_x, max_y) =
        ink_bounds(narrow.frame.pixels(), 400).expect("expected rasterized ink");
    let (_, _, wide_max_x, wide_max_y) =
        ink_bounds(wide.frame.pixels(), 400).expect("expected ink");

    // Every inked pixel stays inside the element box: overflow is clipped, and
    // wrapping never lets a line escape the box width.
    assert!(min_x >= box_bounds.x.try_into().unwrap(), "ink left of box");
    assert!(min_y >= box_bounds.y.try_into().unwrap(), "ink above box");
    assert!(max_x < 20 + box_bounds.width, "ink right of box");
    assert!(max_y < 10 + box_bounds.height, "ink below box");
    // The same text fits on one line in the wide box, so it must be wider than
    // the narrow box: the narrow render really is wrapping, and its extra
    // lines push ink further down.
    assert!(
        wide_max_x > box_bounds.width,
        "the sample text should not fit the narrow box on one line"
    );
    assert!(
        max_y > wide_max_y,
        "narrow box should wrap onto more lines: {max_y} vs {wide_max_y}"
    );
}

#[test]
fn text_longer_than_the_element_glyph_cap_is_refused() {
    let Some(assets) = catalog() else { return };
    let message = "A".repeat(MAX_GLYPHS_PER_ELEMENT + 1);
    let scene = scene_of(
        &message,
        64,
        64,
        text_element(
            Bounds {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            text_style(8, Color::new(255, 255, 255, 255), Alignment::default()),
        ),
    )
    .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
    .expect("valid template");

    let error = ReferenceRenderer.render(&scene, 0, &assets).unwrap_err();
    assert!(
        matches!(
            error,
            RenderError::TextTooLong {
                maximum: MAX_GLYPHS_PER_ELEMENT,
                ..
            }
        ),
        "expected a glyph cap error, got {error}"
    );

    // One glyph fewer renders normally, so the cap is exact rather than a
    // symptom of the text being large.
    let scene = scene_of(
        &message[..MAX_GLYPHS_PER_ELEMENT],
        64,
        64,
        text_element(
            Bounds {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            text_style(8, Color::new(255, 255, 255, 255), Alignment::default()),
        ),
    )
    .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
    .expect("valid template");
    assert!(ReferenceRenderer.render(&scene, 0, &assets).is_ok());
}
