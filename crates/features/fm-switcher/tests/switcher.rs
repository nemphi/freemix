use std::num::NonZeroU128;

use fm_switcher::{
    FADE_TO_BLACK_POSITION_DENOMINATOR, FadeToBlackError, FadeToBlackPosition, FadeToBlackTarget,
    MAX_FADE_TO_BLACK_DURATION_FRAMES, MissingMediaFallback, OVERLAY_CHANNEL_COUNT,
    OverlayChannelId, STINGER_SLOT_COUNT, StingerAudioPolicy, StingerDescriptor,
    StingerPlaybackDecision, StingerPreloadState, StingerSlotId, SwitcherCommand, SwitcherError,
    SwitcherEvent, SwitcherState, TBarPosition, TBarState, TransitionKind,
};
use fm_types::{InputId, OutputId};

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn output(value: u128) -> OutputId {
    OutputId::new(NonZeroU128::new(value).unwrap())
}

fn state() -> SwitcherState {
    SwitcherState::new(vec![input(1), input(2), input(3)], input(1), input(2)).unwrap()
}

#[test]
fn preview_selection_validates_membership() {
    let mut switcher = state();
    assert_eq!(
        switcher.apply(SwitcherCommand::SelectPreview(input(3))),
        Ok(vec![SwitcherEvent::PreviewSelected { input: input(3) }])
    );
    assert_eq!(
        switcher.apply(SwitcherCommand::SelectPreview(input(99))),
        Err(SwitcherError::UnknownInput(input(99)))
    );
}

#[test]
fn cut_swaps_program_and_preview_atomically() {
    let mut switcher = state();
    let events = switcher.apply(SwitcherCommand::Cut).unwrap();

    assert_eq!(switcher.program(), input(2));
    assert_eq!(switcher.preview(), input(1));
    assert_eq!(
        events,
        [SwitcherEvent::ProgramChanged {
            previous: input(1),
            program: input(2),
        }]
    );
}

#[test]
fn fade_realizes_on_exact_frame_boundary() {
    let mut switcher = state();
    switcher
        .apply(SwitcherCommand::Transition {
            kind: TransitionKind::Fade,
            duration_frames: 3,
        })
        .unwrap();

    let initial = switcher.program_frame();
    assert_eq!(initial.transition_kind, Some(TransitionKind::Fade));
    assert_eq!((initial.mix_numerator, initial.mix_denominator), (0, 3));
    assert_eq!(
        (initial.mix_start_numerator, initial.mix_end_numerator),
        (0, 1)
    );
    assert_eq!(switcher.advance_frame(), None);
    let second = switcher.program_frame();
    assert_eq!(second.mix_numerator, 1);
    assert_eq!(
        (second.mix_start_numerator, second.mix_end_numerator),
        (1, 2)
    );
    assert_eq!(switcher.advance_frame(), None);
    let third = switcher.program_frame();
    assert_eq!(third.mix_numerator, 2);
    assert_eq!((third.mix_start_numerator, third.mix_end_numerator), (2, 3));
    assert!(matches!(
        switcher.advance_frame(),
        Some(SwitcherEvent::ProgramChanged { .. })
    ));
    assert_eq!(switcher.program(), input(2));
    assert_eq!(switcher.program_frame().secondary, None);
    assert_eq!(switcher.program_frame().transition_kind, None);
}

#[test]
fn horizontal_wipe_uses_exact_frame_intervals_and_carries_its_kind() {
    let mut switcher = state();
    assert!(matches!(
        switcher
            .apply(SwitcherCommand::Wipe { duration_frames: 3 })
            .unwrap()
            .as_slice(),
        [SwitcherEvent::TransitionStarted {
            kind: TransitionKind::Wipe,
            duration_frames: 3,
            ..
        }]
    ));

    for (start, end) in [(0, 1), (1, 2), (2, 3)] {
        let frame = switcher.program_frame();
        assert_eq!(frame.transition_kind, Some(TransitionKind::Wipe));
        assert_eq!(
            (frame.mix_start_numerator, frame.mix_end_numerator),
            (start, end)
        );
        let _ = switcher.advance_frame_events();
    }

    let endpoint = switcher.program_frame();
    assert_eq!(endpoint.primary, input(2));
    assert_eq!(endpoint.secondary, None);
    assert_eq!(endpoint.transition_kind, None);
}

