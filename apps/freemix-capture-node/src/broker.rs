use core::{fmt, num::NonZeroU128};
use std::collections::VecDeque;

use fm_frame::{MediaTiming, ResourceLease};
use fm_protocol::EngineIdentity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionState {
    Unknown,
    Denied,
    Granted,
    Restricted,
}

impl fmt::Display for PermissionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unknown => "unknown",
            Self::Denied => "denied",
            Self::Granted => "granted",
            Self::Restricted => "restricted",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionKind {
    Camera,
    ScreenRecording,
    ApplicationAudio,
}

impl PermissionKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::ScreenRecording => "screen recording",
            Self::ApplicationAudio => "application audio",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionStatus {
    kind: PermissionKind,
    state: PermissionState,
    request_attempted: bool,
}

impl PermissionStatus {
    #[must_use]
    pub const fn new(kind: PermissionKind) -> Self {
        Self {
            kind,
            state: PermissionState::Unknown,
            request_attempted: false,
        }
    }

    #[must_use]
    pub const fn kind(self) -> PermissionKind {
        self.kind
    }

    #[must_use]
    pub const fn state(self) -> PermissionState {
        self.state
    }

    #[must_use]
    pub const fn request_attempted(self) -> bool {
        self.request_attempted
    }

    /// Marks the one interactive permission request allowed for this session.
    ///
    /// Returns `false` after the first request or when the OS has already
    /// resolved the permission, preventing prompt loops.
    pub fn begin_interactive_request(&mut self) -> bool {
        if self.state == PermissionState::Unknown && !self.request_attempted {
            self.request_attempted = true;
            true
        } else {
            false
        }
    }

