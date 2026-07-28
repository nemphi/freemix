use core::{num::NonZeroU128, str::FromStr};
use std::collections::BTreeMap;

use crate::{
    CapabilityReportMessage, CapabilityReportSummary, ClientHello, CodecError, CommandMessage,
    CommandPayload, CommandResult, DurableEventBatch, DurableGap, EngineIdentity, ErrorMessage,
    EventCursor, EventMessage, EventPayload, HandshakeOutcome, HandshakeRequest, HandshakeResponse,
    HeartbeatMessage, ResumeCursor, RuntimeEventMessage, RuntimeFailureDisposition,
    RuntimeLifecycleEvent, ServerHello, ServerIdentity, SnapshotMessage, SnapshotReason,
    StructuredError, WireInputId, WireMessage,
};

use super::value::{
    parse_client_type, parse_durable_events, parse_field_issues, parse_role, parse_runtime_domains,
    parse_string_list, parse_version, parse_versions, unescape,
};
use super::{
    MAX_FIELD_NAME_BYTES, MAX_FIELD_VALUE_BYTES, MAX_FIELDS_PER_MESSAGE, MAX_LINE_BYTES,
    MAX_LIST_ITEMS, MAX_MESSAGES_PER_PUSH,
};

/// Decodes exactly one newline-terminated wire record.
///
/// Unknown fields prefixed with `?` are treated as optional extensions and
/// ignored. Other unknown fields are rejected.
///
/// # Errors
///
/// Returns a [`CodecError`] for invalid framing, escaping, required fields,
/// field values, or message types.
pub fn decode_line(line: &str) -> Result<WireMessage, CodecError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(CodecError::LineTooLong);
    }
    let Some(record) = line.strip_suffix('\n') else {
        return Err(CodecError::MissingNewline);
    };
    if record.contains('\n') || record.contains('\r') {
        return Err(CodecError::MultipleLines);
    }
    let mut parts = record.split('\t');
    let kind = parts
        .next()
        .filter(|kind| !kind.is_empty())
        .ok_or(CodecError::InvalidRecord)?;
    let mut fields = Fields::parse(parts)?;
    let message = match kind {
        "client_hello" => WireMessage::ClientHello(decode_client_hello(&mut fields)?),
        "server_hello" => WireMessage::ServerHello(decode_server_hello(&mut fields)?),
        "command" => WireMessage::Command(decode_command(&mut fields)?),
        "command_result" => WireMessage::CommandResult(decode_result(&mut fields)?),
        "snapshot" => WireMessage::Snapshot(decode_snapshot(&mut fields)?),
        "event" => WireMessage::Event(decode_event(&mut fields)?),
        "handshake_request" => {
            WireMessage::HandshakeRequest(decode_handshake_request(&mut fields)?)
        }
        "handshake_response" => {
            WireMessage::HandshakeResponse(decode_handshake_response(&mut fields)?)
        }
        "durable_event_batch" => WireMessage::DurableEventBatch(decode_durable_batch(&mut fields)?),
        "durable_gap" => WireMessage::DurableGap(decode_durable_gap(&mut fields)?),
        "runtime_event" => WireMessage::RuntimeEvent(decode_runtime_event(&mut fields)?),
        "heartbeat" => WireMessage::Heartbeat(decode_heartbeat(&mut fields)?),
        "capability_report" => {
            WireMessage::CapabilityReport(decode_capability_report(&mut fields)?)
        }
        "error" => WireMessage::Error(decode_error_message(&mut fields)?),
        _ => return Err(CodecError::UnknownMessage(kind.to_owned())),
    };
    fields.finish()?;
    Ok(message)
}

#[derive(Clone, Debug, Default)]
pub struct LineDecoder {
    buffer: Vec<u8>,
}