#[test]
fn active_transition_rejects_conflicting_operator_commands() {
    let mut switcher = state();
    switcher
        .apply(SwitcherCommand::Transition {
            kind: TransitionKind::Fade,
            duration_frames: 10,
        })
        .unwrap();

    assert_eq!(
        switcher.apply(SwitcherCommand::Cut),
        Err(SwitcherError::TransitionInProgress)
    );
    assert_eq!(
        switcher.apply(SwitcherCommand::SelectPreview(input(3))),
        Err(SwitcherError::TransitionInProgress)
    );
}

#[test]
fn zero_frame_transition_is_rejected() {
    assert_eq!(
        state().apply(SwitcherCommand::Transition {
            kind: TransitionKind::Fade,
            duration_frames: 0,
        }),
        Err(SwitcherError::ZeroDuration)
    );
}

#[test]
fn transition_descriptors_cover_the_standard_phase_three_family() {
    let slot = StingerSlotId::new(1).unwrap();
    let kinds = [
        TransitionKind::Fade,
        TransitionKind::Wipe,
        TransitionKind::Slide,
        TransitionKind::Zoom,
        TransitionKind::AlphaFade,
        TransitionKind::Stinger(slot),
    ];

    assert_eq!(kinds.len(), 6);
}

#[test]
fn t_bar_can_reverse_then_commit_or_cancel() {
    let mut switcher = state();
    switcher
        .apply(SwitcherCommand::StartTBar {
            kind: TransitionKind::Wipe,
        })
        .unwrap();

    switcher
        .apply(SwitcherCommand::SetTBarPosition(
            TBarPosition::new(8_000).unwrap(),
        ))
        .unwrap();
    let forward = switcher.program_frame();
    assert_eq!(forward.mix_numerator, 8_000);
    assert_eq!(
        (forward.mix_start_numerator, forward.mix_end_numerator),
        (0, 8_000)
    );
    assert_eq!(switcher.advance_frame(), None);
    let held = switcher.program_frame();
    assert_eq!(
        (held.mix_start_numerator, held.mix_end_numerator),
        (8_000, 8_000)
    );
    switcher
        .apply(SwitcherCommand::SetTBarPosition(
            TBarPosition::new(2_500).unwrap(),
        ))
        .unwrap();
    let reverse = switcher.program_frame();
    assert_eq!(reverse.mix_numerator, 2_500);
    assert_eq!(
        (reverse.mix_start_numerator, reverse.mix_end_numerator),
        (8_000, 2_500)
    );
    assert_eq!(switcher.advance_frame(), None);
    switcher
        .apply(SwitcherCommand::SetTBarPosition(
            TBarPosition::new(7_333).unwrap(),
        ))
        .unwrap();
    let irregular = switcher.program_frame();
    assert_eq!(
        (irregular.mix_start_numerator, irregular.mix_end_numerator),
        (2_500, 7_333)
    );
    assert_eq!(switcher.program(), input(1));

    let events = switcher.apply(SwitcherCommand::CommitTBar).unwrap();
    assert_eq!(switcher.program(), input(2));
    assert_eq!(
        events.last(),
        Some(&SwitcherEvent::TransitionCompleted {
            kind: TransitionKind::Wipe,
            program: input(2),
        })
    );

    switcher
        .apply(SwitcherCommand::StartTBar {
            kind: TransitionKind::Fade,
        })
        .unwrap();
    switcher
        .apply(SwitcherCommand::SetTBarPosition(TBarPosition::END))
        .unwrap();
    assert_eq!(
        switcher.apply(SwitcherCommand::CancelTBar),
        Ok(vec![SwitcherEvent::TBarCancelled])
    );
    assert_eq!(switcher.program(), input(2));
    assert_eq!(switcher.program_frame().secondary, None);
}

