use std::num::{NonZeroU128, NonZeroUsize};

use crate::fake::{FakePeerConnection, FakeSessionConfig, FakeWebRtcTransport};
use crate::*;

fn id(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn queue(capacity: usize) -> QueueLimits {
    QueueLimits {
        capacity: NonZeroUsize::new(capacity).unwrap(),
        max_record_bytes: NonZeroUsize::new(64).unwrap(),
    }
}

fn audio_track(value: u128, capacity: usize) -> TrackConfig {
    TrackConfig {
        id: TrackId::new(id(value)),
        direction: TrackDirection::SendReceive,
        capabilities: TrackCapabilities {
            media: MediaCapability::Audio {
                sample_rates: vec![48_000],
                channel_counts: vec![2],
            },
            codecs: vec![CodecCapability {
                name: "opus".to_owned(),
                clock_rate: 48_000,
                channels: Some(2),
            }],
            queue: queue(capacity),
        },
    }
}

fn video_track(value: u128, capacity: usize) -> TrackConfig {
    TrackConfig {
        id: TrackId::new(id(value)),
        direction: TrackDirection::SendReceive,
        capabilities: TrackCapabilities {
            media: MediaCapability::Video {
                max_width: 1_920,
                max_height: 1_080,
                max_frame_rate: 60,
            },
            codecs: vec![CodecCapability {
                name: "vp9".to_owned(),
                clock_rate: 90_000,
                channels: None,
            }],
            queue: queue(capacity),
        },
    }
}

fn channel(value: u128, capacity: usize, ordered: bool) -> DataChannelConfig {
    DataChannelConfig {
        id: DataChannelId::new(id(value)),
        label: "production".to_owned(),
        kind: DataChannelKind::Control,
        ordered,
        queue: queue(capacity),
    }
}

fn audio(sequence: u64) -> MediaRecord {
    MediaRecord::Audio(ReturnAudioRecord {
        sequence,
        timestamp_ms: sequence * 20,
        sample_rate: 48_000,
        channels: 2,
        payload: vec![0; 8],
    })
}

fn bare_connection(config: FakeSessionConfig) -> FakePeerConnection {
    FakePeerConnection::new(
        SessionId::new(id(1)),
        PeerId::new(id(2)),
        PeerId::new(id(3)),
        config,
    )
}

fn negotiate_and_connect(connection: &mut FakePeerConnection, now_ms: u64) {
    let offer = connection.create_offer().unwrap();
    connection
        .receive_answer(SessionDescription {
            kind: DescriptionKind::Answer,
            session_id: offer.session_id,
            revision: offer.revision,
            opaque: "answer".to_owned(),
        })
        .unwrap();
    connection.gather_ice(now_ms).unwrap();
    connection.check_ice(now_ms).unwrap();
}

#[test]
fn signaling_enforces_offer_answer_order() {
    let mut connection = bare_connection(FakeSessionConfig::default());
    assert!(matches!(
        connection.create_answer(),
        Err(WebRtcError::InvalidSignalingOrder { .. })
    ));
    let offer = connection.create_offer().unwrap();
    assert_eq!(connection.signaling_state(), SignalingState::HaveLocalOffer);
    assert!(matches!(
        connection.create_offer(),
        Err(WebRtcError::InvalidSignalingOrder { .. })
    ));
    connection
        .receive_answer(SessionDescription {
            kind: DescriptionKind::Answer,
            session_id: offer.session_id,
            revision: offer.revision,
            opaque: String::new(),
        })
        .unwrap();
    assert_eq!(connection.signaling_state(), SignalingState::Stable);
    connection.start_ice_gathering().unwrap();
    assert_eq!(
        connection.ice_gathering_state(),
        IceGatheringState::Gathering
    );
    connection.finish_ice_gathering(0).unwrap();
    connection.check_ice(0).unwrap();
    assert_eq!(connection.state(), SessionState::Connected);
}

#[test]
fn symmetric_nat_requires_turn_and_credentials_are_redacted() {
    let credential = CredentialRef::new("vault://production/turn-password", 1_000);
    assert!(!format!("{credential:?}").contains("turn-password"));
    let mut without_turn = bare_connection(FakeSessionConfig {
        ice: IceConfig {
            servers: vec![IceServer::Stun {
                urls: vec!["stun:example.test".to_owned()],
            }],
            policy: IceTransportPolicy::All,
        },
        nat: NatScenario {
            local: NatType::Symmetric,
            remote: NatType::Symmetric,
        },
        impairment: ImpairmentProfile::default(),
    });
    let offer = without_turn.create_offer().unwrap();
    without_turn
        .receive_answer(SessionDescription {
            kind: DescriptionKind::Answer,
            session_id: offer.session_id,
            revision: offer.revision,
            opaque: String::new(),
        })
        .unwrap();
    without_turn.gather_ice(10).unwrap();
    assert_eq!(
        without_turn.check_ice(10),
        Err(WebRtcError::IceConnectionFailed)
    );

    let mut with_turn = bare_connection(FakeSessionConfig {
        ice: IceConfig {
            servers: vec![IceServer::Turn {
                urls: vec!["turn:example.test".to_owned()],
                username: "guest".to_owned(),
                credential,
            }],
            policy: IceTransportPolicy::RelayOnly,
        },
        nat: NatScenario {
            local: NatType::Symmetric,
            remote: NatType::Symmetric,
        },
        impairment: ImpairmentProfile::default(),
    });
    negotiate_and_connect(&mut with_turn, 10);
    assert_eq!(with_turn.candidates()[0].kind, IceCandidateKind::Relay);
}

#[test]
fn supports_eight_peers_and_tracks_without_shared_queue_state() {
    let mut transport = FakeWebRtcTransport::new();
    let hub = transport.add_peer("hub").peer_id;
    let peers: Vec<_> = (0..8)
        .map(|index| transport.add_peer(format!("guest-{index}")).peer_id)
        .collect();
    let mut sessions = Vec::new();
    for (index, peer) in peers.into_iter().enumerate() {
        let session = transport
            .create_session(hub, peer, FakeSessionConfig::default())
            .unwrap();
        let connection = transport.session_mut(session).unwrap();
        connection
            .add_track(audio_track(100 + index as u128, 2))
            .unwrap();
        negotiate_and_connect(connection, 0);
        connection
            .send_media(TrackId::new(id(100 + index as u128)), audio(1), 0)
            .unwrap();
        sessions.push((session, TrackId::new(id(100 + index as u128))));
    }
    assert_eq!(sessions.len(), 8);
    for (session, track) in sessions {
        assert!(
            transport
                .session_mut(session)
                .unwrap()
                .try_receive_media(track)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn bandwidth_adaptation_selects_highest_viable_layer() {
    let low = LayerId::new(id(20));
    let medium = LayerId::new(id(21));
    let high = LayerId::new(id(22));
    let track = TrackId::new(id(10));
    let mut connection = bare_connection(FakeSessionConfig::default());
    connection.add_track(video_track(10, 2)).unwrap();
    connection
        .set_adaptation_plan(
            track,
            AdaptationPlan {
                layers: vec![
                    BandwidthLayer {
                        id: low,
                        min_bitrate_bps: 100_000,
                        max_bitrate_bps: 300_000,
                        width: 320,
                        height: 180,
                        frame_rate: 15,
                    },
                    BandwidthLayer {
                        id: medium,
                        min_bitrate_bps: 500_000,
                        max_bitrate_bps: 1_000_000,
                        width: 640,
                        height: 360,
                        frame_rate: 30,
                    },
                    BandwidthLayer {
                        id: high,
                        min_bitrate_bps: 1_500_000,
                        max_bitrate_bps: 3_000_000,
                        width: 1_920,
                        height: 1_080,
                        frame_rate: 60,
                    },
                ],
            },
        )
        .unwrap();
    connection.update_bandwidth(700_000);
    assert_eq!(connection.selected_layer(track), Some(medium));
    connection.update_bandwidth(50_000);
    assert_eq!(connection.selected_layer(track), Some(low));
    connection.update_bandwidth(2_000_000);
    assert_eq!(connection.selected_layer(track), Some(high));
}

#[test]
fn media_and_data_queues_are_bounded() {
    let track = TrackId::new(id(10));
    let channel_id = DataChannelId::new(id(11));
    let mut connection = bare_connection(FakeSessionConfig::default());
    connection.add_track(audio_track(10, 2)).unwrap();
    connection.add_data_channel(channel(11, 1, true)).unwrap();
    negotiate_and_connect(&mut connection, 0);
    connection.send_media(track, audio(1), 0).unwrap();
    connection.send_media(track, audio(2), 0).unwrap();
    assert_eq!(
        connection.send_media(track, audio(3), 0),
        Err(WebRtcError::QueueFull)
    );
    connection.try_receive_media(track).unwrap();
    connection.send_media(track, audio(3), 0).unwrap();

    connection
        .send_data(
            channel_id,
            DataChannelRecord::Tally(TallyRecord {
                sequence: 1,
                state: TallyState::Program,
            }),
            0,
        )
        .unwrap();
    assert_eq!(
        connection.send_data(
            channel_id,
            DataChannelRecord::Chat(ChatRecord {
                sequence: 2,
                from: connection.local_peer,
                text: "ready".to_owned(),
            }),
            0,
        ),
        Err(WebRtcError::QueueFull)
    );
    assert_eq!(connection.health().queue_drops, 2);
}

#[test]
fn reconnect_preserves_peer_identity_and_rejects_stale_token() {
    let mut transport = FakeWebRtcTransport::new();
    let local = transport.add_peer("local");
    let remote = transport.add_peer("remote");
    let session = transport
        .create_session(local.peer_id, remote.peer_id, FakeSessionConfig::default())
        .unwrap();
    negotiate_and_connect(transport.session_mut(session).unwrap(), 0);
    transport.disconnect_peer(remote.peer_id).unwrap();
    assert_eq!(
        transport.session(session).unwrap().state(),
        SessionState::Reconnecting
    );
    let rotated = transport.reconnect(&remote).unwrap();
    assert_eq!(rotated.peer_id, remote.peer_id);
    assert_eq!(rotated.generation, remote.generation + 1);
    assert_eq!(
        transport.reconnect(&remote),
        Err(WebRtcError::InvalidReconnectIdentity)
    );
    let connection = transport.session_mut(session).unwrap();
    connection.gather_ice(1).unwrap();
    connection.check_ice(1).unwrap();
    assert_eq!(connection.state(), SessionState::Connected);
}

#[test]
fn expired_turn_credentials_fail_gathering_and_checking() {
    let config = FakeSessionConfig {
        ice: IceConfig {
            servers: vec![IceServer::Turn {
                urls: vec!["turn:example.test".to_owned()],
                username: "guest".to_owned(),
                credential: CredentialRef::new("secret://turn", 100),
            }],
            policy: IceTransportPolicy::RelayOnly,
        },
        nat: NatScenario {
            local: NatType::Symmetric,
            remote: NatType::Symmetric,
        },
        impairment: ImpairmentProfile::default(),
    };
    let mut expired_during_gather = bare_connection(config.clone());
    expired_during_gather.start_ice_gathering().unwrap();
    assert_eq!(
        expired_during_gather.finish_ice_gathering(100),
        Err(WebRtcError::IceCredentialExpired)
    );

    let mut expired_during_check = bare_connection(config);
    let offer = expired_during_check.create_offer().unwrap();
    expired_during_check
        .receive_answer(SessionDescription {
            kind: DescriptionKind::Answer,
            session_id: offer.session_id,
            revision: offer.revision,
            opaque: String::new(),
        })
        .unwrap();
    expired_during_check.gather_ice(99).unwrap();
    assert_eq!(
        expired_during_check.check_ice(100),
        Err(WebRtcError::IceCredentialExpired)
    );
}

#[test]
fn deterministic_loss_jitter_health_and_echo_stats() {
    let track = TrackId::new(id(10));
    let mut connection = bare_connection(FakeSessionConfig {
        impairment: ImpairmentProfile {
            one_way_delay_ms: 10,
            jitter_ms: 3,
            loss_every: Some(3),
            available_bitrate_bps: None,
        },
        ..FakeSessionConfig::default()
    });
    connection.add_track(audio_track(10, 4)).unwrap();
    negotiate_and_connect(&mut connection, 0);
    connection.send_media(track, audio(1), 0).unwrap();
    connection.send_media(track, audio(2), 0).unwrap();
    connection.send_media(track, audio(3), 0).unwrap();
    connection.advance_to(20);
    assert!(connection.try_receive_media(track).unwrap().is_some());
    assert!(connection.try_receive_media(track).unwrap().is_some());
    assert!(connection.try_receive_media(track).unwrap().is_none());
    let health = connection.health();
    assert_eq!(health.packets_sent, 3);
    assert_eq!(health.packets_delivered, 2);
    assert_eq!(health.packets_lost, 1);
    assert_eq!(health.estimated_jitter_ms, 3);

    assert_eq!(connection.echo(20).unwrap(), Some(26));
    assert_eq!(connection.echo(50).unwrap(), None);
    let EchoStats {
        sent,
        returned,
        lost,
        ..
    } = connection.health().echo;
    assert_eq!((sent, returned, lost), (2, 1, 1));
}

#[test]
fn ordered_data_channel_holds_faster_later_records() {
    let channel_id = DataChannelId::new(id(10));
    let mut connection = bare_connection(FakeSessionConfig {
        impairment: ImpairmentProfile {
            one_way_delay_ms: 10,
            jitter_ms: 9,
            loss_every: None,
            available_bitrate_bps: None,
        },
        ..FakeSessionConfig::default()
    });
    connection.add_data_channel(channel(10, 4, true)).unwrap();
    negotiate_and_connect(&mut connection, 0);
    for sequence in 1..=3 {
        connection
            .send_data(
                channel_id,
                DataChannelRecord::Chat(ChatRecord {
                    sequence,
                    from: connection.local_peer,
                    text: format!("message-{sequence}"),
                }),
                0,
            )
            .unwrap();
    }
    connection.advance_to(2);
    assert!(connection.try_receive_data(channel_id).unwrap().is_none());
    connection.advance_to(20);
    let sequences: Vec<_> = (0..3)
        .map(|_| {
            connection
                .try_receive_data(channel_id)
                .unwrap()
                .unwrap()
                .sequence()
        })
        .collect();
    assert_eq!(sequences, vec![1, 2, 3]);
}
