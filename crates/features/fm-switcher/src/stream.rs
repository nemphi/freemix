use std::collections::BTreeSet;
use std::num::NonZeroU128;

/// Maximum number of durable stream targets one show can inventory.
pub const MAX_STREAM_COUNT: usize = 5;
/// Largest stream target label in bytes.
pub const MAX_STREAM_NAME_BYTES: usize = 128;

/// Stable identity of one configurable stream target within a show.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamTargetId(NonZeroU128);

impl StreamTargetId {
    #[must_use]
    pub fn new(value: u128) -> Option<Self> {
        NonZeroU128::new(value).map(Self)
    }

    #[must_use]
    pub const fn from_non_zero(value: NonZeroU128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU128 {
        self.0
    }
}

impl core::fmt::Display for StreamTargetId {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Durable desired identity and label of one stream target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredStream {
    id: StreamTargetId,
    name: String,
}

impl DesiredStream {
    pub(crate) const fn new(id: StreamTargetId, name: String) -> Self {
        Self { id, name }
    }

    #[must_use]
    pub const fn id(&self) -> StreamTargetId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

pub(crate) fn collect_desired_streams(
    streams: impl IntoIterator<Item = (StreamTargetId, String)>,
) -> Result<Vec<DesiredStream>, crate::SwitcherError> {
    let streams: Vec<_> = streams.into_iter().collect();
    if streams.len() > MAX_STREAM_COUNT {
        return Err(crate::SwitcherError::TooManyStreams {
            requested: streams.len(),
            maximum: MAX_STREAM_COUNT,
        });
    }
    let mut desired = Vec::with_capacity(streams.len());
    let mut identifiers = BTreeSet::new();
    for (id, name) in streams {
        if name.trim().is_empty() || name.len() > MAX_STREAM_NAME_BYTES {
            return Err(crate::SwitcherError::InvalidStreamName);
        }
        if !identifiers.insert(id) {
            return Err(crate::SwitcherError::DuplicateStreamTarget(id));
        }
        desired.push(DesiredStream::new(id, name));
    }
    Ok(desired)
}
