use super::*;

fn fixture(id: u128, values: &[u8]) -> PlaybackClip<u8> {
    FixtureClip::new(ClipId::new(id), values.to_vec())
        .unwrap()
        .into()
}

#[test]
fn stable_ids_preserve_caller_values() {
    let clip = ClipId::new(0xfeed);
    let entry = PlaylistEntryId::new(0xbeef);
    assert_eq!(clip.get(), 0xfeed);
    assert_eq!(entry.get(), 0xbeef);
    assert_eq!(clip.to_string(), "65261");
}

#[test]
fn transport_boundaries_and_states_are_exact() {
    let mut transport = Transport::new(fixture(1, &[10, 11, 12]));
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(
        transport.pull_frame(None).unwrap().index,
        FrameIndex::new(0)
    );

    transport.play();
    assert_eq!(
        transport.pull_frame(None).unwrap().index,
        FrameIndex::new(0)
    );
    assert_eq!(
        transport.pull_frame(None).unwrap().index,
        FrameIndex::new(1)
    );
    let last = transport.pull_frame(None).unwrap();
    assert_eq!(last.index, FrameIndex::new(2));
    assert!(last.ended);
    assert_eq!(transport.state(), TransportState::Paused);
    assert_eq!(transport.cursor(), FrameIndex::new(2));

    transport.stop();
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.cursor(), FrameIndex::new(0));
}

#[test]
fn seeks_and_marks_reject_out_of_range_frames() {
    let mut transport = Transport::new(fixture(1, &[0, 1, 2, 3, 4]));
    let marks = Marks::new(FrameIndex::new(1), FrameIndex::new(3)).unwrap();
    transport.set_marks(marks).unwrap();
    transport.seek(FrameIndex::new(3)).unwrap();
    assert_eq!(transport.cursor(), FrameIndex::new(3));
    assert_eq!(
        transport.seek(FrameIndex::new(4)),
        Err(PlaybackError::SeekOutsideMarks {
            requested: FrameIndex::new(4),
            marks,
        })
    );
    assert!(matches!(
        transport.set_marks(Marks::new(FrameIndex::new(0), FrameIndex::new(5)).unwrap()),
        Err(PlaybackError::MarkOutOfRange { .. })
    ));
    assert!(matches!(
        transport.set_marks(Marks {
            mark_in: FrameIndex::new(3),
            mark_out: FrameIndex::new(2),
        }),
        Err(PlaybackError::InvalidMarks(MarkError::Reversed { .. }))
    ));
}

#[test]
fn loop_and_two_x_wrap_without_drift() {
    let mut transport = Transport::new(fixture(1, &[0, 1, 2, 3]));
    transport.set_looping(true);
    transport.set_speed(Speed::Forward2x);
    let outputs: Vec<_> = (0..5)
        .map(|_| transport.pull_frame(None).unwrap().index.get())
        .collect();
    assert_eq!(outputs, [0, 2, 0, 2, 0]);
    assert_eq!(transport.state(), TransportState::Playing);
}

#[test]
fn reverse_starts_and_ends_at_inclusive_marks() {
    let mut transport = Transport::new(fixture(1, &[0, 1, 2, 3]));
    let marks = Marks::new(FrameIndex::new(1), FrameIndex::new(3)).unwrap();
    transport.set_marks(marks).unwrap();
    transport.seek(marks.mark_out).unwrap();
    transport.set_speed(Speed::Reverse1x);
    let outputs: Vec<_> = (0..3)
        .map(|_| transport.pull_frame(None).unwrap().index.get())
        .collect();
    assert_eq!(outputs, [3, 2, 1]);
    assert_eq!(transport.state(), TransportState::Paused);
    assert_eq!(transport.cursor(), marks.mark_in);
}

#[test]
fn stop_end_behavior_is_distinct_from_hold() {
    let mut transport = Transport::new(fixture(1, &[7]));
    transport.set_end_behavior(EndBehavior::Stop);
    transport.play();
    assert!(transport.pull_frame(None).unwrap().ended);
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.cursor(), FrameIndex::new(0));
}

fn two_entry_player(first_end: EndAction) -> PlaylistPlayer<u8> {
    let mut library = ClipLibrary::default();
    library.insert(fixture(1, &[10, 11])).unwrap();
    library.insert(fixture(2, &[20])).unwrap();
    let playlist = Playlist::new(vec![
        PlaylistEntry::new(PlaylistEntryId::new(1), ClipId::new(1)).with_end_action(first_end),
        PlaylistEntry::new(PlaylistEntryId::new(2), ClipId::new(2)),
    ])
    .unwrap();
    PlaylistPlayer::new(library, playlist).unwrap()
}

