//! Configured RTMP/RTMPS streaming destinations.
//!
//! A [`StreamTarget`] is the authored, persisted half of going live: it names
//! an ingest server, carries the service's stream key, and says which
//! [`Output`](crate::Output) supplies its video and audio. It is not a running
//! stream. Nothing here starts, stops, reconnects, or observes a sink.
//!
//! # The stream key is a secret
//!
//! [`StreamKey`] never renders its value. `Debug` shows `StreamKey(****)`, it
//! has no `Display`, and no error type in this module carries key or endpoint
//! text, so a rejected destination cannot leak one through a message. The only
//! way to read the secret is [`StreamKey::expose_secret`], which is named to be
//! obvious in review and in `grep`.
//!
//! The key *is* written to `project.json` in the clear. That is the existing
//! trust model of a `.freemix` bundle: the manifest is plaintext JSON on disk
//! and every other authored field is stored the same way. Anyone who can read
//! the bundle can read the key, so the bundle must be protected like a
//! credential. This module makes the weaker promise that the key does not
//! escape the bundle: not into logs, not into `Debug` output, not into an
//! error, not into a status or inventory line.
//!
//! # Validation
//!
//! The rules mirror `fm_codec_ffmpeg::StreamDestination`, which is what
//! actually opens the socket, because a destination that this crate accepts and
//! the sink then refuses is a show that fails at air time instead of at
//! authoring time. `fm-model` is a foundation crate and cannot depend on the
//! `io` layer, so the rules are restated here against the same contract:
//! `rtmp`/`rtmps` only, printable ASCII, no `user:password@host` userinfo, a
//! stream key of at least [`MIN_STREAM_KEY_BYTES`], and a total URL that stays
//! inside the sink's bound. The composed URL is [`StreamTarget::expose_url`].

use core::{fmt, num::NonZeroU128};

use fm_types::OutputId;

use crate::StartupPolicy;

/// Longest operator-facing destination name, matching the input-name bound.
pub const MAX_STREAM_TARGET_NAME_BYTES: usize = 128;

/// Longest scheme-relative endpoint, as `host[:port]/application/path`.
pub const MAX_STREAM_ENDPOINT_BYTES: usize = 1_024;

/// Shortest stream key that can be substituted out of captured child output
/// without mangling unrelated text. Matches the sink's minimum.
pub const MIN_STREAM_KEY_BYTES: usize = 4;

/// Longest stream key. `MAX_STREAM_ENDPOINT_BYTES` plus this plus the longest
/// scheme and the separator stays inside the sink's 2 KiB URL bound.
pub const MAX_STREAM_KEY_BYTES: usize = 512;

/// What a redacted stream key renders as, byte for byte as the sink renders it.
pub const REDACTED_STREAM_KEY: &str = "****";

/// Stable identity of one configured streaming destination.
///
/// Domain ids normally live in `fm-types`; this one lives here because the
/// streaming destination model is the only thing that refers to it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamTargetId(NonZeroU128);

impl StreamTargetId {
    #[must_use]
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU128 {
        self.0
    }
}

impl From<NonZeroU128> for StreamTargetId {
    fn from(value: NonZeroU128) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for StreamTargetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The transport a destination is published over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamProtocol {
    Rtmp,
    Rtmps,
}

impl StreamProtocol {
    /// The URL scheme, without `://`.
    #[must_use]
    pub const fn scheme(self) -> &'static str {
        match self {
            Self::Rtmp => "rtmp",
            Self::Rtmps => "rtmps",
        }
    }
}

impl fmt::Display for StreamProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.scheme())
    }
}

/// Why an ingest endpoint was refused.
///
/// No variant carries endpoint text, so an error may be logged or surfaced to
/// an operator without repeating what was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamEndpointError {
    Empty,
    TooLong,
    /// The endpoint contains whitespace, control, or non-ASCII bytes.
    InvalidCharacter,
    /// Only `rtmp://` and `rtmps://` are accepted.
    UnsupportedScheme,
    /// A scheme appeared inside the scheme-relative endpoint text.
    EmbeddedScheme,
    MissingHost,
    /// `user:password@host` credentials are refused; they cannot be redacted
    /// as reliably as a trailing stream key.
    EmbeddedCredentials,
    /// The endpoint has no `/application` path, so appending the stream key
    /// would produce a URL with no redactable final segment.
    MissingApplicationPath,
    /// A `//` or a trailing `/` left a path segment empty.
    EmptyPathSegment,
    /// `?` and `#` belong to the stream key, which is where services put
    /// per-session tokens, not to the endpoint.
    QueryOrFragment,
}

