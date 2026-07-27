use core::{fmt::Write, num::NonZeroU128};

use fm_command::{Deadline, Revision, StateEpoch};
use fm_protocol::{
    BASE_PROTOCOL_VERSION, CURRENT_PROTOCOL_VERSION, CapabilityReportMessage,
    CapabilityReportSummary, ClientHello, ClientType, CodecError, CommandMessage, CommandPayload,
    CommandResult, DurableEvent, DurableEventBatch, DurableGap, EngineIdentity, ErrorMessage,
    EventCursor, EventMessage, EventPayload, FieldIssue, HandshakeOutcome, HandshakeRequest,
    HandshakeResponse, HeartbeatMessage, LineDecoder, MAX_FIELD_VALUE_BYTES,
    MAX_FIELDS_PER_MESSAGE, MAX_LINE_BYTES, MAX_LIST_ITEMS, MAX_MESSAGES_PER_PUSH, ProtocolVersion,
    ResumeCursor, Role, RuntimeDomainBoundary, RuntimeEventMessage, RuntimeFailureDisposition,
    RuntimeLifecycleEvent, ServerHello, ServerIdentity, SnapshotMessage, SnapshotReason,
    StructuredError, WIPE_PROTOCOL_VERSION, WireInputId, WireMessage, choose_handshake_outcome,
    decode_line, encode_line, negotiate_version,
};

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
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

fn capabilities() -> CapabilityReportSummary {
    CapabilityReportSummary {
        digest: "sha256:abc".to_owned(),
        total: 4,
        available: 3,
        degraded: 1,
        unavailable: 0,
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
        protocol: ProtocolVersion::new(1, 2),
        id: "01K:test".to_owned(),
        idempotency_key: "operator-7:01K".to_owned(),
        expected_revision: Some(1_842),
        deadline_ms: Some(500),
        payload: CommandPayload::SelectPreview { input: input(42) },
    }
}

#[test]
fn golden_command_fixture_is_stable() {
    let fixture = include_str!("fixtures/command_select.wire");
    let message = WireMessage::Command(command());
    assert_eq!(encode_line(&message).unwrap(), fixture);
    assert_eq!(decode_line(fixture).unwrap(), message);
}

#[test]
fn additive_wipe_command_has_a_stable_wire_form_without_changing_existing_bytes() {
    let existing_fixture = include_str!("fixtures/command_select.wire");
    assert_eq!(
        encode_line(&WireMessage::Command(command())).unwrap(),
        existing_fixture
    );

    let fixture = include_str!("fixtures/command_wipe.wire");
    let message = WireMessage::Command(CommandMessage {
        protocol: CURRENT_PROTOCOL_VERSION,
        payload: CommandPayload::Wipe {
            duration_frames: 45,
        },
        ..command()
    });
    assert_eq!(encode_line(&message).unwrap(), fixture);
    assert_eq!(decode_line(fixture).unwrap(), message);
}

#[test]
fn golden_client_hello_fixture_is_stable() {
    let fixture = include_str!("fixtures/client_hello.wire");
    let message = WireMessage::ClientHello(ClientHello {
        versions: vec![ProtocolVersion::new(1, 0), ProtocolVersion::new(1, 2)],
        build: "studio 0.1".to_owned(),
        client_type: ClientType::Studio,
        desired_role: Role::Operator,
        cached_cursor: Some(cursor()),
    });
    assert_eq!(encode_line(&message).unwrap(), fixture);
    assert_eq!(decode_line(fixture).unwrap(), message);
}

#[test]
fn every_message_variant_round_trips() {
    let messages = vec![
        WireMessage::ServerHello(ServerHello {
            negotiated: ProtocolVersion::new(1, 2),
            granted_role: Role::Operator,
            permissions: vec!["switcher.take".to_owned(), "preview:view".to_owned()],
            capabilities_digest: "sha256:a b".to_owned(),
            engine: identity(),
            current_revision: 1_842,
            resume: true,
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
            payload: CommandPayload::Wipe { duration_frames: 9 },
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
        }),
        WireMessage::Event(EventMessage {
            cursor: cursor(),
            payload: EventPayload::DesiredSwitcher {
                program: input(2),
                preview: input(1),
            },
        }),
    ];
    for message in messages {
        let encoded = encode_line(&message).unwrap();
        assert_eq!(decode_line(&encoded).unwrap(), message);
    }
}

#[test]
fn command_minimum_versions_gate_only_wipe() {
    for payload in [
        CommandPayload::SelectPreview { input: input(1) },
        CommandPayload::Cut,
        CommandPayload::Fade { duration_frames: 1 },
    ] {
        assert_eq!(payload.minimum_protocol_version(), BASE_PROTOCOL_VERSION);
        assert!(payload.is_supported_by(BASE_PROTOCOL_VERSION));
    }

    let wipe = CommandPayload::Wipe { duration_frames: 1 };
    assert_eq!(WIPE_PROTOCOL_VERSION, ProtocolVersion::new(1, 3));
    assert_eq!(CURRENT_PROTOCOL_VERSION, WIPE_PROTOCOL_VERSION);
    assert_eq!(wipe.minimum_protocol_version(), WIPE_PROTOCOL_VERSION);
    assert!(!wipe.is_supported_by(ProtocolVersion::new(1, 2)));
    assert!(wipe.is_supported_by(CURRENT_PROTOCOL_VERSION));
}

