use crate::{CapabilityId, CommandEnvelope, CrashReport, Deadline, PluginId, StateSnapshot};
use core::fmt;
use std::collections::HashSet;

pub const HARD_MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub const HARD_MAX_BATCH_ITEMS: usize = 4096;
pub const HARD_MAX_CAPABILITIES: usize = 4096;
pub const HARD_MAX_IDENTIFIER_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolLimits {
    pub max_event_bytes: usize,
    pub max_data_bytes: usize,
    pub max_events_per_batch: usize,
    pub max_data_per_batch: usize,
    pub max_snapshot_bytes: usize,
    pub max_crash_report_bytes: usize,
    pub max_identifier_bytes: usize,
    pub max_capabilities: usize,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: 64 * 1024,
            max_data_bytes: 1024 * 1024,
            max_events_per_batch: 256,
            max_data_per_batch: 64,
            max_snapshot_bytes: 4 * 1024 * 1024,
            max_crash_report_bytes: 256 * 1024,
            max_identifier_bytes: 256,
            max_capabilities: 256,
        }
    }
}

impl ProtocolLimits {
    /// Validates that every configurable bound is finite and nonzero.
    ///
    /// # Errors
    ///
    /// Returns [`ExchangeError::InvalidLimits`] for zero or excessive limits.
    pub fn validate(self) -> Result<Self, ExchangeError> {
        let payloads = [
            self.max_event_bytes,
            self.max_data_bytes,
            self.max_snapshot_bytes,
            self.max_crash_report_bytes,
        ];
        if payloads
            .into_iter()
            .any(|limit| limit == 0 || limit > HARD_MAX_PAYLOAD_BYTES)
            || self.max_events_per_batch == 0
            || self.max_events_per_batch > HARD_MAX_BATCH_ITEMS
            || self.max_data_per_batch == 0
            || self.max_data_per_batch > HARD_MAX_BATCH_ITEMS
            || self.max_identifier_bytes == 0
            || self.max_identifier_bytes > HARD_MAX_IDENTIFIER_BYTES
            || self.max_capabilities == 0
            || self.max_capabilities > HARD_MAX_CAPABILITIES
        {
            return Err(ExchangeError::InvalidLimits);
        }
        Ok(self)
    }

    pub(crate) fn validate_identifier(
        &self,
        field: &'static str,
        value: &str,
    ) -> Result<(), ExchangeError> {
        if value.is_empty() {
            return Err(ExchangeError::EmptyIdentifier { field });
        }
        let maximum = self.max_identifier_bytes.min(HARD_MAX_IDENTIFIER_BYTES);
        if value.len() > maximum {
            return Err(ExchangeError::PayloadTooLarge {
                kind: field,
                actual: value.len(),
                maximum,
            });
        }
        Ok(())
    }