impl LineDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Adds an arbitrary stream chunk and returns every complete record.
    ///
    /// # Errors
    ///
    /// Returns a [`CodecError`] for an oversized, non-UTF-8, or malformed
    /// record. Successfully decoded earlier records are not returned when a
    /// later record in the same chunk fails.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<WireMessage>, CodecError> {
        let mut messages = Vec::new();
        for part in chunk.split_inclusive(|byte| *byte == b'\n') {
            if self.buffer.len() + part.len() > MAX_LINE_BYTES {
                self.buffer.clear();
                return Err(CodecError::LineTooLong);
            }
            self.buffer.extend_from_slice(part);
            if part.last() == Some(&b'\n') {
                if messages.len() == MAX_MESSAGES_PER_PUSH {
                    self.buffer.clear();
                    return Err(CodecError::TooManyMessages);
                }
                let Ok(line) = core::str::from_utf8(&self.buffer) else {
                    self.buffer.clear();
                    return Err(CodecError::InvalidUtf8);
                };
                let decoded = decode_line(line);
                self.buffer.clear();
                let message = decoded?;
                messages.push(message);
            } else if self.buffer.len() == MAX_LINE_BYTES {
                self.buffer.clear();
                return Err(CodecError::LineTooLong);
            }
        }
        Ok(messages)
    }

    /// Verifies that the stream ended at a record boundary.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::TrailingData`] when an incomplete record remains.
    pub fn finish(self) -> Result<(), CodecError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(CodecError::TrailingData)
        }
    }
}

struct Fields(BTreeMap<String, String>);

impl Fields {
    fn parse<'a>(parts: impl Iterator<Item = &'a str>) -> Result<Self, CodecError> {
        let mut fields = BTreeMap::new();
        for part in parts {
            if fields.len() == MAX_FIELDS_PER_MESSAGE {
                return Err(CodecError::TooManyFields);
            }
            let (name, value) = part.split_once('=').ok_or(CodecError::InvalidRecord)?;
            if name.len() > MAX_FIELD_NAME_BYTES {
                return Err(CodecError::FieldNameTooLong);
            }
            if value.len() > MAX_FIELD_VALUE_BYTES {
                return Err(CodecError::FieldValueTooLong);
            }
            if name.is_empty() || name.bytes().any(|byte| !valid_field_byte(byte)) {
                return Err(CodecError::InvalidRecord);
            }
            if fields.contains_key(name) {
                return Err(CodecError::DuplicateField(name.to_owned()));
            }
            let value = unescape(value)?;
            fields.insert(name.to_owned(), value);
        }
        Ok(Self(fields))
    }

    fn required(&mut self, name: &'static str) -> Result<String, CodecError> {
        self.0.remove(name).ok_or(CodecError::MissingField(name))
    }

    fn optional(&mut self, name: &'static str) -> Option<String> {
        self.0.remove(name)
    }

    fn parse_required<T: FromStr>(&mut self, name: &'static str) -> Result<T, CodecError> {
        let value = self.required(name)?;
        value
            .parse()
            .map_err(|_| CodecError::InvalidField { field: name, value })
    }

    fn parse_optional<T: FromStr>(&mut self, name: &'static str) -> Result<Option<T>, CodecError> {
        self.optional(name)
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| CodecError::InvalidField { field: name, value })
            })
            .transpose()
    }

    fn boolean(&mut self, name: &'static str) -> Result<bool, CodecError> {
        let value = self.required(name)?;
        match value.as_str() {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(CodecError::InvalidField { field: name, value }),
        }
    }

    fn input(&mut self, name: &'static str) -> Result<WireInputId, CodecError> {
        let value = self.required(name)?;
        parse_input(&value).ok_or(CodecError::InvalidField { field: name, value })
    }

    fn finish(self) -> Result<(), CodecError> {
        self.0
            .into_keys()
            .find(|name| !name.starts_with('?'))
            .map_or(Ok(()), |name| Err(CodecError::UnknownField(name)))
    }
}

const fn valid_field_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'?')
}

fn decode_identity(fields: &mut Fields) -> Result<EngineIdentity, CodecError> {
    Ok(EngineIdentity {
        engine_id: fields.required("engine_id")?,
        state_epoch: fields.parse_required("state_epoch")?,
        log_id: fields.required("log_id")?,
    })
}

fn decode_cursor(fields: &mut Fields) -> Result<EventCursor, CodecError> {
    Ok(EventCursor {
        engine: decode_identity(fields)?,
        revision: fields.parse_required("revision")?,
    })
}

