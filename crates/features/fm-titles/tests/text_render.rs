//! Font rasterization tests.
//!
//! These need a real font file. They look for `FM_TITLES_FONT`, then well
//! known system font paths, then scan the system font roots. When nothing is
//! found they skip, unless `FM_REQUIRE_FONT=1` makes absence a failure.

use fm_titles::{
    Alignment, AssetCatalog, Bounds, Color, Degradation, DegradedElement, Element, ElementId,
    ElementKind, FieldDefinition, FieldId, FieldValue, FontStyle, HorizontalAlignment,
    MAX_GLYPHS_PER_ELEMENT, ReferenceRenderer, Style, TemplateId, TickerDirection, TickerSpec,
    TitleId, TitleTemplate, VerticalAlignment,
};
use std::fs;
use std::num::NonZeroU128;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

/// Renders `message` into a 400x120 frame through a 120x40 box at 20 px, and
/// returns the raw pixels. The box is narrow enough that a mis-placed line
/// leaves the frame entirely.
fn render_in_narrow_box(assets: &AssetCatalog, message: &str) -> Vec<u8> {
    let scene = scene_of(
        message,
        400,
        120,
        text_element(
            Bounds {
                x: 20,
                y: 10,
                width: 120,
                height: 40,
            },
            text_style(20, Color::new(255, 255, 255, 255), Alignment::default()),
        ),
    )
    .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
    .expect("valid template");
    ReferenceRenderer
        .render(&scene, 0, assets)
        .expect("frame renders")
        .frame
        .pixels()
        .to_vec()
}

#[test]
fn leading_whitespace_never_hides_text_and_a_tab_is_one_space() {
    let Some(assets) = catalog() else { return };
    // An operator pasting "    John Smith" out of a spreadsheet: leading
    // whitespace used to advance the pen without committing a glyph, so the
    // wrap test was skipped, the word was laid out past the box, and every
    // pixel of it was clipped. `render` returned Ok with an empty report and
    // the name never appeared on air.
    let plain = render_in_narrow_box(&assets, "WWWW");
    assert!(
        ink_bounds(&plain, 400).is_some(),
        "the sample word must ink at all"
    );
    for indent in [4, 20, 40] {
        let indented = render_in_narrow_box(&assets, &format!("{}WWWW", " ".repeat(indent)));
        assert_eq!(
            indented, plain,
            "{indent} leading spaces must be dropped, not indent the line off the box"
        );
    }

    // Interior whitespace still separates words, and a tab is exactly one
    // space rather than a control character that vanishes.
    let tabbed = render_in_narrow_box(&assets, "W\tW");
    assert_eq!(
        tabbed,
        render_in_narrow_box(&assets, "W W"),
        "a tab must lay out as one space"
    );
    assert_ne!(
        tabbed,
        render_in_narrow_box(&assets, "WW"),
        "a tab must not be dropped, joining the words on either side"
    );
}

#[test]
fn whitespace_only_input_is_bounded_by_the_element_cap() {
    let Some(assets) = catalog() else { return };
    // Newlines and spaces used to be laid out before the cap was charged: each
    // newline pushed a line into an uncapped Vec and each space walked the pen,
    // so this input allocated hundreds of megabytes and took seconds inside a
    // 16 ms frame budget, then returned Ok.
    let mut message = "\n".repeat(2_000_000);
    message.push_str(&" ".repeat(2_000_000));
    message.push_str("END");
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

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let output = ReferenceRenderer
            .render(&scene, 0, &assets)
            .expect("frame renders");
        sender.send(output.report.degraded).ok();
    });
    let degraded = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("layout must be bounded by the cap, not by the length of the input");

    assert_eq!(
        degraded,
        vec![DegradedElement {
            element: ElementId::new(NonZeroU128::new(1).unwrap()),
            reason: Degradation::TextTruncated {
                maximum: MAX_GLYPHS_PER_ELEMENT
            },
        }],
        "whitespace must be charged against the cap and reported"
    );
}

#[test]
fn text_over_the_element_cap_degrades_one_element_not_the_frame() {
    let Some(assets) = catalog() else { return };
    let bounds = Bounds {
        x: 0,
        y: 0,
        width: 64,
        height: 64,
    };
    let message = "A".repeat(MAX_GLYPHS_PER_ELEMENT + 1);
    let scene = scene_of(
        &message,
        64,
        64,
        text_element(
            bounds,
            text_style(8, Color::new(255, 255, 255, 255), Alignment::default()),
        ),
    )
    .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
    .expect("valid template");

    // One over-long field used to fail the whole render: the frame that was
    // supposed to go to air did not exist at all.
    let output = ReferenceRenderer
        .render(&scene, 0, &assets)
        .expect("an over-long field must not take the frame off air");
    assert!(
        ink_bounds(output.frame.pixels(), 64).is_some(),
        "the element must still draw the text that fit"
    );
    assert_eq!(
        output.report.degraded,
        vec![DegradedElement {
            element: ElementId::new(NonZeroU128::new(1).unwrap()),
            reason: Degradation::TextTruncated {
                maximum: MAX_GLYPHS_PER_ELEMENT
            },
        }],
        "the truncation must be reported, not silent"
    );

    // One character fewer is exactly at the cap and is not degraded, so the
    // cap is exact rather than a symptom of the text being large.
    let scene = scene_of(
        &message[..MAX_GLYPHS_PER_ELEMENT],
        64,
        64,
        text_element(
            bounds,
            text_style(8, Color::new(255, 255, 255, 255), Alignment::default()),
        ),
    )
    .instantiate(TitleId::new(NonZeroU128::new(1).unwrap()))
    .expect("valid template");
    assert!(
        ReferenceRenderer
            .render(&scene, 0, &assets)
            .expect("frame renders")
            .report
            .degraded
            .is_empty()
    );
}
