use fm_io_srt::{
    CallerOffer, CallerPeer, Cipher, ConfigError, EncryptionConfig, Endpoint, FailureReason,
    Impairment, LatencyConfig, OverflowPolicy, Packet, PacketQueue, PushOutcome, ReconnectPolicy,
    SecretRef, SessionEvent, SessionState, SrtConfig, SrtMode, SrtSession, select_caller_peer,
};

fn endpoint(value: &str) -> Endpoint {
    Endpoint::new(value).unwrap()
}

fn secret(value: &str) -> SecretRef {
    SecretRef::new(value).unwrap()
}

fn config(mode: SrtMode) -> SrtConfig {
    SrtConfig {
        mode,
        latency: LatencyConfig::default(),
        encryption: EncryptionConfig::default(),
        stream_id: None,
        reconnect: ReconnectPolicy {
            max_attempts: 2,
            initial_delay_ms: 100,
            max_delay_ms: 200,
        },
        send_queue_capacity: 2,
        receive_queue_capacity: 2,
    }
}

fn packet(sequence: u64, size: usize) -> Packet {
    Packet {
        sequence,
        payload: vec![0; size],
        enqueued_at_ms: 0,
    }
}

#[test]
fn caller_connects_through_explicit_states() {
    let remote = endpoint("media.example:9000");
    let mut session = SrtSession::new(config(SrtMode::Caller {
        remote: remote.clone(),
    }))
    .unwrap();

    session.apply(SessionEvent::Start, 10).unwrap();
    assert_eq!(
        session.state(),
        &SessionState::Connecting {
            peer: remote.clone(),
            attempt: 0,
        }
    );
    session
        .apply(
            SessionEvent::ConnectionEstablished {
                peer: remote.clone(),
            },
            20,
        )
        .unwrap();
    assert_eq!(
        session.state(),
        &SessionState::Connected {
            peer: remote,
            connected_at_ms: 20,
        }
    );
}

#[test]
fn listener_selects_the_same_caller_regardless_of_offer_order() {
    let local = endpoint("0.0.0.0:9000");
    let low = endpoint("caller-b:1000");
    let high = endpoint("caller-a:1000");
    let stream = secret("vault://stream/live");
    let allowed = vec![
        CallerPeer {
            endpoint: low.clone(),
            priority: 1,
        },
        CallerPeer {
            endpoint: high.clone(),
            priority: 10,
        },
    ];
    let offers = vec![
        CallerOffer {
            endpoint: low,
            stream_id: Some(stream.clone()),
        },
        CallerOffer {
            endpoint: high.clone(),
            stream_id: Some(stream.clone()),
        },
        CallerOffer {
            endpoint: endpoint("unlisted:1000"),
            stream_id: Some(stream.clone()),
        },
    ];
    assert_eq!(
        select_caller_peer(&allowed, Some(&stream), &offers),
        Some(high.clone())
    );

    let mut listener_config = config(SrtMode::Listener {
        local: local.clone(),
        allowed_callers: allowed,
    });
    listener_config.stream_id = Some(stream);
    let mut session = SrtSession::new(listener_config).unwrap();
    session.apply(SessionEvent::Start, 0).unwrap();
    assert_eq!(session.state(), &SessionState::Listening { local });
    session
        .apply(SessionEvent::CallerOffers(offers), 50)
        .unwrap();
    assert_eq!(
        session.state(),
        &SessionState::Connected {
            peer: high,
            connected_at_ms: 50,
        }
    );
}

#[test]
fn rendezvous_uses_remote_peer_and_rejects_an_unexpected_one() {
    let local = endpoint("local:9000");
    let remote = endpoint("remote:9000");
    let mut session = SrtSession::new(config(SrtMode::Rendezvous {
        local,
        remote: remote.clone(),
    }))
    .unwrap();
    session.apply(SessionEvent::Start, 0).unwrap();
    assert!(
        session
            .apply(
                SessionEvent::ConnectionEstablished {
                    peer: endpoint("other:9000"),
                },
                1,
            )
            .is_err()
    );
    assert_eq!(
        session.state(),
        &SessionState::Connecting {
            peer: remote,
            attempt: 0,
        }
    );
}

