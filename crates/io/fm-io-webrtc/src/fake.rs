//! Deterministic in-memory WebRTC transport.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU128;

use crate::{
    AdaptationPlan, DataChannelConfig, DataChannelId, DataChannelRecord, DescriptionKind,
    IceCandidate, IceCandidateKind, IceConfig, IceConnectionState, IceGatheringState, IceServer,
    IceTransportPolicy, ImpairmentProfile, LayerId, MediaRecord, NatScenario, NatType,
    PeerConnection, PeerId, ReconnectIdentity, SessionDescription, SessionId, SessionState,
    SignalingState, TrackConfig, TrackId, TransportHealth, WebRtcError,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FakeSessionConfig {
    pub ice: IceConfig,
    pub nat: NatScenario,
    pub impairment: ImpairmentProfile,
}

#[derive(Clone, Debug)]
struct FakePeer {
    name: String,
    generation: u64,
    token: String,
    connected: bool,
}

/// Owns deterministic peer identities and their in-memory connections.
#[derive(Debug, Default)]
pub struct FakeWebRtcTransport {
    next_id: u128,
    peers: BTreeMap<PeerId, FakePeer>,
    sessions: BTreeMap<SessionId, FakePeerConnection>,
}

impl FakeWebRtcTransport {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    pub fn add_peer(&mut self, name: impl Into<String>) -> ReconnectIdentity {
        let id = PeerId::new(self.allocate_id());
        let generation = 0;
        let token = reconnect_token(id, generation);
        self.peers.insert(
            id,
            FakePeer {
                name: name.into(),
                generation,
                token: token.clone(),
                connected: true,
            },
        );
        ReconnectIdentity::new(id, generation, token)
    }

    #[must_use]
    pub fn peer_name(&self, id: PeerId) -> Option<&str> {
        self.peers.get(&id).map(|peer| peer.name.as_str())
    }

    /// Creates a connection between two registered peers.
    ///
    /// # Errors
    /// Returns [`WebRtcError::UnknownPeer`] if either endpoint is unknown.
    pub fn create_session(
        &mut self,
        local_peer: PeerId,
        remote_peer: PeerId,
        config: FakeSessionConfig,
    ) -> Result<SessionId, WebRtcError> {
        if !self.peers.contains_key(&local_peer) {
            return Err(WebRtcError::UnknownPeer(local_peer));
        }
        if !self.peers.contains_key(&remote_peer) {
            return Err(WebRtcError::UnknownPeer(remote_peer));
        }
        let id = SessionId::new(self.allocate_id());
        self.sessions.insert(
            id,
            FakePeerConnection::new(id, local_peer, remote_peer, config),
        );
        Ok(id)
    }

    #[must_use]
    pub fn session(&self, id: SessionId) -> Option<&FakePeerConnection> {
        self.sessions.get(&id)
    }

    #[must_use]
    pub fn session_mut(&mut self, id: SessionId) -> Option<&mut FakePeerConnection> {
        self.sessions.get_mut(&id)
    }

    /// Marks a peer and all of its sessions as disconnected.
    ///
    /// # Errors
    /// Returns [`WebRtcError::UnknownPeer`] for an unknown identity.
    pub fn disconnect_peer(&mut self, id: PeerId) -> Result<(), WebRtcError> {
        let peer = self
            .peers
            .get_mut(&id)
            .ok_or(WebRtcError::UnknownPeer(id))?;
        peer.connected = false;
        for session in self
            .sessions
            .values_mut()
            .filter(|session| session.local_peer == id || session.remote_peer == id)
        {
            session.disconnect();
        }
        Ok(())
    }

    /// Restores the same peer identity and rotates its one-use reconnect token.
    ///
    /// # Errors
    /// Returns [`WebRtcError::InvalidReconnectIdentity`] for stale or forged identities.
    pub fn reconnect(
        &mut self,
        identity: &ReconnectIdentity,
    ) -> Result<ReconnectIdentity, WebRtcError> {
        let peer = self
            .peers
            .get_mut(&identity.peer_id)
            .ok_or(WebRtcError::InvalidReconnectIdentity)?;
        if peer.connected
            || peer.generation != identity.generation
            || peer.token != identity.token()
        {
            return Err(WebRtcError::InvalidReconnectIdentity);
        }
        peer.connected = true;
        peer.generation = peer.generation.saturating_add(1);
        peer.token = reconnect_token(identity.peer_id, peer.generation);
        for session in self.sessions.values_mut().filter(|session| {
            session.local_peer == identity.peer_id || session.remote_peer == identity.peer_id
        }) {
            session.prepare_reconnect();
        }
        Ok(ReconnectIdentity::new(
            identity.peer_id,
            peer.generation,
            peer.token.clone(),
        ))
    }

    fn allocate_id(&mut self) -> NonZeroU128 {
        let id = NonZeroU128::new(self.next_id).expect("fake ids start at one");
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("fake id space exhausted");
        id
    }
}

fn reconnect_token(id: PeerId, generation: u64) -> String {
    format!("fake-reconnect-{id}-{generation}")
}

#[derive(Debug)]
struct TrackQueue {
    config: TrackConfig,
    records: VecDeque<MediaRecord>,
    last_sequence: Option<u64>,
    adaptation: Option<AdaptationPlan>,
    current_layer: Option<LayerId>,
}

#[derive(Debug)]
struct DataQueue {
    config: DataChannelConfig,
    records: VecDeque<DataChannelRecord>,
    last_sequence: Option<u64>,
}

#[derive(Debug)]
enum PendingPayload {
    Media(TrackId, MediaRecord),
    Data(DataChannelId, DataChannelRecord),
}

#[derive(Debug)]
struct PendingDelivery {
    deliver_at_ms: u64,
    insertion_order: u64,
    payload: PendingPayload,
}

/// Deterministic peer connection with opaque media and data records.
#[derive(Debug)]
pub struct FakePeerConnection {
    id: SessionId,
    pub local_peer: PeerId,
    pub remote_peer: PeerId,
    config: FakeSessionConfig,
    signaling: SignalingState,
    gathering: IceGatheringState,
    ice_connection: IceConnectionState,
    state: SessionState,
    revision: u64,
    negotiated: bool,
    candidates: Vec<IceCandidate>,
    tracks: BTreeMap<TrackId, TrackQueue>,
    channels: BTreeMap<DataChannelId, DataQueue>,
    pending: Vec<PendingDelivery>,
    packet_counter: u64,
    insertion_counter: u64,
    now_ms: u64,
    health: TransportHealth,
    echo_round_trip_total_ms: u64,
}

impl FakePeerConnection {
    #[must_use]
    pub fn new(
        id: SessionId,
        local_peer: PeerId,
        remote_peer: PeerId,
        config: FakeSessionConfig,
    ) -> Self {
        let health = TransportHealth {
            estimated_jitter_ms: config.impairment.jitter_ms,
            ..TransportHealth::default()
        };
        Self {
            id,
            local_peer,
            remote_peer,
            config,
            signaling: SignalingState::Stable,
            gathering: IceGatheringState::New,
            ice_connection: IceConnectionState::New,
            state: SessionState::New,
            revision: 0,
            negotiated: false,
            candidates: Vec::new(),
            tracks: BTreeMap::new(),
            channels: BTreeMap::new(),
            pending: Vec::new(),
            packet_counter: 0,
            insertion_counter: 0,
            now_ms: 0,
            health,
            echo_round_trip_total_ms: 0,
        }
    }

    #[must_use]
    pub fn candidates(&self) -> &[IceCandidate] {
        &self.candidates
    }

    #[must_use]
    pub const fn health(&self) -> TransportHealth {
        self.health
    }

    /// Starts ICE gathering without completing it, exposing the gathering state.
    ///
    /// # Errors
    /// Returns an error unless gathering has not started and the session is open.
    pub fn start_ice_gathering(&mut self) -> Result<(), WebRtcError> {
        if self.signaling == SignalingState::Closed {
            return Err(WebRtcError::Closed);
        }
        if self.gathering != IceGatheringState::New {
            return Err(self.invalid_ice("start gathering"));
        }
        self.gathering = IceGatheringState::Gathering;
        Ok(())
    }

    /// Completes deterministic candidate gathering.
    ///
    /// # Errors
    /// Returns an error for invalid state or expired relay-only credentials.
    pub fn finish_ice_gathering(&mut self, now_ms: u64) -> Result<&[IceCandidate], WebRtcError> {
        if self.gathering != IceGatheringState::Gathering {
            return Err(self.invalid_ice("finish gathering"));
        }
        self.now_ms = self.now_ms.max(now_ms);
        self.candidates.clear();
        if self.config.ice.policy == IceTransportPolicy::All {
            self.candidates.push(candidate(IceCandidateKind::Host, 1));
        }

        let mut saw_expired_turn = false;
        for server in &self.config.ice.servers {
            match server {
                IceServer::Stun { urls } if !urls.is_empty() => {
                    if !self
                        .candidates
                        .iter()
                        .any(|candidate| candidate.kind == IceCandidateKind::ServerReflexive)
                    {
                        self.candidates
                            .push(candidate(IceCandidateKind::ServerReflexive, 2));
                    }
                }
                IceServer::Turn {
                    urls, credential, ..
                } if !urls.is_empty() => {
                    if credential.is_expired_at(now_ms) {
                        saw_expired_turn = true;
                    } else if !self
                        .candidates
                        .iter()
                        .any(|candidate| candidate.kind == IceCandidateKind::Relay)
                    {
                        self.candidates.push(candidate(IceCandidateKind::Relay, 3));
                    }
                }
                IceServer::Stun { .. } | IceServer::Turn { .. } => {}
            }
        }
        if self.config.ice.policy == IceTransportPolicy::RelayOnly
            && self.candidates.is_empty()
            && saw_expired_turn
        {
            return Err(WebRtcError::IceCredentialExpired);
        }
        self.gathering = IceGatheringState::Complete;
        Ok(&self.candidates)
    }

    /// Registers one bounded media track.
    ///
    /// # Errors
    /// Returns [`WebRtcError::DuplicateId`] if the track already exists.
    pub fn add_track(&mut self, config: TrackConfig) -> Result<(), WebRtcError> {
        if self.tracks.contains_key(&config.id) {
            return Err(WebRtcError::DuplicateId);
        }
        self.tracks.insert(
            config.id,
            TrackQueue {
                config,
                records: VecDeque::new(),
                last_sequence: None,
                adaptation: None,
                current_layer: None,
            },
        );
        Ok(())
    }

    /// Registers one bounded data channel.
    ///
    /// # Errors
    /// Returns [`WebRtcError::DuplicateId`] if the channel already exists.
    pub fn add_data_channel(&mut self, config: DataChannelConfig) -> Result<(), WebRtcError> {
        if self.channels.contains_key(&config.id) {
            return Err(WebRtcError::DuplicateId);
        }
        self.channels.insert(
            config.id,
            DataQueue {
                config,
                records: VecDeque::new(),
                last_sequence: None,
            },
        );
        Ok(())
    }

    /// Assigns ordered simulcast/SVC layers and immediately selects one.
    ///
    /// # Errors
    /// Returns an unknown-track error when the track is absent.
    pub fn set_adaptation_plan(
        &mut self,
        track_id: TrackId,
        plan: AdaptationPlan,
    ) -> Result<Option<LayerId>, WebRtcError> {
        let available = self
            .config
            .impairment
            .available_bitrate_bps
            .unwrap_or(u64::MAX);
        let track = self
            .tracks
            .get_mut(&track_id)
            .ok_or(WebRtcError::UnknownTrack(track_id))?;
        track.current_layer = plan.select(available).map(|layer| layer.id);
        track.adaptation = Some(plan);
        self.health.current_layer = track.current_layer;
        Ok(track.current_layer)
    }

    /// Re-evaluates every adaptation plan for a new bandwidth estimate.
    pub fn update_bandwidth(&mut self, available_bitrate_bps: u64) {
        self.config.impairment.available_bitrate_bps = Some(available_bitrate_bps);
        for track in self.tracks.values_mut() {
            if let Some(plan) = &track.adaptation {
                track.current_layer = plan.select(available_bitrate_bps).map(|layer| layer.id);
                self.health.current_layer = track.current_layer;
            }
        }
    }

    #[must_use]
    pub fn selected_layer(&self, track_id: TrackId) -> Option<LayerId> {
        self.tracks
            .get(&track_id)
            .and_then(|track| track.current_layer)
    }

    /// Sends one media record through the deterministic impairment model.
    ///
    /// # Errors
    /// Returns state, kind, sequence, size, or queue errors.
    pub fn send_media(
        &mut self,
        track_id: TrackId,
        record: MediaRecord,
        now_ms: u64,
    ) -> Result<(), WebRtcError> {
        self.ensure_connected()?;
        let pending_count = self
            .pending
            .iter()
            .filter(|pending| {
                matches!(&pending.payload, PendingPayload::Media(id, _) if *id == track_id)
            })
            .count();
        let track = self
            .tracks
            .get_mut(&track_id)
            .ok_or(WebRtcError::UnknownTrack(track_id))?;
        if record.kind() != track.config.capabilities.media.kind() {
            return Err(WebRtcError::RecordKindMismatch);
        }
        validate_record(
            record.byte_len(),
            track.config.capabilities.queue.max_record_bytes.get(),
        )?;
        validate_sequence(track.last_sequence, record.sequence())?;
        if track.records.len() + pending_count >= track.config.capabilities.queue.capacity.get() {
            self.health.queue_drops = self.health.queue_drops.saturating_add(1);
            return Err(WebRtcError::QueueFull);
        }
        track.last_sequence = Some(record.sequence());
        self.schedule(PendingPayload::Media(track_id, record), now_ms);
        self.advance_to(now_ms);
        Ok(())
    }

    /// Receives the next media record without blocking.
    ///
    /// # Errors
    /// Returns an unknown-track error when the track is absent.
    pub fn try_receive_media(
        &mut self,
        track_id: TrackId,
    ) -> Result<Option<MediaRecord>, WebRtcError> {
        self.tracks
            .get_mut(&track_id)
            .map(|track| track.records.pop_front())
            .ok_or(WebRtcError::UnknownTrack(track_id))
    }

    /// Sends one data-channel record through the impairment model.
    ///
    /// # Errors
    /// Returns state, sequence, size, or queue errors.
    pub fn send_data(
        &mut self,
        channel_id: DataChannelId,
        record: DataChannelRecord,
        now_ms: u64,
    ) -> Result<(), WebRtcError> {
        self.ensure_connected()?;
        let pending_count = self
            .pending
            .iter()
            .filter(|pending| {
                matches!(&pending.payload, PendingPayload::Data(id, _) if *id == channel_id)
            })
            .count();
        let channel = self
            .channels
            .get_mut(&channel_id)
            .ok_or(WebRtcError::UnknownDataChannel(channel_id))?;
        validate_record(
            record.byte_len(),
            channel.config.queue.max_record_bytes.get(),
        )?;
        validate_sequence(channel.last_sequence, record.sequence())?;
        if channel.records.len() + pending_count >= channel.config.queue.capacity.get() {
            self.health.queue_drops = self.health.queue_drops.saturating_add(1);
            return Err(WebRtcError::QueueFull);
        }
        channel.last_sequence = Some(record.sequence());
        self.schedule(PendingPayload::Data(channel_id, record), now_ms);
        self.advance_to(now_ms);
        Ok(())
    }

    /// Receives the next data-channel record without blocking.
    ///
    /// # Errors
    /// Returns an unknown-channel error when the channel is absent.
    pub fn try_receive_data(
        &mut self,
        channel_id: DataChannelId,
    ) -> Result<Option<DataChannelRecord>, WebRtcError> {
        self.channels
            .get_mut(&channel_id)
            .map(|channel| channel.records.pop_front())
            .ok_or(WebRtcError::UnknownDataChannel(channel_id))
    }

    /// Advances simulated time and makes due records available to receivers.
    pub fn advance_to(&mut self, now_ms: u64) {
        self.now_ms = self.now_ms.max(now_ms);
        let mut due = Vec::new();
        let mut waiting = Vec::new();
        for pending in self.pending.drain(..) {
            if pending.deliver_at_ms <= self.now_ms {
                due.push(pending);
            } else {
                waiting.push(pending);
            }
        }

        // An ordered channel cannot overtake a lower sequence still in flight.
        let mut deliverable = Vec::new();
        for ready in due {
            let held_for_ordering = if let PendingPayload::Data(channel_id, record) = &ready.payload
            {
                self.channels
                    .get(channel_id)
                    .is_some_and(|channel| channel.config.ordered)
                    && waiting.iter().any(|pending| {
                        matches!(
                            &pending.payload,
                            PendingPayload::Data(id, pending_record)
                                if id == channel_id
                                    && pending_record.sequence() < record.sequence()
                        )
                    })
            } else {
                false
            };
            if held_for_ordering {
                waiting.push(ready);
            } else {
                deliverable.push(ready);
            }
        }
        let mut due = deliverable;

        due.sort_by_key(|pending| match &pending.payload {
            PendingPayload::Data(channel_id, record)
                if self
                    .channels
                    .get(channel_id)
                    .is_some_and(|channel| channel.config.ordered) =>
            {
                (0, record.sequence(), pending.insertion_order)
            }
            _ => (1, pending.deliver_at_ms, pending.insertion_order),
        });
        self.pending = waiting;
        for pending in due {
            match pending.payload {
                PendingPayload::Media(track_id, record) => {
                    if let Some(track) = self.tracks.get_mut(&track_id) {
                        track.records.push_back(record);
                        self.health.packets_delivered =
                            self.health.packets_delivered.saturating_add(1);
                    }
                }
                PendingPayload::Data(channel_id, record) => {
                    if let Some(channel) = self.channels.get_mut(&channel_id) {
                        channel.records.push_back(record);
                        self.health.packets_delivered =
                            self.health.packets_delivered.saturating_add(1);
                    }
                }
            }
        }
    }

    /// Performs a deterministic round-trip probe and updates health statistics.
    ///
    /// # Errors
    /// Returns an error while the session is not connected.
    pub fn echo(&mut self, now_ms: u64) -> Result<Option<u64>, WebRtcError> {
        self.ensure_connected()?;
        self.health.echo.sent = self.health.echo.sent.saturating_add(1);
        self.health.packets_sent = self.health.packets_sent.saturating_add(1);
        if self.next_packet_is_lost() {
            self.record_echo_loss();
            return Ok(None);
        }
        self.health.packets_sent = self.health.packets_sent.saturating_add(1);
        if self.next_packet_is_lost() {
            self.record_echo_loss();
            return Ok(None);
        }
        let outbound = self.packet_delay_ms();
        let inbound = self.packet_delay_ms();
        let round_trip = outbound.saturating_add(inbound);
        self.now_ms = self.now_ms.max(now_ms.saturating_add(round_trip));
        self.health.packets_delivered = self.health.packets_delivered.saturating_add(2);
        self.health.echo.returned = self.health.echo.returned.saturating_add(1);
        self.health.echo.last_round_trip_ms = Some(round_trip);
        self.echo_round_trip_total_ms = self.echo_round_trip_total_ms.saturating_add(round_trip);
        self.health.echo.average_round_trip_ms =
            Some(self.echo_round_trip_total_ms / self.health.echo.returned);
        Ok(Some(round_trip))
    }

    pub fn close(&mut self) {
        self.signaling = SignalingState::Closed;
        self.ice_connection = IceConnectionState::Closed;
        self.state = SessionState::Closed;
        self.pending.clear();
    }

    fn schedule(&mut self, payload: PendingPayload, now_ms: u64) {
        self.now_ms = self.now_ms.max(now_ms);
        self.health.packets_sent = self.health.packets_sent.saturating_add(1);
        if self.next_packet_is_lost() {
            self.health.packets_lost = self.health.packets_lost.saturating_add(1);
            return;
        }
        self.insertion_counter = self.insertion_counter.saturating_add(1);
        self.pending.push(PendingDelivery {
            deliver_at_ms: now_ms.saturating_add(self.packet_delay_ms()),
            insertion_order: self.insertion_counter,
            payload,
        });
    }

    fn next_packet_is_lost(&mut self) -> bool {
        self.packet_counter = self.packet_counter.saturating_add(1);
        self.config
            .impairment
            .loss_every
            .is_some_and(|every| every != 0 && self.packet_counter.is_multiple_of(u64::from(every)))
    }

    fn packet_delay_ms(&self) -> u64 {
        if self.packet_counter.is_multiple_of(2) {
            self.config
                .impairment
                .one_way_delay_ms
                .saturating_sub(self.config.impairment.jitter_ms)
        } else {
            self.config
                .impairment
                .one_way_delay_ms
                .saturating_add(self.config.impairment.jitter_ms)
        }
    }

    fn record_echo_loss(&mut self) {
        self.health.echo.lost = self.health.echo.lost.saturating_add(1);
        self.health.packets_lost = self.health.packets_lost.saturating_add(1);
    }

    fn ensure_connected(&self) -> Result<(), WebRtcError> {
        if self.state == SessionState::Closed {
            Err(WebRtcError::Closed)
        } else if self.state == SessionState::Connected {
            Ok(())
        } else {
            Err(WebRtcError::SessionNotConnected)
        }
    }

    const fn invalid_ice(&self, operation: &'static str) -> WebRtcError {
        WebRtcError::InvalidIceState {
            operation,
            gathering: self.gathering,
            connection: self.ice_connection,
        }
    }

    fn disconnect(&mut self) {
        if self.state != SessionState::Closed {
            self.state = SessionState::Reconnecting;
            self.ice_connection = IceConnectionState::Failed;
        }
    }

    fn prepare_reconnect(&mut self) {
        if self.state != SessionState::Closed {
            self.state = SessionState::Reconnecting;
            self.gathering = IceGatheringState::New;
            self.ice_connection = IceConnectionState::New;
            self.candidates.clear();
            self.pending.clear();
        }
    }
}

impl PeerConnection for FakePeerConnection {
    fn id(&self) -> SessionId {
        self.id
    }

    fn signaling_state(&self) -> SignalingState {
        self.signaling
    }

    fn ice_gathering_state(&self) -> IceGatheringState {
        self.gathering
    }

    fn ice_connection_state(&self) -> IceConnectionState {
        self.ice_connection
    }

    fn state(&self) -> SessionState {
        self.state
    }

    fn create_offer(&mut self) -> Result<SessionDescription, WebRtcError> {
        if self.signaling != SignalingState::Stable {
            return Err(invalid_signaling("create offer", self.signaling));
        }
        self.revision = self.revision.saturating_add(1);
        self.signaling = SignalingState::HaveLocalOffer;
        self.state = SessionState::Negotiating;
        Ok(description(DescriptionKind::Offer, self.id, self.revision))
    }

    fn receive_offer(&mut self, offer: SessionDescription) -> Result<(), WebRtcError> {
        if self.signaling != SignalingState::Stable || offer.kind != DescriptionKind::Offer {
            return Err(invalid_signaling("receive offer", self.signaling));
        }
        self.revision = self.revision.max(offer.revision);
        self.signaling = SignalingState::HaveRemoteOffer;
        self.state = SessionState::Negotiating;
        Ok(())
    }

    fn create_answer(&mut self) -> Result<SessionDescription, WebRtcError> {
        if self.signaling != SignalingState::HaveRemoteOffer {
            return Err(invalid_signaling("create answer", self.signaling));
        }
        self.revision = self.revision.saturating_add(1);
        self.signaling = SignalingState::Stable;
        self.negotiated = true;
        Ok(description(DescriptionKind::Answer, self.id, self.revision))
    }

    fn receive_answer(&mut self, answer: SessionDescription) -> Result<(), WebRtcError> {
        if self.signaling != SignalingState::HaveLocalOffer
            || answer.kind != DescriptionKind::Answer
        {
            return Err(invalid_signaling("receive answer", self.signaling));
        }
        self.revision = self.revision.max(answer.revision);
        self.signaling = SignalingState::Stable;
        self.negotiated = true;
        Ok(())
    }

    fn gather_ice(&mut self, now_ms: u64) -> Result<&[IceCandidate], WebRtcError> {
        self.start_ice_gathering()?;
        self.finish_ice_gathering(now_ms)
    }

    fn check_ice(&mut self, now_ms: u64) -> Result<(), WebRtcError> {
        if self.gathering != IceGatheringState::Complete
            || self.ice_connection != IceConnectionState::New
            || self.signaling != SignalingState::Stable
            || !self.negotiated
        {
            return Err(self.invalid_ice("check connectivity"));
        }
        self.ice_connection = IceConnectionState::Checking;
        self.now_ms = self.now_ms.max(now_ms);
        let has_relay = self
            .candidates
            .iter()
            .any(|candidate| candidate.kind == IceCandidateKind::Relay);
        let has_reflexive = self
            .candidates
            .iter()
            .any(|candidate| candidate.kind == IceCandidateKind::ServerReflexive);
        let direct = direct_connection_works(self.config.nat, has_reflexive);
        let relay_valid = has_relay
            && self.config.ice.servers.iter().any(|server| {
                matches!(
                    server,
                    IceServer::Turn { credential, .. } if !credential.is_expired_at(now_ms)
                )
            });
        if has_relay && !relay_valid && !direct {
            self.ice_connection = IceConnectionState::Failed;
            self.state = SessionState::Failed;
            return Err(WebRtcError::IceCredentialExpired);
        }
        if direct || relay_valid {
            self.ice_connection = IceConnectionState::Connected;
            self.state = SessionState::Connected;
            Ok(())
        } else {
            self.ice_connection = IceConnectionState::Failed;
            self.state = SessionState::Failed;
            Err(WebRtcError::IceConnectionFailed)
        }
    }
}

fn candidate(kind: IceCandidateKind, foundation: u32) -> IceCandidate {
    IceCandidate {
        kind,
        foundation,
        priority: 100_u32.saturating_sub(foundation),
    }
}

fn description(kind: DescriptionKind, session_id: SessionId, revision: u64) -> SessionDescription {
    SessionDescription {
        kind,
        session_id,
        revision,
        opaque: format!("fake:{session_id}:{revision}:{kind:?}"),
    }
}

const fn invalid_signaling(operation: &'static str, state: SignalingState) -> WebRtcError {
    WebRtcError::InvalidSignalingOrder { operation, state }
}

fn validate_record(actual: usize, maximum: usize) -> Result<(), WebRtcError> {
    if actual > maximum {
        Err(WebRtcError::RecordTooLarge { actual, maximum })
    } else {
        Ok(())
    }
}

fn validate_sequence(previous: Option<u64>, actual: u64) -> Result<(), WebRtcError> {
    if let Some(previous) = previous
        && previous.checked_add(1) != Some(actual)
    {
        return Err(WebRtcError::SequenceOutOfOrder { previous, actual });
    }
    Ok(())
}

const fn direct_connection_works(scenario: NatScenario, has_reflexive: bool) -> bool {
    if matches!(
        (scenario.local, scenario.remote),
        (NatType::Open, NatType::Open)
    ) {
        return true;
    }
    has_reflexive
        && !matches!(
            (scenario.local, scenario.remote),
            (NatType::Symmetric, NatType::Symmetric)
        )
}