    pub(crate) const fn validate_payload(
        kind: &'static str,
        actual: usize,
        maximum: usize,
    ) -> Result<(), ExchangeError> {
        let maximum = if maximum < HARD_MAX_PAYLOAD_BYTES {
            maximum
        } else {
            HARD_MAX_PAYLOAD_BYTES
        };
        if actual > maximum {
            Err(ExchangeError::PayloadTooLarge {
                kind,
                actual,
                maximum,
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExchangeError {
    InvalidLimits,
    EmptyIdentifier {
        field: &'static str,
    },
    PayloadTooLarge {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
    BatchTooLarge {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
    DuplicateCapability,
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("protocol limits are zero or excessive"),
            Self::EmptyIdentifier { field } => write!(formatter, "{field} must not be empty"),
            Self::PayloadTooLarge {
                kind,
                actual,
                maximum,
            } => write!(
                formatter,
                "{kind} is {actual} bytes, exceeding the {maximum} byte limit"
            ),
            Self::BatchTooLarge {
                kind,
                actual,
                maximum,
            } => write!(
                formatter,
                "{kind} has {actual} items, exceeding the {maximum} item limit"
            ),
            Self::DuplicateCapability => formatter.write_str("capability appears more than once"),
        }
    }
}

impl std::error::Error for ExchangeError {}

macro_rules! payload_message {
    ($name:ident, $limit:ident, $kind:literal) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name {
            plugin_id: PluginId,
            channel: String,
            payload: Vec<u8>,
        }

        impl $name {
            /// Constructs a bounded exchange message.
            ///
            /// # Errors
            ///
            /// Returns [`ExchangeError`] when an identifier or payload exceeds
            /// the configured protocol limits.
            pub fn new(
                plugin_id: impl Into<PluginId>,
                channel: impl Into<String>,
                payload: impl Into<Vec<u8>>,
                limits: &ProtocolLimits,
            ) -> Result<Self, ExchangeError> {
                limits.validate()?;
                let plugin_id = plugin_id.into();
                limits.validate_identifier("plugin_id", plugin_id.as_str())?;
                let channel = channel.into();
                limits.validate_identifier("channel", &channel)?;
                let payload = payload.into();
                ProtocolLimits::validate_payload($kind, payload.len(), limits.$limit)?;
                Ok(Self {
                    plugin_id,
                    channel,
                    payload,
                })
            }

            #[must_use]
            pub const fn plugin_id(&self) -> &PluginId {
                &self.plugin_id
            }

            #[must_use]
            pub fn channel(&self) -> &str {
                &self.channel
            }

            #[must_use]
            pub fn payload(&self) -> &[u8] {
                &self.payload
            }
        }
    };
}

payload_message!(EventMessage, max_event_bytes, "event");
payload_message!(DataMessage, max_data_bytes, "data");

macro_rules! batch {
    (
        $name:ident,
        $item:ident,
        $limit:ident,
        $kind:literal,
        $payload_limit:ident,
        $payload_kind:literal
    ) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(Vec<$item>);

        impl $name {
            /// Constructs a batch whose item count is bounded.
            ///
            /// # Errors
            ///
            /// Returns [`ExchangeError::BatchTooLarge`] when the batch exceeds
            /// the configured item count.
            pub fn new(
                items: impl IntoIterator<Item = $item>,
                limits: &ProtocolLimits,
            ) -> Result<Self, ExchangeError> {
                limits.validate()?;
                let items: Vec<_> = items.into_iter().collect();
                if items.len() > limits.$limit {
                    return Err(ExchangeError::BatchTooLarge {
                        kind: $kind,
                        actual: items.len(),
                        maximum: limits.$limit,
                    });
                }
                for item in &items {
                    limits.validate_identifier("plugin_id", item.plugin_id.as_str())?;
                    limits.validate_identifier("channel", &item.channel)?;
                    ProtocolLimits::validate_payload(
                        $payload_kind,
                        item.payload.len(),
                        limits.$payload_limit,
                    )?;
                }
                Ok(Self(items))
            }

            #[must_use]
            pub fn items(&self) -> &[$item] {
                &self.0
            }

            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }
    };
}

batch!(
    EventBatch,
    EventMessage,
    max_events_per_batch,
    "event batch",
    max_event_bytes,
    "event"
);
batch!(
    DataBatch,
    DataMessage,
    max_data_per_batch,
    "data batch",
    max_data_bytes,
    "data"
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeartbeatMessage {
    pub sequence: u64,
    pub sent_at_millis: u64,
    pub reply_deadline: Deadline,
}

impl HeartbeatMessage {
    #[must_use]
    pub const fn new(sequence: u64, sent_at_millis: u64, reply_deadline: Deadline) -> Self {
        Self {
            sequence,
            sent_at_millis,
            reply_deadline,
        }
    }

    #[must_use]
    pub const fn deadline_exceeded_at(self, now_millis: u64) -> bool {
        self.reply_deadline.is_exceeded_at(now_millis)
    }
}

/// The only plugin-originated representation of an engine mutation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationIntent<C> {
    command: CommandEnvelope<C>,
}

impl<C> MutationIntent<C> {
    #[must_use]
    pub const fn new(command: CommandEnvelope<C>) -> Self {
        Self { command }
    }

    #[must_use]
    pub const fn command(&self) -> &CommandEnvelope<C> {
        &self.command
    }

    #[must_use]
    pub fn into_command(self) -> CommandEnvelope<C> {
        self.command
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginToEngine<C> {
    Events(EventBatch),
    Data(DataBatch),
    CapabilityRequest {
        plugin_id: PluginId,
        capability: CapabilityId,
    },
    StateSnapshot(StateSnapshot),
    Heartbeat {
        plugin_id: PluginId,
        heartbeat: HeartbeatMessage,
    },
    Crash(CrashReport),
    Mutation(MutationIntent<C>),
}

pub(crate) fn validate_capabilities(
    capabilities: &[CapabilityId],
    limits: &ProtocolLimits,
) -> Result<(), ExchangeError> {
    let maximum = limits.max_capabilities.min(HARD_MAX_CAPABILITIES);
    if capabilities.len() > maximum {
        return Err(ExchangeError::BatchTooLarge {
            kind: "capabilities",
            actual: capabilities.len(),
            maximum,
        });
    }
    let mut unique = HashSet::with_capacity(capabilities.len());
    for capability in capabilities {
        limits.validate_identifier("capability", capability.as_str())?;
        if !unique.insert(capability) {
            return Err(ExchangeError::DuplicateCapability);
        }
    }
    Ok(())
}
