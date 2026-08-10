use core::{fmt::Write, num::NonZeroU128};

use fm_command::{Deadline, Revision, StateEpoch};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CapabilityReportMessage, CapabilityReportSummary, CodecError,
    CommandMessage, CommandPayload, CommandResult, DurableEvent, DurableEventBatch, DurableGap,
    EngineIdentity, ErrorMessage, EventCursor, EventMessage, EventPayload, FadeToBlackPosition,
    FadeToBlackState, FieldIssue, HandshakeOutcome, HeartbeatAcknowledgementMessage,
    HeartbeatMessage, InputAudioStripStatus, InputStatus, LineDecoder, MAX_FIELD_VALUE_BYTES,
    MAX_FIELDS_PER_MESSAGE, MAX_LINE_BYTES, MAX_LIST_ITEMS, MAX_MESSAGES_PER_PUSH,
    ManualTransitionKind, ManualTransitionPosition, ManualTransitionState, ManualTransitionStatus,
    OverlayStatus, OverlayTransitionKind, ProtocolVersion, ResumeCursor, RuntimeDomainBoundary,
    RuntimeEventMessage, RuntimeFailureDisposition, RuntimeLifecycleEvent, ServerIdentity,
    SnapshotMessage, SnapshotReason, StingerAudioPolicy, StingerMissingMediaFallback,
    StingerReadiness, StingerStatus, StructuredError, WireInputId, WireMessage, WireOutputId,
    WireOverlayChannelId, WireStingerSlotId, choose_handshake_outcome, decode_line, encode_line,
};

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
}

fn input_statuses(values: &[(u128, &str)]) -> Vec<InputStatus> {
    values
        .iter()
        .map(|&(value, name)| InputStatus {
            input: input(value),
            name: name.into(),
        })
        .collect()
}

fn input_audio_strips(values: &[u128]) -> Vec<InputAudioStripStatus> {
    values
        .iter()
        .map(|&value| InputAudioStripStatus {
            input: input(value),
            gain_millidb: 0,
            balance_basis_points: 0,
            muted: false,
            soloed: false,
            follow_video: true,
            delay_samples: 0,
        })
        .collect()
}

fn output(value: u128) -> WireOutputId {
    WireOutputId::new(NonZeroU128::new(value).unwrap())
}

fn identity() -> EngineIdentity {
    EngineIdentity {
        engine_id: "engine-a".to_owned(),
        state_epoch: 7,
        log_id: "log-a".to_owned(),
    }
}

fn cursor() -> EventCursor {
    EventCursor {
        engine: identity(),
        revision: 1_842,
    }
}

fn server_identity() -> ServerIdentity {
    ServerIdentity {
        engine_id: "engine-a".to_owned(),
        project_id: "project-9".to_owned(),
        state_epoch: 7,
        log_id: "log-a".to_owned(),
    }
}

fn resume_cursor() -> ResumeCursor {
    ResumeCursor {
        server: server_identity(),
        revision: 1_842,
    }
}

fn structured_error() -> StructuredError {
    StructuredError {
        code: "invalid_command".to_owned(),
        message: "bad request".to_owned(),
        fields: vec![FieldIssue {
            field: "command.duration".to_owned(),
            code: "positive".to_owned(),
            message: "must be > 0".to_owned(),
        }],
        retryable: false,
    }
}

fn command() -> CommandMessage {
    CommandMessage {
        protocol: CURRENT_PROTOCOL_VERSION,
        id: "01K:test".to_owned(),
        idempotency_key: "operator-7:01K".to_owned(),
        expected_revision: Some(1_842),
        deadline_ms: Some(500),
        payload: CommandPayload::SelectPreview { input: input(42) },
    }
}

fn snapshot(inputs: Vec<InputStatus>) -> WireMessage {
    WireMessage::Snapshot(SnapshotMessage {
        engine: identity(),
        revision: 9,
        show_name: "My Show\nA".to_owned(),
        inputs,
        input_audio_strips: input_audio_strips(&[1, 2]),
        desired_program: input(2),
        desired_preview: input(1),
        realized_program: input(1),
        realized_preview: input(2),
        desired_manual_transition: ManualTransitionStatus::Inactive,
        realized_manual_transition: ManualTransitionStatus::Inactive,
        desired_fade_to_black: FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        },
        realized_fade_to_black: FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        },
        stingers: Vec::new(),
        desired_overlays: OverlayStatus::empty_channels(),
        realized_overlays: OverlayStatus::empty_channels(),
    })
}