#[test]
fn manual_transition_rejects_every_unsupported_kind_without_mutation() {
    let unsupported = [
        TransitionKind::Slide,
        TransitionKind::Zoom,
        TransitionKind::AlphaFade,
        TransitionKind::Stinger(StingerSlotId::new(1).unwrap()),
    ];

    for kind in unsupported {
        let mut switcher = state();
        assert_eq!(
            switcher.start_t_bar(kind),
            Err(SwitcherError::UnsupportedManualTransitionKind)
        );
        assert!(switcher.t_bar().is_none());

        assert_eq!(
            switcher.restore_t_bar(TBarState::restore(
                kind,
                input(1),
                input(2),
                TBarPosition::START,
                TBarPosition::START,
            )),
            Err(SwitcherError::UnsupportedManualTransitionKind)
        );
        assert!(switcher.t_bar().is_none());
    }
}

#[test]
fn fade_to_black_tracks_explicit_on_and_off_state() {
    let mut switcher = state();
    assert!(!switcher.fade_to_black());
    assert_eq!(
        switcher.apply(SwitcherCommand::SetFadeToBlack(true)),
        Ok(vec![SwitcherEvent::FadeToBlackChanged { active: true }])
    );
    assert!(switcher.fade_to_black());
    switcher
        .apply(SwitcherCommand::SetFadeToBlack(false))
        .unwrap();
    assert!(!switcher.fade_to_black());
}

#[test]
fn automatic_fade_to_black_exposes_exact_frame_intervals_and_endpoints() {
    let mut switcher = state();
    assert_eq!(
        switcher.request_fade_to_black(true, 3),
        Ok(vec![SwitcherEvent::FadeToBlackStarted {
            from: FadeToBlackPosition::LIVE,
            target: FadeToBlackTarget::Black,
            duration_frames: 3,
        }])
    );
    assert!(switcher.fade_to_black());

    for (frame, (start, end)) in [(0, 21_845), (21_845, 43_690), (43_690, 65_535)]
        .into_iter()
        .enumerate()
    {
        let plan = switcher.fade_to_black_frame();
        assert_eq!(plan.interval_start().numerator(), start);
        assert_eq!(plan.interval_end().numerator(), end);
        assert_eq!(
            (
                plan.progress_start_numerator(),
                plan.progress_end_numerator(),
                plan.progress_denominator(),
            ),
            (
                u32::try_from(frame).unwrap(),
                u32::try_from(frame + 1).unwrap(),
                3
            )
        );
        assert_eq!(plan.target(), FadeToBlackTarget::Black);
        let events = switcher.advance_frame_events();
        assert!(events.iter().any(|event| matches!(
            event,
            SwitcherEvent::FadeToBlackPositionChanged { position }
                if position.numerator() == end
        )));
    }

    assert_eq!(
        switcher.fade_to_black_position(),
        FadeToBlackPosition::BLACK
    );
    assert_eq!(
        switcher.fade_to_black_frame().interval_start(),
        FadeToBlackPosition::BLACK
    );
}

#[test]
fn automatic_fade_to_black_reverses_without_a_jump() {
    let mut switcher = state();
    switcher.request_fade_to_black(true, 5).unwrap();
    let _ = switcher.advance_frame_events();
    let _ = switcher.advance_frame_events();
    let reversal_position = switcher.fade_to_black_position();
    assert_eq!(reversal_position.numerator(), 26_214);

    assert_eq!(
        switcher.request_fade_to_black(false, 3),
        Ok(vec![SwitcherEvent::FadeToBlackStarted {
            from: reversal_position,
            target: FadeToBlackTarget::Live,
            duration_frames: 3,
        }])
    );
    assert_eq!(
        switcher.fade_to_black_frame().interval_start(),
        reversal_position
    );
    assert_eq!(
        switcher.fade_to_black_frame().interval_end().numerator(),
        17_476
    );

    for expected in [17_476, 8_738, 0] {
        let _ = switcher.advance_frame_events();
        assert_eq!(switcher.fade_to_black_position().numerator(), expected);
    }
    assert_eq!(switcher.fade_to_black_position(), FadeToBlackPosition::LIVE);
    assert!(!switcher.fade_to_black());
}