impl fmt::Display for StreamEndpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "stream endpoint must not be empty",
            Self::TooLong => "stream endpoint is too long",
            Self::InvalidCharacter => "stream endpoint must be printable ASCII without whitespace",
            Self::UnsupportedScheme => "stream endpoint must use rtmp:// or rtmps://",
            Self::EmbeddedScheme => "stream endpoint must not repeat the URL scheme",
            Self::MissingHost => "stream endpoint must name a host",
            Self::EmbeddedCredentials => "stream endpoint must not embed user:password credentials",
            Self::MissingApplicationPath => "stream endpoint must include an application path",
            Self::EmptyPathSegment => "stream endpoint must not contain an empty path segment",
            Self::QueryOrFragment => {
                "stream endpoint must not contain a query or fragment; put tokens in the key"
            }
        })
    }
}

impl std::error::Error for StreamEndpointError {}

/// One validated ingest location, stored without its scheme.
///
/// The text is `host[:port]/application[/path]`. The scheme lives on
/// [`StreamTarget::protocol`] so that it is not stored twice and cannot
/// disagree with itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamEndpoint {
    text: String,
}

impl StreamEndpoint {
    /// Validates a scheme-relative `host[:port]/application` endpoint.
    ///
    /// # Errors
    ///
    /// Returns a typed [`StreamEndpointError`]. No variant carries URL text.
    pub fn parse(text: &str) -> Result<Self, StreamEndpointError> {
        if text.is_empty() {
            return Err(StreamEndpointError::Empty);
        }
        if text.len() > MAX_STREAM_ENDPOINT_BYTES {
            return Err(StreamEndpointError::TooLong);
        }
        if !text.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(StreamEndpointError::InvalidCharacter);
        }
        if text.contains("://") {
            return Err(StreamEndpointError::EmbeddedScheme);
        }
        if text.contains('?') || text.contains('#') {
            return Err(StreamEndpointError::QueryOrFragment);
        }
        let (authority, path) = text
            .split_once('/')
            .ok_or(StreamEndpointError::MissingApplicationPath)?;
        if authority.is_empty() {
            return Err(StreamEndpointError::MissingHost);
        }
        if authority.contains('@') {
            return Err(StreamEndpointError::EmbeddedCredentials);
        }
        if path.split('/').any(str::is_empty) {
            return Err(StreamEndpointError::EmptyPathSegment);
        }
        Ok(Self {
            text: text.to_owned(),
        })
    }

    /// Splits and validates a full `rtmp://host/application` URL.
    ///
    /// The URL must not already carry a stream key: the key is a separate
    /// authored field so that it can be redacted independently.
    ///
    /// # Errors
    ///
    /// Returns a typed [`StreamEndpointError`]. No variant carries URL text.
    pub fn parse_url(url: &str) -> Result<(StreamProtocol, Self), StreamEndpointError> {
        let (protocol, rest) = if let Some(rest) = url.strip_prefix("rtmps://") {
            (StreamProtocol::Rtmps, rest)
        } else if let Some(rest) = url.strip_prefix("rtmp://") {
            (StreamProtocol::Rtmp, rest)
        } else if url.is_empty() {
            return Err(StreamEndpointError::Empty);
        } else {
            return Err(StreamEndpointError::UnsupportedScheme);
        };
        Ok((protocol, Self::parse(rest)?))
    }

    /// The scheme-relative `host[:port]/application` text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// Why a stream key was refused.
///
/// No variant carries key text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamKeyError {
    Empty,
    /// Shorter than [`MIN_STREAM_KEY_BYTES`], so the sink could not scrub it
    /// out of captured output without mangling unrelated text.
    TooShort,
    TooLong,
    /// The key contains whitespace, control, or non-ASCII bytes.
    InvalidCharacter,
    /// The key is the final URL segment and cannot contain a separator.
    PathSeparator,
}

