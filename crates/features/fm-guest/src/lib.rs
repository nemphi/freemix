//! Transport-neutral guest invitations, lobby, sessions, routing, and chat.
//!
//! All identifiers and timestamps are supplied by the caller. This keeps the
//! model deterministic and leaves persistence, clocks, networking, and
//! authentication to adapters outside this crate.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::num::NonZeroU128;

use fm_audio::Gain;
use fm_types::InputId;

/// Maximum number of retained guest sessions.
pub const MAX_GUEST_SESSIONS: usize = 8;
/// Maximum UTF-8 byte length of a guest display name.
pub const MAX_DISPLAY_NAME_BYTES: usize = 128;
/// Maximum UTF-8 byte length of one chat message.
pub const MAX_CHAT_BODY_BYTES: usize = 2_000;
/// Maximum number of chat messages retained by a manager.
pub const MAX_CHAT_MESSAGES: usize = 256;
/// Maximum number of chat groups retained by a manager.
pub const MAX_CHAT_GROUPS: usize = 64;
/// Maximum number of members in one chat group.
pub const MAX_CHAT_GROUP_MEMBERS: usize = MAX_GUEST_SESSIONS;
/// Maximum UTF-8 byte length of a rejection reason.
pub const MAX_REASON_BYTES: usize = 512;

macro_rules! domain_id {
    ($name:ident) => {
        #[doc = concat!("Stable identifier for a `", stringify!($name), "` domain object.")]
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

domain_id!(InvitationId);
domain_id!(GuestId);
domain_id!(GuestSessionId);
domain_id!(ChatMessageId);
domain_id!(ChatGroupId);

/// A one-based guest slot in the inclusive range 1 through 8.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GuestSlot(u8);

impl GuestSlot {
    /// Creates a validated slot.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::InvalidSlot`] when `number` is not in `1..=8`.
    pub const fn new(number: u8) -> Result<Self, GuestError> {
        if number == 0 || number as usize > MAX_GUEST_SESSIONS {
            return Err(GuestError::InvalidSlot(number));
        }
        Ok(Self(number))
    }

    #[must_use]
    pub const fn number(self) -> u8 {
        self.0
    }
}

/// A validated identity chosen by a guest when entering the lobby.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestIdentity {
    pub id: GuestId,
    display_name: String,
}

impl GuestIdentity {
    /// Creates an identity with a non-empty, bounded display name.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::InvalidDisplayName`] for an empty, whitespace-only,
    /// or oversized name.
    pub fn new(id: GuestId, display_name: impl Into<String>) -> Result<Self, GuestError> {
        let display_name = display_name.into();
        if display_name.trim().is_empty() || display_name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(GuestError::InvalidDisplayName);
        }
        Ok(Self { id, display_name })
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

/// Availability of one guest-side media device.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeviceHealth {
    #[default]
    Unknown,
    Ready,
    Disabled,
    PermissionDenied,
    Unavailable,
}

/// Aggregate severity used by latency and echo diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum HealthSeverity {
    #[default]
    Unknown,
    Healthy,
    Degraded,
    Unhealthy,
}

/// Transport-reported round-trip latency and acoustic echo health.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LatencyEchoHealth {
    pub round_trip_ms: Option<u32>,
    pub echo_return_loss_db: Option<f32>,
}

impl LatencyEchoHealth {
    /// Creates a health sample. Echo return loss must be finite and non-negative.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::InvalidEchoReturnLoss`] for an invalid echo value.
    pub fn new(
        round_trip_ms: Option<u32>,
        echo_return_loss_db: Option<f32>,
    ) -> Result<Self, GuestError> {
        if echo_return_loss_db.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Err(GuestError::InvalidEchoReturnLoss);
        }
        Ok(Self {
            round_trip_ms,
            echo_return_loss_db,
        })
    }

    #[must_use]
    pub const fn latency_severity(self) -> HealthSeverity {
        match self.round_trip_ms {
            None => HealthSeverity::Unknown,
            Some(0..=150) => HealthSeverity::Healthy,
            Some(151..=300) => HealthSeverity::Degraded,
            Some(301..) => HealthSeverity::Unhealthy,
        }
    }

    #[must_use]
    pub fn echo_severity(self) -> HealthSeverity {
        match self.echo_return_loss_db {
            None => HealthSeverity::Unknown,
            Some(value) if value >= 35.0 => HealthSeverity::Healthy,
            Some(value) if value >= 20.0 => HealthSeverity::Degraded,
            Some(_) => HealthSeverity::Unhealthy,
        }
    }

    #[must_use]
    pub fn severity(self) -> HealthSeverity {
        combine_health(self.latency_severity(), self.echo_severity())
    }
}

fn combine_health(left: HealthSeverity, right: HealthSeverity) -> HealthSeverity {
    use HealthSeverity::{Degraded, Healthy, Unhealthy, Unknown};
    if left == Unhealthy || right == Unhealthy {
        Unhealthy
    } else if left == Degraded || right == Degraded {
        Degraded
    } else if left == Healthy || right == Healthy {
        Healthy
    } else {
        Unknown
    }
}

/// Lobby device and signal checks. A camera may intentionally be disabled.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LobbyPreflight {
    pub camera: DeviceHealth,
    pub microphone: DeviceHealth,
    pub speaker: DeviceHealth,
    pub signal: LatencyEchoHealth,
}

impl LobbyPreflight {
    /// Required devices must be ready and known signal health must not be poor.
    #[must_use]
    pub fn is_ready(self) -> bool {
        matches!(self.camera, DeviceHealth::Ready | DeviceHealth::Disabled)
            && self.microphone == DeviceHealth::Ready
            && self.speaker == DeviceHealth::Ready
            && self.signal.severity() != HealthSeverity::Unhealthy
    }
}