#[test]
fn repeated_fade_to_black_target_does_not_restart_progress() {
    let mut switcher = state();
    switcher.request_fade_to_black(true, 4).unwrap();
    let _ = switcher.advance_frame_events();
    assert_eq!(switcher.fade_to_black_position().numerator(), 16_383);

    assert_eq!(switcher.request_fade_to_black(true, 2), Ok(Vec::new()));
    let frame = switcher.fade_to_black_frame();
    assert_eq!(
        (
            frame.progress_start_numerator(),
            frame.progress_end_numerator(),
            frame.progress_denominator(),
        ),
        (1, 2, 4)
    );
}

#[test]
fn reversing_before_progress_cancels_without_noop_frame_spam() {
    let mut switcher = state();
    switcher.request_fade_to_black(true, 20).unwrap();
    assert_eq!(
        switcher.request_fade_to_black(false, 20),
        Ok(vec![SwitcherEvent::FadeToBlackCompleted { active: false }])
    );
    assert!(!switcher.fade_to_black());
    assert_eq!(switcher.fade_to_black_position(), FadeToBlackPosition::LIVE);
    assert!(switcher.advance_frame_events().is_empty());
}

#[test]
fn automatic_fade_to_black_rejects_invalid_durations_transactionally() {
    let mut switcher = state();
    assert_eq!(
        switcher.request_fade_to_black(true, 0),
        Err(FadeToBlackError::ZeroDuration)
    );
    assert_eq!(
        switcher.request_fade_to_black(true, MAX_FADE_TO_BLACK_DURATION_FRAMES + 1),
        Err(FadeToBlackError::DurationLimit {
            duration_frames: MAX_FADE_TO_BLACK_DURATION_FRAMES + 1,
            maximum: MAX_FADE_TO_BLACK_DURATION_FRAMES,
        })
    );
    assert_eq!(switcher.fade_to_black_position(), FadeToBlackPosition::LIVE);
    assert!(!switcher.fade_to_black());
}

#[test]
fn immediate_fade_to_black_cancels_automatic_motion_compatibly() {
    let mut switcher = state();
    switcher.request_fade_to_black(true, 10).unwrap();
    let _ = switcher.advance_frame_events();

    assert_eq!(
        switcher.apply(SwitcherCommand::SetFadeToBlack(false)),
        Ok(vec![SwitcherEvent::FadeToBlackChanged { active: false }])
    );
    assert_eq!(switcher.fade_to_black_position(), FadeToBlackPosition::LIVE);
    assert!(switcher.advance_frame_events().is_empty());
}

