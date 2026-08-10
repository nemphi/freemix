use crate::{
    ClientType, CodecError, DurableEvent, FieldIssue, InputStatus, ProtocolVersion, Role,
    RuntimeDomainBoundary,
};

use super::{MAX_BATCH_EVENTS, MAX_FIELD_VALUE_BYTES, MAX_LIST_ITEMS};

pub(super) fn unescape(value: &str) -> Result<String, CodecError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(hex) = bytes.get(index + 1..index + 3) else {
                return Err(CodecError::InvalidEscape);
            };
            let high = hex_value(hex[0]).ok_or(CodecError::InvalidEscape)?;
            let low = hex_value(hex[1]).ok_or(CodecError::InvalidEscape)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| CodecError::InvalidUtf8)
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'A'..=b'F' => Some(value - b'A' + 10),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

pub(super) const fn client_type(value: ClientType) -> &'static str {
    match value {
        ClientType::Studio => "studio",
        ClientType::Web => "web",
        ClientType::Cli => "cli",
        ClientType::Integration => "integration",
    }
}

pub(super) fn parse_client_type(value: &str) -> Option<ClientType> {
    match value {
        "studio" => Some(ClientType::Studio),
        "web" => Some(ClientType::Web),
        "cli" => Some(ClientType::Cli),
        "integration" => Some(ClientType::Integration),
        _ => None,
    }
}

pub(super) const fn role(value: Role) -> &'static str {
    match value {
        Role::Viewer => "viewer",
        Role::Graphics => "graphics",
        Role::Audio => "audio",
        Role::Replay => "replay",
        Role::Operator => "operator",
        Role::Admin => "admin",
    }
}

pub(super) fn parse_role(value: &str) -> Option<Role> {
    match value {
        "viewer" => Some(Role::Viewer),
        "graphics" => Some(Role::Graphics),
        "audio" => Some(Role::Audio),
        "replay" => Some(Role::Replay),
        "operator" => Some(Role::Operator),
        "admin" => Some(Role::Admin),
        _ => None,
    }
}

pub(super) fn parse_version(value: &str) -> Option<ProtocolVersion> {
    let (major, minor) = value.split_once('.')?;
    Some(ProtocolVersion::new(
        major.parse().ok()?,
        minor.parse().ok()?,
    ))
}

pub(super) fn string_list(values: &[String]) -> Result<String, CodecError> {
    bounded_join(values, MAX_LIST_ITEMS, "permissions", |value| {
        escape_bounded(value)
    })
}

pub(super) fn parse_string_list(value: &str) -> Result<Vec<String>, CodecError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    check_items(value, ',', MAX_LIST_ITEMS, "permissions")?;
    value.split(',').map(unescape).collect()
}

pub(super) fn input_statuses(values: &[InputStatus]) -> Result<String, CodecError> {
    bounded_join(values, MAX_LIST_ITEMS, "inputs", |input| {
        if input.name.trim().is_empty() {
            return Err(CodecError::InvalidField {
                field: "inputs",
                value: input.name.clone(),
            });
        }
        Ok(format!("{}~{}", input.input, escape_bounded(&input.name)?))
    })
}

pub(super) fn parse_input_statuses(value: &str) -> Result<Vec<InputStatus>, CodecError> {
    if value.is_empty() {
        return Err(CodecError::InvalidField {
            field: "inputs",
            value: value.to_owned(),
        });
    }
    check_items(value, ',', MAX_LIST_ITEMS, "inputs")?;
    value
        .split(',')
        .map(|entry| {
            let (input, name) = entry.split_once('~').ok_or(CodecError::InvalidRecord)?;
            let input = input
                .parse::<u128>()
                .ok()
                .and_then(core::num::NonZeroU128::new);
            let name = unescape(name)?;
            if name.trim().is_empty() {
                return Err(CodecError::InvalidField {
                    field: "inputs",
                    value: value.to_owned(),
                });
            }
            Ok(InputStatus {
                input: crate::WireInputId::new(input.ok_or_else(|| CodecError::InvalidField {
                    field: "inputs",
                    value: value.to_owned(),
                })?),
                name,
            })
        })
        .collect()
}

pub(super) fn field_issues(values: &[FieldIssue]) -> Result<String, CodecError> {
    bounded_join(values, MAX_LIST_ITEMS, "fields", |issue| {
        Ok(format!(
            "{}~{}~{}",
            escape_bounded(&issue.field)?,
            escape_bounded(&issue.code)?,
            escape_bounded(&issue.message)?
        ))
    })
}