    #[must_use]
    pub const fn remediation(self) -> Option<&'static str> {
        match self.state {
            PermissionState::Unknown => {
                Some("Grant access when prompted from the logged-in desktop session.")
            }
            PermissionState::Denied => {
                Some("Enable access in system privacy settings, then restart capture serving.")
            }
            PermissionState::Granted => None,
            PermissionState::Restricted => {
                Some("Access is restricted by system policy; contact the device administrator.")
            }
        }
    }

    pub const fn transition(&mut self, to: PermissionState) -> PermissionTransition {
        let transition = PermissionTransition {
            kind: self.kind,
            from: self.state,
            to,
        };
        self.state = to;
        transition
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionTransition {
    pub kind: PermissionKind,
    pub from: PermissionState,
    pub to: PermissionState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureSource {
    Camera,
    Screen,
    ApplicationAudio,
}

impl CaptureSource {
    #[must_use]
    pub const fn required_permission(self) -> PermissionKind {
        match self {
            Self::Camera => PermissionKind::Camera,
            Self::Screen => PermissionKind::ScreenRecording,
            Self::ApplicationAudio => PermissionKind::ApplicationAudio,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PublicationId(NonZeroU128);

impl PublicationId {
    #[must_use]
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU128 {
        self.0
    }
}

impl fmt::Display for PublicationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimedMediaDescriptor {
    source: CaptureSource,
    media_kind: MediaKind,
    timing: MediaTiming,
}

impl TimedMediaDescriptor {
    #[must_use]
    pub const fn new(source: CaptureSource, media_kind: MediaKind, timing: MediaTiming) -> Self {
        Self {
            source,
            media_kind,
            timing,
        }
    }

    #[must_use]
    pub const fn source(self) -> CaptureSource {
        self.source
    }

    #[must_use]
    pub const fn media_kind(self) -> MediaKind {
        self.media_kind
    }

    #[must_use]
    pub const fn timing(self) -> MediaTiming {
        self.timing
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Publication {
    id: PublicationId,
    descriptor: TimedMediaDescriptor,
    lease: Option<ResourceLease>,
}

impl Publication {
    #[must_use]
    pub const fn new(
        id: PublicationId,
        descriptor: TimedMediaDescriptor,
        lease: Option<ResourceLease>,
    ) -> Self {
        Self {
            id,
            descriptor,
            lease,
        }
    }

    #[must_use]
    pub const fn id(&self) -> PublicationId {
        self.id
    }

    #[must_use]
    pub const fn descriptor(&self) -> TimedMediaDescriptor {
        self.descriptor
    }

    #[must_use]
    pub const fn lease(&self) -> Option<&ResourceLease> {
        self.lease.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    InvalidCapacity { value: usize, maximum: usize },
    Full { maximum: usize },
    Duplicate(PublicationId),
    Unknown(PublicationId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity { value, maximum } => {
                write!(
                    formatter,
                    "publication capacity {value} must be between 1 and {maximum}"
                )
            }
            Self::Full { maximum } => write!(formatter, "publication registry is full ({maximum})"),
            Self::Duplicate(id) => write!(formatter, "publication {id} already exists"),
            Self::Unknown(id) => write!(formatter, "publication {id} does not exist"),
        }
    }
}

impl std::error::Error for RegistryError {}

#[derive(Debug)]
pub struct PublicationRegistry {
    maximum: usize,
    publications: Vec<Publication>,
}

impl PublicationRegistry {
    pub const MAX_PUBLICATIONS: usize = 256;

    /// Creates a registry with a hard, preallocated publication bound.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidCapacity`] for zero or excessive bounds.
    pub fn new(maximum: usize) -> Result<Self, RegistryError> {
        if !(1..=Self::MAX_PUBLICATIONS).contains(&maximum) {
            return Err(RegistryError::InvalidCapacity {
                value: maximum,
                maximum: Self::MAX_PUBLICATIONS,
            });
        }
        Ok(Self {
            maximum,
            publications: Vec::with_capacity(maximum),
        })
    }

    #[must_use]
    pub const fn maximum(&self) -> usize {
        self.maximum
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.publications.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.publications.is_empty()
    }

    #[must_use]
    pub fn publications(&self) -> &[Publication] {
        &self.publications
    }

    #[must_use]
    pub fn get(&self, id: PublicationId) -> Option<&Publication> {
        self.publications.iter().find(|item| item.id == id)
    }

    /// Publishes descriptor metadata and an optional erased media resource.
    ///
    /// # Errors
    ///
    /// Returns an error when the ID exists or the configured bound is reached.
    pub fn publish(&mut self, publication: Publication) -> Result<(), RegistryError> {
        if self.get(publication.id).is_some() {
            return Err(RegistryError::Duplicate(publication.id));
        }
        if self.publications.len() == self.maximum {
            return Err(RegistryError::Full {
                maximum: self.maximum,
            });
        }
        self.publications.push(publication);
        Ok(())
    }

    /// Removes a publication and drops its resource lease.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Unknown`] when no publication has the ID.
    pub fn revoke(&mut self, id: PublicationId) -> Result<Publication, RegistryError> {
        let index = self
            .publications
            .iter()
            .position(|item| item.id == id)
            .ok_or(RegistryError::Unknown(id))?;
        Ok(self.publications.swap_remove(index))
    }

    #[must_use]
    pub fn revoke_all(&mut self) -> LogoutSummary {
        let summary = LogoutSummary {
            revoked_publications: self.publications.len(),
            revoked_leases: self
                .publications
                .iter()
                .filter(|publication| publication.lease.is_some())
                .count(),
        };
        self.publications.clear();
        summary
    }

    fn revoke_permission(&mut self, permission: PermissionKind) -> LogoutSummary {
        let mut summary = LogoutSummary::default();
        self.publications.retain(|publication| {
            if publication.descriptor.source.required_permission() == permission {
                summary.revoked_publications += 1;
                summary.revoked_leases += usize::from(publication.lease.is_some());
                false
            } else {
                true
            }
        });
        summary
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LogoutSummary {
    pub revoked_publications: usize,
    pub revoked_leases: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackoffPolicy {
    initial_ms: u64,
    maximum_ms: u64,
}

impl BackoffPolicy {
    /// Creates a bounded exponential reconnect policy.
    ///
    /// # Errors
    ///
    /// Both delays must be nonzero and the initial delay cannot exceed the cap.
    pub const fn new(initial_ms: u64, maximum_ms: u64) -> Result<Self, BackoffPolicyError> {
        if initial_ms == 0 {
            Err(BackoffPolicyError::ZeroInitial)
        } else if maximum_ms == 0 {
            Err(BackoffPolicyError::ZeroMaximum)
        } else if initial_ms > maximum_ms {
            Err(BackoffPolicyError::InitialExceedsMaximum {
                initial_ms,
                maximum_ms,
            })
        } else {
            Ok(Self {
                initial_ms,
                maximum_ms,
            })
        }
    }

    #[must_use]
    pub const fn initial_ms(self) -> u64 {
        self.initial_ms
    }

    #[must_use]
    pub const fn maximum_ms(self) -> u64 {
        self.maximum_ms
    }

    fn delay_for_attempt(self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(63);
        let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
        self.initial_ms
            .saturating_mul(multiplier)
            .min(self.maximum_ms)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackoffPolicyError {
    ZeroInitial,
    ZeroMaximum,
    InitialExceedsMaximum { initial_ms: u64, maximum_ms: u64 },
}

impl fmt::Display for BackoffPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroInitial => formatter.write_str("initial reconnect backoff must be nonzero"),
            Self::ZeroMaximum => formatter.write_str("maximum reconnect backoff must be nonzero"),
            Self::InitialExceedsMaximum {
                initial_ms,
                maximum_ms,
            } => write!(
                formatter,
                "initial reconnect backoff {initial_ms} ms exceeds maximum {maximum_ms} ms"
            ),
        }
    }
}

impl std::error::Error for BackoffPolicyError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconnectRecord {
    pub attempt: u32,
    pub disconnected_at_ms: u64,
    pub delay_ms: u64,
    pub reconnect_at_ms: u64,
    pub reason: String,
}

#[derive(Debug)]
pub struct ReconnectTracker {
    policy: BackoffPolicy,
    consecutive_attempts: u32,
    pending: bool,
    history: VecDeque<ReconnectRecord>,
}

impl ReconnectTracker {
    pub const MAX_HISTORY: usize = 32;

    #[must_use]
    pub fn new(policy: BackoffPolicy) -> Self {
        Self {
            policy,
            consecutive_attempts: 0,
            pending: false,
            history: VecDeque::with_capacity(Self::MAX_HISTORY),
        }
    }

    pub fn record_disconnect(&mut self, disconnected_at_ms: u64, reason: String) {
        self.consecutive_attempts = self.consecutive_attempts.saturating_add(1);
        let delay_ms = self.policy.delay_for_attempt(self.consecutive_attempts);
        if self.history.len() == Self::MAX_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(ReconnectRecord {
            attempt: self.consecutive_attempts,
            disconnected_at_ms,
            delay_ms,
            reconnect_at_ms: disconnected_at_ms.saturating_add(delay_ms),
            reason,
        });
        self.pending = true;
    }

    pub const fn record_connected(&mut self) {
        self.consecutive_attempts = 0;
        self.pending = false;
    }

    pub const fn cancel(&mut self) {
        self.pending = false;
        self.consecutive_attempts = 0;
    }

    #[must_use]
    pub fn pending(&self) -> Option<&ReconnectRecord> {
        self.pending.then(|| self.history.back()).flatten()
    }

    #[must_use]
    pub fn history(&self) -> impl ExactSizeIterator<Item = &ReconnectRecord> {
        self.history.iter()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Connected,
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PairingState {
    Unpaired,
    Paired {
        engine: EngineIdentity,
        connection: ConnectionState,
    },
}

impl PairingState {
    #[must_use]
    pub const fn engine(&self) -> Option<&EngineIdentity> {
        match self {
            Self::Unpaired => None,
            Self::Paired { engine, .. } => Some(engine),
        }
    }

    #[must_use]
    pub const fn connection(&self) -> Option<ConnectionState> {
        match self {
            Self::Unpaired => None,
            Self::Paired { connection, .. } => Some(*connection),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PairingError {
    IdentityMismatch {
        paired: EngineIdentity,
        attempted: EngineIdentity,
    },
    NotPaired,
    AlreadyDisconnected,
}

impl fmt::Display for PairingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityMismatch { paired, attempted } => write!(
                formatter,
                "paired engine identity {}:{}:{} does not match {}:{}:{}",
                paired.engine_id,
                paired.state_epoch,
                paired.log_id,
                attempted.engine_id,
                attempted.state_epoch,
                attempted.log_id
            ),
            Self::NotPaired => formatter.write_str("capture session is not paired"),
            Self::AlreadyDisconnected => {
                formatter.write_str("paired engine is already disconnected")
            }
        }
    }
}

impl std::error::Error for PairingError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Active,
    LoggedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionUpdate {
    pub transition: PermissionTransition,
    pub revoked_publications: usize,
    pub revoked_leases: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrokerError {
    BlankSessionId,
    LoggedOut,
    Pairing(PairingError),
    PermissionDenied {
        permission: PermissionKind,
        state: PermissionState,
        remediation: &'static str,
    },
    Registry(RegistryError),
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankSessionId => formatter.write_str("session id must not be blank"),
            Self::LoggedOut => formatter.write_str("user session is logged out"),
            Self::Pairing(error) => error.fmt(formatter),
            Self::PermissionDenied {
                permission,
                state,
                remediation,
            } => write!(
                formatter,
                "{} permission is {state}: {remediation}",
                permission.label()
            ),
            Self::Registry(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BrokerError {}

impl From<PairingError> for BrokerError {
    fn from(value: PairingError) -> Self {
        Self::Pairing(value)
    }
}

impl From<RegistryError> for BrokerError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}

#[derive(Debug)]
pub struct CaptureBroker {
    session_id: String,
    session_state: SessionState,
    permissions: [PermissionStatus; 3],
    pairing: PairingState,
    publications: PublicationRegistry,
    reconnect: ReconnectTracker,
}

impl CaptureBroker {
    /// Creates an inactive-media broker for one logged-in user session.
    ///
    /// # Errors
    ///
    /// Returns an error for a blank session ID or invalid registry capacity.
    pub fn new(
        session_id: String,
        maximum_publications: usize,
        backoff: BackoffPolicy,
    ) -> Result<Self, BrokerError> {
        if session_id.trim().is_empty() {
            return Err(BrokerError::BlankSessionId);
        }
        Ok(Self {
            session_id,
            session_state: SessionState::Active,
            permissions: [
                PermissionStatus::new(PermissionKind::Camera),
                PermissionStatus::new(PermissionKind::ScreenRecording),
                PermissionStatus::new(PermissionKind::ApplicationAudio),
            ],
            pairing: PairingState::Unpaired,
            publications: PublicationRegistry::new(maximum_publications)?,
            reconnect: ReconnectTracker::new(backoff),
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub const fn session_state(&self) -> SessionState {
        self.session_state
    }

    #[must_use]
    pub fn permission(&self, kind: PermissionKind) -> PermissionStatus {
        let [camera, screen_recording, application_audio] = self.permissions;
        match kind {
            PermissionKind::Camera => camera,
            PermissionKind::ScreenRecording => screen_recording,
            PermissionKind::ApplicationAudio => application_audio,
        }
    }

    pub fn begin_permission_request(&mut self, kind: PermissionKind) -> bool {
        self.permission_mut(kind).begin_interactive_request()
    }

    pub fn update_permission(
        &mut self,
        kind: PermissionKind,
        state: PermissionState,
    ) -> PermissionUpdate {
        let transition = self.permission_mut(kind).transition(state);
        let revoked = if state == PermissionState::Granted {
            LogoutSummary::default()
        } else {
            self.publications.revoke_permission(kind)
        };
        PermissionUpdate {
            transition,
            revoked_publications: revoked.revoked_publications,
            revoked_leases: revoked.revoked_leases,
        }
    }

    #[must_use]
    pub const fn pairing(&self) -> &PairingState {
        &self.pairing
    }

    /// Pairs once, or reconnects only when the complete engine identity matches.
    ///
    /// # Errors
    ///
    /// Rejects logged-out sessions and identity changes.
    pub fn pair(&mut self, engine: EngineIdentity) -> Result<(), BrokerError> {
        self.require_active()?;
        if let Some(paired) = self.pairing.engine()
            && paired != &engine
        {
            return Err(PairingError::IdentityMismatch {
                paired: paired.clone(),
                attempted: engine,
            }
            .into());
        }
        self.pairing = PairingState::Paired {
            engine,
            connection: ConnectionState::Connected,
        };
        self.reconnect.record_connected();
        Ok(())
    }

    /// Records a dropped engine connection and its capped retry deadline.
    ///
    /// # Errors
    ///
    /// Rejects logged-out, unpaired, or already-disconnected sessions.
    pub fn disconnect(&mut self, at_ms: u64, reason: String) -> Result<(), BrokerError> {
        self.require_active()?;
        match &mut self.pairing {
            PairingState::Unpaired => return Err(PairingError::NotPaired.into()),
            PairingState::Paired { connection, .. }
                if *connection == ConnectionState::Disconnected =>
            {
                return Err(PairingError::AlreadyDisconnected.into());
            }
            PairingState::Paired { connection, .. } => {
                *connection = ConnectionState::Disconnected;
            }
        }
        self.reconnect.record_disconnect(at_ms, reason);
        Ok(())
    }

    #[must_use]
    pub const fn reconnect(&self) -> &ReconnectTracker {
        &self.reconnect
    }

    #[must_use]
    pub const fn publications(&self) -> &PublicationRegistry {
        &self.publications
    }

    /// Publishes timing metadata after pairing and permission checks.
    ///
    /// # Errors
    ///
    /// Rejects inactive sessions, disconnected engines, missing permission, and
    /// registry bound violations.
    pub fn publish(&mut self, publication: Publication) -> Result<(), BrokerError> {
        self.require_active()?;
        match self.pairing.connection() {
            Some(ConnectionState::Connected) => {}
            Some(ConnectionState::Disconnected) | None => {
                return Err(PairingError::NotPaired.into());
            }
        }
        let permission = publication.descriptor.source.required_permission();
        let status = self.permission(permission);
        if status.state != PermissionState::Granted {
            return Err(BrokerError::PermissionDenied {
                permission,
                state: status.state,
                remediation: status
                    .remediation()
                    .unwrap_or("Recheck access in system privacy settings."),
            });
        }
        self.publications.publish(publication)?;
        Ok(())
    }

    /// Ends the user session, dropping every publication resource and pairing.
    #[must_use]
    pub fn logout(&mut self) -> LogoutSummary {
        let summary = self.publications.revoke_all();
        self.pairing = PairingState::Unpaired;
        self.reconnect.cancel();
        self.session_state = SessionState::LoggedOut;
        summary
    }

    fn permission_mut(&mut self, kind: PermissionKind) -> &mut PermissionStatus {
        let [camera, screen_recording, application_audio] = &mut self.permissions;
        match kind {
            PermissionKind::Camera => camera,
            PermissionKind::ScreenRecording => screen_recording,
            PermissionKind::ApplicationAudio => application_audio,
        }
    }

    fn require_active(&self) -> Result<(), BrokerError> {
        if self.session_state == SessionState::Active {
            Ok(())
        } else {
            Err(BrokerError::LoggedOut)
        }
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use fm_frame::{
        BridgeId, ClockDomainId, MediaTiming, NormalizedDuration, NormalizedTimestamp,
        OriginalTimestamp, ReleaseOwner, ReleaseOwnerId, ReleaseOwnership, ResourceId,
        SequenceNumber, TimeBase,
    };

    use super::*;

    fn nonzero(value: u128) -> NonZeroU128 {
        NonZeroU128::new(value).expect("test IDs are nonzero")
    }

    fn engine(id: &str, epoch: u64, log: &str) -> EngineIdentity {
        EngineIdentity {
            engine_id: id.to_owned(),
            state_epoch: epoch,
            log_id: log.to_owned(),
        }
    }

    fn timing(sequence: u64) -> MediaTiming {
        let time_base = TimeBase::new(1, 1_000).unwrap();
        let original = OriginalTimestamp::new(
            fm_frame::MediaTimestamp::new(i64::try_from(sequence).unwrap()),
            time_base,
        );
        MediaTiming::new(
            original,
            NormalizedTimestamp::from_nanos(i64::try_from(sequence).unwrap() * 1_000_000),
            NormalizedDuration::from_nanos(1_000_000).unwrap(),
            ClockDomainId::new(nonzero(50)),
            SequenceNumber::new(sequence),
        )
        .unwrap()
    }

    fn publication(id: u128, source: CaptureSource, with_lease: bool) -> Publication {
        let kind = if source == CaptureSource::ApplicationAudio {
            MediaKind::Audio
        } else {
            MediaKind::Video
        };
        let lease = with_lease.then(|| {
            ResourceLease::new(
                BridgeId::new(nonzero(90)),
                ResourceId::new(nonzero(id + 100)),
                fm_frame::MemoryDomain::Cpu,
                None,
                None,
                ReleaseOwner::new(
                    ReleaseOwnerId::new(nonzero(91)),
                    ReleaseOwnership::OwnerReclaims,
                ),
            )
            .unwrap()
        });
        Publication::new(
            PublicationId::new(nonzero(id)),
            TimedMediaDescriptor::new(source, kind, timing(u64::try_from(id).unwrap())),
            lease,
        )
    }

    fn broker(maximum: usize) -> CaptureBroker {
        CaptureBroker::new(
            "desktop-user".to_owned(),
            maximum,
            BackoffPolicy::new(100, 400).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn permission_transitions_include_remediation_and_do_not_loop_prompts() {
        let mut broker = broker(2);
        let initial = broker.permission(PermissionKind::ScreenRecording);
        assert_eq!(initial.state(), PermissionState::Unknown);
        assert!(initial.remediation().is_some());
        assert!(broker.begin_permission_request(PermissionKind::ScreenRecording));
        assert!(!broker.begin_permission_request(PermissionKind::ScreenRecording));

        let denied =
            broker.update_permission(PermissionKind::ScreenRecording, PermissionState::Denied);
        assert_eq!(denied.transition.from, PermissionState::Unknown);
        assert_eq!(denied.transition.to, PermissionState::Denied);
        assert!(
            broker
                .permission(PermissionKind::ScreenRecording)
                .remediation()
                .unwrap()
                .contains("privacy settings")
        );

        broker.update_permission(PermissionKind::ScreenRecording, PermissionState::Restricted);
        assert!(
            broker
                .permission(PermissionKind::ScreenRecording)
                .remediation()
                .unwrap()
                .contains("system policy")
        );
        broker.update_permission(PermissionKind::ScreenRecording, PermissionState::Granted);
        assert_eq!(
            broker
                .permission(PermissionKind::ScreenRecording)
                .remediation(),
            None
        );
    }

    #[test]
    fn pairing_preserves_complete_engine_identity() {
        let mut broker = broker(2);
        let paired = engine("engine-a", 7, "log-a");
        broker.pair(paired.clone()).unwrap();
        broker.disconnect(1_000, "closed".to_owned()).unwrap();
        broker.pair(paired.clone()).unwrap();
        assert_eq!(broker.pairing().engine(), Some(&paired));
        assert_eq!(
            broker.pairing().connection(),
            Some(ConnectionState::Connected)
        );

        assert!(matches!(
            broker.pair(engine("engine-a", 8, "log-b")),
            Err(BrokerError::Pairing(PairingError::IdentityMismatch { .. }))
        ));
        assert_eq!(broker.pairing().engine(), Some(&paired));
    }

    #[test]
    fn publication_registry_enforces_its_bound_and_unique_ids() {
        let mut registry = PublicationRegistry::new(2).unwrap();
        registry
            .publish(publication(1, CaptureSource::Camera, false))
            .unwrap();
        assert_eq!(
            registry.publish(publication(1, CaptureSource::Camera, false)),
            Err(RegistryError::Duplicate(PublicationId::new(nonzero(1))))
        );
        registry
            .publish(publication(2, CaptureSource::Screen, false))
            .unwrap();
        assert_eq!(
            registry.publish(publication(3, CaptureSource::ApplicationAudio, false)),
            Err(RegistryError::Full { maximum: 2 })
        );
        assert_eq!(registry.len(), 2);
        assert!(matches!(
            PublicationRegistry::new(0),
            Err(RegistryError::InvalidCapacity { .. })
        ));
    }

    #[test]
    fn logout_revokes_publications_leases_pairing_and_reconnect() {
        let mut broker = broker(2);
        broker.pair(engine("engine-a", 1, "log-a")).unwrap();
        broker.update_permission(PermissionKind::Camera, PermissionState::Granted);
        broker
            .publish(publication(1, CaptureSource::Camera, true))
            .unwrap();
        broker
            .disconnect(1_000, "engine stopped".to_owned())
            .unwrap();
        assert_eq!(broker.reconnect().pending().unwrap().delay_ms, 100);

        let summary = broker.logout();
        assert_eq!(
            summary,
            LogoutSummary {
                revoked_publications: 1,
                revoked_leases: 1,
            }
        );
        assert!(broker.publications().is_empty());
        assert_eq!(broker.pairing(), &PairingState::Unpaired);
        assert!(broker.reconnect().pending().is_none());
        assert!(matches!(
            broker.pair(engine("engine-a", 1, "log-a")),
            Err(BrokerError::LoggedOut)
        ));
    }

    #[test]
    fn reconnect_backoff_is_capped_and_resets_after_reconnect() {
        let mut tracker = ReconnectTracker::new(BackoffPolicy::new(100, 250).unwrap());
        tracker.record_disconnect(10, "one".to_owned());
        assert_eq!(tracker.pending().unwrap().delay_ms, 100);
        tracker.record_disconnect(20, "two".to_owned());
        assert_eq!(tracker.pending().unwrap().delay_ms, 200);
        tracker.record_disconnect(30, "three".to_owned());
        assert_eq!(tracker.pending().unwrap().delay_ms, 250);
        tracker.record_connected();
        assert!(tracker.pending().is_none());
        tracker.record_disconnect(40, "again".to_owned());
        assert_eq!(tracker.pending().unwrap().attempt, 1);
        assert_eq!(tracker.history().len(), 4);
    }
}