/// Persisted lifecycle state of an invitation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvitationState {
    Available,
    Consumed(GuestId),
    Revoked,
}

/// Effective invitation status at a caller-provided time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvitationStatus {
    Available,
    Expired,
    Consumed(GuestId),
    Revoked,
}

/// A one-use invitation, optionally reserving a specific slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invitation {
    pub id: InvitationId,
    pub expires_at_ms: u64,
    pub reserved_slot: Option<GuestSlot>,
    pub state: InvitationState,
}

impl Invitation {
    #[must_use]
    pub const fn status_at(&self, now_ms: u64) -> InvitationStatus {
        match self.state {
            InvitationState::Available if now_ms >= self.expires_at_ms => InvitationStatus::Expired,
            InvitationState::Available => InvitationStatus::Available,
            InvitationState::Consumed(guest_id) => InvitationStatus::Consumed(guest_id),
            InvitationState::Revoked => InvitationStatus::Revoked,
        }
    }
}

/// State of a guest waiting in the lobby.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LobbyState {
    Waiting,
    Rejected { reason: String },
}

/// One guest's lobby record.
#[derive(Clone, Debug, PartialEq)]
pub struct LobbyEntry {
    pub identity: GuestIdentity,
    pub invitation_id: InvitationId,
    pub reserved_slot: Option<GuestSlot>,
    pub preflight: LobbyPreflight,
    pub state: LobbyState,
    pub entered_at_ms: u64,
}

/// Video source sent back to a guest. This affects video only, never audio.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReturnVideoSelection {
    None,
    #[default]
    Program,
    Preview,
    Input(InputId),
}

/// Whether private manager talkback is routed to a guest's return.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TalkbackState {
    #[default]
    Off,
    On,
}

/// Guest connection state. Disconnected sessions continue to own their slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionConnection {
    Connected,
    Disconnected { reconnect_until_ms: u64 },
}

/// A retained guest session and all routing state that survives reconnects.
#[derive(Clone, Debug, PartialEq)]
pub struct GuestSession {
    pub id: GuestSessionId,
    pub identity: GuestIdentity,
    pub slot: GuestSlot,
    pub connection: SessionConnection,
    pub return_video: ReturnVideoSelection,
    pub talkback: TalkbackState,
    pub health: LatencyEchoHealth,
    pub admitted_at_ms: u64,
}

/// Source endpoint in a deterministic mix-minus plan.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MixSource {
    /// Program bed guaranteed by the producer to exclude all guest microphones
    /// and manager talkback.
    CleanProgram,
    GuestMicrophone(GuestId),
    ManagerTalkback,
}

/// Destination endpoint in a deterministic mix-minus plan.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MixDestination {
    Program,
    GuestReturn(GuestId),
}

/// One unity-gain (or attenuated) route described with [`fm_audio`] gain rules.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixRoute {
    pub source: MixSource,
    pub destination: MixDestination,
    pub gain: Gain,
}

/// Ordered route list for an audio engine adapter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MixMinusMatrix {
    routes: Vec<MixRoute>,
}

impl MixMinusMatrix {
    #[must_use]
    pub fn routes(&self) -> &[MixRoute] {
        &self.routes
    }
}

/// Guest resources accepted by scoped manager permissions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestScope {
    All,
    Guest(GuestId),
}

/// Chat resources accepted by scoped manager permissions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatScope {
    All,
    PrivateWith(GuestId),
    Group(ChatGroupId),
}

/// Permissions consumed by a host application's authorization adapter.
///
/// This crate describes required permission values but does not grant them or
/// depend on an authentication service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestManagerPermission {
    ManageInvitations,
    ManageLobby,
    ManageSession(GuestScope),
    SelectReturnVideo(GuestScope),
    UseTalkback(GuestScope),
    ModerateChat(ChatScope),
    ViewHealth(GuestScope),
}

impl GuestManagerPermission {
    /// Returns whether this granted permission covers `required`.
    #[must_use]
    pub fn allows(self, required: Self) -> bool {
        match (self, required) {
            (Self::ManageInvitations, Self::ManageInvitations)
            | (Self::ManageLobby, Self::ManageLobby) => true,
            (Self::ManageSession(granted), Self::ManageSession(required))
            | (Self::SelectReturnVideo(granted), Self::SelectReturnVideo(required))
            | (Self::UseTalkback(granted), Self::UseTalkback(required))
            | (Self::ViewHealth(granted), Self::ViewHealth(required)) => {
                guest_scope_allows(granted, required)
            }
            (Self::ModerateChat(granted), Self::ModerateChat(required)) => {
                chat_scope_allows(granted, required)
            }
            _ => false,
        }
    }
}

fn guest_scope_allows(granted: GuestScope, required: GuestScope) -> bool {
    matches!(granted, GuestScope::All) || granted == required
}

fn chat_scope_allows(granted: ChatScope, required: ChatScope) -> bool {
    matches!(granted, ChatScope::All) || granted == required
}

/// A canonical private conversation or a group conversation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ChatAudience {
    Private { first: GuestId, second: GuestId },
    Group(ChatGroupId),
}

impl ChatAudience {
    /// Creates a canonical private audience.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::InvalidPrivateAudience`] when both IDs are equal.
    pub fn private(left: GuestId, right: GuestId) -> Result<Self, GuestError> {
        if left == right {
            return Err(GuestError::InvalidPrivateAudience);
        }
        let (first, second) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        Ok(Self::Private { first, second })
    }

