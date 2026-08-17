use fm_titles::{
    Alignment, AnimatedProperty, AnimationTrack, AssetCatalog, Bounds, ClockDirection, ClockFormat,
    ClockSpec, Color, Element, ElementId, ElementKind, FieldDefinition, FieldId, FieldType,
    FieldValue, FontError, FontFace, FontStyle, ImageSource, Interpolation, Keyframe,
    MAX_OUTPUT_WIDTH, ReferenceRenderer, RenderError, Style, TemplateId, TickerDirection,
    TickerSpec, TitleId, TitleTemplate, UpdateError, ValidationError, evaluate_clock,
    evaluate_ticker_position, validate_template,
};
use std::num::NonZeroU128;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn field_id(value: u128) -> FieldId {
    FieldId::new(NonZeroU128::new(value).unwrap())
}

fn element_id(value: u128) -> ElementId {
    ElementId::new(NonZeroU128::new(value).unwrap())
}

fn template_id(value: u128) -> TemplateId {
    TemplateId::new(NonZeroU128::new(value).unwrap())
}

fn title_id(value: u128) -> TitleId {
    TitleId::new(NonZeroU128::new(value).unwrap())
}

fn style(fill: Color) -> Style {
    Style {
        fill,
        background: None,
        opacity: 255,
        font: None,
        alignment: Alignment::default(),
    }
}

fn rectangle(id: u128, z_index: i32, fill: Color, bounds: Bounds) -> Element {
    Element {
        id: element_id(id),
        name: format!("rectangle-{id}"),
        bounds,
        z_index,
        visible: true,
        style: style(fill),
        color_field: None,
        kind: ElementKind::Rectangle,
        animations: Vec::new(),
    }
}

fn template(
    fields: Vec<FieldDefinition>,
    elements: Vec<Element>,
    width: u32,
    height: u32,
) -> TitleTemplate {
    TitleTemplate {
        id: template_id(1),
        name: "test template".into(),
        width,
        height,
        background: Color::TRANSPARENT,
        fields,
        elements,
    }
}

#[test]
fn fields_are_typed_and_ids_survive_instantiation() {
    let fields = vec![
        FieldDefinition {
            id: field_id(1),
            name: "name".into(),
            default: FieldValue::Text("Ada".into()),
        },
        FieldDefinition {
            id: field_id(2),
            name: "score".into(),
            default: FieldValue::Number(3),
        },
        FieldDefinition {
            id: field_id(3),
            name: "logo".into(),
            default: FieldValue::Image(ImageSource::new("logo.png")),
        },
        FieldDefinition {
            id: field_id(4),
            name: "accent".into(),
            default: FieldValue::Color(Color::new(1, 2, 3, 255)),
        },
    ];
    assert_eq!(fields[0].field_type(), FieldType::Text);
    assert_eq!(fields[1].field_type(), FieldType::Number);
    assert_eq!(fields[2].field_type(), FieldType::Image);
    assert_eq!(fields[3].field_type(), FieldType::Color);

    let mut scene = template(fields, Vec::new(), 16, 9)
        .instantiate(title_id(9))
        .unwrap();
    assert_eq!(scene.id(), title_id(9));
    assert_eq!(scene.template_id(), template_id(1));
    assert_eq!(scene.field_value(field_id(2)), Some(&FieldValue::Number(3)));

    assert_eq!(
        scene.update_field(0, field_id(2), FieldValue::Number(4)),
        Ok(1)
    );
    let mismatch = scene.update_field(1, field_id(2), FieldValue::Text("four".into()));
    assert_eq!(
        mismatch,
        Err(UpdateError::TypeMismatch {
            field: field_id(2),
            expected: FieldType::Number,
            actual: FieldType::Text,
        })
    );
    assert_eq!(scene.field_value(field_id(2)), Some(&FieldValue::Number(4)));
    assert_eq!(scene.revision(), 1);
}

#[test]
fn z_order_controls_exact_pixels() {
    let bounds = Bounds {
        x: 0,
        y: 0,
        width: 2,
        height: 2,
    };
    let elements = vec![
        rectangle(1, 10, Color::new(255, 0, 0, 255), bounds),
        rectangle(2, -10, Color::new(0, 0, 255, 255), bounds),
    ];
    let scene = template(Vec::new(), elements, 2, 2)
        .instantiate(title_id(1))
        .unwrap();
    let output = ReferenceRenderer
        .render(&scene, 0, &AssetCatalog::new())
        .unwrap();

    assert_eq!(
        output.frame.pixel(0, 0).unwrap().to_bytes(),
        [255, 0, 0, 255]
    );
    assert_eq!(
        output.frame.pixel(1, 1).unwrap().to_bytes(),
        [255, 0, 0, 255]
    );
}