#[test]
fn fade_to_black_advances_orthogonally_to_automatic_and_manual_transitions() {
    let mut automatic = state();
    automatic
        .apply(SwitcherCommand::Transition {
            kind: TransitionKind::Wipe,
            duration_frames: 2,
        })
        .unwrap();
    automatic.request_fade_to_black(true, 3).unwrap();

    let first = automatic.advance_frame_events();
    assert!(matches!(
        first.as_slice(),
        [SwitcherEvent::FadeToBlackPositionChanged { .. }]
    ));
    let second = automatic.advance_frame_events();
    assert!(matches!(
        second.as_slice(),
        [
            SwitcherEvent::ProgramChanged { .. },
            SwitcherEvent::TransitionCompleted {
                kind: TransitionKind::Wipe,
                ..
            },
            SwitcherEvent::FadeToBlackPositionChanged { .. },
        ]
    ));
    let third = automatic.advance_frame_events();
    assert!(matches!(
        third.as_slice(),
        [
            SwitcherEvent::FadeToBlackPositionChanged { .. },
            SwitcherEvent::FadeToBlackCompleted { active: true },
        ]
    ));

    let mut manual = state();
    manual
        .apply(SwitcherCommand::StartTBar {
            kind: TransitionKind::Fade,
        })
        .unwrap();
    manual
        .apply(SwitcherCommand::SetTBarPosition(
            TBarPosition::new(4_000).unwrap(),
        ))
        .unwrap();
    manual.request_fade_to_black(true, 2).unwrap();
    assert!(matches!(
        manual.advance_frame(),
        Some(SwitcherEvent::FadeToBlackPositionChanged { .. })
    ));
    assert_eq!(
        manual.program_frame().mix_start_numerator,
        manual.program_frame().mix_end_numerator
    );
}

#[test]
fn longest_fade_to_black_has_no_cumulative_drift() {
    let mut switcher = state();
    switcher
        .request_fade_to_black(true, MAX_FADE_TO_BLACK_DURATION_FRAMES)
        .unwrap();
    let mut previous = 0;
    for frame in 0..MAX_FADE_TO_BLACK_DURATION_FRAMES {
        let plan = switcher.fade_to_black_frame();
        assert_eq!(plan.interval_start().numerator(), previous);
        assert_eq!(
            plan.interval_end().numerator(),
            u32::from(u16::MAX) * (frame + 1) / MAX_FADE_TO_BLACK_DURATION_FRAMES
        );
        let _ = switcher.advance_frame_events();
        previous = switcher.fade_to_black_position().numerator();
    }
    assert_eq!(previous, FADE_TO_BLACK_POSITION_DENOMINATOR);
    assert_eq!(
        switcher.fade_to_black_position(),
        FadeToBlackPosition::BLACK
    );
}

#[test]
fn all_eight_overlay_channels_are_independent() {
    let mut switcher = state();
    assert_eq!(switcher.overlays().len(), OVERLAY_CHANNEL_COUNT);

    for index in 0..OVERLAY_CHANNEL_COUNT {
        let channel = OverlayChannelId::from_index(index).unwrap();
        let source = input(u128::try_from(index).unwrap() % 3 + 1);
        switcher.take_overlay(channel, source).unwrap();
    }

    for index in 0..OVERLAY_CHANNEL_COUNT {
        let channel = OverlayChannelId::from_index(index).unwrap();
        assert!(switcher.overlay(channel).is_active());
        assert_eq!(
            switcher.overlay(channel).source(),
            Some(input(u128::try_from(index).unwrap() % 3 + 1))
        );
    }

    let first = OverlayChannelId::new(1).unwrap();
    switcher.update_overlay(first, input(3)).unwrap();
    switcher.overlay_off(first).unwrap();
    assert_eq!(switcher.overlay(first).source(), Some(input(3)));
    assert!(!switcher.overlay(first).is_active());
    assert!(
        switcher
            .overlay(OverlayChannelId::new(2).unwrap())
            .is_active()
    );
}

#[test]
fn overlay_inclusion_is_routed_per_output_without_duplicates() {
    let mut switcher = state();
    let channel = OverlayChannelId::new(4).unwrap();
    let clean = output(10);
    let dirty = output(11);

    let _ = switcher.set_overlay_output_inclusion(channel, dirty, true);
    let _ = switcher.set_overlay_output_inclusion(channel, dirty, true);
    let _ = switcher.set_overlay_output_inclusion(channel, clean, false);
    assert!(switcher.overlay(channel).is_included_in(dirty));
    assert!(!switcher.overlay(channel).is_included_in(clean));
    assert_eq!(switcher.overlay(channel).included_outputs(), &[dirty]);

    let _ = switcher.set_overlay_output_inclusion(channel, dirty, false);
    assert!(switcher.overlay(channel).included_outputs().is_empty());
}

