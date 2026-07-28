use crate::{
    CapabilityReportMessage, CapabilityReportSummary, ClientHello, CommandMessage, CommandPayload,
    CommandResult, DurableEventBatch, DurableGap, EngineIdentity, ErrorMessage, EventCursor,
    EventMessage, EventPayload, FadeToBlackState, HandshakeOutcome, HandshakeRequest,
    HandshakeResponse, HeartbeatMessage, ManualTransitionKind, ManualTransitionStatus,
    ResumeCursor, RuntimeEventMessage, RuntimeFailureDisposition, RuntimeLifecycleEvent,
    ServerHello, ServerIdentity, SnapshotMessage, SnapshotReason, StructuredError, WireMessage,
};

use super::value::{
    client_type, durable_events, field_issues, role, runtime_domains, string_list, versions,
};
use super::{CodecError, MAX_FIELD_VALUE_BYTES, MAX_LINE_BYTES, MAX_LIST_ITEMS};

/// Encodes one message as a single newline-terminated record.
///
/// # Errors
///
/// Returns [`CodecError::LineTooLong`] if the encoded record exceeds
/// [`MAX_LINE_BYTES`].
pub fn encode_line(message: &WireMessage) -> Result<String, CodecError> {
    let mut record = Record::default();
    match message {
        WireMessage::ClientHello(message) => encode_client_hello(&mut record, message)?,
        WireMessage::ServerHello(message) => encode_server_hello(&mut record, message)?,
        WireMessage::Command(message) => encode_command(&mut record, message)?,
        WireMessage::CommandResult(message) => encode_result(&mut record, message)?,
        WireMessage::Snapshot(message) => encode_snapshot(&mut record, message)?,
        WireMessage::Event(message) => encode_event(&mut record, message)?,
        WireMessage::HandshakeRequest(message) => encode_handshake_request(&mut record, message)?,
        WireMessage::HandshakeResponse(message) => {
            encode_handshake_response(&mut record, message)?;
        }
        WireMessage::DurableEventBatch(message) => encode_durable_batch(&mut record, message)?,
        WireMessage::DurableGap(message) => encode_durable_gap(&mut record, message)?,
        WireMessage::RuntimeEvent(message) => encode_runtime_event(&mut record, message)?,
        WireMessage::Heartbeat(message) => encode_heartbeat(&mut record, message)?,
        WireMessage::CapabilityReport(message) => encode_capability_report(&mut record, message)?,
        WireMessage::Error(message) => encode_error_message(&mut record, message)?,
    }
    record.finish()
}

#[derive(Default)]
struct Record {
    kind: &'static str,
    fields: Vec<(&'static str, String)>,
}

impl Record {
    fn kind(&mut self, kind: &'static str) {
        self.kind = kind;
    }

    #[allow(clippy::needless_pass_by_value)]
    fn field(&mut self, name: &'static str, value: impl ToString) -> Result<(), CodecError> {
        let value = value.to_string();
        self.field_string(name, value)
    }

    fn field_str(&mut self, name: &'static str, value: &str) -> Result<(), CodecError> {
        if value.len() > MAX_FIELD_VALUE_BYTES {
            return Err(CodecError::FieldValueTooLong);
        }
        self.fields.push((name, value.to_owned()));
        Ok(())
    }

    fn field_string(&mut self, name: &'static str, value: String) -> Result<(), CodecError> {
        if value.len() > MAX_FIELD_VALUE_BYTES {
            return Err(CodecError::FieldValueTooLong);
        }
        self.fields.push((name, value));
        Ok(())
    }

