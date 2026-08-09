use core::{fmt::Write, num::NonZeroU128};

use fm_command::{Deadline, Revision, StateEpoch};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CodecError, CommandMessage, CommandPayload, CommandResult,
    EngineIdentity, ErrorMessage, EventCursor, EventMessage, EventPayload, FadeToBlackPosition,
    FadeToBlackState, FieldIssue, HandshakeOutcome, LineDecoder, MAX_FIELD_VALUE_BYTES,
    MAX_FIELDS_PER_MESSAGE, MAX_LINE_BYTES, MAX_LIST_ITEMS, MAX_MESSAGES_PER_PUSH,
    ManualTransitionStatus, OverlayStatus, OverlayTransitionKind, ProtocolVersion, ResumeCursor,
    RuntimeDomainBoundary, RuntimeEventMessage, RuntimeFailureDisposition, RuntimeLifecycleEvent,
    ServerIdentity, SnapshotMessage, SnapshotReason, StingerAudioPolicy,
    StingerMissingMediaFallback, StingerReadiness, StingerStatus, StructuredError, WireInputId,
    WireMessage, WireOutputId, WireOverlayChannelId, WireStingerSlotId, choose_handshake_outcome,
    decode_line, encode_line, negotiate_version,
};

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
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

#[test]
#[allow(clippy::too_many_lines)]
fn every_message_variant_round_trips() {
    let messages = vec![
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
        WireMessage::Snapshot(SnapshotMessage {
            engine: identity(),
            revision: 9,
            show_name: "My Show\nA".to_owned(),
            inputs: vec![input(1), input(2)],
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
        }),
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
            },
        }),
    ];
    for message in messages {
        let encoded = encode_line(&message).unwrap();
        assert_eq!(decode_line(&encoded).unwrap(), message);
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
            },
        })
    };
    assert!(matches!(
        encode_line(&event(vec![status, status])),
        Err(CodecError::InvalidField {
            field: "?stingers",
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
    let fixture = include_str!("fixtures/command_manual_position.wire");
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
fn exact_contract_rejects_every_unknown_field() {
    let fixture = include_str!("fixtures/command_select.wire");
    let optional = fixture.replace('\n', "\t?future=value\n");
    assert_eq!(
        decode_line(&optional),
        Err(CodecError::UnknownField("?future".to_owned()))
    );

    let required = fixture.replace('\n', "\tfuture=value\n");
    assert_eq!(
        decode_line(&required),
        Err(CodecError::UnknownField("future".to_owned()))
    );
}

#[test]
fn decoder_rejects_truncation_duplicates_and_invalid_values() {
    let fixture = include_str!("fixtures/command_select.wire");
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
    let command = include_str!("fixtures/command_select.wire");
    let second_command = include_str!("fixtures/command_select.wire");
    let bytes = format!("{command}{second_command}");
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
fn version_negotiation_accepts_only_the_exact_current_contract() {
    assert_eq!(
        negotiate_version(
            &[ProtocolVersion::new(2, 3), CURRENT_PROTOCOL_VERSION],
            &[CURRENT_PROTOCOL_VERSION]
        )
        .unwrap(),
        CURRENT_PROTOCOL_VERSION
    );
    assert!(negotiate_version(&[ProtocolVersion::new(2, 3)], &[CURRENT_PROTOCOL_VERSION]).is_err());
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
    let oversized = include_str!("fixtures/runtime_event.wire")
        .replace("video%7E900%2Caudio%7E48000", &domains);
    assert_eq!(
        decode_line(&oversized),
        Err(CodecError::TooManyItems("domains"))
    );
}

#[test]
fn durable_gaps_are_structural_and_batches_cannot_hide_sequence_gaps() {
    let malformed_batch =
        include_str!("fixtures/durable_event_batch.wire").replace("events=0%7E", "events=1%7E");
    assert!(matches!(
        decode_line(&malformed_batch),
        Err(CodecError::InvalidField {
            field: "events",
            ..
        })
    ));

    let not_a_gap = include_str!("fixtures/durable_gap.wire").replace(
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
    assert_eq!(
        decoder
            .push(include_bytes!("fixtures/error.wire"))
            .unwrap()
            .len(),
        1
    );

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
        write!(fields, "\t?field{index}=x").unwrap();
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
    let missing_project =
        include_str!("fixtures/heartbeat.wire").replace("\tproject_id=project-9", "");
    assert_eq!(
        decode_line(&missing_project),
        Err(CodecError::MissingField("project_id"))
    );

    let malformed_events = include_str!("fixtures/durable_event_batch.wire").replace(
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

    let unknown =
        include_str!("fixtures/capability_report.wire").replace('\n', "\tfuture_required=1\n");
    assert_eq!(
        decode_line(&unknown),
        Err(CodecError::UnknownField("future_required".to_owned()))
    );
}