#[test]
fn secret_references_are_redacted_in_all_containing_debug_output() {
    let passphrase = secret("vault://production/srt-passphrase");
    let stream_id = secret("vault://production/stream-id");
    let mut value = config(SrtMode::Caller {
        remote: endpoint("remote:9000"),
    });
    value.encryption = EncryptionConfig {
        cipher: Cipher::Aes256,
        passphrase: Some(passphrase.clone()),
    };
    value.stream_id = Some(stream_id);

    let rendered = format!("{value:?} {passphrase}");
    assert!(!rendered.contains("production"));
    assert!(!rendered.contains("srt-passphrase"));
    assert!(!rendered.contains("stream-id"));
    assert!(rendered.contains("[REDACTED]"));
    assert_eq!(passphrase.locator(), "vault://production/srt-passphrase");
}

#[test]
fn latency_encryption_and_reconnect_settings_are_validated() {
    let mut value = config(SrtMode::Caller {
        remote: endpoint("remote:9000"),
    });
    value.latency.receive_ms = 0;
    assert!(matches!(
        value.validate(),
        Err(ConfigError::LatencyOutOfRange {
            name: "receive",
            value: 0
        })
    ));

    value.latency = LatencyConfig::default();
    value.encryption.cipher = Cipher::Aes128;
    assert_eq!(value.validate(), Err(ConfigError::MissingPassphrase));

    value.encryption = EncryptionConfig {
        cipher: Cipher::None,
        passphrase: Some(secret("vault://passphrase")),
    };
    assert_eq!(
        value.validate(),
        Err(ConfigError::PassphraseWithoutEncryption)
    );

    value.encryption = EncryptionConfig::default();
    value.reconnect.initial_delay_ms = 300;
    value.reconnect.max_delay_ms = 200;
    assert_eq!(value.validate(), Err(ConfigError::ReconnectDelayOrder));
}

#[test]
fn reconnect_backoff_is_bounded_and_exhaustion_is_terminal() {
    let remote = endpoint("remote:9000");
    let mut session = SrtSession::new(config(SrtMode::Caller {
        remote: remote.clone(),
    }))
    .unwrap();
    session.apply(SessionEvent::Start, 0).unwrap();
    session
        .apply(
            SessionEvent::ConnectionFailed {
                detail: "first".into(),
            },
            10,
        )
        .unwrap();
    assert_eq!(
        session.state(),
        &SessionState::Reconnecting {
            peer: remote.clone(),
            attempt: 1,
            retry_at_ms: 110,
        }
    );
    session.apply(SessionEvent::AdvanceTime, 109).unwrap();
    assert!(matches!(session.state(), SessionState::Reconnecting { .. }));
    session.apply(SessionEvent::AdvanceTime, 110).unwrap();
    session
        .apply(
            SessionEvent::ConnectionFailed {
                detail: "second".into(),
            },
            120,
        )
        .unwrap();
    assert_eq!(
        session.state(),
        &SessionState::Reconnecting {
            peer: remote,
            attempt: 2,
            retry_at_ms: 320,
        }
    );
    session.apply(SessionEvent::AdvanceTime, 320).unwrap();
    session
        .apply(
            SessionEvent::ConnectionFailed {
                detail: "final".into(),
            },
            321,
        )
        .unwrap();
    assert_eq!(
        session.state(),
        &SessionState::Failed(FailureReason::ReconnectExhausted("final".into()))
    );
}

