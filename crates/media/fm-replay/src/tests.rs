use std::{
    collections::BTreeSet,
    fs,
    num::NonZeroU128,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use fm_audio::Gain;
use fm_frame::{ClockDomainId, NormalizedTimestamp, SequenceNumber};
use fm_playback::{FrameIndex, Speed};
use fm_record::{RecorderId, RecoveredSegment, RecoveryReport};
use fm_types::{Channel, ChannelLayout};

use super::*;

fn nonzero(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn camera(value: u128) -> CameraId {
    CameraId::new(nonzero(value))
}

fn root(value: u128) -> StorageRootId {
    StorageRootId::new(nonzero(value))
}

fn timestamp(nanos: i64) -> NormalizedTimestamp {
    NormalizedTimestamp::from_nanos(nanos)
}

fn range(start: i64, end: i64) -> TimelineRange {
    TimelineRange::new(timestamp(start), timestamp(end)).unwrap()
}

fn four_channels() -> ChannelLayout {
    ChannelLayout::new(vec![
        Channel::Left,
        Channel::Right,
        Channel::Center,
        Channel::LowFrequency,
    ])
    .unwrap()
}

fn sources(count: u128) -> SourceCatalog {
    let descriptors = (1..=count)
        .map(|value| CameraSource {
            id: camera(value),
            name: format!("Camera {value}"),
            storage_root_id: root(value),
            recorder_id: RecorderId::new(nonzero(value)),
            clock_domain: ClockDomainId::new(nonzero(value)),
            audio_layout: four_channels(),
        })
        .collect();
    SourceCatalog::new(descriptors).unwrap()
}

fn event(id: u128, start: i64, end: i64, preferred: CameraId) -> ReplayEvent {
    ReplayEvent {
        id: EventId::new(nonzero(id)),
        name: format!("Event {id}"),
        timeline: range(start, end),
        angles: BTreeSet::from([camera(1), camera(2), camera(3), camera(4)]),
        preferred_angle: preferred,
        tags: BTreeSet::new(),
        note: String::new(),
        folders: BTreeSet::new(),
    }
}

fn segment(id: u128, start: i64, end: i64, bytes: u64, path: PathBuf) -> SegmentMetadata {
    SegmentMetadata {
        id: SegmentId::new(nonzero(id)),
        camera_id: camera(1),
        storage_root_id: root(1),
        recorder_segment_index: u64::try_from(id).unwrap(),
        timeline: range(start, end),
        path,
        bytes,
        state: SegmentState::Closed,
        protections: BTreeSet::new(),
    }
}

fn catalog(path: PathBuf) -> RollingSegmentCatalog {
    let mut catalog = RollingSegmentCatalog::default();
    catalog
        .add_root(StorageRoot {
            id: root(1),
            path,
            capacity_bytes: 10_000,
        })
        .unwrap();
    catalog.assign_camera(camera(1), root(1)).unwrap();
    catalog
}

#[test]
fn supports_eight_sources_with_four_channel_descriptors() {
    let catalog = sources(8);
    assert_eq!(catalog.len(), MAX_REPLAY_SOURCES);
    assert!(
        catalog
            .iter()
            .all(|source| source.audio_layout.channels().len() == MAX_AUDIO_CHANNELS)
    );
    assert!(matches!(
        SourceCatalog::new(
            (1..=9)
                .map(|value| CameraSource {
                    id: camera(value),
                    name: value.to_string(),
                    storage_root_id: root(value),
                    recorder_id: RecorderId::new(nonzero(value)),
                    clock_domain: ClockDomainId::new(nonzero(value)),
                    audio_layout: four_channels(),
                })
                .collect()
        ),
        Err(ReplayError::TooManySources(9))
    ));
}

#[test]
fn retention_never_removes_protected_or_open_segments() {
    let mut catalog = catalog(PathBuf::from("/metadata-only"));
    catalog
        .insert_segment(segment(1, 0, 10, 100, PathBuf::from("one.seg")))
        .unwrap();
    catalog
        .insert_segment(segment(2, 10, 20, 100, PathBuf::from("two.seg")))
        .unwrap();
    let mut third = segment(3, 20, 30, 100, PathBuf::from("three.seg"));
    third.state = SegmentState::Open;
    catalog.insert_segment(third).unwrap();
    let protected = catalog.protect_range(
        camera(1),
        range(0, 10),
        ProtectionReference::Event(EventId::new(nonzero(1))),
    );
    assert_eq!(protected, vec![SegmentId::new(nonzero(1))]);

    let report = catalog.reclaim(root(1), 250).unwrap();
    assert_eq!(report.deleted, vec![SegmentId::new(nonzero(2))]);
    assert_eq!(report.blocked_bytes, 150);
    assert!(catalog.segment(SegmentId::new(nonzero(1))).is_some());
    assert!(catalog.segment(SegmentId::new(nonzero(3))).is_some());

    catalog.release_reference(ProtectionReference::Event(EventId::new(nonzero(1))));
    let report = catalog.reclaim(root(1), 100).unwrap();
    assert_eq!(report.deleted, vec![SegmentId::new(nonzero(1))]);
}

#[test]
fn marks_last_n_events_tags_notes_folders_and_lists_are_deterministic() {
    let mut marks = ReplayMarks::default();
    marks.set_in(timestamp(5_000_000_000));
    marks.set_out(timestamp(8_000_000_000));
    assert_eq!(marks.range().unwrap(), range(5_000_000_000, 8_000_000_000));
    assert_eq!(
        ReplayMarks::last_n(timestamp(30_000_000_000), 20).unwrap(),
        range(10_000_000_000, 30_000_000_000)
    );

    let event_id = EventId::new(nonzero(1));
    let folder_id = FolderId::new(nonzero(1));
    let list_id = ListId::new(nonzero(1));
    let mut database = EventDatabase::default();
    database.insert_event(event(1, 5, 8, camera(2))).unwrap();
    database.add_tag(event_id, "goal").unwrap();
    database.set_note(event_id, "left-foot finish").unwrap();
    database
        .add_folder(EventFolder {
            id: folder_id,
            name: "First half".into(),
        })
        .unwrap();
    database.place_in_folder(event_id, folder_id).unwrap();
    database
        .add_list(EventList {
            id: list_id,
            name: "Top plays".into(),
            events: Vec::new(),
        })
        .unwrap();
    database.append_to_list(list_id, event_id).unwrap();

    let stored = database.event(event_id).unwrap();
    assert_eq!(stored.tags.iter().collect::<Vec<_>>(), vec!["goal"]);
    assert_eq!(stored.note, "left-foot finish");
    assert!(stored.folders.contains(&folder_id));
    assert_eq!(database.list(list_id).unwrap().events, vec![event_id]);
}

#[test]
fn replay_a_and_b_are_independent_until_transport_is_linked() {
    let mut decks = ReplayDecks::new(camera(1), camera(2), timestamp(100));
    let first = event(1, 0, 100, camera(1));
    let second = event(2, 200, 300, camera(2));
    decks.cue_event(ReplayChannelId::A, &first);
    decks.play(ReplayChannelId::A, PlaybackRate::FORWARD_1X);
    assert!(matches!(
        decks.channel(ReplayChannelId::B).mode,
        ChannelMode::Live
    ));

    decks.cue_event(ReplayChannelId::B, &second);
    decks.select_angle(ReplayChannelId::A, camera(3));
    decks.set_linked(true);
    decks.pause(ReplayChannelId::A);
    assert_eq!(
        decks.channel(ReplayChannelId::A).transport,
        ChannelTransport::Paused
    );
    assert_eq!(
        decks.channel(ReplayChannelId::B).transport,
        ChannelTransport::Paused
    );
    assert_eq!(decks.channel(ReplayChannelId::A).angle, camera(3));
    assert_eq!(decks.channel(ReplayChannelId::B).angle, camera(2));
}

#[test]
fn transport_supports_speed_jog_shuttle_reverse_and_auto_return() {
    let replay_event = event(1, 0, 1_000, camera(1));
    let mut decks = ReplayDecks::new(camera(1), camera(2), timestamp(2_000));
    decks.cue_event(ReplayChannelId::A, &replay_event);
    decks.jog(ReplayChannelId::A, 3, 100).unwrap();
    assert_eq!(decks.channel(ReplayChannelId::A).cursor, timestamp(300));
    decks.shuttle(
        ReplayChannelId::A,
        PlaybackRate::from_milli(-2_000).unwrap(),
    );
    decks.advance(ReplayChannelId::A, 100, timestamp(2_000));
    assert_eq!(decks.channel(ReplayChannelId::A).cursor, timestamp(100));

    decks.set_auto_return(ReplayChannelId::A, true);
    decks.play(ReplayChannelId::A, PlaybackRate::from(Speed::Forward2x));
    decks.advance(ReplayChannelId::A, 1_000, timestamp(5_000));
    assert!(matches!(
        decks.channel(ReplayChannelId::A).mode,
        ChannelMode::Live
    ));
    assert_eq!(decks.channel(ReplayChannelId::A).cursor, timestamp(5_000));
}

#[test]
fn quad_view_resolves_one_synchronized_target_and_reports_skew() {
    let source_catalog = sources(8);
    let mut timeline = SourceTimeline::new(&source_catalog);
    for value in 1..=4 {
        timeline
            .observe(
                camera(value),
                timestamp(1_000 + i64::try_from(value).unwrap()),
                SequenceNumber::new(10),
                FrameIndex::new(10),
            )
            .unwrap();
    }
    timeline
        .record_discontinuity(camera(1), timestamp(1_010), "signal return")
        .unwrap();
    timeline
        .observe(
            camera(1),
            timestamp(1_011),
            SequenceNumber::new(0),
            FrameIndex::new(0),
        )
        .unwrap();

    let quad = timeline
        .quad_view(
            [camera(1), camera(2), camera(3), camera(4)],
            timestamp(1_020),
            10,
        )
        .unwrap();
    assert_eq!(quad.cells.len(), 4);
    assert_eq!(quad.skew_nanos, 9);
    assert!(quad.in_sync);
    assert_eq!(quad.cells[0].observation.discontinuity_epoch, 1);
}

#[test]
fn highlight_export_progresses_while_recording_continues() {
    let mut timeline = HighlightTimeline::default();
    timeline
        .push(HighlightItem {
            id: HighlightId::new(nonzero(1)),
            event_id: EventId::new(nonzero(1)),
            timeline: range(0, 1_000),
            angle: camera(1),
            speed: PlaybackRate::from_milli(500).unwrap(),
            transition: Transition::Dissolve {
                duration_nanos: 250,
            },
        })
        .unwrap();
    timeline.set_music(Some(MusicBed {
        id: MusicTrackId::new(nonzero(1)),
        locator: "music.wav".into(),
        gain: Gain::from_db(-6.0).unwrap(),
    }));
    let job_id = ExportJobId::new(nonzero(1));
    let mut session = ReplaySession::default();
    session.set_capture_state(CaptureState::Recording);
    session
        .submit_export(job_id, &timeline, "highlights.mov")
        .unwrap();
    session
        .update_export(job_id, ExportJobState::Running { completed_items: 1 })
        .unwrap();
    session
        .update_export(job_id, ExportJobState::Completed { bytes: 500 })
        .unwrap();

    assert_eq!(session.capture_state(), CaptureState::Recording);
    let job = session.export(job_id).unwrap();
    assert!(job.submitted_while_recording);
    assert_eq!(job.timeline_snapshot.items().len(), 1);
    assert_eq!(job.state, ExportJobState::Completed { bytes: 500 });
}

#[test]
fn capacity_low_space_and_recovery_reports_are_conservative() {
    let prediction = CapacityPrediction::from_sample(
        CapacitySample {
            free_bytes: 1_000,
            bytes_written: 200,
            elapsed_nanos: 1_000_000_000,
        },
        200,
    );
    assert_eq!(prediction.bytes_per_second, 200);
    assert_eq!(prediction.seconds_until_full, Some(5));
    assert_eq!(prediction.seconds_until_low_space, Some(4));

    let directory = temporary_directory();
    let present = directory.join("present.seg");
    let missing = directory.join("missing.seg");
    let orphan = directory.join("orphan.seg");
    fs::write(&present, b"segment").unwrap();
    fs::write(&orphan, b"orphan").unwrap();
    let mut catalog = catalog(directory.clone());
    let mut protected = segment(1, 0, 10, 100, present);
    protected
        .protections
        .insert(ProtectionReference::Event(EventId::new(nonzero(1))));
    catalog.insert_segment(protected).unwrap();
    catalog
        .insert_segment(segment(2, 10, 20, 100, missing))
        .unwrap();

    let decision = LowSpacePolicy {
        reserve_bytes: 200,
        action: LowSpaceAction::DeleteOldestUnprotected,
    }
    .apply(&mut catalog, root(1), 100, 150)
    .unwrap();
    assert!(matches!(decision, LowSpaceDecision::ProtectionBlocked(_)));

    let recorder_report = RecoveryReport {
        manifest_records: 2,
        manifest_truncated_bytes: 3,
        segments: vec![RecoveredSegment {
            index: 1,
            records: 4,
            bytes: 100,
            truncated_bytes: 5,
        }],
    };
    let report = inspect_recovery(
        &catalog,
        vec![(RecorderId::new(nonzero(1)), recorder_report)],
    )
    .unwrap();
    assert_eq!(report.missing_segments, Vec::<SegmentId>::new());
    assert_eq!(report.orphan_files, vec![orphan]);
    assert_eq!(report.recorder_truncated_bytes, 8);
    fs::remove_dir_all(directory).unwrap();
}

fn temporary_directory() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("fm-replay-{}-{sequence}", std::process::id()));
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    fs::create_dir_all(&path).unwrap();
    path
}