#[test]
fn protocol_2_10_heartbeat_acknowledgement_codec_is_exact() {
    assert_eq!(CURRENT_PROTOCOL_VERSION, ProtocolVersion::new(2, 10));
    let acknowledgement = WireMessage::HeartbeatAcknowledgement(HeartbeatAcknowledgementMessage {
        server: server_identity(),
        heartbeat_sequence: 88,
        received_at_ms: 1_720_000_000_003,
    });
    let encoded = encode_line(&acknowledgement).unwrap();
    assert_eq!(
        encoded,
        "heartbeat_acknowledgement\tengine_id=engine-a\tproject_id=project-9\tstate_epoch=7\tlog_id=log-a\theartbeat_sequence=88\treceived_at_ms=1720000000003\n"
    );
    assert_eq!(decode_line(&encoded).unwrap(), acknowledgement);
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_message_variant_round_trips() {
    let messages = vec![
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::SetInputAudioStrip {
                input: input(42),
                gain_millidb: -6_000,
                balance_basis_points: 2_500,
                muted: true,
                soloed: true,
                follow_video: false,
                delay_samples: 2_400,
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::Cut,
            expected_revision: None,
            deadline_ms: None,
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::Fade { duration_frames: 9 },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            payload: CommandPayload::AlphaFade { duration_frames: 9 },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            payload: CommandPayload::Slide { duration_frames: 9 },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            payload: CommandPayload::Zoom { duration_frames: 9 },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            payload: CommandPayload::Wipe { duration_frames: 9 },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::TakeOverlay {
                channel: WireOverlayChannelId::new(1).unwrap(),
                source: input(42),
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::UpdateOverlay {
                channel: WireOverlayChannelId::new(8).unwrap(),
                source: input(7),
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::OverlayOff {
                channel: WireOverlayChannelId::new(2).unwrap(),
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::SetOverlayOutputInclusion {
                channel: WireOverlayChannelId::new(3).unwrap(),
                output: output(9),
                included: true,
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::ConfigureOverlayTransition {
                channel: WireOverlayChannelId::new(4).unwrap(),
                transition: OverlayTransitionKind::Fade,
                duration_frames: 24,
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::ConfigureOverlayAppearance {
                channel: WireOverlayChannelId::new(5).unwrap(),
                position: fm_protocol::OverlayPositionPreset::BottomRight,
                border: fm_protocol::OverlayBorderPreset::ThickWhite,
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::QueueOverlay {
                channel: WireOverlayChannelId::new(6).unwrap(),
                source: input(3),
            },
            ..command()
        }),
        WireMessage::Command(CommandMessage {
            payload: CommandPayload::TakeNextOverlay {
                channel: WireOverlayChannelId::new(6).unwrap(),
            },
            ..command()
        }),
        WireMessage::CommandResult(CommandResult::Accepted {
            id: "accepted".to_owned(),
            revision: 9,
            scheduled_frame: Some(12),
        }),
        WireMessage::CommandResult(CommandResult::Rejected {
            id: "rejected".to_owned(),
            code: "invalid_command".to_owned(),
            message: "bad\tvalue".to_owned(),
            fields: vec![FieldIssue {
                field: "payload.duration".to_owned(),
                code: "zero".to_owned(),
                message: "must be > 0".to_owned(),
            }],
            current_revision: 9,
            retryable: false,
        }),
        snapshot(input_statuses(&[(1, "Camera, A"), (2, "Slides ~ Main")])),
        WireMessage::Event(EventMessage {
            cursor: cursor(),
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(1),
                manual_transition: ManualTransitionStatus::Inactive,
                fade_to_black: FadeToBlackState {
                    target_active: false,
                    position: FadeToBlackPosition::LIVE,
                },
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(&[1, 2]),
            },
        }),
    ];
    for message in messages {
        let encoded = encode_line(&message).unwrap();
        assert_eq!(decode_line(&encoded).unwrap(), message);
    }
}

#[test]
fn input_statuses_reject_duplicate_ids() {
    let duplicate = snapshot(input_statuses(&[(1, "camera"), (1, "slides")]));
    assert!(matches!(
        encode_line(&duplicate),
        Err(CodecError::InvalidField {
            field: "inputs",
            ..
        })
    ));

    let valid = encode_line(&snapshot(input_statuses(&[(1, "camera"), (2, "slides")])))
        .unwrap();
    let duplicate = valid.replace(
        "inputs=1%7Ecamera%2C2%7Eslides",
        "inputs=1%7Ecamera%2C1%7Eslides",
    );
    assert!(matches!(
        decode_line(&duplicate),
        Err(CodecError::InvalidField {
            field: "inputs",
            ..
        })
    ));
}

#[test]
fn overlay_included_outputs_reject_duplicates() {
    let mut duplicate = snapshot(input_statuses(&[(1, "camera"), (2, "slides")]));
    let WireMessage::Snapshot(snapshot_message) = &mut duplicate else {
        unreachable!();
    };
    snapshot_message.desired_overlays[0].included_outputs =
        vec![output(7_000_000_001), output(7_000_000_001)];
    assert!(matches!(
        encode_line(&duplicate),
        Err(CodecError::InvalidField {
            field: "desired_overlays",
            ..
        })
    ));

    let mut valid = snapshot(input_statuses(&[(1, "camera"), (2, "slides")]));
    let WireMessage::Snapshot(snapshot_message) = &mut valid else {
        unreachable!();
    };
    snapshot_message.desired_overlays[0].included_outputs =
        vec![output(7_000_000_001), output(7_000_000_002)];
    let valid = encode_line(&valid).unwrap();
    let duplicate = valid.replace(
        "7000000001%2C7000000002",
        "7000000001%2C7000000001",
    );
    assert_ne!(duplicate, valid);
    assert!(matches!(
        decode_line(&duplicate),
        Err(CodecError::InvalidField {
            field: "desired_overlays",
            ..
        })
    ));
}

#[test]
fn input_audio_strip_status_rejects_duplicates_and_out_of_range_controls() {
    let strip_event = |input_audio_strips| {
        WireMessage::Event(EventMessage {
            cursor: cursor(),
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition: ManualTransitionStatus::Inactive,
                fade_to_black: FadeToBlackState {
                    target_active: false,
                    position: FadeToBlackPosition::LIVE,
                },
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips,
            },
        })
    };

    let duplicate = strip_event(vec![
        InputAudioStripStatus {
            input: input(1),
            gain_millidb: 0,
            balance_basis_points: 0,
            muted: false,
            soloed: false,
            follow_video: true,
            delay_samples: 0,
        },
        InputAudioStripStatus {
            input: input(1),
            gain_millidb: -6_000,
            balance_basis_points: 5_000,
            muted: true,
            soloed: true,
            follow_video: false,
            delay_samples: 1,
        },
    ]);
    assert!(matches!(
        encode_line(&duplicate),
        Err(CodecError::InvalidField {
            field: "input_audio_strips",
            ..
        })
    ));

    let valid = encode_line(&strip_event(input_audio_strips(&[1, 2]))).unwrap();
    for malformed in [
        valid.replace(
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A0%3A0%3A0%3A1%3A0",
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C1%3A0%3A0%3A0%3A0%3A1%3A0",
        ),
        valid.replace(
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A0%3A0%3A0%3A1%3A0",
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A0%3A0%3A0%3A1%3A48001",
        ),
        valid.replace(
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A0%3A0%3A0%3A1%3A0",
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A24001%3A0%3A0%3A0%3A1%3A0",
        ),
        valid.replace(
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A0%3A0%3A0%3A1%3A0",
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A10001%3A0%3A0%3A1%3A0",
        ),
        valid.replace(
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A0%3A0%3A0%3A1%3A0",
            "1%3A0%3A0%3A0%3A0%3A1%3A0%2C2%3A0%3A0%3A0%3A2%3A1%3A0",
        ),
    ] {
        assert!(matches!(
            decode_line(&malformed),
            Err(CodecError::InvalidField {
                field: "input_audio_strips",
                ..
            })
        ));
    }
}

#[test]
fn stinger_slot_mutations_round_trip_exact_configuration() {
    for payload in [
        CommandPayload::ConfigureStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            media_input: input(42),
            preload: true,
            cut_point_frames: 17,
            audio_policy: StingerAudioPolicy::MixWithProgram,
            missing_media_fallback: StingerMissingMediaFallback::KeepProgram,
        },
        CommandPayload::RemoveStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
        },
    ] {
        let message = WireMessage::Command(CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            payload,
            ..command()
        });
        let encoded = encode_line(&message).unwrap();
        assert_eq!(decode_line(&encoded).unwrap(), message);
    }

    let event = WireMessage::Event(EventMessage {
        cursor: cursor(),
        payload: EventPayload::StingerSlotsChanged {
            program: input(1),
            preview: input(2),
            manual_transition: ManualTransitionStatus::Inactive,
            fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
            stingers: vec![StingerStatus {
                slot: WireStingerSlotId::new(8).unwrap(),
                media_input: input(42),
                preload: true,
                cut_point_frames: 17,
                audio_policy: StingerAudioPolicy::MixWithProgram,
                missing_media_fallback: StingerMissingMediaFallback::KeepProgram,
                readiness: StingerReadiness::Ready,
            }],
            overlays: OverlayStatus::empty_channels(),
            input_audio_strips: input_audio_strips(&[1, 2, 42]),
        },
    });
    let encoded = encode_line(&event).unwrap();
    assert_eq!(decode_line(&encoded).unwrap(), event);
}

#[test]
fn stinger_encoder_rejects_too_many_or_duplicate_slots() {
    let status = StingerStatus {
        slot: WireStingerSlotId::new(1).unwrap(),
        media_input: input(42),
        preload: true,
        cut_point_frames: 17,
        audio_policy: StingerAudioPolicy::MixWithProgram,
        missing_media_fallback: StingerMissingMediaFallback::KeepProgram,
        readiness: StingerReadiness::Ready,
    };
    let event = |stingers| {
        WireMessage::Event(EventMessage {
            cursor: cursor(),
            payload: EventPayload::StingerSlotsChanged {
                program: input(1),
                preview: input(2),
                manual_transition: ManualTransitionStatus::Inactive,
                fade_to_black: FadeToBlackState {
                    target_active: false,
                    position: FadeToBlackPosition::LIVE,
                },
                stingers,
                overlays: OverlayStatus::empty_channels(),
                input_audio_strips: input_audio_strips(&[1, 2]),
            },
        })
    };
    assert!(matches!(
        encode_line(&event(vec![status, status])),
        Err(CodecError::InvalidField {
            field: "stingers",
            ..
        })
    ));
    assert_eq!(
        encode_line(&event(vec![status; 9])),
        Err(CodecError::TooManyItems("stingers"))
    );
}

#[test]
fn decoder_rejects_out_of_range_manual_position() {
    let fixture = encode_line(&WireMessage::Command(CommandMessage {
        payload: CommandPayload::SetManualTransitionPosition {
            position: fm_protocol::ManualTransitionPosition::new(6_250).unwrap(),
        },
        ..command()
    }))
    .unwrap();
    let invalid = fixture.replace("position_basis_points=6250", "position_basis_points=10001");
    assert!(matches!(
        decode_line(&invalid),
        Err(CodecError::InvalidField {
            field: "position_basis_points",
            ..
        })
    ));
}

#[test]
fn manual_slide_codec_preserves_exact_current_kind_and_interval() {
    let start = WireMessage::Command(CommandMessage {
        payload: CommandPayload::StartManualTransition {
            kind: ManualTransitionKind::Slide,
        },
        ..command()
    });
    let encoded = encode_line(&start).unwrap();
    assert!(encoded.contains("transition=slide"));
    assert_eq!(decode_line(&encoded).unwrap(), start);

    let status = ManualTransitionStatus::Active(ManualTransitionState {
        kind: ManualTransitionKind::Slide,
        from: input(1),
        to: input(2),
        interval_start: ManualTransitionPosition::new(8_000).unwrap(),
        position: ManualTransitionPosition::new(2_500).unwrap(),
    });
    let event = WireMessage::Event(EventMessage {
        cursor: cursor(),
        payload: EventPayload::DesiredSwitcher {
            program: input(1),
            preview: input(2),
            manual_transition: status,
            fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
            overlays: OverlayStatus::empty_channels(),
            input_audio_strips: input_audio_strips(&[1, 2]),
        },
    });
    let encoded = encode_line(&event).unwrap();
    assert_eq!(decode_line(&encoded).unwrap(), event);
}

#[test]
fn exact_contract_rejects_every_unknown_field() {
    let fixture = encode_line(&WireMessage::Command(command())).unwrap();
    let required = fixture.replace('\n', "\tfuture=value\n");
    assert_eq!(
        decode_line(&required),
        Err(CodecError::UnknownField("future".to_owned()))
    );
}

#[test]
fn decoder_rejects_truncation_duplicates_and_invalid_values() {
    let fixture = encode_line(&WireMessage::Command(command())).unwrap();
    assert_eq!(
        decode_line(fixture.trim_end()),
        Err(CodecError::MissingNewline)
    );
    let duplicate = fixture.replace('\n', "\tid=again\n");
    assert_eq!(
        decode_line(&duplicate),
        Err(CodecError::DuplicateField("id".to_owned()))
    );
    let zero_input = fixture.replace("input=42", "input=0");
    assert!(matches!(
        decode_line(&zero_input),
        Err(CodecError::InvalidField { field: "input", .. })
    ));
}

#[test]
fn streaming_decoder_handles_split_and_multiple_records() {
    let command = encode_line(&WireMessage::Command(command())).unwrap();
    let bytes = format!("{command}{command}");
    let split = bytes.len() / 3;
    let mut decoder = LineDecoder::new();
    assert!(decoder.push(&bytes.as_bytes()[..split]).unwrap().is_empty());
    let messages = decoder.push(&bytes.as_bytes()[split..]).unwrap();
    assert_eq!(messages.len(), 2);
    decoder.finish().unwrap();

    let mut incomplete = LineDecoder::new();
    incomplete.push(b"command").unwrap();
    assert_eq!(incomplete.finish(), Err(CodecError::TrailingData));
}

#[test]
fn domain_conversion_helpers_are_explicit() {
    let message = command();
    assert_eq!(message.expected_revision, Some(1_842));
    let envelope = message.domain_envelope("converted");
    assert_eq!(envelope.expected_revision, Some(Revision::new(1_842)));
    assert_eq!(envelope.deadline, Some(Deadline::from_millis(500)));
    assert_eq!(cursor().domain_revision(), Revision::new(1_842));
    assert_eq!(identity().domain_state_epoch(), StateEpoch::new(7));
    assert_eq!(input(42).to_domain().get(), NonZeroU128::new(42).unwrap());
    assert_eq!(resume_cursor().domain_revision(), Revision::new(1_842));
    assert_eq!(server_identity().domain_state_epoch(), StateEpoch::new(7));
}

#[test]
fn handshake_choice_requires_an_exact_cursor_identity_and_available_history() {
    let server = server_identity();
    assert_eq!(
        choose_handshake_outcome(&server, 100, 90, None),
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::NoCursor
        }
    );

    let mut cursor = ResumeCursor {
        server: server.clone(),
        revision: 89,
    };
    assert_eq!(
        choose_handshake_outcome(&server, 100, 90, Some(&cursor)),
        HandshakeOutcome::Resume {
            cursor: cursor.clone()
        }
    );
    cursor.revision = 88;
    assert_eq!(
        choose_handshake_outcome(&server, 100, 90, Some(&cursor)),
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::HistoryUnavailable
        }
    );
    cursor.revision = 101;
    assert_eq!(
        choose_handshake_outcome(&server, 100, 90, Some(&cursor)),
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::CursorAhead
        }
    );
    cursor.revision = 100;
    cursor.server.project_id = "replaced".to_owned();
    assert_eq!(
        choose_handshake_outcome(&server, 100, 90, Some(&cursor)),
        HandshakeOutcome::Snapshot {
            reason: SnapshotReason::IdentityChanged
        }
    );
}