    #[must_use]
    pub fn contains_private_guest(self, guest_id: GuestId) -> bool {
        matches!(self, Self::Private { first, second } if first == guest_id || second == guest_id)
    }
}

/// Why message content is no longer visible.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RedactionFlag {
    Sender,
    Moderator,
}

/// A bounded chat message. Redaction removes the body and retains audit flags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatMessage {
    pub id: ChatMessageId,
    pub sender: GuestId,
    pub audience: ChatAudience,
    pub sent_at_ms: u64,
    body: Option<String>,
    redaction_flags: BTreeSet<RedactionFlag>,
}

impl ChatMessage {
    #[must_use]
    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    #[must_use]
    pub fn redaction_flags(&self) -> &BTreeSet<RedactionFlag> {
        &self.redaction_flags
    }

    #[must_use]
    pub fn is_redacted(&self) -> bool {
        !self.redaction_flags.is_empty()
    }
}

/// Errors produced by guest-domain validation and lifecycle operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuestError {
    InvalidSlot(u8),
    InvalidDisplayName,
    InvalidEchoReturnLoss,
    InvalidExpiry,
    InvalidReconnectWindow,
    InvitationExists,
    InvitationNotFound,
    InvitationExpired,
    InvitationConsumed,
    InvitationRevoked,
    GuestExists,
    GuestNotFound,
    LobbyEntryNotFound,
    LobbyEntryNotWaiting,
    PreflightNotReady,
    InvalidReason,
    SlotUnavailable(GuestSlot),
    SessionLimitReached,
    SessionExists,
    SessionNotFound,
    SessionAlreadyConnected,
    ReconnectExpired,
    InvalidPrivateAudience,
    ChatGroupExists,
    ChatGroupNotFound,
    ChatGroupLimitReached,
    ChatGroupEmpty,
    ChatGroupTooLarge,
    ChatParticipantNotFound,
    ChatAccessDenied,
    InvalidChatBody,
    ChatMessageExists,
    ChatMessageNotFound,
}

impl fmt::Display for GuestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSlot(slot) => write!(formatter, "guest slot {slot} is outside 1..=8"),
            Self::InvalidDisplayName => formatter.write_str("display name is empty or too long"),
            Self::InvalidEchoReturnLoss => formatter.write_str("invalid echo return loss"),
            Self::InvalidExpiry => formatter.write_str("invitation expiry must be in the future"),
            Self::InvalidReconnectWindow => {
                formatter.write_str("reconnect window must end in the future")
            }
            Self::InvitationExists => formatter.write_str("invitation ID already exists"),
            Self::InvitationNotFound => formatter.write_str("invitation was not found"),
            Self::InvitationExpired => formatter.write_str("invitation has expired"),
            Self::InvitationConsumed => formatter.write_str("invitation has already been consumed"),
            Self::InvitationRevoked => formatter.write_str("invitation has been revoked"),
            Self::GuestExists => formatter.write_str("guest ID already exists"),
            Self::GuestNotFound => formatter.write_str("guest was not found"),
            Self::LobbyEntryNotFound => formatter.write_str("lobby entry was not found"),
            Self::LobbyEntryNotWaiting => formatter.write_str("guest is not waiting in the lobby"),
            Self::PreflightNotReady => formatter.write_str("lobby preflight is not ready"),
            Self::InvalidReason => formatter.write_str("reason is empty or too long"),
            Self::SlotUnavailable(slot) => {
                write!(formatter, "guest slot {} is unavailable", slot.0)
            }
            Self::SessionLimitReached => formatter.write_str("guest session limit reached"),
            Self::SessionExists => formatter.write_str("session ID already exists"),
            Self::SessionNotFound => formatter.write_str("guest session was not found"),
            Self::SessionAlreadyConnected => {
                formatter.write_str("guest session is already connected")
            }
            Self::ReconnectExpired => formatter.write_str("guest reconnect window has expired"),
            Self::InvalidPrivateAudience => {
                formatter.write_str("private chat requires two different guests")
            }
            Self::ChatGroupExists => formatter.write_str("chat group ID already exists"),
            Self::ChatGroupNotFound => formatter.write_str("chat group was not found"),
            Self::ChatGroupLimitReached => formatter.write_str("chat group limit reached"),
            Self::ChatGroupEmpty => formatter.write_str("chat group must have a member"),
            Self::ChatGroupTooLarge => formatter.write_str("chat group has too many members"),
            Self::ChatParticipantNotFound => formatter.write_str("chat participant was not found"),
            Self::ChatAccessDenied => formatter.write_str("chat audience is private"),
            Self::InvalidChatBody => formatter.write_str("chat body is empty or too long"),
            Self::ChatMessageExists => formatter.write_str("chat message ID already exists"),
            Self::ChatMessageNotFound => formatter.write_str("chat message was not found"),
        }
    }
}

impl std::error::Error for GuestError {}

/// Deterministic aggregate for guest lifecycle and planning state.
#[derive(Clone, Debug, Default)]
pub struct GuestManager {
    invitations: BTreeMap<InvitationId, Invitation>,
    identities: BTreeMap<GuestId, GuestIdentity>,
    lobby: BTreeMap<GuestId, LobbyEntry>,
    sessions: BTreeMap<GuestId, GuestSession>,
    chat_groups: BTreeMap<ChatGroupId, BTreeSet<GuestId>>,
    chat_messages: VecDeque<ChatMessage>,
}