impl fmt::Display for StreamKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "stream key must not be empty",
            Self::TooShort => "stream key is too short to be redacted safely",
            Self::TooLong => "stream key is too long",
            Self::InvalidCharacter => "stream key must be printable ASCII without whitespace",
            Self::PathSeparator => "stream key must not contain `/`",
        })
    }
}

impl std::error::Error for StreamKeyError {}

/// One validated stream key.
///
/// The value is never rendered. There is no `Display`, `Debug` prints
/// `StreamKey(****)`, and reading the secret requires the deliberately
/// conspicuous [`StreamKey::expose_secret`].
#[derive(Clone, Eq, PartialEq)]
pub struct StreamKey {
    secret: String,
}

impl StreamKey {
    /// Validates length and character set.
    ///
    /// # Errors
    ///
    /// Returns a typed [`StreamKeyError`]. No variant carries key text.
    pub fn parse(secret: &str) -> Result<Self, StreamKeyError> {
        if secret.is_empty() {
            return Err(StreamKeyError::Empty);
        }
        if !secret.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(StreamKeyError::InvalidCharacter);
        }
        if secret.contains('/') {
            return Err(StreamKeyError::PathSeparator);
        }
        if secret.len() < MIN_STREAM_KEY_BYTES {
            return Err(StreamKeyError::TooShort);
        }
        if secret.len() > MAX_STREAM_KEY_BYTES {
            return Err(StreamKeyError::TooLong);
        }
        Ok(Self {
            secret: secret.to_owned(),
        })
    }

    /// Returns the secret in the clear.
    ///
    /// Call this only to compose the URL handed to a sink or to write the
    /// bundle manifest. It must not reach a log, a status line, or an error.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StreamKey")
            .field(&REDACTED_STREAM_KEY)
            .finish()
    }
}

/// Why a streaming destination was refused at construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamTargetError {
    EmptyName,
    NameTooLong,
    /// Scheme, endpoint, key and separators together exceed what the sink
    /// accepts as one URL.
    UrlTooLong,
}

impl fmt::Display for StreamTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("stream destination name must not be empty"),
            Self::NameTooLong => write!(
                formatter,
                "stream destination name must not exceed {MAX_STREAM_TARGET_NAME_BYTES} bytes"
            ),
            Self::UrlTooLong => write!(
                formatter,
                "stream destination URL must not exceed {} bytes",
                StreamTarget::MAX_URL_BYTES
            ),
        }
    }
}

impl std::error::Error for StreamTargetError {}

/// One configured streaming destination.
///
/// Every field is validated at construction and the id is stable across
/// updates, so a destination can be re-authored without a downstream reference
/// to it changing meaning. See the [module docs](self) for how the stream key
/// is handled.
#[derive(Clone, Eq, PartialEq)]
pub struct StreamTarget {
    id: StreamTargetId,
    name: String,
    protocol: StreamProtocol,
    endpoint: StreamEndpoint,
    backup_endpoint: Option<StreamEndpoint>,
    key: StreamKey,
    startup: StartupPolicy,
    output: OutputId,
}

impl StreamTarget {
    /// The sink's bound on a whole destination URL.
    pub const MAX_URL_BYTES: usize = 2 * 1_024;

    /// Builds a destination that starts [`StartupPolicy::Stopped`] with no
    /// backup endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`StreamTargetError`] when the name is blank or oversized, or
    /// when the composed URL would exceed what the sink accepts.
    pub fn new(
        id: StreamTargetId,
        name: String,
        protocol: StreamProtocol,
        endpoint: StreamEndpoint,
        key: StreamKey,
        output: OutputId,
    ) -> Result<Self, StreamTargetError> {
        if name.trim().is_empty() {
            return Err(StreamTargetError::EmptyName);
        }
        if name.len() > MAX_STREAM_TARGET_NAME_BYTES {
            return Err(StreamTargetError::NameTooLong);
        }
        if url_bytes(protocol, &endpoint, &key) > Self::MAX_URL_BYTES {
            return Err(StreamTargetError::UrlTooLong);
        }
        Ok(Self {
            id,
            name,
            protocol,
            endpoint,
            backup_endpoint: None,
            key,
            startup: StartupPolicy::Stopped,
            output,
        })
    }

