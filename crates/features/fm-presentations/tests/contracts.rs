use std::{collections::BTreeSet, time::Duration};

use fm_presentations::{
    BuildStep, Deck, DeckId, Document, DocumentId, FeatureLoss, ImportError, ImportLossKind,
    ImportOutcome, ImportReport, ImportSource, LegalScope, LinkTarget, NavigationEvent,
    PresentationFeature, PresentationImporter, PresentationNavigator, PresenterNotes, Slide,
    SlideId, SlideImage, SlideLink, UnsupportedSurface,
};

struct FakeImporter;

impl PresentationImporter for FakeImporter {
    fn import(&self, source: ImportSource<'_>) -> Result<ImportOutcome, ImportError> {
        match source.bytes {
            b"malformed" => Err(ImportError::Malformed {
                message: "fake source has no slide table".into(),
            }),
            b"duplicate-slide" => ImportOutcome::new(
                Document {
                    id: document_id(1),
                    title: source.name.into(),
                    decks: vec![Deck {
                        id: deck_id(10),
                        title: "Invalid".into(),
                        slides: vec![slide(100, 0, None), slide(100, 0, None)],
                        looping: false,
                    }],
                },
                ImportReport::new([], vec![]),
            ),
            _ => ImportOutcome::new(sample_document(false), sample_report()),
        }
    }
}

fn document_id(value: u128) -> DocumentId {
    DocumentId::try_from(value).unwrap()
}

fn deck_id(value: u128) -> DeckId {
    DeckId::try_from(value).unwrap()
}

fn slide_id(value: u128) -> SlideId {
    SlideId::try_from(value).unwrap()
}

fn slide(id: u128, builds: usize, auto_advance: Option<Duration>) -> Slide {
    Slide {
        id: slide_id(id),
        image: Some(SlideImage {
            media_type: "image/png".into(),
            bytes: vec![u8::try_from(id).unwrap()],
            alternative_text: Some(format!("slide {id}")),
        }),
        presenter_notes: Some(PresenterNotes(format!("notes {id}"))),
        build_steps: (0..builds)
            .map(|index| BuildStep {
                description: format!("build {index}"),
            })
            .collect(),
        links: vec![
            SlideLink {
                label: "always".into(),
                target: LinkTarget::Uri("https://example.invalid/always".into()),
                available_after_build: 0,
            },
            SlideLink {
                label: "after build".into(),
                target: LinkTarget::Slide(slide_id(101)),
                available_after_build: builds.min(1),
            },
        ],
        auto_advance,
    }
}

fn sample_document(looping: bool) -> Document {
    Document {
        id: document_id(1),
        title: "Fake deck document".into(),
        decks: vec![Deck {
            id: deck_id(10),
            title: "Main".into(),
            slides: vec![
                slide(100, 2, Some(Duration::from_secs(5))),
                slide(101, 0, Some(Duration::from_secs(3))),
            ],
            looping,
        }],
    }
}

fn sample_report() -> ImportReport {
    ImportReport::new(
        [
            PresentationFeature::SlideImages,
            PresentationFeature::PresenterNotes,
            PresentationFeature::BuildSteps,
            PresentationFeature::Links,
            PresentationFeature::TimedAutoAdvance,
        ],
        vec![FeatureLoss {
            feature: PresentationFeature::Links,
            kind: ImportLossKind::Downgraded,
            detail: "script actions were retained as ordinary links".into(),
            affected_slides: vec![slide_id(100)],
        }],
    )
}

fn imported() -> ImportOutcome {
    FakeImporter
        .import(ImportSource {
            name: "fixture.fake",
            bytes: b"valid",
        })
        .unwrap()
}

#[test]
fn fake_import_preserves_images_notes_builds_and_links() {
    let imported = imported();
    let first = &imported.document.decks[0].slides[0];
    assert_eq!(first.image.as_ref().unwrap().bytes, [100]);

    let mut navigator = PresentationNavigator::new(&imported.document, deck_id(10)).unwrap();
    assert_eq!(navigator.presenter_notes().unwrap().as_str(), "notes 100");
    assert_eq!(
        navigator
            .visible_links()
            .map(|link| link.label.as_str())
            .collect::<Vec<_>>(),
        ["always"]
    );

    assert_eq!(
        navigator.next(),
        NavigationEvent::BuildAdvanced {
            slide: slide_id(100),
            revealed: 1,
        }
    );
    assert_eq!(navigator.visible_build_steps().len(), 1);
    assert_eq!(navigator.visible_links().count(), 2);
    assert_eq!(
        navigator.next(),
        NavigationEvent::BuildAdvanced {
            slide: slide_id(100),
            revealed: 2,
        }
    );
    assert_eq!(
        navigator.next(),
        NavigationEvent::SlideChanged {
            slide: slide_id(101),
        }
    );
}

