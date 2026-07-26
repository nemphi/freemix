use core::{fmt, num::NonZeroU128, num::NonZeroUsize};

macro_rules! stable_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU128);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> NonZeroU128 {
                self.0
            }
        }

        impl From<NonZeroU128> for $name {
            fn from(value: NonZeroU128) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(PeerId);
stable_id!(SessionId);
stable_id!(TrackId);
stable_id!(DataChannelId);
stable_id!(LayerId);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DescriptionKind {
    Offer,
    Answer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDescription {
    pub kind: DescriptionKind,
    pub session_id: SessionId,
    pub revision: u64,
    pub opaque: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SignalingState {
    Stable,
    HaveLocalOffer,
    HaveRemoteOffer,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IceGatheringState {
    New,
    Gathering,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IceConnectionState {
    New,
    Checking,
    Connected,
    Failed,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionState {
    New,
    Negotiating,
    Connected,
    Reconnecting,
    Failed,
    Closed,
}

/// Opaque reference to credentials held by a secret provider.
///
/// Debug output deliberately omits the reference as it may disclose a secret
/// store path or short-lived token.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct CredentialRef {
    reference: String,
    pub expires_at_ms: u64,
}

impl CredentialRef {
    #[must_use]
    pub fn new(reference: impl Into<String>, expires_at_ms: u64) -> Self {
        Self {
            reference: reference.into(),
            expires_at_ms,
        }
    }

    #[must_use]
    pub const fn is_expired_at(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

impl fmt::Debug for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialRef")
            .field("reference", &"[REDACTED]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IceServer {
    Stun {
        urls: Vec<String>,
    },
    Turn {
        urls: Vec<String>,
        username: String,
        credential: CredentialRef,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IceTransportPolicy {
    All,
    RelayOnly,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceConfig {
    pub servers: Vec<IceServer>,
    pub policy: IceTransportPolicy,
}

impl Default for IceConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            policy: IceTransportPolicy::All,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IceCandidateKind {
    Host,
    ServerReflexive,
    Relay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceCandidate {
    pub kind: IceCandidateKind,
    pub foundation: u32,
    pub priority: u32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NatType {
    Open,
    FullCone,
    RestrictedCone,
    PortRestricted,
    Symmetric,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NatScenario {
    pub local: NatType,
    pub remote: NatType,
}

impl Default for NatScenario {
    fn default() -> Self {
        Self {
            local: NatType::Open,
            remote: NatType::Open,
        }
    }
}

/// Deterministic network behavior. Every `loss_every`th packet is dropped and
/// jitter alternates between adding and subtracting the configured amount.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImpairmentProfile {
    pub one_way_delay_ms: u64,
    pub jitter_ms: u64,
    pub loss_every: Option<u32>,
    pub available_bitrate_bps: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TrackDirection {
    Send,
    Receive,
    SendReceive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecCapability {
    pub name: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaCapability {
    Audio {
        sample_rates: Vec<u32>,
        channel_counts: Vec<u16>,
    },
    Video {
        max_width: u32,
        max_height: u32,
        max_frame_rate: u16,
    },
}

impl MediaCapability {
    #[must_use]
    pub const fn kind(&self) -> MediaKind {
        match self {
            Self::Audio { .. } => MediaKind::Audio,
            Self::Video { .. } => MediaKind::Video,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueLimits {
    pub capacity: NonZeroUsize,
    pub max_record_bytes: NonZeroUsize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackCapabilities {
    pub media: MediaCapability,
    pub codecs: Vec<CodecCapability>,
    pub queue: QueueLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackConfig {
    pub id: TrackId,
    pub direction: TrackDirection,
    pub capabilities: TrackCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioRecord {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoRecord {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    pub layer: Option<LayerId>,
    pub payload: Vec<u8>,
}

/// Audio returned from a remote peer into the production graph.
pub type ReturnAudioRecord = AudioRecord;

/// Video returned from a remote peer into the production graph.
pub type ReturnVideoRecord = VideoRecord;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaRecord {
    Audio(AudioRecord),
    Video(VideoRecord),
}

impl MediaRecord {
    #[must_use]
    pub const fn kind(&self) -> MediaKind {
        match self {
            Self::Audio(_) => MediaKind::Audio,
            Self::Video(_) => MediaKind::Video,
        }
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        match self {
            Self::Audio(record) => record.sequence,
            Self::Video(record) => record.sequence,
        }
    }

    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Audio(record) => record.payload.len(),
            Self::Video(record) => record.payload.len(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TallyState {
    Off,
    Preview,
    Program,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TallyRecord {
    pub sequence: u64,
    pub state: TallyState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRecord {
    pub sequence: u64,
    pub from: PeerId,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EchoRecord {
    pub sequence: u64,
    pub sent_at_ms: u64,
    pub returned_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataChannelRecord {
    Tally(TallyRecord),
    Chat(ChatRecord),
    Echo(EchoRecord),
    Binary { sequence: u64, payload: Vec<u8> },
}

impl DataChannelRecord {
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        match self {
            Self::Tally(record) => record.sequence,
            Self::Chat(record) => record.sequence,
            Self::Echo(record) => record.sequence,
            Self::Binary { sequence, .. } => *sequence,
        }
    }

    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Tally(_) => size_of::<TallyRecord>(),
            Self::Chat(record) => record.text.len(),
            Self::Echo(_) => size_of::<EchoRecord>(),
            Self::Binary { payload, .. } => payload.len(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DataChannelKind {
    Tally,
    Chat,
    Control,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelConfig {
    pub id: DataChannelId,
    pub label: String,
    pub kind: DataChannelKind,
    pub ordered: bool,
    pub queue: QueueLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BandwidthLayer {
    pub id: LayerId,
    pub min_bitrate_bps: u64,
    pub max_bitrate_bps: u64,
    pub width: u32,
    pub height: u32,
    pub frame_rate: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdaptationPlan {
    /// Layers ordered from lowest to highest quality.
    pub layers: Vec<BandwidthLayer>,
}

impl AdaptationPlan {
    #[must_use]
    pub fn select(&self, available_bitrate_bps: u64) -> Option<&BandwidthLayer> {
        self.layers
            .iter()
            .rev()
            .find(|layer| layer.min_bitrate_bps <= available_bitrate_bps)
            .or_else(|| self.layers.first())
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReconnectIdentity {
    pub peer_id: PeerId,
    pub generation: u64,
    token: String,
}

impl ReconnectIdentity {
    pub(crate) fn new(peer_id: PeerId, generation: u64, token: String) -> Self {
        Self {
            peer_id,
            generation,
            token,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for ReconnectIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReconnectIdentity")
            .field("peer_id", &self.peer_id)
            .field("generation", &self.generation)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EchoStats {
    pub sent: u64,
    pub returned: u64,
    pub lost: u64,
    pub last_round_trip_ms: Option<u64>,
    pub average_round_trip_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransportHealth {
    pub packets_sent: u64,
    pub packets_delivered: u64,
    pub packets_lost: u64,
    pub queue_drops: u64,
    pub estimated_jitter_ms: u64,
    pub current_layer: Option<LayerId>,
    pub echo: EchoStats,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebRtcError {
    UnknownPeer(PeerId),
    UnknownSession(SessionId),
    UnknownTrack(TrackId),
    UnknownDataChannel(DataChannelId),
    DuplicateId,
    InvalidSignalingOrder {
        operation: &'static str,
        state: SignalingState,
    },
    InvalidIceState {
        operation: &'static str,
        gathering: IceGatheringState,
        connection: IceConnectionState,
    },
    IceCredentialExpired,
    IceConnectionFailed,
    SessionNotConnected,
    RecordKindMismatch,
    RecordTooLarge {
        actual: usize,
        maximum: usize,
    },
    QueueFull,
    SequenceOutOfOrder {
        previous: u64,
        actual: u64,
    },
    InvalidReconnectIdentity,
    Closed,
}

impl fmt::Display for WebRtcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPeer(id) => write!(formatter, "unknown peer {id}"),
            Self::UnknownSession(id) => write!(formatter, "unknown session {id}"),
            Self::UnknownTrack(id) => write!(formatter, "unknown track {id}"),
            Self::UnknownDataChannel(id) => write!(formatter, "unknown data channel {id}"),
            Self::DuplicateId => formatter.write_str("identifier is already in use"),
            Self::InvalidSignalingOrder { operation, state } => {
                write!(formatter, "cannot {operation} while signaling is {state:?}")
            }
            Self::InvalidIceState {
                operation,
                gathering,
                connection,
            } => write!(
                formatter,
                "cannot {operation} while ICE is {gathering:?}/{connection:?}"
            ),
            Self::IceCredentialExpired => formatter.write_str("TURN credential has expired"),
            Self::IceConnectionFailed => formatter.write_str("ICE connectivity checks failed"),
            Self::SessionNotConnected => formatter.write_str("session is not connected"),
            Self::RecordKindMismatch => {
                formatter.write_str("record does not match track media kind")
            }
            Self::RecordTooLarge { actual, maximum } => {
                write!(formatter, "record size {actual} exceeds {maximum}")
            }
            Self::QueueFull => formatter.write_str("bounded queue is full"),
            Self::SequenceOutOfOrder { previous, actual } => {
                write!(formatter, "sequence {actual} does not follow {previous}")
            }
            Self::InvalidReconnectIdentity => formatter.write_str("reconnect identity is invalid"),
            Self::Closed => formatter.write_str("session is closed"),
        }
    }
}

impl std::error::Error for WebRtcError {}

/// Synchronous, transport-neutral peer connection operations.
pub trait PeerConnection {
    fn id(&self) -> SessionId;
    fn signaling_state(&self) -> SignalingState;
    fn ice_gathering_state(&self) -> IceGatheringState;
    fn ice_connection_state(&self) -> IceConnectionState;
    fn state(&self) -> SessionState;

    /// # Errors
    /// Returns an ordering or closed-session error.
    fn create_offer(&mut self) -> Result<SessionDescription, WebRtcError>;

    /// # Errors
    /// Returns an ordering or closed-session error.
    fn receive_offer(&mut self, offer: SessionDescription) -> Result<(), WebRtcError>;

    /// # Errors
    /// Returns an ordering or closed-session error.
    fn create_answer(&mut self) -> Result<SessionDescription, WebRtcError>;

    /// # Errors
    /// Returns an ordering or closed-session error.
    fn receive_answer(&mut self, answer: SessionDescription) -> Result<(), WebRtcError>;

    /// # Errors
    /// Returns an ICE state or expired credential error.
    fn gather_ice(&mut self, now_ms: u64) -> Result<&[IceCandidate], WebRtcError>;

    /// # Errors
    /// Returns an ICE state, connectivity, or expired credential error.
    fn check_ice(&mut self, now_ms: u64) -> Result<(), WebRtcError>;
}