fn decode_client_hello(fields: &mut Fields) -> Result<ClientHello, CodecError> {
    let value = fields.required("versions")?;
    let versions = parse_versions(&value)?;
    let value = fields.required("client_type")?;
    let client_type = parse_client_type(&value).ok_or(CodecError::InvalidField {
        field: "client_type",
        value,
    })?;
    let value = fields.required("desired_role")?;
    let desired_role = parse_role(&value).ok_or(CodecError::InvalidField {
        field: "desired_role",
        value,
    })?;
    let cached_cursor = if fields.boolean("cached")? {
        Some(EventCursor {
            engine: EngineIdentity {
                engine_id: fields.required("cached_engine_id")?,
                state_epoch: fields.parse_required("cached_state_epoch")?,
                log_id: fields.required("cached_log_id")?,
            },
            revision: fields.parse_required("cached_revision")?,
        })
    } else {
        None
    };
    Ok(ClientHello {
        versions,
        build: fields.required("build")?,
        client_type,
        desired_role,
        cached_cursor,
    })
}

fn decode_server_hello(fields: &mut Fields) -> Result<ServerHello, CodecError> {
    let value = fields.required("protocol")?;
    let negotiated = parse_version(&value).ok_or(CodecError::InvalidField {
        field: "protocol",
        value,
    })?;
    let value = fields.required("granted_role")?;
    let granted_role = parse_role(&value).ok_or(CodecError::InvalidField {
        field: "granted_role",
        value,
    })?;
    let permissions = parse_string_list(&fields.required("permissions")?)?;
    Ok(ServerHello {
        negotiated,
        granted_role,
        permissions,
        capabilities_digest: fields.required("capabilities")?,
        engine: decode_identity(fields)?,
        current_revision: fields.parse_required("current_revision")?,
        resume: fields.boolean("resume")?,
    })
}