impl GuestManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a one-use invitation and optionally reserves a slot.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate IDs, non-future expiry, or a slot already
    /// occupied or reserved at `now_ms`.
    pub fn create_invitation(
        &mut self,
        id: InvitationId,
        expires_at_ms: u64,
        reserved_slot: Option<GuestSlot>,
        now_ms: u64,
    ) -> Result<(), GuestError> {
        if self.invitations.contains_key(&id) {
            return Err(GuestError::InvitationExists);
        }
        if expires_at_ms <= now_ms {
            return Err(GuestError::InvalidExpiry);
        }
        if let Some(slot) = reserved_slot
            && self.slot_is_reserved(slot, now_ms, None)
        {
            return Err(GuestError::SlotUnavailable(slot));
        }
        self.invitations.insert(
            id,
            Invitation {
                id,
                expires_at_ms,
                reserved_slot,
                state: InvitationState::Available,
            },
        );
        Ok(())
    }

    /// Revokes an available invitation.
    ///
    /// # Errors
    ///
    /// Returns an error if the invitation is absent or no longer available.
    pub fn revoke_invitation(&mut self, id: InvitationId) -> Result<(), GuestError> {
        let invitation = self
            .invitations
            .get_mut(&id)
            .ok_or(GuestError::InvitationNotFound)?;
        match invitation.state {
            InvitationState::Available => invitation.state = InvitationState::Revoked,
            InvitationState::Consumed(_) => return Err(GuestError::InvitationConsumed),
            InvitationState::Revoked => return Err(GuestError::InvitationRevoked),
        }
        Ok(())
    }

    #[must_use]
    pub fn invitation(&self, id: InvitationId) -> Option<&Invitation> {
        self.invitations.get(&id)
    }

    /// Consumes an invitation and creates a lobby identity atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for an unavailable invitation or duplicate guest ID.
    pub fn enter_lobby(
        &mut self,
        invitation_id: InvitationId,
        identity: GuestIdentity,
        preflight: LobbyPreflight,
        now_ms: u64,
    ) -> Result<(), GuestError> {
        if self.identities.contains_key(&identity.id) {
            return Err(GuestError::GuestExists);
        }
        let invitation = self
            .invitations
            .get_mut(&invitation_id)
            .ok_or(GuestError::InvitationNotFound)?;
        match invitation.status_at(now_ms) {
            InvitationStatus::Available => {}
            InvitationStatus::Expired => return Err(GuestError::InvitationExpired),
            InvitationStatus::Consumed(_) => return Err(GuestError::InvitationConsumed),
            InvitationStatus::Revoked => return Err(GuestError::InvitationRevoked),
        }
        let reserved_slot = invitation.reserved_slot;
        invitation.state = InvitationState::Consumed(identity.id);
        self.identities.insert(identity.id, identity.clone());
        self.lobby.insert(
            identity.id,
            LobbyEntry {
                identity,
                invitation_id,
                reserved_slot,
                preflight,
                state: LobbyState::Waiting,
                entered_at_ms: now_ms,
            },
        );
        Ok(())
    }

    /// Replaces a waiting guest's device and signal preflight.
    ///
    /// # Errors
    ///
    /// Returns an error unless the guest is waiting in the lobby.
    pub fn update_preflight(
        &mut self,
        guest_id: GuestId,
        preflight: LobbyPreflight,
    ) -> Result<(), GuestError> {
        let entry = self
            .lobby
            .get_mut(&guest_id)
            .ok_or(GuestError::LobbyEntryNotFound)?;
        if entry.state != LobbyState::Waiting {
            return Err(GuestError::LobbyEntryNotWaiting);
        }
        entry.preflight = preflight;
        Ok(())
    }

    /// Marks a waiting lobby entry rejected while retaining the decision.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid reason or non-waiting guest.
    pub fn reject_lobby_guest(
        &mut self,
        guest_id: GuestId,
        reason: impl Into<String>,
    ) -> Result<(), GuestError> {
        let reason = reason.into();
        if reason.trim().is_empty() || reason.len() > MAX_REASON_BYTES {
            return Err(GuestError::InvalidReason);
        }
        let entry = self
            .lobby
            .get_mut(&guest_id)
            .ok_or(GuestError::LobbyEntryNotFound)?;
        if entry.state != LobbyState::Waiting {
            return Err(GuestError::LobbyEntryNotWaiting);
        }
        entry.state = LobbyState::Rejected { reason };
        Ok(())
    }

    #[must_use]
    pub fn lobby_entry(&self, guest_id: GuestId) -> Option<&LobbyEntry> {
        self.lobby.get(&guest_id)
    }

    /// Admits a healthy waiting guest into its reserved or first free slot.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid lobby state, unhealthy preflight, duplicate
    /// session ID, exhausted capacity, or an unavailable slot.
    pub fn admit_lobby_guest(
        &mut self,
        guest_id: GuestId,
        session_id: GuestSessionId,
        now_ms: u64,
    ) -> Result<GuestSlot, GuestError> {
        let entry = self
            .lobby
            .get(&guest_id)
            .ok_or(GuestError::LobbyEntryNotFound)?;
        if entry.state != LobbyState::Waiting {
            return Err(GuestError::LobbyEntryNotWaiting);
        }
        if !entry.preflight.is_ready() {
            return Err(GuestError::PreflightNotReady);
        }
        if self.sessions.len() >= MAX_GUEST_SESSIONS {
            return Err(GuestError::SessionLimitReached);
        }
        if self
            .sessions
            .values()
            .any(|session| session.id == session_id)
        {
            return Err(GuestError::SessionExists);
        }
        let slot = if let Some(slot) = entry.reserved_slot {
            if self.sessions.values().any(|session| session.slot == slot) {
                return Err(GuestError::SlotUnavailable(slot));
            }
            slot
        } else {
            (1..=8)
                .map(GuestSlot)
                .find(|slot| !self.slot_is_reserved(*slot, now_ms, Some(guest_id)))
                .ok_or(GuestError::SessionLimitReached)?
        };
        let identity = entry.identity.clone();
        let health = entry.preflight.signal;
        self.sessions.insert(
            guest_id,
            GuestSession {
                id: session_id,
                identity,
                slot,
                connection: SessionConnection::Connected,
                return_video: ReturnVideoSelection::default(),
                talkback: TalkbackState::default(),
                health,
                admitted_at_ms: now_ms,
            },
        );
        self.lobby.remove(&guest_id);
        Ok(slot)
    }

    /// Marks a session disconnected while retaining its slot and routing.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent session or a reconnect deadline that is
    /// not in the future.
    pub fn disconnect_guest(
        &mut self,
        guest_id: GuestId,
        now_ms: u64,
        reconnect_until_ms: u64,
    ) -> Result<(), GuestError> {
        if reconnect_until_ms <= now_ms {
            return Err(GuestError::InvalidReconnectWindow);
        }
        let session = self
            .sessions
            .get_mut(&guest_id)
            .ok_or(GuestError::SessionNotFound)?;
        session.connection = SessionConnection::Disconnected { reconnect_until_ms };
        Ok(())
    }

    /// Reconnects a retained session without changing its slot or routing.
    ///
    /// # Errors
    ///
    /// Returns an error for an absent/already-connected session or expired
    /// reconnect window.
    pub fn reconnect_guest(&mut self, guest_id: GuestId, now_ms: u64) -> Result<(), GuestError> {
        let session = self
            .sessions
            .get_mut(&guest_id)
            .ok_or(GuestError::SessionNotFound)?;
        match session.connection {
            SessionConnection::Connected => return Err(GuestError::SessionAlreadyConnected),
            SessionConnection::Disconnected { reconnect_until_ms }
                if now_ms > reconnect_until_ms =>
            {
                return Err(GuestError::ReconnectExpired);
            }
            SessionConnection::Disconnected { .. } => {}
        }
        session.connection = SessionConnection::Connected;
        Ok(())
    }

    /// Ends a retained session and frees its slot.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::SessionNotFound`] for an unknown guest.
    pub fn end_session(&mut self, guest_id: GuestId) -> Result<GuestSession, GuestError> {
        self.sessions
            .remove(&guest_id)
            .ok_or(GuestError::SessionNotFound)
    }

    #[must_use]
    pub fn session(&self, guest_id: GuestId) -> Option<&GuestSession> {
        self.sessions.get(&guest_id)
    }

    pub fn sessions(&self) -> impl Iterator<Item = &GuestSession> {
        self.sessions.values()
    }

    /// Selects the video-only return source for a retained session.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::SessionNotFound`] for an unknown guest.
    pub fn set_return_video(
        &mut self,
        guest_id: GuestId,
        selection: ReturnVideoSelection,
    ) -> Result<(), GuestError> {
        self.sessions
            .get_mut(&guest_id)
            .ok_or(GuestError::SessionNotFound)?
            .return_video = selection;
        Ok(())
    }

    /// Sets private talkback routing for a retained session.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::SessionNotFound`] for an unknown guest.
    pub fn set_talkback(
        &mut self,
        guest_id: GuestId,
        state: TalkbackState,
    ) -> Result<(), GuestError> {
        self.sessions
            .get_mut(&guest_id)
            .ok_or(GuestError::SessionNotFound)?
            .talkback = state;
        Ok(())
    }

    /// Updates transport-reported health without interpreting transport data.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::SessionNotFound`] for an unknown guest.
    pub fn update_session_health(
        &mut self,
        guest_id: GuestId,
        health: LatencyEchoHealth,
    ) -> Result<(), GuestError> {
        self.sessions
            .get_mut(&guest_id)
            .ok_or(GuestError::SessionNotFound)?
            .health = health;
        Ok(())
    }

    /// Builds a stable mix-minus matrix ordered by slot and guest ID.
    ///
    /// Only connected guests participate. Every microphone feeds program and
    /// every other guest return, but never its own return. Returns use a clean
    /// program bed, and manager talkback can feed only its selected guest, so
    /// talkback cannot leak to program or another guest.
    #[must_use]
    pub fn mix_minus_matrix(&self) -> MixMinusMatrix {
        let mut connected: Vec<_> = self
            .sessions
            .values()
            .filter(|session| session.connection == SessionConnection::Connected)
            .collect();
        connected.sort_by_key(|session| (session.slot, session.identity.id));

        let mut routes = Vec::new();
        for session in &connected {
            routes.push(MixRoute {
                source: MixSource::GuestMicrophone(session.identity.id),
                destination: MixDestination::Program,
                gain: Gain::UNITY,
            });
        }
        for destination in &connected {
            routes.push(MixRoute {
                source: MixSource::CleanProgram,
                destination: MixDestination::GuestReturn(destination.identity.id),
                gain: Gain::UNITY,
            });
            for source in &connected {
                if source.identity.id != destination.identity.id {
                    routes.push(MixRoute {
                        source: MixSource::GuestMicrophone(source.identity.id),
                        destination: MixDestination::GuestReturn(destination.identity.id),
                        gain: Gain::UNITY,
                    });
                }
            }
            if destination.talkback == TalkbackState::On {
                routes.push(MixRoute {
                    source: MixSource::ManagerTalkback,
                    destination: MixDestination::GuestReturn(destination.identity.id),
                    gain: Gain::UNITY,
                });
            }
        }
        MixMinusMatrix { routes }
    }

    /// Creates a bounded group from known guest identities.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate/too many groups, invalid group size, or
    /// an unknown member.
    pub fn create_chat_group(
        &mut self,
        id: ChatGroupId,
        members: impl IntoIterator<Item = GuestId>,
    ) -> Result<(), GuestError> {
        if self.chat_groups.contains_key(&id) {
            return Err(GuestError::ChatGroupExists);
        }
        if self.chat_groups.len() >= MAX_CHAT_GROUPS {
            return Err(GuestError::ChatGroupLimitReached);
        }
        let members: BTreeSet<_> = members.into_iter().collect();
        if members.is_empty() {
            return Err(GuestError::ChatGroupEmpty);
        }
        if members.len() > MAX_CHAT_GROUP_MEMBERS {
            return Err(GuestError::ChatGroupTooLarge);
        }
        if members
            .iter()
            .any(|guest_id| !self.identities.contains_key(guest_id))
        {
            return Err(GuestError::ChatParticipantNotFound);
        }
        self.chat_groups.insert(id, members);
        Ok(())
    }

    /// Appends a private or group message, evicting the oldest global message
    /// when the fixed history bound is reached.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid participants/audience/body or a duplicate
    /// retained message ID.
    pub fn send_chat(
        &mut self,
        id: ChatMessageId,
        sender: GuestId,
        audience: ChatAudience,
        body: impl Into<String>,
        sent_at_ms: u64,
    ) -> Result<(), GuestError> {
        let body = body.into();
        if body.trim().is_empty() || body.len() > MAX_CHAT_BODY_BYTES {
            return Err(GuestError::InvalidChatBody);
        }
        if !self.identities.contains_key(&sender) {
            return Err(GuestError::ChatParticipantNotFound);
        }
        if self.chat_messages.iter().any(|message| message.id == id) {
            return Err(GuestError::ChatMessageExists);
        }
        match audience {
            ChatAudience::Private { first, second } => {
                if first == second || !audience.contains_private_guest(sender) {
                    return Err(GuestError::ChatAccessDenied);
                }
                if !self.identities.contains_key(&first) || !self.identities.contains_key(&second) {
                    return Err(GuestError::ChatParticipantNotFound);
                }
            }
            ChatAudience::Group(group_id) => {
                let members = self
                    .chat_groups
                    .get(&group_id)
                    .ok_or(GuestError::ChatGroupNotFound)?;
                if !members.contains(&sender) {
                    return Err(GuestError::ChatAccessDenied);
                }
            }
        }
        if self.chat_messages.len() == MAX_CHAT_MESSAGES {
            self.chat_messages.pop_front();
        }
        self.chat_messages.push_back(ChatMessage {
            id,
            sender,
            audience,
            sent_at_ms,
            body: Some(body),
            redaction_flags: BTreeSet::new(),
        });
        Ok(())
    }

    /// Redacts a retained message and records who requested the redaction.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::ChatMessageNotFound`] when the message was evicted
    /// or never existed.
    pub fn redact_chat(
        &mut self,
        id: ChatMessageId,
        flag: RedactionFlag,
    ) -> Result<(), GuestError> {
        let message = self
            .chat_messages
            .iter_mut()
            .find(|message| message.id == id)
            .ok_or(GuestError::ChatMessageNotFound)?;
        message.body = None;
        message.redaction_flags.insert(flag);
        Ok(())
    }

    /// Returns messages only when `viewer` belongs to the requested audience.
    ///
    /// # Errors
    ///
    /// Returns [`GuestError::ChatAccessDenied`] when the viewer is not a member.
    pub fn chat_messages_for(
        &self,
        viewer: GuestId,
        audience: ChatAudience,
    ) -> Result<Vec<&ChatMessage>, GuestError> {
        if !self.audience_contains(audience, viewer)? {
            return Err(GuestError::ChatAccessDenied);
        }
        Ok(self
            .chat_messages
            .iter()
            .filter(|message| message.audience == audience)
            .collect())
    }

    /// Returns a conversation for a trusted manager adapter.
    #[must_use]
    pub fn chat_messages_for_manager(&self, audience: ChatAudience) -> Vec<&ChatMessage> {
        self.chat_messages
            .iter()
            .filter(|message| message.audience == audience)
            .collect()
    }

    #[must_use]
    pub fn retained_chat_message_count(&self) -> usize {
        self.chat_messages.len()
    }

    fn audience_contains(
        &self,
        audience: ChatAudience,
        guest_id: GuestId,
    ) -> Result<bool, GuestError> {
        match audience {
            ChatAudience::Private { .. } => Ok(audience.contains_private_guest(guest_id)),
            ChatAudience::Group(group_id) => self
                .chat_groups
                .get(&group_id)
                .map(|members| members.contains(&guest_id))
                .ok_or(GuestError::ChatGroupNotFound),
        }
    }

    fn slot_is_reserved(
        &self,
        slot: GuestSlot,
        now_ms: u64,
        except_lobby_guest: Option<GuestId>,
    ) -> bool {
        self.sessions.values().any(|session| session.slot == slot)
            || self.invitations.values().any(|invitation| {
                invitation.reserved_slot == Some(slot)
                    && invitation.status_at(now_ms) == InvitationStatus::Available
            })
            || self.lobby.iter().any(|(guest_id, entry)| {
                Some(*guest_id) != except_lobby_guest
                    && entry.state == LobbyState::Waiting
                    && entry.reserved_slot == Some(slot)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(value: u128) -> NonZeroU128 {
        NonZeroU128::new(value).unwrap()
    }

    fn invitation_id(value: u128) -> InvitationId {
        InvitationId::new(nz(value))
    }

    fn guest_id(value: u128) -> GuestId {
        GuestId::new(nz(value))
    }

    fn session_id(value: u128) -> GuestSessionId {
        GuestSessionId::new(nz(value))
    }

    fn message_id(value: u128) -> ChatMessageId {
        ChatMessageId::new(nz(value))
    }

    fn group_id(value: u128) -> ChatGroupId {
        ChatGroupId::new(nz(value))
    }

    fn ready_preflight() -> LobbyPreflight {
        LobbyPreflight {
            camera: DeviceHealth::Ready,
            microphone: DeviceHealth::Ready,
            speaker: DeviceHealth::Ready,
            signal: LatencyEchoHealth::new(Some(80), Some(40.0)).unwrap(),
        }
    }

    fn invite_and_enter(manager: &mut GuestManager, number: u128, slot: Option<GuestSlot>) {
        manager
            .create_invitation(invitation_id(number), 10_000, slot, 0)
            .unwrap();
        manager
            .enter_lobby(
                invitation_id(number),
                GuestIdentity::new(guest_id(number), format!("Guest {number}")).unwrap(),
                ready_preflight(),
                1,
            )
            .unwrap();
    }

    fn admit(manager: &mut GuestManager, number: u128) {
        invite_and_enter(manager, number, None);
        manager
            .admit_lobby_guest(guest_id(number), session_id(number), 2)
            .unwrap();
    }

    #[test]
    fn invitation_expiry_reservation_revocation_and_one_use_are_enforced() {
        let mut manager = GuestManager::new();
        let slot = GuestSlot::new(3).unwrap();
        manager
            .create_invitation(invitation_id(1), 100, Some(slot), 10)
            .unwrap();
        assert_eq!(
            manager.create_invitation(invitation_id(2), 100, Some(slot), 10),
            Err(GuestError::SlotUnavailable(slot))
        );
        assert_eq!(
            manager.enter_lobby(
                invitation_id(1),
                GuestIdentity::new(guest_id(1), "First").unwrap(),
                ready_preflight(),
                100,
            ),
            Err(GuestError::InvitationExpired)
        );

        manager
            .create_invitation(invitation_id(3), 200, Some(slot), 100)
            .unwrap();
        manager
            .enter_lobby(
                invitation_id(3),
                GuestIdentity::new(guest_id(1), "First").unwrap(),
                ready_preflight(),
                101,
            )
            .unwrap();
        assert_eq!(
            manager.enter_lobby(
                invitation_id(3),
                GuestIdentity::new(guest_id(2), "Second").unwrap(),
                ready_preflight(),
                102,
            ),
            Err(GuestError::InvitationConsumed)
        );
        assert_eq!(
            manager.admit_lobby_guest(guest_id(1), session_id(1), 103),
            Ok(slot)
        );

        manager
            .create_invitation(invitation_id(4), 300, None, 200)
            .unwrap();
        manager.revoke_invitation(invitation_id(4)).unwrap();
        assert_eq!(
            manager.enter_lobby(
                invitation_id(4),
                GuestIdentity::new(guest_id(4), "Revoked").unwrap(),
                ready_preflight(),
                201,
            ),
            Err(GuestError::InvitationRevoked)
        );
    }

    #[test]
    fn preflight_and_eight_session_limit_are_enforced() {
        let mut manager = GuestManager::new();
        manager
            .create_invitation(invitation_id(99), 100, None, 0)
            .unwrap();
        let mut unhealthy = ready_preflight();
        unhealthy.microphone = DeviceHealth::PermissionDenied;
        manager
            .enter_lobby(
                invitation_id(99),
                GuestIdentity::new(guest_id(99), "Needs mic").unwrap(),
                unhealthy,
                1,
            )
            .unwrap();
        assert_eq!(
            manager.admit_lobby_guest(guest_id(99), session_id(99), 2),
            Err(GuestError::PreflightNotReady)
        );

        for number in 1..=8 {
            admit(&mut manager, number);
        }
        let slots: BTreeSet<_> = manager.sessions().map(|session| session.slot).collect();
        assert_eq!(slots.len(), MAX_GUEST_SESSIONS);
        assert_eq!(manager.sessions().count(), MAX_GUEST_SESSIONS);
        invite_and_enter(&mut manager, 9, None);
        assert_eq!(
            manager.admit_lobby_guest(guest_id(9), session_id(9), 2),
            Err(GuestError::SessionLimitReached)
        );
    }

    #[test]
    fn reconnect_preserves_slot_return_routing_talkback_and_health() {
        let mut manager = GuestManager::new();
        admit(&mut manager, 1);
        manager
            .set_return_video(guest_id(1), ReturnVideoSelection::Preview)
            .unwrap();
        manager
            .set_talkback(guest_id(1), TalkbackState::On)
            .unwrap();
        let health = LatencyEchoHealth::new(Some(240), Some(25.0)).unwrap();
        manager.update_session_health(guest_id(1), health).unwrap();
        let before = manager.session(guest_id(1)).unwrap().clone();

        manager.disconnect_guest(guest_id(1), 10, 50).unwrap();
        assert!(manager.mix_minus_matrix().routes().is_empty());
        manager.reconnect_guest(guest_id(1), 50).unwrap();
        let after = manager.session(guest_id(1)).unwrap();
        assert_eq!(after.slot, before.slot);
        assert_eq!(after.return_video, before.return_video);
        assert_eq!(after.talkback, before.talkback);
        assert_eq!(after.health, before.health);
        assert_eq!(manager.disconnect_guest(guest_id(1), 60, 70), Ok(()));
        assert_eq!(
            manager.reconnect_guest(guest_id(1), 71),
            Err(GuestError::ReconnectExpired)
        );
    }

    #[test]
    fn return_video_mix_minus_and_talkback_never_leak_program_audio() {
        let mut manager = GuestManager::new();
        admit(&mut manager, 2);
        admit(&mut manager, 1);
        manager
            .set_return_video(guest_id(1), ReturnVideoSelection::Program)
            .unwrap();
        manager
            .set_talkback(guest_id(1), TalkbackState::On)
            .unwrap();

        let routes = manager.mix_minus_matrix().routes().to_vec();
        for guest in [guest_id(1), guest_id(2)] {
            assert!(!routes.iter().any(|route| {
                route.source == MixSource::GuestMicrophone(guest)
                    && route.destination == MixDestination::GuestReturn(guest)
            }));
            assert!(routes.iter().any(|route| {
                route.source == MixSource::CleanProgram
                    && route.destination == MixDestination::GuestReturn(guest)
            }));
        }
        assert!(routes.iter().any(|route| {
            route.source == MixSource::ManagerTalkback
                && route.destination == MixDestination::GuestReturn(guest_id(1))
        }));
        assert!(!routes.iter().any(|route| {
            route.source == MixSource::ManagerTalkback
                && (route.destination == MixDestination::Program
                    || route.destination == MixDestination::GuestReturn(guest_id(2)))
        }));
        assert_eq!(routes[0].source, MixSource::GuestMicrophone(guest_id(2)));
        assert_eq!(routes[1].source, MixSource::GuestMicrophone(guest_id(1)));
    }

    #[test]
    fn manager_permissions_are_action_and_resource_scoped() {
        let all_sessions = GuestManagerPermission::ManageSession(GuestScope::All);
        let first_session = GuestManagerPermission::ManageSession(GuestScope::Guest(guest_id(1)));
        let second_session = GuestManagerPermission::ManageSession(GuestScope::Guest(guest_id(2)));
        assert!(all_sessions.allows(first_session));
        assert!(!first_session.allows(second_session));
        assert!(
            !first_session.allows(GuestManagerPermission::UseTalkback(GuestScope::Guest(
                guest_id(1)
            )))
        );

        let group = ChatScope::Group(group_id(1));
        assert!(
            GuestManagerPermission::ModerateChat(ChatScope::All)
                .allows(GuestManagerPermission::ModerateChat(group))
        );
        assert!(!GuestManagerPermission::ModerateChat(group).allows(
            GuestManagerPermission::ModerateChat(ChatScope::PrivateWith(guest_id(1)))
        ));
    }

    #[test]
    fn chat_is_bounded_private_and_redactable() {
        let mut manager = GuestManager::new();
        for number in 1..=3 {
            invite_and_enter(&mut manager, number, None);
        }
        let private = ChatAudience::private(guest_id(2), guest_id(1)).unwrap();
        manager
            .send_chat(message_id(1), guest_id(1), private, "secret", 1)
            .unwrap();
        assert_eq!(
            manager.chat_messages_for(guest_id(3), private),
            Err(GuestError::ChatAccessDenied)
        );
        manager
            .redact_chat(message_id(1), RedactionFlag::Moderator)
            .unwrap();
        let private_messages = manager.chat_messages_for(guest_id(2), private).unwrap();
        assert_eq!(private_messages[0].body(), None);
        assert!(private_messages[0].is_redacted());

        let group = group_id(1);
        manager
            .create_chat_group(group, [guest_id(1), guest_id(2)])
            .unwrap();
        let audience = ChatAudience::Group(group);
        assert_eq!(
            manager.send_chat(message_id(2), guest_id(3), audience, "intrusion", 2),
            Err(GuestError::ChatAccessDenied)
        );
        assert_eq!(
            manager.send_chat(
                message_id(2),
                guest_id(1),
                audience,
                "x".repeat(MAX_CHAT_BODY_BYTES + 1),
                2,
            ),
            Err(GuestError::InvalidChatBody)
        );

        for index in 0..=MAX_CHAT_MESSAGES {
            manager
                .send_chat(
                    message_id(1_000 + index as u128),
                    guest_id(1),
                    audience,
                    format!("message {index}"),
                    index as u64,
                )
                .unwrap();
        }
        assert_eq!(manager.retained_chat_message_count(), MAX_CHAT_MESSAGES);
        assert_eq!(
            manager.redact_chat(message_id(1), RedactionFlag::Sender),
            Err(GuestError::ChatMessageNotFound)
        );
    }

    #[test]
    fn latency_and_echo_health_thresholds_are_deterministic() {
        let healthy = LatencyEchoHealth::new(Some(150), Some(35.0)).unwrap();
        assert_eq!(healthy.latency_severity(), HealthSeverity::Healthy);
        assert_eq!(healthy.echo_severity(), HealthSeverity::Healthy);
        assert_eq!(healthy.severity(), HealthSeverity::Healthy);

        let degraded = LatencyEchoHealth::new(Some(151), Some(20.0)).unwrap();
        assert_eq!(degraded.severity(), HealthSeverity::Degraded);
        let unhealthy = LatencyEchoHealth::new(Some(301), Some(19.9)).unwrap();
        assert_eq!(unhealthy.severity(), HealthSeverity::Unhealthy);
        assert_eq!(
            LatencyEchoHealth::new(None, Some(f32::NAN)),
            Err(GuestError::InvalidEchoReturnLoss)
        );
        assert!(
            !LobbyPreflight {
                signal: unhealthy,
                ..ready_preflight()
            }
            .is_ready()
        );
    }
}