    fn optional(
        &mut self,
        name: &'static str,
        value: Option<impl ToString>,
    ) -> Result<(), CodecError> {
        if let Some(value) = value {
            self.field(name, value)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<String, CodecError> {
        let mut line = self.kind.to_owned();
        for (name, value) in self.fields {
            if line.len() + name.len() + 2 > MAX_LINE_BYTES {
                return Err(CodecError::LineTooLong);
            }
            line.push('\t');
            line.push_str(name);
            line.push('=');
            for byte in value.bytes() {
                let additional =
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                        1
                    } else {
                        3
                    };
                if line.len() + additional + 1 > MAX_LINE_BYTES {
                    return Err(CodecError::LineTooLong);
                }
                if additional == 1 {
                    line.push(char::from(byte));
                } else {
                    use core::fmt::Write;
                    write!(line, "%{byte:02X}").expect("writing to a string cannot fail");
                }
            }
        }
        if line.len() == MAX_LINE_BYTES {
            return Err(CodecError::LineTooLong);
        }
        line.push('\n');
        Ok(line)
    }
}

fn encode_identity(record: &mut Record, identity: &EngineIdentity) -> Result<(), CodecError> {
    record.field_str("engine_id", &identity.engine_id)?;
    record.field("state_epoch", identity.state_epoch)?;
    record.field_str("log_id", &identity.log_id)
}

fn encode_cursor(record: &mut Record, cursor: &EventCursor) -> Result<(), CodecError> {
    encode_identity(record, &cursor.engine)?;
    record.field("revision", cursor.revision)
}

fn encode_client_hello(record: &mut Record, message: &ClientHello) -> Result<(), CodecError> {
    record.kind("client_hello");
    check_count(message.versions.len(), "versions")?;
    record.field("versions", versions(&message.versions))?;
    record.field_str("build", &message.build)?;
    record.field("client_type", client_type(message.client_type))?;
    record.field("desired_role", role(message.desired_role))?;
    record.field("cached", u8::from(message.cached_cursor.is_some()))?;
    if let Some(cursor) = &message.cached_cursor {
        record.field_str("cached_engine_id", &cursor.engine.engine_id)?;
        record.field("cached_state_epoch", cursor.engine.state_epoch)?;
        record.field_str("cached_log_id", &cursor.engine.log_id)?;
        record.field("cached_revision", cursor.revision)?;
    }
    Ok(())
}

fn encode_server_hello(record: &mut Record, message: &ServerHello) -> Result<(), CodecError> {
    record.kind("server_hello");
    record.field("protocol", message.negotiated)?;
    record.field("granted_role", role(message.granted_role))?;
    record.field_string("permissions", string_list(&message.permissions)?)?;
    record.field_str("capabilities", &message.capabilities_digest)?;
    encode_identity(record, &message.engine)?;
    record.field("current_revision", message.current_revision)?;
    record.field("resume", u8::from(message.resume))
}

fn encode_command(record: &mut Record, message: &CommandMessage) -> Result<(), CodecError> {
    record.kind("command");
    record.field("protocol", message.protocol)?;
    record.field_str("id", &message.id)?;
    record.field_str("idempotency_key", &message.idempotency_key)?;
    record.optional("expected_revision", message.expected_revision)?;
    record.optional("deadline_ms", message.deadline_ms)?;
    match message.payload {
        CommandPayload::SelectPreview { input } => {
            record.field("payload", "select_preview")?;
            record.field("input", input)?;
        }
        CommandPayload::Cut => record.field("payload", "cut")?,
        CommandPayload::Fade { duration_frames } => {
            record.field("payload", "fade")?;
            record.field("duration_frames", duration_frames)?;
        }
        CommandPayload::Wipe { duration_frames } => {
            record.field("payload", "wipe")?;
            record.field("duration_frames", duration_frames)?;
        }
        CommandPayload::FadeToBlack {
            active,
            duration_frames,
        } => {
            record.field("payload", "fade_to_black")?;
            record.field("active", u8::from(active))?;
            record.field("duration_frames", duration_frames)?;
        }
        CommandPayload::StartManualTransition { kind } => {
            record.field("payload", "manual_start")?;
            record.field(
                "transition",
                match kind {
                    crate::ManualTransitionKind::Fade => "fade",
                    crate::ManualTransitionKind::Wipe => "wipe",
                },
            )?;
        }
        CommandPayload::SetManualTransitionPosition { position } => {
            record.field("payload", "manual_position")?;
            record.field("position_basis_points", position.basis_points())?;
        }
        CommandPayload::CommitManualTransition => {
            record.field("payload", "manual_commit")?;
        }
        CommandPayload::CancelManualTransition => {
            record.field("payload", "manual_cancel")?;
        }
    }
    Ok(())
}

fn encode_result(record: &mut Record, message: &CommandResult) -> Result<(), CodecError> {
    record.kind("command_result");
    match message {
        CommandResult::Accepted {
            id,
            revision,
            scheduled_frame,
        } => {
            record.field("status", "accepted")?;
            record.field_str("id", id)?;
            record.field("revision", revision)?;
            record.optional("scheduled_frame", *scheduled_frame)?;
        }
        CommandResult::Rejected {
            id,
            code,
            message,
            fields,
            current_revision,
            retryable,
        } => {
            record.field("status", "rejected")?;
            record.field_str("id", id)?;
            record.field_str("code", code)?;
            record.field_str("message", message)?;
            record.field_string("fields", field_issues(fields)?)?;
            record.field("current_revision", current_revision)?;
            record.field("retryable", u8::from(*retryable))?;
        }
    }
    Ok(())
}

fn encode_snapshot(record: &mut Record, message: &SnapshotMessage) -> Result<(), CodecError> {
    record.kind("snapshot");
    encode_identity(record, &message.engine)?;
    record.field("revision", message.revision)?;
    record.field_str("show_name", &message.show_name)?;
    check_count(message.inputs.len(), "inputs")?;
    record.field_string(
        "inputs",
        message
            .inputs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
    )?;
    record.field("desired_program", message.desired_program)?;
    record.field("desired_preview", message.desired_preview)?;
    record.field("realized_program", message.realized_program)?;
    record.field("realized_preview", message.realized_preview)?;
    encode_manual_status(
        record,
        message.desired_manual_transition,
        ManualStatusFields::Desired,
    )?;
    encode_manual_status(
        record,
        message.realized_manual_transition,
        ManualStatusFields::Realized,
    )?;
    encode_fade_to_black_state(
        record,
        message.desired_fade_to_black,
        FadeToBlackStateFields::Desired,
    )?;
    encode_fade_to_black_state(
        record,
        message.realized_fade_to_black,
        FadeToBlackStateFields::Realized,
    )
}

fn encode_event(record: &mut Record, message: &EventMessage) -> Result<(), CodecError> {
    record.kind("event");
    encode_cursor(record, &message.cursor)?;
    match message.payload {
        EventPayload::DesiredSwitcher {
            program,
            preview,
            manual_transition,
            fade_to_black,
        } => {
            record.field("event", "desired_switcher")?;
            record.field("program", program)?;
            record.field("preview", preview)?;
            encode_manual_status(record, manual_transition, ManualStatusFields::Unqualified)?;
            encode_fade_to_black_state(record, fade_to_black, FadeToBlackStateFields::Unqualified)?;
        }
    }
    Ok(())
}

fn encode_server_identity(
    record: &mut Record,
    identity: &ServerIdentity,
) -> Result<(), CodecError> {
    record.field_str("engine_id", &identity.engine_id)?;
    record.field_str("project_id", &identity.project_id)?;
    record.field("state_epoch", identity.state_epoch)?;
    record.field_str("log_id", &identity.log_id)
}

fn encode_resume_cursor(
    record: &mut Record,
    cursor: &ResumeCursor,
    prefix: &'static str,
) -> Result<(), CodecError> {
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
    record.field_str(engine, &cursor.server.engine_id)?;
    record.field_str(project, &cursor.server.project_id)?;
    record.field(epoch, cursor.server.state_epoch)?;
    record.field_str(log, &cursor.server.log_id)?;
    record.field(revision, cursor.revision)
}

fn encode_handshake_request(
    record: &mut Record,
    message: &HandshakeRequest,
) -> Result<(), CodecError> {
    record.kind("handshake_request");
    check_count(message.versions.len(), "versions")?;
    record.field("versions", versions(&message.versions))?;
    record.field_str("build", &message.build)?;
    record.field("client_type", client_type(message.client_type))?;
    record.field("desired_role", role(message.desired_role))?;
    record.field("resume", u8::from(message.resume_cursor.is_some()))?;
    if let Some(cursor) = &message.resume_cursor {
        encode_resume_cursor(record, cursor, "resume")?;
    }
    Ok(())
}

fn encode_handshake_response(
    record: &mut Record,
    message: &HandshakeResponse,
) -> Result<(), CodecError> {
    record.kind("handshake_response");
    record.field("protocol", message.negotiated)?;
    record.field("granted_role", role(message.granted_role))?;
    record.field_string("permissions", string_list(&message.permissions)?)?;
    encode_capability_summary(record, &message.capabilities)?;
    encode_server_identity(record, &message.server)?;
    record.field("current_revision", message.current_revision)?;
    match &message.outcome {
        HandshakeOutcome::Snapshot { reason } => {
            record.field("outcome", "snapshot")?;
            record.field("snapshot_reason", snapshot_reason(*reason))?;
        }
        HandshakeOutcome::Resume { cursor } => {
            if cursor.server != message.server || cursor.revision > message.current_revision {
                return Err(CodecError::InvalidField {
                    field: "resume_revision",
                    value: cursor.revision.to_string(),
                });
            }
            record.field("outcome", "resume")?;
            encode_resume_cursor(record, cursor, "resume")?;
        }
        HandshakeOutcome::Rejected { error } => {
            record.field("outcome", "rejected")?;
            encode_structured_error(record, error)?;
        }
    }
    Ok(())
}

fn encode_durable_batch(
    record: &mut Record,
    message: &DurableEventBatch,
) -> Result<(), CodecError> {
    record.kind("durable_event_batch");
    if message.events.is_empty()
        || message
            .events
            .iter()
            .enumerate()
            .any(|(expected, event)| usize::from(event.sequence) != expected)
    {
        return Err(CodecError::InvalidField {
            field: "events",
            value: "event sequences must be contiguous from zero".to_owned(),
        });
    }
    encode_server_identity(record, &message.cursor.server)?;
    record.field("revision", message.cursor.revision)?;
    record.field_string("events", durable_events(&message.events)?)
}

fn encode_durable_gap(record: &mut Record, message: &DurableGap) -> Result<(), CodecError> {
    record.kind("durable_gap");
    if message.requested_after_revision.saturating_add(1) >= message.available_from_revision
        || message.available_from_revision > message.current_revision
    {
        return Err(CodecError::InvalidField {
            field: "available_from_revision",
            value: message.available_from_revision.to_string(),
        });
    }
    encode_server_identity(record, &message.server)?;
    record.field("requested_after_revision", message.requested_after_revision)?;
    record.field("available_from_revision", message.available_from_revision)?;
    record.field("current_revision", message.current_revision)
}

fn encode_runtime_event(
    record: &mut Record,
    message: &RuntimeEventMessage,
) -> Result<(), CodecError> {
    record.kind("runtime_event");
    encode_server_identity(record, &message.server)?;
    record.field("revision", message.revision)?;
    record.field("generation", message.generation)?;
    record.field("sequence", message.sequence)?;
    match &message.event {
        RuntimeLifecycleEvent::Accepted => record.field("event", "accepted")?,
        RuntimeLifecycleEvent::Preparing => record.field("event", "preparing")?,
        RuntimeLifecycleEvent::Scheduled { domains } => {
            record.field("event", "scheduled")?;
            record.field_string("domains", runtime_domains(domains)?)?;
        }
        RuntimeLifecycleEvent::Realized {
            domain,
            manual_transition,
            fade_to_black,
        } => {
            record.field("event", "realized")?;
            record.field_str("domain", domain)?;
            encode_manual_status(record, *manual_transition, ManualStatusFields::Unqualified)?;
            encode_fade_to_black_state(
                record,
                *fade_to_black,
                FadeToBlackStateFields::Unqualified,
            )?;
        }
        RuntimeLifecycleEvent::Failed { error, disposition } => {
            record.field("event", "failed")?;
            record.field("disposition", failure_disposition(*disposition))?;
            encode_structured_error(record, error)?;
        }
        RuntimeLifecycleEvent::Superseded { by_revision } => {
            record.field("event", "superseded")?;
            record.field("by_revision", by_revision)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ManualStatusFields {
    Desired,
    Realized,
    Unqualified,
}

fn encode_manual_status(
    record: &mut Record,
    status: Option<ManualTransitionStatus>,
    fields: ManualStatusFields,
) -> Result<(), CodecError> {
    let Some(status) = status else {
        return Ok(());
    };
    let (active, kind, from, to, interval_start, position) = match fields {
        ManualStatusFields::Desired => (
            "?desired_manual_active",
            "?desired_manual_kind",
            "?desired_manual_from",
            "?desired_manual_to",
            "?desired_manual_interval_start_basis_points",
            "?desired_manual_position_basis_points",
        ),
        ManualStatusFields::Realized => (
            "?realized_manual_active",
            "?realized_manual_kind",
            "?realized_manual_from",
            "?realized_manual_to",
            "?realized_manual_interval_start_basis_points",
            "?realized_manual_position_basis_points",
        ),
        ManualStatusFields::Unqualified => (
            "?manual_active",
            "?manual_kind",
            "?manual_from",
            "?manual_to",
            "?manual_interval_start_basis_points",
            "?manual_position_basis_points",
        ),
    };
    match status {
        ManualTransitionStatus::Inactive => record.field(active, 0),
        ManualTransitionStatus::Active(state) => {
            record.field(active, 1)?;
            record.field(
                kind,
                match state.kind {
                    ManualTransitionKind::Fade => "fade",
                    ManualTransitionKind::Wipe => "wipe",
                },
            )?;
            record.field(from, state.from)?;
            record.field(to, state.to)?;
            record.field(interval_start, state.interval_start.basis_points())?;
            record.field(position, state.position.basis_points())
        }
    }
}

#[derive(Clone, Copy)]
enum FadeToBlackStateFields {
    Desired,
    Realized,
    Unqualified,
}

fn encode_fade_to_black_state(
    record: &mut Record,
    state: Option<FadeToBlackState>,
    fields: FadeToBlackStateFields,
) -> Result<(), CodecError> {
    let Some(state) = state else {
        return Ok(());
    };
    let (target_active, position) = match fields {
        FadeToBlackStateFields::Desired => (
            "?desired_ftb_target_active",
            "?desired_ftb_position_numerator",
        ),
        FadeToBlackStateFields::Realized => (
            "?realized_ftb_target_active",
            "?realized_ftb_position_numerator",
        ),
        FadeToBlackStateFields::Unqualified => ("?ftb_target_active", "?ftb_position_numerator"),
    };
    record.field(target_active, u8::from(state.target_active))?;
    record.field(position, state.position.numerator())
}

fn encode_heartbeat(record: &mut Record, message: &HeartbeatMessage) -> Result<(), CodecError> {
    record.kind("heartbeat");
    encode_server_identity(record, &message.server)?;
    record.field("sequence", message.sequence)?;
    record.field("sent_at_ms", message.sent_at_ms)?;
    record.field("applied", u8::from(message.last_applied.is_some()))?;
    if let Some(cursor) = &message.last_applied {
        if cursor.server != message.server {
            return Err(CodecError::InvalidField {
                field: "applied_engine_id",
                value: cursor.server.engine_id.clone(),
            });
        }
        encode_resume_cursor(record, cursor, "applied")?;
    }
    Ok(())
}

fn encode_capability_report(
    record: &mut Record,
    message: &CapabilityReportMessage,
) -> Result<(), CodecError> {
    record.kind("capability_report");
    encode_server_identity(record, &message.server)?;
    record.field("revision", message.revision)?;
    encode_capability_summary(record, &message.summary)
}

fn encode_capability_summary(
    record: &mut Record,
    summary: &CapabilityReportSummary,
) -> Result<(), CodecError> {
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
    record.field_str("capability_digest", &summary.digest)?;
    record.field("capability_total", summary.total)?;
    record.field("capability_available", summary.available)?;
    record.field("capability_degraded", summary.degraded)?;
    record.field("capability_unavailable", summary.unavailable)
}

fn encode_error_message(record: &mut Record, message: &ErrorMessage) -> Result<(), CodecError> {
    record.kind("error");
    if let Some(request_id) = &message.request_id {
        record.field_str("request_id", request_id)?;
    }
    record.optional("current_revision", message.current_revision)?;
    encode_structured_error(record, &message.error)
}

fn encode_structured_error(record: &mut Record, error: &StructuredError) -> Result<(), CodecError> {
    if error.code.is_empty()
        || error.code.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.'))
        })
    {
        return Err(CodecError::InvalidField {
            field: "code",
            value: error.code.clone(),
        });
    }
    record.field_str("code", &error.code)?;
    record.field_str("message", &error.message)?;
    record.field_string("fields", field_issues(&error.fields)?)?;
    record.field("retryable", u8::from(error.retryable))
}

const fn snapshot_reason(reason: SnapshotReason) -> &'static str {
    match reason {
        SnapshotReason::NoCursor => "no_cursor",
        SnapshotReason::IdentityChanged => "identity_changed",
        SnapshotReason::CursorAhead => "cursor_ahead",
        SnapshotReason::HistoryUnavailable => "history_unavailable",
    }
}

const fn failure_disposition(disposition: RuntimeFailureDisposition) -> &'static str {
    match disposition {
        RuntimeFailureDisposition::RolledBack => "rolled_back",
        RuntimeFailureDisposition::RetainedForRetry => "retained_for_retry",
        RuntimeFailureDisposition::FallbackRealized => "fallback_realized",
    }
}

fn check_count(count: usize, field: &'static str) -> Result<(), CodecError> {
    if count > MAX_LIST_ITEMS {
        Err(CodecError::TooManyItems(field))
    } else {
        Ok(())
    }
}