#[test]
fn animation_interpolation_is_integer_and_deterministic() {
    let linear = AnimationTrack {
        property: AnimatedProperty::X,
        keyframes: vec![
            Keyframe {
                at_ms: 0,
                value: -10,
                interpolation: Interpolation::Linear,
            },
            Keyframe {
                at_ms: 1_000,
                value: 11,
                interpolation: Interpolation::Hold,
            },
        ],
    };
    assert_eq!(linear.value_at(0), Some(-10));
    assert_eq!(linear.value_at(500), Some(0));
    assert_eq!(linear.value_at(1_000), Some(11));

    let hold = AnimationTrack {
        property: AnimatedProperty::Opacity,
        keyframes: vec![
            Keyframe {
                at_ms: 0,
                value: 0,
                interpolation: Interpolation::Hold,
            },
            Keyframe {
                at_ms: 100,
                value: 255,
                interpolation: Interpolation::Linear,
            },
        ],
    };
    assert_eq!(hold.value_at(99), Some(0));
    assert_eq!(hold.value_at(100), Some(255));
}

#[test]
fn ticker_wrap_and_both_clock_directions_are_stable() {
    let ticker = TickerSpec {
        field: field_id(1),
        direction: TickerDirection::Left,
        pixels_per_second: 10,
        gap_px: 10,
        starts_at_ms: 0,
    };
    assert_eq!(evaluate_ticker_position(ticker, 0, 100, 20), 100);
    assert_eq!(evaluate_ticker_position(ticker, 5_000, 100, 20), 50);
    assert_eq!(evaluate_ticker_position(ticker, 13_000, 100, 20), 100);

    let count_up = ClockSpec {
        direction: ClockDirection::CountUp,
        start_value_ms: 59_000,
        starts_at_ms: 1_000,
        format: ClockFormat::MinutesSeconds,
    };
    assert_eq!(evaluate_clock(count_up, 0), "00:59");
    assert_eq!(evaluate_clock(count_up, 2_500), "01:00");

    let count_down = ClockSpec {
        direction: ClockDirection::CountDown,
        start_value_ms: 3_600_000,
        starts_at_ms: 500,
        format: ClockFormat::HoursMinutesSeconds,
    };
    assert_eq!(evaluate_clock(count_down, 1_500), "00:59:59");
    assert_eq!(evaluate_clock(count_down, 4_000_000), "00:00:00");
}

#[test]
fn optimistic_conflicts_and_batch_errors_are_atomic() {
    let fields = vec![
        FieldDefinition {
            id: field_id(1),
            name: "left".into(),
            default: FieldValue::Number(1),
        },
        FieldDefinition {
            id: field_id(2),
            name: "right".into(),
            default: FieldValue::Number(2),
        },
    ];
    let mut scene = template(fields, Vec::new(), 1, 1)
        .instantiate(title_id(1))
        .unwrap();
    scene
        .update_field(0, field_id(1), FieldValue::Number(10))
        .unwrap();
    assert_eq!(
        scene.update_field(0, field_id(2), FieldValue::Number(20)),
        Err(UpdateError::RevisionConflict {
            expected: 0,
            actual: 1,
        })
    );

    let bad_batch = [
        (field_id(1), FieldValue::Number(11)),
        (field_id(2), FieldValue::Text("wrong".into())),
    ];
    assert!(matches!(
        scene.update_fields(1, &bad_batch),
        Err(UpdateError::TypeMismatch { field, .. }) if field == field_id(2)
    ));
    assert_eq!(
        scene.field_value(field_id(1)),
        Some(&FieldValue::Number(10))
    );
    assert_eq!(scene.field_value(field_id(2)), Some(&FieldValue::Number(2)));
    assert_eq!(scene.revision(), 1);
}