#[test]
fn packet_queues_remain_bounded_with_declared_overflow_behavior() {
    let mut reject = PacketQueue::new(2, OverflowPolicy::RejectNewest).unwrap();
    assert_eq!(reject.push(packet(1, 1)), PushOutcome::Enqueued);
    assert_eq!(reject.push(packet(2, 1)), PushOutcome::Enqueued);
    assert!(matches!(
        reject.push(packet(3, 1)),
        PushOutcome::Rejected(Packet { sequence: 3, .. })
    ));
    assert_eq!(reject.len(), 2);

    let mut evict = PacketQueue::new(2, OverflowPolicy::DropOldest).unwrap();
    evict.push(packet(1, 1));
    evict.push(packet(2, 1));
    assert!(matches!(
        evict.push(packet(3, 1)),
        PushOutcome::DroppedOldest(Packet { sequence: 1, .. })
    ));
    assert_eq!(evict.pop().unwrap().sequence, 2);
    assert_eq!(evict.pop().unwrap().sequence, 3);
}

#[test]
fn statistics_cover_rtt_loss_retransmit_bandwidth_and_queue_drops() {
    let mut session = SrtSession::new(config(SrtMode::Caller {
        remote: endpoint("remote:9000"),
    }))
    .unwrap();
    session.enqueue_send(packet(1, 1_000));
    session.enqueue_send(packet(2, 1_000));
    assert!(matches!(
        session.enqueue_send(packet(3, 1_000)),
        PushOutcome::Rejected(_)
    ));
    session.dequeue_send(1_000);
    session.dequeue_send(1_000);

    session.enqueue_receive(packet(1, 500), 1_000);
    session.enqueue_receive(packet(2, 500), 1_000);
    assert!(matches!(
        session.enqueue_receive(packet(3, 500), 1_000),
        PushOutcome::DroppedOldest(_)
    ));
    session.statistics_mut().record_rtt(42);
    session.statistics_mut().record_loss(1);
    session.statistics_mut().record_retransmit(2);

    let stats = session.statistics().snapshot(2_000);
    assert_eq!(stats.rtt_ms, Some(42));
    assert_eq!(stats.loss_basis_points, 2_500);
    assert_eq!(stats.packets_retransmitted, 2);
    assert_eq!(stats.tx_bandwidth_bps, 16_000);
    assert_eq!(stats.rx_bandwidth_bps, 12_000);
    assert_eq!(stats.send_queue_drops, 1);
    assert_eq!(stats.receive_queue_drops, 1);
}

#[test]
fn impairment_updates_are_validated_and_revisioned() {
    let mut session = SrtSession::new(config(SrtMode::Caller {
        remote: endpoint("remote:9000"),
    }))
    .unwrap();
    let profile = Impairment {
        additional_rtt_ms: 50,
        loss_basis_points: 125,
        bandwidth_limit_bps: Some(5_000_000),
    };
    assert_eq!(session.update_impairment(profile).unwrap(), 1);
    assert_eq!(session.impairment().profile, profile);
    assert_eq!(
        session.update_impairment(Impairment {
            loss_basis_points: 10_001,
            ..Impairment::default()
        }),
        Err(ConfigError::ImpairmentLossOutOfRange(10_001))
    );
    assert_eq!(session.impairment().revision, 1);
}

#[test]
fn fatal_failure_is_independent_between_sessions() {
    let mode = SrtMode::Caller {
        remote: endpoint("remote:9000"),
    };
    let mut failed = SrtSession::new(config(mode.clone())).unwrap();
    let mut healthy = SrtSession::new(config(mode)).unwrap();
    failed.apply(SessionEvent::Start, 0).unwrap();
    healthy.apply(SessionEvent::Start, 0).unwrap();
    failed
        .apply(
            SessionEvent::Fatal {
                detail: "adapter crashed".into(),
            },
            1,
        )
        .unwrap();

    assert!(matches!(failed.state(), SessionState::Failed(_)));
    assert!(matches!(
        healthy.state(),
        SessionState::Connecting { attempt: 0, .. }
    ));
}
