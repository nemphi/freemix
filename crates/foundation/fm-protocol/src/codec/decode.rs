use core::{num::NonZeroU128, str::FromStr};
use std::collections::BTreeMap;

use crate::{
    CapabilityReportMessage, CapabilityReportSummary, CodecError, CommandMessage, CommandPayload,
    CommandResult, DurableEventBatch, DurableGap, EngineIdentity, ErrorMessage, EventCursor,
    EventMessage, EventPayload, FadeToBlackPosition, FadeToBlackState, HandshakeOutcome,
    HandshakeRequest, HandshakeResponse, HeartbeatMessage, ManualTransitionKind,
    ManualTransitionPosition, ManualTransitionState, ManualTransitionStatus, OverlayStatus,
    ResumeCursor, RuntimeEventMessage, RuntimeFailureDisposition, RuntimeLifecycleEvent,
    ServerIdentity, SnapshotMessage, SnapshotReason, StingerAudioPolicy,
    StingerMissingMediaFallback, StingerReadiness, StingerStatus, StructuredError, WireInputId,
    WireMessage, WireOutputId, WireOverlayChannelId, WireStingerSlotId,
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
            .next()
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

fn decode_command(fields: &mut Fields) -> Result<CommandMessage, CodecError> {
    let value = fields.required("protocol")?;
    let protocol = parse_version(&value).ok_or(CodecError::InvalidField {
        field: "protocol",
        value,
    })?;
    let payload_name = fields.required("payload")?;
    let payload = decode_command_payload(fields, payload_name)?;
    Ok(CommandMessage {
        protocol,
        id: fields.required("id")?,
        idempotency_key: fields.required("idempotency_key")?,
        expected_revision: fields.parse_optional("expected_revision")?,
        deadline_ms: fields.parse_optional("deadline_ms")?,
        payload,
    })
}

fn decode_command_payload(
    fields: &mut Fields,
    payload_name: String,
) -> Result<CommandPayload, CodecError> {
    Ok(match payload_name.as_str() {
        "select_preview" => CommandPayload::SelectPreview {
            input: fields.input("input")?,
        },
        "cut" => CommandPayload::Cut,
        "fade" => CommandPayload::Fade {
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "alpha_fade" => CommandPayload::AlphaFade {
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "slide" => CommandPayload::Slide {
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "zoom" => CommandPayload::Zoom {
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "stinger" => {
            let number: u8 = fields.parse_required("slot")?;
            let slot = crate::WireStingerSlotId::new(number).ok_or(CodecError::InvalidField {
                field: "slot",
                value: number.to_string(),
            })?;
            CommandPayload::Stinger {
                slot,
                duration_frames: fields.parse_required("duration_frames")?,
            }
        }
        "configure_stinger" => decode_configure_stinger(fields)?,
        "remove_stinger" => CommandPayload::RemoveStinger {
            slot: decode_stinger_slot(fields)?,
        },
        overlay @ ("overlay_take" | "overlay_update" | "overlay_off" | "overlay_output"
        | "overlay_transition" | "overlay_appearance" | "overlay_queue"
        | "overlay_next") => decode_overlay_command(fields, overlay)?,
        "wipe" => CommandPayload::Wipe {
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "fade_to_black" => CommandPayload::FadeToBlack {
            active: fields.boolean("active")?,
            duration_frames: fields.parse_required("duration_frames")?,
        },
        "manual_start" => {
            let value = fields.required("transition")?;
            let kind = match value.as_str() {
                "fade" => crate::ManualTransitionKind::Fade,
                "wipe" => crate::ManualTransitionKind::Wipe,
                "alpha_fade" => crate::ManualTransitionKind::AlphaFade,
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
    })
}

fn decode_overlay_command(fields: &mut Fields, name: &str) -> Result<CommandPayload, CodecError> {
    let channel = decode_overlay_channel(fields)?;
    Ok(match name {
        "overlay_take" => CommandPayload::TakeOverlay {
            channel,
            source: fields.input("source")?,
        },
        "overlay_update" => CommandPayload::UpdateOverlay {
            channel,
            source: fields.input("source")?,
        },
        "overlay_off" => CommandPayload::OverlayOff { channel },
        "overlay_output" => CommandPayload::SetOverlayOutputInclusion {
            channel,
            output: decode_output(fields, "output")?,
            included: fields.boolean("included")?,
        },
        "overlay_transition" => {
            let transition =
                decode_overlay_transition(fields.required("transition")?, "transition")?;
            let duration_frames = fields.parse_required("duration_frames")?;
            validate_overlay_duration(duration_frames, "duration_frames")?;
            CommandPayload::ConfigureOverlayTransition {
                channel,
                transition,
                duration_frames,
            }
        }
        "overlay_appearance" => CommandPayload::ConfigureOverlayAppearance {
            channel,
            position: decode_overlay_position(fields.required("position")?, "position")?,
            border: decode_overlay_border(fields.required("border")?, "border")?,
        },
        "overlay_queue" => CommandPayload::QueueOverlay {
            channel,
            source: fields.input("source")?,
        },
        "overlay_next" => CommandPayload::TakeNextOverlay { channel },
        _ => unreachable!("only overlay command names are delegated"),
    })
}

fn decode_overlay_channel(fields: &mut Fields) -> Result<WireOverlayChannelId, CodecError> {
    let number: u8 = fields.parse_required("channel")?;
    WireOverlayChannelId::new(number).ok_or(CodecError::InvalidField {
        field: "channel",
        value: number.to_string(),
    })
}

fn decode_overlay_transition(
    value: String,
    field: &'static str,
) -> Result<crate::OverlayTransitionKind, CodecError> {
    match value.as_str() {
        "cut" => Ok(crate::OverlayTransitionKind::Cut),
        "fade" => Ok(crate::OverlayTransitionKind::Fade),
        _ => Err(CodecError::InvalidField { field, value }),
    }
}

fn decode_overlay_position(
    value: String,
    field: &'static str,
) -> Result<crate::OverlayPositionPreset, CodecError> {
    match value.as_str() {
        "full_frame" => Ok(crate::OverlayPositionPreset::FullFrame),
        "top_left" => Ok(crate::OverlayPositionPreset::TopLeft),
        "top_right" => Ok(crate::OverlayPositionPreset::TopRight),
        "bottom_left" => Ok(crate::OverlayPositionPreset::BottomLeft),
        "bottom_right" => Ok(crate::OverlayPositionPreset::BottomRight),
        _ => Err(CodecError::InvalidField { field, value }),
    }
}

fn decode_overlay_border(
    value: String,
    field: &'static str,
) -> Result<crate::OverlayBorderPreset, CodecError> {
    match value.as_str() {
        "none" => Ok(crate::OverlayBorderPreset::None),
        "thin_white" => Ok(crate::OverlayBorderPreset::ThinWhite),
        "thick_white" => Ok(crate::OverlayBorderPreset::ThickWhite),
        _ => Err(CodecError::InvalidField { field, value }),
    }
}

fn validate_overlay_duration(duration_frames: u32, field: &'static str) -> Result<(), CodecError> {
    if (1..=3_600).contains(&duration_frames) {
        Ok(())
    } else {
        Err(CodecError::InvalidField {
            field,
            value: duration_frames.to_string(),
        })
    }
}

fn decode_output(fields: &mut Fields, field: &'static str) -> Result<WireOutputId, CodecError> {
    let value = fields.required(field)?;
    let id = NonZeroU128::new(value.parse().map_err(|_| CodecError::InvalidField {
        field,
        value: value.clone(),
    })?)
    .ok_or(CodecError::InvalidField { field, value })?;
    Ok(WireOutputId::new(id))
}

fn decode_configure_stinger(fields: &mut Fields) -> Result<CommandPayload, CodecError> {
    let audio_policy = fields.required("audio_policy")?;
    let missing_media_fallback = fields.required("missing_media_fallback")?;
    Ok(CommandPayload::ConfigureStinger {
        slot: decode_stinger_slot(fields)?,
        media_input: fields.input("media_input")?,
        preload: fields.boolean("preload")?,
        cut_point_frames: fields.parse_required("cut_point_frames")?,
        audio_policy: match audio_policy.as_str() {
            "muted" => StingerAudioPolicy::Muted,
            "stinger_only" => StingerAudioPolicy::StingerOnly,
            "mix_with_program" => StingerAudioPolicy::MixWithProgram,
            _ => {
                return Err(CodecError::InvalidField {
                    field: "audio_policy",
                    value: audio_policy,
                });
            }
        },
        missing_media_fallback: match missing_media_fallback.as_str() {
            "cut" => StingerMissingMediaFallback::Cut,
            "fade" => StingerMissingMediaFallback::Fade,
            "keep_program" => StingerMissingMediaFallback::KeepProgram,
            _ => {
                return Err(CodecError::InvalidField {
                    field: "missing_media_fallback",
                    value: missing_media_fallback,
                });
            }
        },
    })
}

fn decode_stinger_slot(fields: &mut Fields) -> Result<WireStingerSlotId, CodecError> {
    let number: u8 = fields.parse_required("slot")?;
    WireStingerSlotId::new(number).ok_or(CodecError::InvalidField {
        field: "slot",
        value: number.to_string(),
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
        desired_manual_transition: decode_manual_status(fields, ManualStatusFields::Desired)?,
        realized_manual_transition: decode_manual_status(fields, ManualStatusFields::Realized)?,
        desired_fade_to_black: decode_fade_to_black_state(fields, FadeToBlackStateFields::Desired)?,
        realized_fade_to_black: decode_fade_to_black_state(
            fields,
            FadeToBlackStateFields::Realized,
        )?,
        stingers: decode_stingers(fields)?,
        desired_overlays: decode_overlays(fields, "desired_overlays")?,
        realized_overlays: decode_overlays(fields, "realized_overlays")?,
    })
}

fn decode_overlays(
    fields: &mut Fields,
    field: &'static str,
) -> Result<Vec<OverlayStatus>, CodecError> {
    let value = fields.required(field)?;
    let entries = value.split(';').collect::<Vec<_>>();
    if entries.len() != 8 {
        return Err(CodecError::InvalidField { field, value });
    }
    let mut seen = [false; 8];
    entries
        .into_iter()
        .map(|entry| decode_overlay_status(entry, field, &mut seen))
        .collect()
}

fn decode_overlay_status(
    entry: &str,
    field: &'static str,
    seen: &mut [bool; 8],
) -> Result<OverlayStatus, CodecError> {
    let parts = entry.split(':').collect::<Vec<_>>();
    let [
        channel,
        source,
        active,
        opacity,
        transition,
        duration_frames,
        position,
        border,
        queue,
        outputs,
    ] = parts.as_slice()
    else {
        return Err(CodecError::InvalidField {
            field,
            value: entry.to_owned(),
        });
    };
    let channel_number: u8 = channel.parse().map_err(|_| CodecError::InvalidField {
        field,
        value: entry.to_owned(),
    })?;
    let channel =
        WireOverlayChannelId::new(channel_number).ok_or_else(|| CodecError::InvalidField {
            field,
            value: entry.to_owned(),
        })?;
    let index = usize::from(channel_number - 1);
    if seen[index] {
        return Err(CodecError::InvalidField {
            field,
            value: entry.to_owned(),
        });
    }
    seen[index] = true;
    let has_source = *source != "0";
    let source = if has_source {
        parse_input(source)
    } else {
        None
    };
    if has_source && source.is_none() {
        return Err(CodecError::InvalidField {
            field,
            value: entry.to_owned(),
        });
    }
    let active = match *active {
        "0" => false,
        "1" => true,
        _ => {
            return Err(CodecError::InvalidField {
                field,
                value: entry.to_owned(),
            });
        }
    };
    let opacity = opacity.parse().map_err(|_| CodecError::InvalidField {
        field,
        value: entry.to_owned(),
    })?;
    let transition = decode_overlay_transition((*transition).to_owned(), field)?;
    let duration_frames = duration_frames
        .parse()
        .map_err(|_| CodecError::InvalidField {
            field,
            value: entry.to_owned(),
        })?;
    validate_overlay_duration(duration_frames, field)?;
    let position = decode_overlay_position((*position).to_owned(), field)?;
    let border = decode_overlay_border((*border).to_owned(), field)?;
    let queued_sources = decode_overlay_queue(queue, entry, field)?;
    let included_outputs = if outputs.is_empty() {
        Vec::new()
    } else {
        outputs
            .split(',')
            .map(|output| NonZeroU128::new(output.parse().ok()?).map(WireOutputId::new))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| CodecError::InvalidField {
                field,
                value: entry.to_owned(),
            })?
    };
    Ok(OverlayStatus {
        channel,
        source,
        active,
        opacity,
        transition,
        duration_frames,
        position,
        border,
        queued_sources,
        included_outputs,
    })
}

fn decode_overlay_queue(
    value: &str,
    entry: &str,
    field: &'static str,
) -> Result<Vec<WireInputId>, CodecError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let values = value
        .split(',')
        .map(parse_input)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CodecError::InvalidField {
            field,
            value: entry.to_owned(),
        })?;
    if values.len() > 64 {
        return Err(CodecError::TooManyItems("overlay queue"));
    }
    Ok(values)
}

fn decode_stingers(fields: &mut Fields) -> Result<Vec<StingerStatus>, CodecError> {
    let value = fields.required("?stingers")?;
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let entries: Vec<_> = value.split(',').collect();
    if entries.len() > 8 {
        return Err(CodecError::TooManyItems("stingers"));
    }
    let mut seen = [false; 8];
    let mut stingers = Vec::with_capacity(entries.len());
    for entry in entries {
        let status = decode_stinger_status(entry).ok_or_else(|| invalid_stingers(&value))?;
        let slot_index = usize::from(status.slot.number() - 1);
        if seen[slot_index] {
            return Err(invalid_stingers(&value));
        }
        seen[slot_index] = true;
        stingers.push(status);
    }
    Ok(stingers)
}

fn decode_stinger_status(entry: &str) -> Option<StingerStatus> {
    let parts: Vec<_> = entry.split(':').collect();
    let [
        slot,
        media_input,
        preload,
        cut_point_frames,
        audio_policy,
        fallback,
        readiness,
    ] = parts.as_slice()
    else {
        return None;
    };
    Some(StingerStatus {
        slot: slot.parse().ok().and_then(WireStingerSlotId::new)?,
        media_input: parse_input(media_input)?,
        preload: match *preload {
            "0" => false,
            "1" => true,
            _ => return None,
        },
        cut_point_frames: cut_point_frames.parse().ok()?,
        audio_policy: match *audio_policy {
            "muted" => StingerAudioPolicy::Muted,
            "stinger_only" => StingerAudioPolicy::StingerOnly,
            "mix_with_program" => StingerAudioPolicy::MixWithProgram,
            _ => return None,
        },
        missing_media_fallback: match *fallback {
            "cut" => StingerMissingMediaFallback::Cut,
            "fade" => StingerMissingMediaFallback::Fade,
            "keep_program" => StingerMissingMediaFallback::KeepProgram,
            _ => return None,
        },
        readiness: match *readiness {
            "not_requested" => StingerReadiness::NotRequested,
            "ready" => StingerReadiness::Ready,
            "missing" => StingerReadiness::Missing,
            _ => return None,
        },
    })
}

fn invalid_stingers(value: &str) -> CodecError {
    CodecError::InvalidField {
        field: "?stingers",
        value: value.to_owned(),
    }
}

fn decode_event(fields: &mut Fields) -> Result<EventMessage, CodecError> {
    let event = fields.required("event")?;
    let payload = match event.as_str() {
        "desired_switcher" => EventPayload::DesiredSwitcher {
            program: fields.input("program")?,
            preview: fields.input("preview")?,
            manual_transition: decode_manual_status(fields, ManualStatusFields::Unqualified)?,
            fade_to_black: decode_fade_to_black_state(fields, FadeToBlackStateFields::Unqualified)?,
            overlays: decode_overlays(fields, "overlays")?,
        },
        "stinger_slots_changed" => EventPayload::StingerSlotsChanged {
            program: fields.input("program")?,
            preview: fields.input("preview")?,
            manual_transition: decode_manual_status(fields, ManualStatusFields::Unqualified)?,
            fade_to_black: decode_fade_to_black_state(fields, FadeToBlackStateFields::Unqualified)?,
            stingers: decode_stingers(fields)?,
            overlays: decode_overlays(fields, "overlays")?,
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
            manual_transition: decode_manual_status(fields, ManualStatusFields::Unqualified)?,
            fade_to_black: decode_fade_to_black_state(fields, FadeToBlackStateFields::Unqualified)?,
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

#[derive(Clone, Copy)]
enum ManualStatusFields {
    Desired,
    Realized,
    Unqualified,
}

fn decode_manual_status(
    fields: &mut Fields,
    names: ManualStatusFields,
) -> Result<ManualTransitionStatus, CodecError> {
    let (active, kind, from, to, interval_start, position) = match names {
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
    let active_value = fields.boolean(active)?;
    if !active_value {
        return Ok(ManualTransitionStatus::Inactive);
    }
    let kind_value = fields.required(kind)?;
    let kind = match kind_value.as_str() {
        "fade" => ManualTransitionKind::Fade,
        "wipe" => ManualTransitionKind::Wipe,
        "alpha_fade" => ManualTransitionKind::AlphaFade,
        _ => {
            return Err(CodecError::InvalidField {
                field: kind,
                value: kind_value,
            });
        }
    };
    let from_value = fields.required(from)?;
    let from_input = parse_input(&from_value).ok_or(CodecError::InvalidField {
        field: from,
        value: from_value,
    })?;
    let to_value = fields.required(to)?;
    let to_input = parse_input(&to_value).ok_or(CodecError::InvalidField {
        field: to,
        value: to_value,
    })?;
    let interval_value: u16 = fields.parse_required(interval_start)?;
    let interval_start_position =
        ManualTransitionPosition::new(interval_value).ok_or(CodecError::InvalidField {
            field: interval_start,
            value: interval_value.to_string(),
        })?;
    let position_value: u16 = fields.parse_required(position)?;
    let exact_position =
        ManualTransitionPosition::new(position_value).ok_or(CodecError::InvalidField {
            field: position,
            value: position_value.to_string(),
        })?;
    Ok(ManualTransitionStatus::Active(ManualTransitionState {
        kind,
        from: from_input,
        to: to_input,
        interval_start: interval_start_position,
        position: exact_position,
    }))
}

#[derive(Clone, Copy)]
enum FadeToBlackStateFields {
    Desired,
    Realized,
    Unqualified,
}

fn decode_fade_to_black_state(
    fields: &mut Fields,
    names: FadeToBlackStateFields,
) -> Result<FadeToBlackState, CodecError> {
    let (target_active, position) = match names {
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
    Ok(FadeToBlackState {
        target_active: fields.boolean(target_active)?,
        position: FadeToBlackPosition::new(fields.parse_required(position)?),
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