fn decode_command(fields: &mut Fields) -> Result<CommandMessage, CodecError> {
    let value = fields.required("protocol")?;
    let protocol = parse_version(&value).ok_or(CodecError::InvalidField {
        field: "protocol",
        value,
    })?;
    let payload_name = fields.required("payload")?;
    let payload = match payload_name.as_str() {
        "select_preview" => CommandPayload::SelectPreview {
            input: fields.input("input")?,
        },
        "cut" => CommandPayload::Cut,
        "fade" => CommandPayload::Fade {
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "wipe" => CommandPayload::Wipe {
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "manual_start" => {
            let value = fields.required("transition")?;
            let kind = match value.as_str() {
                "fade" => crate::ManualTransitionKind::Fade,
                "wipe" => crate::ManualTransitionKind::Wipe,
                _ => {
                    return Err(CodecError::InvalidField {
                        field: "transition",
                        value,
                    });
                }
            };
            CommandPayload::StartManualTransition { kind }
        }
        "manual_position" => {
            let value = fields.required("position_basis_points")?;
            let basis_points = value.parse().map_err(|_| CodecError::InvalidField {
                field: "position_basis_points",
                value: value.clone(),
            })?;
            let position = crate::ManualTransitionPosition::new(basis_points).ok_or(
                CodecError::InvalidField {
                    field: "position_basis_points",
                    value,
                },
            )?;
            CommandPayload::SetManualTransitionPosition { position }
        }
        "manual_commit" => CommandPayload::CommitManualTransition,
        "manual_cancel" => CommandPayload::CancelManualTransition,
        _ => {
            return Err(CodecError::InvalidField {
                field: "payload",
                value: payload_name,
            });
        }
    };
    Ok(CommandMessage {
        protocol,
        id: fields.required("id")?,
        idempotency_key: fields.required("idempotency_key")?,
        expected_revision: fields.parse_optional("expected_revision")?,
        deadline_ms: fields.parse_optional("deadline_ms")?,
        payload,
    })
}

fn decode_result(fields: &mut Fields) -> Result<CommandResult, CodecError> {
    let status = fields.required("status")?;
    match status.as_str() {
        "accepted" => Ok(CommandResult::Accepted {
            id: fields.required("id")?,
            revision: fields.parse_required("revision")?,
            scheduled_frame: fields.parse_optional("scheduled_frame")?,
        }),
        "rejected" => Ok(CommandResult::Rejected {
            id: fields.required("id")?,
            code: fields.required("code")?,
            message: fields.required("message")?,
            fields: parse_field_issues(&fields.required("fields")?)?,
            current_revision: fields.parse_required("current_revision")?,
            retryable: fields.boolean("retryable")?,
        }),
        _ => Err(CodecError::InvalidField {
            field: "status",
            value: status,
        }),
    }
}

fn decode_snapshot(fields: &mut Fields) -> Result<SnapshotMessage, CodecError> {
    let inputs_value = fields.required("inputs")?;
    let inputs = parse_inputs(&inputs_value).ok_or(CodecError::InvalidField {
        field: "inputs",
        value: inputs_value,
    })?;
    Ok(SnapshotMessage {
        engine: decode_identity(fields)?,
        revision: fields.parse_required("revision")?,
        show_name: fields.required("show_name")?,
        inputs,
        desired_program: fields.input("desired_program")?,
        desired_preview: fields.input("desired_preview")?,
        realized_program: fields.input("realized_program")?,
        realized_preview: fields.input("realized_preview")?,
    })
}

fn decode_event(fields: &mut Fields) -> Result<EventMessage, CodecError> {
    let event = fields.required("event")?;
    let payload = match event.as_str() {
        "desired_switcher" => EventPayload::DesiredSwitcher {
            program: fields.input("program")?,
            preview: fields.input("preview")?,
        },
        _ => {
            return Err(CodecError::InvalidField {
                field: "event",
                value: event,
            });
        }
    };
    Ok(EventMessage {
        cursor: decode_cursor(fields)?,
        payload,
    })
}

fn parse_input(value: &str) -> Option<WireInputId> {
    Some(WireInputId::new(NonZeroU128::new(value.parse().ok()?)?))
}

fn parse_inputs(value: &str) -> Option<Vec<WireInputId>> {
    if value.is_empty() {
        return None;
    }
    if value.split(',').take(MAX_LIST_ITEMS + 1).count() > MAX_LIST_ITEMS {
        return None;
    }
    value.split(',').map(parse_input).collect()
}

fn decode_server_identity(fields: &mut Fields) -> Result<ServerIdentity, CodecError> {
    Ok(ServerIdentity {
        engine_id: fields.required("engine_id")?,
        project_id: fields.required("project_id")?,
        state_epoch: fields.parse_required("state_epoch")?,
        log_id: fields.required("log_id")?,
    })
}

fn decode_resume_cursor(
    fields: &mut Fields,
    prefix: &'static str,
) -> Result<ResumeCursor, CodecError> {
    let (engine, project, epoch, log, revision) = match prefix {
        "resume" => (
            "resume_engine_id",
            "resume_project_id",
            "resume_state_epoch",
            "resume_log_id",
            "resume_revision",
        ),
        "applied" => (
            "applied_engine_id",
            "applied_project_id",
            "applied_state_epoch",
            "applied_log_id",
            "applied_revision",
        ),
        _ => unreachable!("cursor prefixes are fixed by the wire schema"),
    };
    Ok(ResumeCursor {
        server: ServerIdentity {
            engine_id: fields.required(engine)?,
            project_id: fields.required(project)?,
            state_epoch: fields.parse_required(epoch)?,
            log_id: fields.required(log)?,
        },
        revision: fields.parse_required(revision)?,
    })
}

fn decode_handshake_request(fields: &mut Fields) -> Result<HandshakeRequest, CodecError> {
    let versions = parse_versions(&fields.required("versions")?)?;
    let client_type_value = fields.required("client_type")?;
    let client_type = parse_client_type(&client_type_value).ok_or(CodecError::InvalidField {
        field: "client_type",
        value: client_type_value,
    })?;
    let role_value = fields.required("desired_role")?;
    let desired_role = parse_role(&role_value).ok_or(CodecError::InvalidField {
        field: "desired_role",
        value: role_value,
    })?;
    let resume_cursor = if fields.boolean("resume")? {
        Some(decode_resume_cursor(fields, "resume")?)
    } else {
        None
    };
    Ok(HandshakeRequest {
        versions,
        build: fields.required("build")?,
        client_type,
        desired_role,
        resume_cursor,
    })
}

fn decode_handshake_response(fields: &mut Fields) -> Result<HandshakeResponse, CodecError> {
    let protocol_value = fields.required("protocol")?;
    let negotiated = parse_version(&protocol_value).ok_or(CodecError::InvalidField {
        field: "protocol",
        value: protocol_value,
    })?;
    let role_value = fields.required("granted_role")?;
    let granted_role = parse_role(&role_value).ok_or(CodecError::InvalidField {
        field: "granted_role",
        value: role_value,
    })?;
    let permissions = parse_string_list(&fields.required("permissions")?)?;
    let capabilities = decode_capability_summary(fields)?;
    let server = decode_server_identity(fields)?;
    let current_revision = fields.parse_required("current_revision")?;
    let outcome_value = fields.required("outcome")?;
    let outcome = match outcome_value.as_str() {
        "snapshot" => {
            let reason_value = fields.required("snapshot_reason")?;
            let reason = parse_snapshot_reason(&reason_value).ok_or(CodecError::InvalidField {
                field: "snapshot_reason",
                value: reason_value,
            })?;
            HandshakeOutcome::Snapshot { reason }
        }
        "resume" => {
            let cursor = decode_resume_cursor(fields, "resume")?;
            if cursor.server != server || cursor.revision > current_revision {
                return Err(CodecError::InvalidField {
                    field: "resume_revision",
                    value: cursor.revision.to_string(),
                });
            }
            HandshakeOutcome::Resume { cursor }
        }
        "rejected" => HandshakeOutcome::Rejected {
            error: decode_structured_error(fields)?,
        },
        _ => {
            return Err(CodecError::InvalidField {
                field: "outcome",
                value: outcome_value,
            });
        }
    };
    Ok(HandshakeResponse {
        negotiated,
        granted_role,
        permissions,
        capabilities,
        server,
        current_revision,
        outcome,
    })
}

fn decode_durable_batch(fields: &mut Fields) -> Result<DurableEventBatch, CodecError> {
    let server = decode_server_identity(fields)?;
    let revision = fields.parse_required("revision")?;
    let events_value = fields.required("events")?;
    let events = parse_durable_events(&events_value)?;
    for (expected, event) in events.iter().enumerate() {
        if usize::from(event.sequence) != expected {
            return Err(CodecError::InvalidField {
                field: "events",
                value: events_value,
            });
        }
    }
    Ok(DurableEventBatch {
        cursor: ResumeCursor { server, revision },
        events,
    })
}

fn decode_durable_gap(fields: &mut Fields) -> Result<DurableGap, CodecError> {
    let gap = DurableGap {
        server: decode_server_identity(fields)?,
        requested_after_revision: fields.parse_required("requested_after_revision")?,
        available_from_revision: fields.parse_required("available_from_revision")?,
        current_revision: fields.parse_required("current_revision")?,
    };
    if gap.requested_after_revision.saturating_add(1) >= gap.available_from_revision
        || gap.available_from_revision > gap.current_revision
    {
        return Err(CodecError::InvalidField {
            field: "available_from_revision",
            value: gap.available_from_revision.to_string(),
        });
    }
    Ok(gap)
}

fn decode_runtime_event(fields: &mut Fields) -> Result<RuntimeEventMessage, CodecError> {
    let server = decode_server_identity(fields)?;
    let revision = fields.parse_required("revision")?;
    let generation = fields.parse_required("generation")?;
    let sequence = fields.parse_required("sequence")?;
    let event_value = fields.required("event")?;
    let event = match event_value.as_str() {
        "accepted" => RuntimeLifecycleEvent::Accepted,
        "preparing" => RuntimeLifecycleEvent::Preparing,
        "scheduled" => RuntimeLifecycleEvent::Scheduled {
            domains: parse_runtime_domains(&fields.required("domains")?)?,
        },
        "realized" => RuntimeLifecycleEvent::Realized {
            domain: fields.required("domain")?,
        },
        "failed" => {
            let disposition_value = fields.required("disposition")?;
            let disposition =
                parse_failure_disposition(&disposition_value).ok_or(CodecError::InvalidField {
                    field: "disposition",
                    value: disposition_value,
                })?;
            RuntimeLifecycleEvent::Failed {
                error: decode_structured_error(fields)?,
                disposition,
            }
        }
        "superseded" => RuntimeLifecycleEvent::Superseded {
            by_revision: fields.parse_required("by_revision")?,
        },
        _ => {
            return Err(CodecError::InvalidField {
                field: "event",
                value: event_value,
            });
        }
    };
    Ok(RuntimeEventMessage {
        server,
        revision,
        generation,
        sequence,
        event,
    })
}

fn decode_heartbeat(fields: &mut Fields) -> Result<HeartbeatMessage, CodecError> {
    let server = decode_server_identity(fields)?;
    let sequence = fields.parse_required("sequence")?;
    let sent_at_ms = fields.parse_required("sent_at_ms")?;
    let last_applied = if fields.boolean("applied")? {
        let cursor = decode_resume_cursor(fields, "applied")?;
        if cursor.server != server {
            return Err(CodecError::InvalidField {
                field: "applied_engine_id",
                value: cursor.server.engine_id,
            });
        }
        Some(cursor)
    } else {
        None
    };
    Ok(HeartbeatMessage {
        server,
        sequence,
        sent_at_ms,
        last_applied,
    })
}

fn decode_capability_report(fields: &mut Fields) -> Result<CapabilityReportMessage, CodecError> {
    Ok(CapabilityReportMessage {
        server: decode_server_identity(fields)?,
        revision: fields.parse_required("revision")?,
        summary: decode_capability_summary(fields)?,
    })
}

fn decode_capability_summary(fields: &mut Fields) -> Result<CapabilityReportSummary, CodecError> {
    let summary = CapabilityReportSummary {
        digest: fields.required("capability_digest")?,
        total: fields.parse_required("capability_total")?,
        available: fields.parse_required("capability_available")?,
        degraded: fields.parse_required("capability_degraded")?,
        unavailable: fields.parse_required("capability_unavailable")?,
    };
    let classified = summary
        .available
        .checked_add(summary.degraded)
        .and_then(|value| value.checked_add(summary.unavailable));
    if classified != Some(summary.total) {
        return Err(CodecError::InvalidField {
            field: "capability_total",
            value: summary.total.to_string(),
        });
    }
    Ok(summary)
}

fn decode_error_message(fields: &mut Fields) -> Result<ErrorMessage, CodecError> {
    Ok(ErrorMessage {
        request_id: fields.optional("request_id"),
        current_revision: fields.parse_optional("current_revision")?,
        error: decode_structured_error(fields)?,
    })
}

fn decode_structured_error(fields: &mut Fields) -> Result<StructuredError, CodecError> {
    let error = StructuredError {
        code: fields.required("code")?,
        message: fields.required("message")?,
        fields: parse_field_issues(&fields.required("fields")?)?,
        retryable: fields.boolean("retryable")?,
    };
    if error.code.is_empty()
        || error.code.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.'))
        })
    {
        return Err(CodecError::InvalidField {
            field: "code",
            value: error.code,
        });
    }
    Ok(error)
}

fn parse_snapshot_reason(value: &str) -> Option<SnapshotReason> {
    match value {
        "no_cursor" => Some(SnapshotReason::NoCursor),
        "identity_changed" => Some(SnapshotReason::IdentityChanged),
        "cursor_ahead" => Some(SnapshotReason::CursorAhead),
        "history_unavailable" => Some(SnapshotReason::HistoryUnavailable),
        _ => None,
    }
}

fn parse_failure_disposition(value: &str) -> Option<RuntimeFailureDisposition> {
    match value {
        "rolled_back" => Some(RuntimeFailureDisposition::RolledBack),
        "retained_for_retry" => Some(RuntimeFailureDisposition::RetainedForRetry),
        "fallback_realized" => Some(RuntimeFailureDisposition::FallbackRealized),
        _ => None,
    }
}