#[test]
fn playlist_advances_to_programmed_next() {
    let first = PlaylistEntryId::new(1);
    let second = PlaylistEntryId::new(2);
    let mut player = two_entry_player(EndAction::Next(second));
    player.go(first).unwrap();
    assert_eq!(player.pull_frame(None).unwrap().frame, 10);
    assert_eq!(player.pull_frame(None).unwrap().frame, 11);
    assert_eq!(player.current_entry(), Some(second));
    assert_eq!(player.pull_frame(None).unwrap().frame, 20);
}

#[test]
fn playlist_stop_uses_stopped_transport_state() {
    let first = PlaylistEntryId::new(1);
    let mut player = two_entry_player(EndAction::Stop);
    player.go(first).unwrap();
    player.pull_frame(None).unwrap();
    player.pull_frame(None).unwrap();
    assert_eq!(player.transport().unwrap().state(), TransportState::Stopped);
    assert_eq!(player.transport().unwrap().cursor(), FrameIndex::new(0));
}

#[test]
fn playlist_programmed_loop_restarts_entry() {
    let first = PlaylistEntryId::new(1);
    let mut player = two_entry_player(EndAction::Loop);
    player.go(first).unwrap();
    let outputs: Vec<_> = (0..5)
        .map(|_| player.pull_frame(None).unwrap().frame)
        .collect();
    assert_eq!(outputs, [10, 11, 10, 11, 10]);
}

#[test]
fn programmed_go_is_ordered_cancellable_and_idempotent() {
    let first = PlaylistEntryId::new(1);
    let second = PlaylistEntryId::new(2);
    let mut player = two_entry_player(EndAction::Stop);
    let cancelled = ProgrammedGo {
        id: GoId::new(9),
        coordinate: ScheduleCoordinate::new(10),
        entry: first,
    };
    let active = ProgrammedGo {
        id: GoId::new(10),
        coordinate: ScheduleCoordinate::new(10),
        entry: second,
    };
    assert_eq!(
        player.schedule_go(cancelled).unwrap(),
        GoScheduleOutcome::Scheduled
    );
    assert_eq!(
        player.schedule_go(cancelled).unwrap(),
        GoScheduleOutcome::Unchanged(GoStatus::Pending)
    );
    player.schedule_go(active).unwrap();
    assert_eq!(
        player.cancel_go(cancelled.id).unwrap(),
        CancelGoOutcome::Cancelled
    );
    assert_eq!(
        player.cancel_go(cancelled.id).unwrap(),
        CancelGoOutcome::AlreadyCancelled
    );
    assert!(
        player
            .apply_scheduled(ScheduleCoordinate::new(9))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        player.apply_scheduled(ScheduleCoordinate::new(10)).unwrap(),
        [active.id]
    );
    assert_eq!(player.current_entry(), Some(second));
    assert_eq!(
        player.schedule_go(active).unwrap(),
        GoScheduleOutcome::Unchanged(GoStatus::Executed)
    );
}

#[test]
fn missing_fixture_frame_does_not_advance_cursor() {
    let clip = FixtureClip::from_slots(ClipId::new(1), vec![Some(1_u8), None]).unwrap();
    let mut transport = Transport::new(clip.into());
    transport.play();
    transport.pull_frame(None).unwrap();
    assert_eq!(
        transport.pull_frame(None),
        Err(PlaybackError::MissingFrame {
            clip: ClipId::new(1),
            frame: FrameIndex::new(1),
        })
    );
    assert_eq!(transport.cursor(), FrameIndex::new(1));
    assert_eq!(transport.state(), TransportState::Playing);
}

struct FailingCodec;

impl FrameCodec<u8> for FailingCodec {
    fn read_frame(&mut self, _clip: &EncodedClip, frame: FrameIndex) -> Result<u8, CodecError> {
        Err(CodecError::DecodeFailed {
            frame,
            reason: "fixture failure".to_owned(),
        })
    }
}

#[test]
fn encoded_clips_report_missing_and_failed_codecs() {
    let encoded = EncodedClip::new(ClipId::new(7), "memory://test", 2).unwrap();
    let mut transport: Transport<u8> = Transport::new(encoded.into());
    assert_eq!(
        transport.pull_frame(None),
        Err(PlaybackError::MissingCodec(ClipId::new(7)))
    );
    let mut codec = FailingCodec;
    assert!(matches!(
        transport.pull_frame(Some(&mut codec)),
        Err(PlaybackError::Codec(CodecError::DecodeFailed { .. }))
    ));
    assert_eq!(transport.cursor(), FrameIndex::new(0));
}
