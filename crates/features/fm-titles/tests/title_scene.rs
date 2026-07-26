use fm_titles::{
    Alignment, AnimatedProperty, AnimationTrack, AssetCatalog, Bounds, ClockDirection, ClockFormat,
    ClockSpec, Color, Element, ElementId, ElementKind, FieldDefinition, FieldId, FieldType,
    FieldValue, FontStyle, HorizontalAlignment, ImageSource, Interpolation, Keyframe,
    ReferenceRenderer, Style, TemplateId, TickerDirection, TickerSpec, TitleId, TitleTemplate,
    UpdateError, VerticalAlignment, evaluate_clock, evaluate_ticker_position, validate_template,
};
use std::num::NonZeroU128;

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
            .any(|limitation| limitation.contains("without shaping"))
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
#[allow(clippy::too_many_lines)]
fn all_element_kinds_produce_repeatable_pixels() {
    let text_field = field_id(1);
    let image_field = field_id(2);
    let color_field = field_id(3);
    let fields = vec![
        FieldDefinition {
            id: text_field,
            name: "message".into(),
            default: FieldValue::Text("AB".into()),
        },
        FieldDefinition {
            id: image_field,
            name: "image".into(),
            default: FieldValue::Image(ImageSource::new("image.png")),
        },
        FieldDefinition {
            id: color_field,
            name: "color".into(),
            default: FieldValue::Color(Color::new(12, 34, 56, 255)),
        },
    ];
    let text_style = Style {
        fill: Color::new(255, 255, 255, 255),
        background: None,
        opacity: 255,
        font: Some(FontStyle {
            family: "Block Test".into(),
            size_px: 4,
        }),
        alignment: Alignment {
            horizontal: HorizontalAlignment::Center,
            vertical: VerticalAlignment::Center,
        },
    };
    let bounds = Bounds {
        x: 0,
        y: 0,
        width: 16,
        height: 8,
    };
    let elements = vec![
        rectangle(1, 0, Color::BLACK, bounds),
        Element {
            id: element_id(2),
            name: "text".into(),
            bounds,
            z_index: 1,
            visible: true,
            style: text_style.clone(),
            color_field: Some(color_field),
            kind: ElementKind::Text { field: text_field },
            animations: vec![AnimationTrack {
                property: AnimatedProperty::Opacity,
                keyframes: vec![Keyframe {
                    at_ms: 0,
                    value: 255,
                    interpolation: Interpolation::Hold,
                }],
            }],
        },
        Element {
            id: element_id(3),
            name: "image".into(),
            bounds: Bounds {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            z_index: 2,
            visible: true,
            style: style(Color::new(200, 0, 0, 255)),
            color_field: None,
            kind: ElementKind::ImagePlaceholder { field: image_field },
            animations: Vec::new(),
        },
        Element {
            id: element_id(4),
            name: "clock".into(),
            bounds,
            z_index: 3,
            visible: true,
            style: text_style.clone(),
            color_field: None,
            kind: ElementKind::Clock(ClockSpec {
                direction: ClockDirection::CountUp,
                start_value_ms: 0,
                starts_at_ms: 0,
                format: ClockFormat::MinutesSeconds,
            }),
            animations: Vec::new(),
        },
        Element {
            id: element_id(5),
            name: "ticker".into(),
            bounds,
            z_index: 4,
            visible: true,
            style: text_style,
            color_field: None,
            kind: ElementKind::Ticker(TickerSpec {
                field: text_field,
                direction: TickerDirection::Right,
                pixels_per_second: 3,
                gap_px: 2,
                starts_at_ms: 0,
            }),
            animations: Vec::new(),
        },
    ];
    let scene = template(fields, elements, 16, 8)
        .instantiate(title_id(1))
        .unwrap();
    let assets = AssetCatalog::new()
        .with_font("Block Test")
        .with_image("image.png");
    let first = ReferenceRenderer.render(&scene, 1_234, &assets).unwrap();
    let second = ReferenceRenderer.render(&scene, 1_234, &assets).unwrap();

    assert_eq!(first.frame.pixels(), second.frame.pixels());
    assert!(first.frame.pixels().iter().any(|channel| *channel != 0));
    assert!(first.report.missing_fonts.is_empty());
    assert!(first.report.missing_images.is_empty());
}