#[test]
fn unknown_optional_fields_are_ignored_but_required_fields_are_strict() {
    let fixture = include_str!("fixtures/command_select.wire");
    let optional = fixture.replace('\n', "\t?future=value\n");
    assert_eq!(
        decode_line(&optional).unwrap(),
        WireMessage::Command(command())
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
fn durable_event_rejects_legacy_runtime_realized_payload() {
    let legacy = concat!(
        "event\tengine_id=engine-a\tstate_epoch=7\tlog_id=log-a\trevision=1842",
        "\tevent=runtime_realized\tgeneration=4\tprogram=2\n"
    );
    assert_eq!(
        decode_line(legacy),
        Err(CodecError::InvalidField {
            field: "event",
            value: "runtime_realized".to_owned(),
        })
    );
}

#[test]
fn streaming_decoder_handles_split_and_multiple_records() {
    let command = include_str!("fixtures/command_select.wire");
    let hello = include_str!("fixtures/client_hello.wire");
    let bytes = format!("{command}{hello}");
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
fn version_negotiation_uses_newest_compatible_major_and_minor() {
    assert_eq!(
        negotiate_version(
            &[ProtocolVersion::new(1, 4), ProtocolVersion::new(2, 1)],
            &[ProtocolVersion::new(1, 6), ProtocolVersion::new(2, 0)]
        )
        .unwrap(),
        ProtocolVersion::new(2, 0)
    );
    assert!(
        negotiate_version(&[ProtocolVersion::new(3, 0)], &[ProtocolVersion::new(2, 9)]).is_err()
    );
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
fn phase_one_golden_messages_are_stable() {
    let messages = [
        (
            include_str!("fixtures/handshake_request.wire"),
            WireMessage::HandshakeRequest(HandshakeRequest {
                versions: vec![ProtocolVersion::new(1, 0), ProtocolVersion::new(1, 2)],
                build: "studio 0.2".to_owned(),
                client_type: ClientType::Studio,
                desired_role: Role::Operator,
                resume_cursor: Some(resume_cursor()),
            }),
        ),
        (
            include_str!("fixtures/handshake_resume.wire"),
            WireMessage::HandshakeResponse(HandshakeResponse {
                negotiated: ProtocolVersion::new(1, 2),
                granted_role: Role::Operator,
                permissions: vec!["switcher.take".to_owned()],
                capabilities: capabilities(),
                server: server_identity(),
                current_revision: 1_845,
                outcome: HandshakeOutcome::Resume {
                    cursor: resume_cursor(),
                },
            }),
        ),
        (
            include_str!("fixtures/durable_event_batch.wire"),
            WireMessage::DurableEventBatch(DurableEventBatch {
                cursor: ResumeCursor {
                    server: server_identity(),
                    revision: 1_843,
                },
                events: vec![DurableEvent {
                    sequence: 0,
                    event_type: "switcher.desired".to_owned(),
                    payload: "program=2&preview=1".to_owned(),
                }],
            }),
        ),
        (
            include_str!("fixtures/durable_gap.wire"),
            WireMessage::DurableGap(DurableGap {
                server: server_identity(),
                requested_after_revision: 1_800,
                available_from_revision: 1_820,
                current_revision: 1_845,
            }),
        ),
        (
            include_str!("fixtures/runtime_event.wire"),
            WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: server_identity(),
                revision: 1_843,
                generation: 12,
                sequence: 3,
                event: RuntimeLifecycleEvent::Scheduled {
                    domains: vec![
                        RuntimeDomainBoundary {
                            domain: "video".to_owned(),
                            boundary: 900,
                        },
                        RuntimeDomainBoundary {
                            domain: "audio".to_owned(),
                            boundary: 48_000,
                        },
                    ],
                },
            }),
        ),
        (
            include_str!("fixtures/heartbeat.wire"),
            WireMessage::Heartbeat(HeartbeatMessage {
                server: server_identity(),
                sequence: 88,
                sent_at_ms: 1_720_000_000_000,
                last_applied: Some(resume_cursor()),
            }),
        ),
        (
            include_str!("fixtures/capability_report.wire"),
            WireMessage::CapabilityReport(CapabilityReportMessage {
                server: server_identity(),
                revision: 1_845,
                summary: capabilities(),
            }),
        ),
        (
            include_str!("fixtures/error.wire"),
            WireMessage::Error(ErrorMessage {
                request_id: Some("01K:test".to_owned()),
                current_revision: Some(1_845),
                error: structured_error(),
            }),
        ),
    ];
    for (fixture, message) in messages {
        assert_eq!(encode_line(&message).unwrap(), fixture);
        assert_eq!(decode_line(fixture).unwrap(), message);
    }
}

#[test]
fn all_handshake_outcomes_have_stable_wire_forms() {
    let base = HandshakeResponse {
        negotiated: ProtocolVersion::new(1, 2),
        granted_role: Role::Operator,
        permissions: vec!["switcher.take".to_owned()],
        capabilities: capabilities(),
        server: server_identity(),
        current_revision: 1_845,
        outcome: HandshakeOutcome::Snapshot {
            reason: SnapshotReason::HistoryUnavailable,
        },
    };
    let snapshot = WireMessage::HandshakeResponse(base.clone());
    let snapshot_fixture = include_str!("fixtures/handshake_snapshot.wire");
    assert_eq!(encode_line(&snapshot).unwrap(), snapshot_fixture);
    assert_eq!(decode_line(snapshot_fixture).unwrap(), snapshot);

    let rejected = WireMessage::HandshakeResponse(HandshakeResponse {
        granted_role: Role::Viewer,
        permissions: Vec::new(),
        outcome: HandshakeOutcome::Rejected {
            error: StructuredError {
                code: "permission_denied".to_owned(),
                message: "role is not allowed".to_owned(),
                fields: Vec::new(),
                retryable: false,
            },
        },
        ..base
    });
    let rejected_fixture = include_str!("fixtures/handshake_rejected.wire");
    assert_eq!(encode_line(&rejected).unwrap(), rejected_fixture);
    assert_eq!(decode_line(rejected_fixture).unwrap(), rejected);
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