#[test]
fn runtime_lifecycle_variants_use_the_independent_runtime_sequence() {
    let events = [
        RuntimeLifecycleEvent::Accepted,
        RuntimeLifecycleEvent::Preparing,
        RuntimeLifecycleEvent::Scheduled {
            domains: vec![RuntimeDomainBoundary {
                domain: "video".to_owned(),
                boundary: 900,
            }],
        },
        RuntimeLifecycleEvent::Realized {
            domain: "video".to_owned(),
            manual_transition: ManualTransitionStatus::Inactive,
            fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
        },
        RuntimeLifecycleEvent::Failed {
            error: structured_error(),
            disposition: RuntimeFailureDisposition::RetainedForRetry,
        },
        RuntimeLifecycleEvent::Superseded { by_revision: 1_844 },
    ];
    for (index, event) in events.into_iter().enumerate() {
        let message = WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: server_identity(),
            revision: 1_843,
            generation: 12,
            sequence: u64::try_from(index + 1).unwrap(),
            event,
        });
        let encoded = encode_line(&message).unwrap();
        assert!(encoded.starts_with("runtime_event\t"));
        assert_eq!(decode_line(&encoded).unwrap(), message);
    }
}

#[test]
fn runtime_scheduled_domain_bounds_are_enforced() {
    let domains = (0..=MAX_LIST_ITEMS)
        .map(|index| RuntimeDomainBoundary {
            domain: format!("domain-{index}"),
            boundary: u64::try_from(index).unwrap(),
        })
        .collect();
    let message = WireMessage::RuntimeEvent(RuntimeEventMessage {
        server: server_identity(),
        revision: 1_843,
        generation: 12,
        sequence: 3,
        event: RuntimeLifecycleEvent::Scheduled { domains },
    });
    assert_eq!(
        encode_line(&message),
        Err(CodecError::TooManyItems("domains"))
    );

    let domains = (0..=MAX_LIST_ITEMS)
        .map(|index| format!("domain-{index}%7E{index}"))
        .collect::<Vec<_>>()
        .join("%2C");
    let current = encode_line(&WireMessage::RuntimeEvent(RuntimeEventMessage {
        server: server_identity(),
        revision: 1_843,
        generation: 12,
        sequence: 3,
        event: RuntimeLifecycleEvent::Scheduled {
            domains: vec![
                RuntimeDomainBoundary {
                    domain: "video".into(),
                    boundary: 900,
                },
                RuntimeDomainBoundary {
                    domain: "audio".into(),
                    boundary: 48_000,
                },
            ],
        },
    }))
    .unwrap();
    let oversized = current.replace("video%7E900%2Caudio%7E48000", &domains);
    assert_eq!(
        decode_line(&oversized),
        Err(CodecError::TooManyItems("domains"))
    );
}