    /// Attaches or clears the failover endpoint, which uses the same protocol
    /// and the same key as the primary.
    ///
    /// # Errors
    ///
    /// Returns [`StreamTargetError::UrlTooLong`] when the composed backup URL
    /// would exceed what the sink accepts.
    pub fn with_backup_endpoint(
        mut self,
        backup_endpoint: Option<StreamEndpoint>,
    ) -> Result<Self, StreamTargetError> {
        if let Some(endpoint) = &backup_endpoint
            && url_bytes(self.protocol, endpoint, &self.key) > Self::MAX_URL_BYTES
        {
            return Err(StreamTargetError::UrlTooLong);
        }
        self.backup_endpoint = backup_endpoint;
        Ok(self)
    }

    #[must_use]
    pub fn with_startup(mut self, startup: StartupPolicy) -> Self {
        self.startup = startup;
        self
    }

    #[must_use]
    pub const fn id(&self) -> StreamTargetId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn protocol(&self) -> StreamProtocol {
        self.protocol
    }

    #[must_use]
    pub const fn endpoint(&self) -> &StreamEndpoint {
        &self.endpoint
    }

    #[must_use]
    pub const fn backup_endpoint(&self) -> Option<&StreamEndpoint> {
        self.backup_endpoint.as_ref()
    }

    /// The stream key. Reading its value still requires
    /// [`StreamKey::expose_secret`].
    #[must_use]
    pub const fn key(&self) -> &StreamKey {
        &self.key
    }

    #[must_use]
    pub const fn startup(&self) -> StartupPolicy {
        self.startup
    }

    /// The output this destination takes video and audio from.
    #[must_use]
    pub const fn output(&self) -> OutputId {
        self.output
    }

    /// Renames in place, preserving the exact supplied text.
    ///
    /// # Errors
    ///
    /// Returns [`StreamTargetError`] when the name is blank or oversized.
    pub fn rename(&mut self, name: String) -> Result<(), StreamTargetError> {
        if name.trim().is_empty() {
            return Err(StreamTargetError::EmptyName);
        }
        if name.len() > MAX_STREAM_TARGET_NAME_BYTES {
            return Err(StreamTargetError::NameTooLong);
        }
        self.name = name;
        Ok(())
    }

    /// The full destination URL, stream key included.
    ///
    /// This is what a sink is handed. It must not reach a log or a status
    /// line; use [`StreamTarget::redacted_url`] for anything an operator or a
    /// file might see.
    #[must_use]
    pub fn expose_url(&self) -> String {
        compose_url(self.protocol, &self.endpoint, self.key.expose_secret())
    }

    /// The full failover URL, stream key included, when one is configured.
    #[must_use]
    pub fn expose_backup_url(&self) -> Option<String> {
        self.backup_endpoint
            .as_ref()
            .map(|endpoint| compose_url(self.protocol, endpoint, self.key.expose_secret()))
    }

    /// The destination URL with the stream key replaced by `****`.
    #[must_use]
    pub fn redacted_url(&self) -> String {
        compose_url(self.protocol, &self.endpoint, REDACTED_STREAM_KEY)
    }

    /// The failover URL with the stream key replaced by `****`.
    #[must_use]
    pub fn redacted_backup_url(&self) -> Option<String> {
        self.backup_endpoint
            .as_ref()
            .map(|endpoint| compose_url(self.protocol, endpoint, REDACTED_STREAM_KEY))
    }
}

/// Renders the redacted URLs instead of the raw protocol, endpoint and key.
///
/// The omitted fields are exactly the three the redacted URLs already
/// summarise, and one of them is the secret, so this impl is deliberately not
/// exhaustive.
impl fmt::Debug for StreamTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamTarget")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("url", &self.redacted_url())
            .field("backup_url", &self.redacted_backup_url())
            .field("startup", &self.startup)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for StreamTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted_url())
    }
}

fn compose_url(protocol: StreamProtocol, endpoint: &StreamEndpoint, tail: &str) -> String {
    format!("{}://{}/{tail}", protocol.scheme(), endpoint.as_str())
}

fn url_bytes(protocol: StreamProtocol, endpoint: &StreamEndpoint, key: &StreamKey) -> usize {
    protocol.scheme().len() + "://".len() + endpoint.as_str().len() + 1 + key.expose_secret().len()
}