#[test]
fn all_eight_stinger_slots_retain_independent_configuration() {
    let mut switcher = state();
    assert_eq!(switcher.stingers().len(), STINGER_SLOT_COUNT);

    for index in 0..STINGER_SLOT_COUNT {
        let slot = StingerSlotId::from_index(index).unwrap();
        switcher.configure_stinger(
            slot,
            StingerDescriptor::new(
                format!("stinger-{index}.mov"),
                index % 2 == 0,
                u32::try_from(index).unwrap() + 5,
                StingerAudioPolicy::MixWithProgram,
                MissingMediaFallback::Fade,
            ),
        );
    }

    for index in 0..STINGER_SLOT_COUNT {
        let descriptor = switcher
            .stinger(StingerSlotId::from_index(index).unwrap())
            .descriptor()
            .unwrap();
        assert_eq!(descriptor.media, format!("stinger-{index}.mov"));
        assert_eq!(
            descriptor.cut_point_frames,
            u32::try_from(index).unwrap() + 5
        );
    }
}

#[test]
fn stinger_preload_and_missing_media_fallback_are_deterministic() {
    let mut switcher = state();
    let slot = StingerSlotId::new(8).unwrap();
    switcher.configure_stinger(
        slot,
        StingerDescriptor::new(
            "missing.mov",
            true,
            12,
            StingerAudioPolicy::StingerOnly,
            MissingMediaFallback::KeepProgram,
        ),
    );
    assert_eq!(
        switcher.stinger(slot).preload_state(),
        StingerPreloadState::NotRequested
    );
    assert_eq!(
        switcher.preload_stinger(slot, false),
        SwitcherEvent::StingerPreloadChanged {
            slot,
            state: StingerPreloadState::Missing,
        }
    );
    assert_eq!(
        switcher.stinger_playback_decision(slot),
        StingerPlaybackDecision::Fallback(MissingMediaFallback::KeepProgram)
    );
    assert_eq!(
        switcher
            .apply(SwitcherCommand::Transition {
                kind: TransitionKind::Stinger(slot),
                duration_frames: 20,
            })
            .unwrap(),
        [SwitcherEvent::StingerFallbackApplied {
            slot,
            fallback: MissingMediaFallback::KeepProgram,
        }]
    );
    assert_eq!(switcher.program(), input(1));

    switcher.preload_stinger(slot, true);
    assert_eq!(
        switcher.stinger_playback_decision(slot),
        StingerPlaybackDecision::Play
    );
    assert!(matches!(
        switcher
            .apply(SwitcherCommand::Transition {
                kind: TransitionKind::Stinger(slot),
                duration_frames: 20,
            })
            .unwrap()
            .as_slice(),
        [SwitcherEvent::TransitionStarted {
            kind: TransitionKind::Stinger(event_slot),
            ..
        }] if *event_slot == slot
    ));
}

#[test]
fn timed_transition_reports_completion_without_changing_legacy_api() {
    let mut switcher = state();
    switcher
        .apply(SwitcherCommand::Transition {
            kind: TransitionKind::AlphaFade,
            duration_frames: 2,
        })
        .unwrap();

    assert!(switcher.advance_frame_events().is_empty());
    assert_eq!(
        switcher.advance_frame_events(),
        [
            SwitcherEvent::ProgramChanged {
                previous: input(1),
                program: input(2),
            },
            SwitcherEvent::TransitionCompleted {
                kind: TransitionKind::AlphaFade,
                program: input(2),
            },
        ]
    );

    let mut legacy = state();
    legacy
        .apply(SwitcherCommand::Transition {
            kind: TransitionKind::Fade,
            duration_frames: 1,
        })
        .unwrap();
    assert_eq!(
        legacy.advance_frame(),
        Some(SwitcherEvent::ProgramChanged {
            previous: input(1),
            program: input(2),
        })
    );
}