#[test]
fn durable_gaps_are_structural_and_batches_cannot_hide_sequence_gaps() {
    let batch = WireMessage::DurableEventBatch(DurableEventBatch {
        cursor: resume_cursor(),
        events: vec![DurableEvent {
            sequence: 0,
            event_type: "switcher.desired".into(),
            payload: "program=2&preview=1".into(),
        }],
    });
    let malformed_batch = encode_line(&batch)
        .unwrap()
        .replace("events=0%7E", "events=1%7E");
    assert!(matches!(
        decode_line(&malformed_batch),
        Err(CodecError::InvalidField {
            field: "events",
            ..
        })
    ));

    let gap = WireMessage::DurableGap(DurableGap {
        server: server_identity(),
        requested_after_revision: 1_800,
        available_from_revision: 1_820,
        current_revision: 1_845,
    });
    let not_a_gap = encode_line(&gap).unwrap().replace(
        "requested_after_revision=1800",
        "requested_after_revision=1819",
    );
    assert!(matches!(
        decode_line(&not_a_gap),
        Err(CodecError::InvalidField {
            field: "available_from_revision",
            ..
        })
    ));
}

#[test]
fn streaming_and_field_bounds_are_enforced_before_growth() {
    let mut decoder = LineDecoder::new();
    let oversized = vec![b'x'; MAX_LINE_BYTES + 1];
    assert_eq!(decoder.push(&oversized), Err(CodecError::LineTooLong));
    let error = encode_line(&WireMessage::Error(ErrorMessage {
        request_id: Some("01K:test".into()),
        current_revision: Some(1_845),
        error: structured_error(),
    }))
    .unwrap();
    assert_eq!(decoder.push(error.as_bytes()).unwrap().len(), 1);

    let oversized_field = format!(
        "error\trequest_id={}\tcode=e\tmessage=m\tfields=\tretryable=0\n",
        "x".repeat(MAX_FIELD_VALUE_BYTES + 1)
    );
    assert_eq!(
        decode_line(&oversized_field),
        Err(CodecError::FieldValueTooLong)
    );

    let one = "error\tcode=e\tmessage=m\tfields=\tretryable=0\n";
    let too_many = one.repeat(MAX_MESSAGES_PER_PUSH + 1);
    assert_eq!(
        decoder.push(too_many.as_bytes()),
        Err(CodecError::TooManyMessages)
    );

    let oversized_dto = WireMessage::Error(ErrorMessage {
        request_id: None,
        current_revision: None,
        error: StructuredError {
            code: "too_large".to_owned(),
            message: "x".repeat(MAX_FIELD_VALUE_BYTES + 1),
            fields: Vec::new(),
            retryable: false,
        },
    });
    assert_eq!(
        encode_line(&oversized_dto),
        Err(CodecError::FieldValueTooLong)
    );

    let mut fields = String::new();
    for index in 0..=MAX_FIELDS_PER_MESSAGE {
        write!(fields, "\tfield{index}=x").unwrap();
    }
    let too_many_fields = format!("error{fields}\n");
    assert_eq!(
        decode_line(&too_many_fields),
        Err(CodecError::TooManyFields)
    );

    let issues = (0..257).map(|_| "f~c~m").collect::<Vec<_>>().join(",");
    let too_many_items = format!("error\tcode=e\tmessage=m\tfields={issues}\tretryable=0\n");
    assert_eq!(
        decode_line(&too_many_items),
        Err(CodecError::TooManyItems("fields"))
    );
}