#[test]
fn previous_go_to_and_end_have_deterministic_build_positions() {
    let imported = imported();
    let mut navigator = PresentationNavigator::new(&imported.document, deck_id(10)).unwrap();

    navigator.go_to(slide_id(101)).unwrap();
    assert_eq!(navigator.next(), NavigationEvent::Ended);
    assert!(navigator.is_ended());
    assert_eq!(navigator.next(), NavigationEvent::Ended);
    assert_eq!(
        navigator.previous(),
        NavigationEvent::EndLeft {
            slide: slide_id(101),
        }
    );
    assert!(!navigator.is_ended());
    assert_eq!(
        navigator.previous(),
        NavigationEvent::SlideChanged {
            slide: slide_id(100),
        }
    );
    assert_eq!(navigator.revealed_build_count(), 2);
    assert_eq!(
        navigator.previous(),
        NavigationEvent::BuildReversed {
            slide: slide_id(100),
            revealed: 1,
        }
    );
    navigator.go_to(slide_id(100)).unwrap();
    assert_eq!(navigator.revealed_build_count(), 0);
    assert_eq!(navigator.previous(), NavigationEvent::AtStart);
}

#[test]
fn looping_wraps_in_both_directions_without_entering_end() {
    let document = sample_document(true);
    let mut navigator = PresentationNavigator::new(&document, deck_id(10)).unwrap();

    assert_eq!(
        navigator.previous(),
        NavigationEvent::Looped {
            slide: slide_id(101),
        }
    );
    assert_eq!(
        navigator.next(),
        NavigationEvent::Looped {
            slide: slide_id(100),
        }
    );
    assert!(!navigator.is_ended());
}

#[test]
fn auto_advance_consumes_time_across_slides_and_honors_end_and_loop() {
    let imported = imported();
    let mut navigator = PresentationNavigator::new(&imported.document, deck_id(10)).unwrap();
    assert!(navigator.advance_time(Duration::from_secs(4)).is_empty());
    assert_eq!(navigator.current_slide().id, slide_id(100));
    assert_eq!(
        navigator.advance_time(Duration::from_secs(3)),
        [NavigationEvent::SlideChanged {
            slide: slide_id(101),
        }]
    );
    assert_eq!(
        navigator.advance_time(Duration::from_secs(1)),
        [NavigationEvent::Ended]
    );

    let looping = sample_document(true);
    let mut navigator = PresentationNavigator::new(&looping, deck_id(10)).unwrap();
    assert_eq!(
        navigator.advance_time(Duration::from_secs(9)),
        [
            NavigationEvent::SlideChanged {
                slide: slide_id(101),
            },
            NavigationEvent::Looped {
                slide: slide_id(100),
            },
        ]
    );
    assert_eq!(navigator.current_slide().id, slide_id(100));
    assert!(!navigator.is_ended());
}

#[test]
fn fake_importer_reports_malformed_input_and_invalid_normalized_models() {
    let malformed = FakeImporter.import(ImportSource {
        name: "broken.fake",
        bytes: b"malformed",
    });
    assert!(matches!(malformed, Err(ImportError::Malformed { .. })));

    let duplicate = FakeImporter.import(ImportSource {
        name: "duplicate.fake",
        bytes: b"duplicate-slide",
    });
    assert!(matches!(duplicate, Err(ImportError::Malformed { .. })));
}

#[test]
fn import_report_distinguishes_supported_downgraded_and_lost_features() {
    let mut report = sample_report();
    report.losses.push(FeatureLoss {
        feature: PresentationFeature::Looping,
        kind: ImportLossKind::Dropped,
        detail: "source loop trigger was not available".into(),
        affected_slides: vec![],
    });

    assert!(report.supports(PresentationFeature::Links));
    assert!(!report.supports(PresentationFeature::Looping));
    assert_eq!(
        report
            .losses_for(PresentationFeature::Links)
            .map(|loss| loss.kind)
            .collect::<Vec<_>>(),
        [ImportLossKind::Downgraded]
    );
    assert_eq!(
        report
            .losses_for(PresentationFeature::Looping)
            .map(|loss| loss.kind)
            .collect::<Vec<_>>(),
        [ImportLossKind::Dropped]
    );
    assert_eq!(
        report.supported_features,
        BTreeSet::from([
            PresentationFeature::SlideImages,
            PresentationFeature::PresenterNotes,
            PresentationFeature::BuildSteps,
            PresentationFeature::Links,
            PresentationFeature::TimedAutoAdvance,
        ])
    );
}

#[test]
fn report_explicitly_marks_legacy_surfaces_and_parser_legal_scope() {
    let report = sample_report();
    assert_eq!(
        report
            .unsupported_surfaces
            .iter()
            .map(|entry| entry.surface)
            .collect::<Vec<_>>(),
        [
            UnsupportedSurface::DvdPlaybackAndAuthoring,
            UnsupportedSurface::InteractiveMenus,
            UnsupportedSurface::LegacyProprietaryFormats,
        ]
    );
    assert_eq!(
        report.legal_scope.scope,
        LegalScope::ParserNeutralDataContract
    );
    assert!(!report.legal_scope.proprietary_parser_included);
}