#[test]
fn validation_and_rendering_report_missing_assets_and_limitations() {
    let logo = field_id(1);
    let fields = vec![FieldDefinition {
        id: logo,
        name: "logo".into(),
        default: FieldValue::Image(ImageSource::new("missing-logo.png")),
    }];
    let element = Element {
        id: element_id(1),
        name: "logo".into(),
        bounds: Bounds {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        },
        z_index: 0,
        visible: true,
        style: Style {
            fill: Color::new(255, 255, 255, 255),
            background: Some(Color::BLACK),
            opacity: 255,
            font: Some(FontStyle {
                family: "Missing Sans".into(),
                size_px: 8,
            }),
            alignment: Alignment::default(),
        },
        color_field: None,
        kind: ElementKind::ImagePlaceholder { field: logo },
        animations: Vec::new(),
    };
    let template = template(fields, vec![element], 8, 8);
    let report = validate_template(&template, &AssetCatalog::new());
    assert!(report.is_valid());
    assert_eq!(report.missing_fonts[0].family, "Missing Sans");
    assert_eq!(report.missing_images[0].asset, "missing-logo.png");

    let scene = template.instantiate(title_id(1)).unwrap();
    let rendered = ReferenceRenderer
        .render(&scene, 0, &AssetCatalog::new())
        .unwrap();
    assert_eq!(rendered.report.missing_fonts, report.missing_fonts);
    assert_eq!(rendered.report.missing_images, report.missing_images);
    assert!(
        rendered
            .report
            .limitations
            .iter()
            .any(|limitation| limitation.contains("no complex-script shaping"))
    );
    assert!(
        rendered
            .report
            .limitations
            .iter()
            .any(|limitation| limitation.contains("never decoded"))
    );
}

#[test]
fn text_elements_require_a_resolvable_font_style() {
    let message = field_id(1);
    let fields = vec![FieldDefinition {
        id: message,
        name: "message".into(),
        default: FieldValue::Text("hello".into()),
    }];
    let bounds = Bounds {
        x: 0,
        y: 0,
        width: 8,
        height: 8,
    };
    let mut element = Element {
        id: element_id(1),
        name: "text".into(),
        bounds,
        z_index: 0,
        visible: true,
        style: style(Color::new(255, 255, 255, 255)),
        color_field: None,
        kind: ElementKind::Text { field: message },
        animations: Vec::new(),
    };
    let without_font = template(fields.clone(), vec![element.clone()], 8, 8);
    assert!(
        validate_template(&without_font, &AssetCatalog::new())
            .errors
            .contains(&ValidationError::MissingFontStyle(element_id(1)))
    );

    element.style.font = Some(FontStyle {
        family: "  ".into(),
        size_px: 8,
    });
    let blank_family = template(fields, vec![element], 8, 8);
    assert!(
        validate_template(&blank_family, &AssetCatalog::new())
            .errors
            .contains(&ValidationError::EmptyFontFamily(element_id(1)))
    );
}

#[test]
fn invalid_font_bytes_produce_typed_errors() {
    assert_eq!(
        FontFace::from_bytes(Vec::new()).err(),
        Some(FontError::Empty)
    );
    assert_eq!(
        FontFace::from_bytes(vec![0; 64]).err(),
        Some(FontError::Unparsable)
    );
    // A plausible-looking TrueType header with no tables must be refused too,
    // rather than parsing into a face that panics during layout.
    let mut truncated = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x09];
    truncated.extend_from_slice(&[0xff; 250]);
    assert!(FontFace::from_bytes(truncated).is_err());
}

#[test]
fn an_element_far_larger_than_the_canvas_costs_only_the_canvas() {
    // Element extents are `u32` and animatable to `u32::MAX`, so an operator
    // can put this on air with a valid template. Filling it must iterate the
    // canvas intersection; iterating the declared extent and discarding the
    // off-canvas pixels one at a time never finishes.
    let bounds = Bounds {
        x: -1_000_000,
        y: -1_000_000,
        width: 4_000_000_000,
        height: 4_000_000_000,
    };
    let scene = template(
        Vec::new(),
        vec![rectangle(1, 0, Color::new(0, 255, 0, 255), bounds)],
        64,
        64,
    )
    .instantiate(title_id(1))
    .unwrap();

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let output = ReferenceRenderer
            .render(&scene, 0, &AssetCatalog::new())
            .unwrap();
        sender.send(output.frame.pixels().to_vec()).ok();
    });
    let pixels = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("a 64x64 render must be bounded by the canvas, not by the element extent");

    // The canvas is entirely inside the element, so every pixel is filled.
    assert!(
        pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [0, 255, 0, 255]),
        "the clipped fill must still cover the whole canvas"
    );
}

#[test]
fn oversized_output_is_refused_before_allocating() {
    let scene = template(Vec::new(), Vec::new(), MAX_OUTPUT_WIDTH + 1, 16)
        .instantiate(title_id(1))
        .unwrap();
    let error = ReferenceRenderer
        .render(&scene, 0, &AssetCatalog::new())
        .unwrap_err();
    assert!(
        matches!(
            error,
            RenderError::OutputTooLarge {
                max_width: MAX_OUTPUT_WIDTH,
                ..
            }
        ),
        "expected an output size cap error, got {error}"
    );
}