pub(super) fn parse_field_issues(value: &str) -> Result<Vec<FieldIssue>, CodecError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    check_items(value, ',', MAX_LIST_ITEMS, "fields")?;
    value
        .split(',')
        .map(|entry| {
            let mut parts = entry.split('~');
            let field = parts.next().ok_or(CodecError::InvalidRecord)?;
            let code = parts.next().ok_or(CodecError::InvalidRecord)?;
            let message = parts.next().ok_or(CodecError::InvalidRecord)?;
            if parts.next().is_some() {
                return Err(CodecError::InvalidRecord);
            }
            Ok(FieldIssue {
                field: unescape(field)?,
                code: unescape(code)?,
                message: unescape(message)?,
            })
        })
        .collect()
}

pub(super) fn durable_events(values: &[DurableEvent]) -> Result<String, CodecError> {
    bounded_join(values, MAX_BATCH_EVENTS, "events", |event| {
        Ok(format!(
            "{}~{}~{}",
            event.sequence,
            escape_bounded(&event.event_type)?,
            escape_bounded(&event.payload)?
        ))
    })
}

pub(super) fn parse_durable_events(value: &str) -> Result<Vec<DurableEvent>, CodecError> {
    if value.is_empty() {
        return Err(CodecError::InvalidField {
            field: "events",
            value: value.to_owned(),
        });
    }
    check_items(value, ',', MAX_BATCH_EVENTS, "events")?;
    value
        .split(',')
        .map(|entry| {
            let mut parts = entry.split('~');
            let sequence = parts
                .next()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| CodecError::InvalidField {
                    field: "events",
                    value: value.to_owned(),
                })?;
            let event_type = parts.next().ok_or(CodecError::InvalidRecord)?;
            let payload = parts.next().ok_or(CodecError::InvalidRecord)?;
            if parts.next().is_some() {
                return Err(CodecError::InvalidRecord);
            }
            Ok(DurableEvent {
                sequence,
                event_type: unescape(event_type)?,
                payload: unescape(payload)?,
            })
        })
        .collect()
}

pub(super) fn runtime_domains(values: &[RuntimeDomainBoundary]) -> Result<String, CodecError> {
    bounded_join(values, MAX_LIST_ITEMS, "domains", |entry| {
        Ok(format!(
            "{}~{}",
            escape_bounded(&entry.domain)?,
            entry.boundary
        ))
    })
}

pub(super) fn parse_runtime_domains(value: &str) -> Result<Vec<RuntimeDomainBoundary>, CodecError> {
    if value.is_empty() {
        return Err(CodecError::InvalidField {
            field: "domains",
            value: value.to_owned(),
        });
    }
    check_items(value, ',', MAX_LIST_ITEMS, "domains")?;
    value
        .split(',')
        .map(|entry| {
            let (domain, boundary) = entry.split_once('~').ok_or(CodecError::InvalidRecord)?;
            Ok(RuntimeDomainBoundary {
                domain: unescape(domain)?,
                boundary: boundary.parse().map_err(|_| CodecError::InvalidField {
                    field: "domains",
                    value: value.to_owned(),
                })?,
            })
        })
        .collect()
}

fn check_items(
    value: &str,
    separator: char,
    maximum: usize,
    field: &'static str,
) -> Result<(), CodecError> {
    if value.split(separator).take(maximum + 1).count() > maximum {
        Err(CodecError::TooManyItems(field))
    } else {
        Ok(())
    }
}

fn bounded_join<T>(
    values: &[T],
    maximum: usize,
    field: &'static str,
    encode: impl Fn(&T) -> Result<String, CodecError>,
) -> Result<String, CodecError> {
    if values.len() > maximum {
        return Err(CodecError::TooManyItems(field));
    }
    let mut output = String::new();
    for value in values {
        let encoded = encode(value)?;
        let separator = usize::from(!output.is_empty());
        if output.len() + separator + encoded.len() > MAX_FIELD_VALUE_BYTES {
            return Err(CodecError::FieldValueTooLong);
        }
        if separator != 0 {
            output.push(',');
        }
        output.push_str(&encoded);
    }
    Ok(output)
}

fn escape_bounded(value: &str) -> Result<String, CodecError> {
    let mut encoded = String::with_capacity(value.len().min(MAX_FIELD_VALUE_BYTES));
    for byte in value.bytes() {
        let additional = if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            1
        } else {
            3
        };
        if encoded.len() + additional > MAX_FIELD_VALUE_BYTES {
            return Err(CodecError::FieldValueTooLong);
        }
        if additional == 1 {
            encoded.push(char::from(byte));
        } else {
            use core::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    Ok(encoded)
}