#[test]
fn phase_one_records_reject_malformed_and_unknown_required_fields() {
    let heartbeat = WireMessage::Heartbeat(HeartbeatMessage {
        server: server_identity(),
        sequence: 88,
        sent_at_ms: 1_720_000_000_000,
        last_applied: Some(resume_cursor()),
    });
    let missing_project = encode_line(&heartbeat)
        .unwrap()
        .replace("\tproject_id=project-9", "");
    assert_eq!(
        decode_line(&missing_project),
        Err(CodecError::MissingField("project_id"))
    );

    let batch = WireMessage::DurableEventBatch(DurableEventBatch {
        cursor: resume_cursor(),
        events: vec![DurableEvent {
            sequence: 0,
            event_type: "switcher.desired".into(),
            payload: "program=2&preview=1".into(),
        }],
    });
    let malformed_events = encode_line(&batch).unwrap().replace(
        "0%7Eswitcher.desired%7Eprogram%253D2%2526preview%253D1",
        "broken",
    );
    assert!(matches!(
        decode_line(&malformed_events),
        Err(CodecError::InvalidField {
            field: "events",
            ..
        })
    ));

    let report = WireMessage::CapabilityReport(CapabilityReportMessage {
        server: server_identity(),
        revision: 1_845,
        summary: CapabilityReportSummary {
            digest: "sha256:abc".into(),
            total: 4,
            available: 3,
            degraded: 1,
            unavailable: 0,
        },
    });
    let unknown = encode_line(&report)
        .unwrap()
        .replace('\n', "\tfuture_required=1\n");
    assert_eq!(
        decode_line(&unknown),
        Err(CodecError::UnknownField("future_required".to_owned()))
    );
}
