use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    num::{NonZeroU32, NonZeroU64, NonZeroU128},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use fm_frame::{
    ClockDomainId, CodecConfigGeneration, CodecId, EncodedPacket, EncodedPacketMetadata,
    MediaFlags, MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    PacketFlags, SequenceNumber, StreamId,
};
use fm_types::{ChannelLayout, MediaTimestamp, PixelFormat, SampleRate, TimeBase, VideoDimensions};

use crate::{
    ActionOutcome, ActionReceiptId, AppendError, Discontinuity, DurableWriter, EnqueueError,
    QueueLimits, RecordEvent, RecorderConfig, RecorderCoordinator, RecorderError, RecorderId,
    RecorderState, SegmentPolicy, WriterFactory, format, repair_recording,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fm-record-{name}-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(value: u128) -> RecorderId {
    RecorderId::new(NonZeroU128::new(value).expect("nonzero recorder id"))
}

fn receipt(value: u128) -> ActionReceiptId {
    ActionReceiptId::new(NonZeroU128::new(value).expect("nonzero receipt id"))
}

fn config(policy: SegmentPolicy) -> RecorderConfig {
    RecorderConfig::new(QueueLimits::new(8, 1024 * 1024), policy)
}

fn timing(sequence: u64) -> MediaTiming {
    let time_base = TimeBase::new(1, 30).expect("valid time base");
    MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::try_from(sequence).expect("small sequence")),
            time_base,
        ),
        NormalizedTimestamp::from_nanos(
            i64::try_from(sequence * 33_333_333).expect("small timestamp"),
        ),
        NormalizedDuration::from_nanos(33_333_333).expect("nonzero duration"),
        ClockDomainId::new(NonZeroU128::new(1).expect("nonzero clock")),
        SequenceNumber::new(sequence),
    )
    .expect("valid timing")
}

fn packet(sequence: u64, payload_len: usize) -> EncodedPacket {
    let metadata = EncodedPacketMetadata::new(
        CodecId::new("video/h264").expect("valid codec"),
        CodecConfigGeneration::new(NonZeroU64::new(1).expect("nonzero generation")),
        StreamId::new(NonZeroU32::new(1).expect("nonzero stream")),
        None,
        timing(sequence),
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::try_from(sequence).expect("small sequence")),
            TimeBase::new(1, 30).expect("valid time base"),
        ),
        PacketFlags::RANDOM_ACCESS,
    )
    .expect("matching timestamps");
    EncodedPacket::from_bytes(
        metadata,
        vec![u8::try_from(sequence).unwrap_or(255); payload_len],
    )
    .expect("nonempty bounded payload")
}

fn video(sequence: u64, payload_len: usize) -> RecordEvent {
    RecordEvent::video(
        packet(sequence, payload_len),
        VideoDimensions::new(1920, 1080).expect("valid dimensions"),
        PixelFormat::Nv12,
    )
    .expect("byte-backed packet")
}

fn recording_directory(root: &Path, recorder: RecorderId) -> PathBuf {
    root.join(format!("recorder-{:032x}", recorder.get().get()))
}

#[test]
fn segments_by_frames_bytes_and_manual_rotation() {
    let temp = TempDirectory::new("segmentation");
    let (mut coordinator, report) =
        RecorderCoordinator::open(temp.path()).expect("open coordinator");
    assert!(report.failures.is_empty());

    let frame_recorder = id(1);
    coordinator
        .start(
            frame_recorder,
            receipt(1),
            config(SegmentPolicy::by_frames(2)),
        )
        .expect("start frame recorder");
    for sequence in 0..5 {
        coordinator
            .append(frame_recorder, video(sequence, 16))
            .expect("append frame");
    }
    coordinator
        .stop(frame_recorder, receipt(2))
        .expect("stop frame recorder");
    let frame_report = repair_recording(recording_directory(temp.path(), frame_recorder))
        .expect("repair frame recorder");
    assert_eq!(
        frame_report
            .segments
            .iter()
            .map(|segment| segment.records)
            .collect::<Vec<_>>(),
        vec![2, 2, 1]
    );

    let byte_recorder = id(2);
    let probe = video(0, 80);
    let (_, encoded) = format::encoded_event(&probe).expect("encode probe");
    let one_record = format::framed_len(encoded.len()).expect("record length");
    coordinator
        .start(
            byte_recorder,
            receipt(3),
            config(SegmentPolicy::by_bytes(one_record * 2 - 1)),
        )
        .expect("start byte recorder");
    coordinator
        .append(byte_recorder, probe)
        .expect("append byte frame");
    coordinator
        .append(byte_recorder, video(1, 80))
        .expect("rotate on byte limit");
    coordinator
        .stop(byte_recorder, receipt(4))
        .expect("stop byte recorder");
    let byte_report = repair_recording(recording_directory(temp.path(), byte_recorder))
        .expect("repair byte recorder");
    assert_eq!(
        byte_report
            .segments
            .iter()
            .map(|segment| segment.records)
            .collect::<Vec<_>>(),
        vec![1, 1]
    );

    let manual_recorder = id(3);
    coordinator
        .start(
            manual_recorder,
            receipt(5),
            config(SegmentPolicy::default()),
        )
        .expect("start manual recorder");
    coordinator
        .append(manual_recorder, video(0, 8))
        .expect("append before rotation");
    coordinator
        .rotate(manual_recorder)
        .expect("manual rotation");
    coordinator
        .append(manual_recorder, video(1, 8))
        .expect("append after rotation");
    coordinator
        .stop(manual_recorder, receipt(6))
        .expect("stop manual recorder");
    let manual_report = repair_recording(recording_directory(temp.path(), manual_recorder))
        .expect("repair manual recorder");
    assert_eq!(manual_report.segments.len(), 2);
}

#[test]
fn records_audio_video_timed_metadata_and_discontinuity() {
    let temp = TempDirectory::new("packet-kinds");
    let (mut coordinator, _) = RecorderCoordinator::open(temp.path()).expect("open coordinator");
    let recorder = id(10);
    coordinator
        .start(recorder, receipt(10), config(SegmentPolicy::default()))
        .expect("start recorder");
    let audio = RecordEvent::audio(
        packet(0, 12),
        SampleRate::new(48_000).expect("valid sample rate"),
        ChannelLayout::stereo(),
    )
    .expect("audio event");
    let timed = RecordEvent::timed(packet(2, 5), "application/id3").expect("timed event");
    let discontinuity = RecordEvent::discontinuity(Discontinuity {
        stream_id: None,
        timing: timing(3).with_flags(MediaFlags::DISCONTINUITY),
        reason: "source switch".to_owned(),
    })
    .expect("discontinuity");
    for event in [audio, video(1, 20), timed, discontinuity] {
        coordinator.append(recorder, event).expect("append event");
    }
    coordinator
        .stop(recorder, receipt(11))
        .expect("stop recorder");
    let report =
        repair_recording(recording_directory(temp.path(), recorder)).expect("scan records");
    assert_eq!(report.segments[0].records, 4);
}

#[test]
fn repair_truncates_only_a_torn_final_record() {
    let temp = TempDirectory::new("torn");
    let recorder = id(20);
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open(temp.path()).expect("open coordinator");
        coordinator
            .start(recorder, receipt(20), config(SegmentPolicy::default()))
            .expect("start recorder");
        coordinator
            .append(recorder, video(0, 32))
            .expect("append event");
        // Dropping without stop models process loss after the durable append.
    }
    let directory = recording_directory(temp.path(), recorder);
    let segment = format::segment_path(&directory, 0);
    let valid_len = fs::metadata(&segment).expect("segment metadata").len();
    OpenOptions::new()
        .append(true)
        .open(&segment)
        .expect("open segment")
        .write_all(b"FMRC\x01")
        .expect("write torn header");

    let report = repair_recording(&directory).expect("repair torn record");
    assert_eq!(report.segments[0].records, 1);
    assert_eq!(report.segments[0].truncated_bytes, 5);
    assert_eq!(
        fs::metadata(segment).expect("segment metadata").len(),
        valid_len
    );
}

#[test]
fn repair_truncates_a_torn_manifest_tail_and_reopens_active_recording() {
    let temp = TempDirectory::new("torn-manifest");
    let recorder = id(21);
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open(temp.path()).expect("open coordinator");
        coordinator
            .start(recorder, receipt(21), config(SegmentPolicy::default()))
            .expect("start recorder");
        coordinator
            .append(recorder, video(0, 16))
            .expect("append event");
    }

    let directory = recording_directory(temp.path(), recorder);
    let manifest = directory.join(format::MANIFEST_NAME);
    let valid_len = fs::metadata(&manifest).expect("manifest metadata").len();
    let mut file = OpenOptions::new()
        .append(true)
        .open(&manifest)
        .expect("open manifest");
    file.write_all(b"FMRC\x01\x13")
        .expect("write torn manifest header");
    file.sync_all().expect("sync torn manifest header");

    let report = repair_recording(&directory).expect("repair manifest");
    assert_eq!(report.manifest_records, 1);
    assert_eq!(report.manifest_truncated_bytes, 6);
    assert_eq!(
        fs::metadata(&manifest).expect("manifest metadata").len(),
        valid_len
    );

    let (mut reopened, report) =
        RecorderCoordinator::open(temp.path()).expect("reopen active recording");
    assert!(report.failures.is_empty());
    assert_eq!(
        reopened.snapshot(recorder).expect("snapshot").state,
        RecorderState::Recording
    );
    reopened
        .append(recorder, video(1, 16))
        .expect("continue active recording");
    reopened
        .stop(recorder, receipt(22))
        .expect("stop recovered recording");
}

#[test]
fn checksum_corruption_is_reported_without_truncation() {
    let temp = TempDirectory::new("checksum");
    let recorder = id(30);
    let (mut coordinator, _) = RecorderCoordinator::open(temp.path()).expect("open coordinator");
    coordinator
        .start(recorder, receipt(30), config(SegmentPolicy::default()))
        .expect("start recorder");
    coordinator
        .append(recorder, video(0, 32))
        .expect("append event");
    drop(coordinator);

    let directory = recording_directory(temp.path(), recorder);
    let segment = format::segment_path(&directory, 0);
    let original_len = fs::metadata(&segment).expect("segment metadata").len();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&segment)
        .expect("open segment");
    file.seek(SeekFrom::End(-1)).expect("seek payload");
    let mut byte = [0];
    file.read_exact(&mut byte).expect("read payload byte");
    file.seek(SeekFrom::End(-1)).expect("seek payload again");
    file.write_all(&[byte[0] ^ 0xff]).expect("corrupt payload");
    file.sync_all().expect("sync corruption");

    let error = repair_recording(&directory).expect_err("checksum corruption must fail");
    assert!(error.to_string().contains("checksum mismatch"));
    assert_eq!(
        fs::metadata(segment).expect("segment metadata").len(),
        original_len
    );
}

#[test]
fn reconciliation_isolates_corruption_to_one_recorder() {
    let temp = TempDirectory::new("recovery-corruption-isolation");
    let corrupt = id(31);
    let healthy = id(32);
    let healthy_stop = receipt(35);
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open(temp.path()).expect("open coordinator");
        coordinator
            .start(corrupt, receipt(31), config(SegmentPolicy::default()))
            .expect("start recorder to corrupt");
        coordinator
            .append(corrupt, video(0, 16))
            .expect("append recorder to corrupt");
        coordinator
            .stop(corrupt, receipt(32))
            .expect("stop recorder to corrupt");

        coordinator
            .start(healthy, receipt(34), config(SegmentPolicy::default()))
            .expect("start healthy recorder");
        coordinator
            .append(healthy, video(0, 16))
            .expect("append healthy recorder");
        coordinator
            .stop(healthy, healthy_stop)
            .expect("stop healthy recorder");
    }

    let segment = format::segment_path(&recording_directory(temp.path(), corrupt), 0);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&segment)
        .expect("open corrupt segment");
    file.seek(SeekFrom::End(-1)).expect("seek payload");
    let mut byte = [0];
    file.read_exact(&mut byte).expect("read payload byte");
    file.seek(SeekFrom::End(-1)).expect("seek payload again");
    file.write_all(&[byte[0] ^ 0xff]).expect("corrupt payload");
    file.sync_all().expect("sync corruption");

    let (mut reopened, report) =
        RecorderCoordinator::open(temp.path()).expect("reconcile coordinator");
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].recorder_id, Some(corrupt));
    assert_eq!(report.recovered.len(), 1);
    assert_eq!(report.recovered[0].0, healthy);
    assert_eq!(
        reopened.snapshot(corrupt).expect("corrupt snapshot").state,
        RecorderState::Failed
    );
    assert_eq!(
        reopened.snapshot(healthy).expect("healthy snapshot").state,
        RecorderState::Stopped
    );
    assert_eq!(
        reopened
            .stop(healthy, healthy_stop)
            .expect("healthy receipt survives reconciliation"),
        ActionOutcome::AlreadyApplied
    );
}

struct OpenFailingFactory {
    target: String,
}

impl WriterFactory for OpenFailingFactory {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>> {
        if path.to_string_lossy().contains(&self.target) {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected open failure",
            ));
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|file| Box::new(file) as Box<dyn DurableWriter>)
    }
}

#[test]
fn reconciliation_failure_after_receipts_does_not_poison_later_recorders() {
    let temp = TempDirectory::new("recovery-receipt-transaction");
    let staged = TempDirectory::new("recovery-receipt-healthy");
    let failed = id(33);
    let healthy = id(34);
    let shared_start = receipt(36);
    let healthy_stop = receipt(37);

    {
        let (mut coordinator, _) =
            RecorderCoordinator::open(temp.path()).expect("open failed recorder root");
        coordinator
            .start(failed, shared_start, config(SegmentPolicy::default()))
            .expect("start recorder that will fail reconciliation");
    }
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open(staged.path()).expect("open healthy recorder root");
        coordinator
            .start(healthy, shared_start, config(SegmentPolicy::default()))
            .expect("start healthy recorder");
        coordinator
            .stop(healthy, healthy_stop)
            .expect("stop healthy recorder");
    }
    fs::rename(
        recording_directory(staged.path(), healthy),
        recording_directory(temp.path(), healthy),
    )
    .expect("move healthy recorder into reconciliation root");

    let factory = Arc::new(OpenFailingFactory {
        target: format!("recorder-{:032x}", failed.get().get()),
    });
    let (mut reopened, report) =
        RecorderCoordinator::open_with_writer_factory(temp.path(), factory)
            .expect("reconcile coordinator");
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].recorder_id, Some(failed));
    assert_eq!(report.recovered.len(), 1);
    assert_eq!(report.recovered[0].0, healthy);
    assert_eq!(
        reopened
            .start(healthy, shared_start, config(SegmentPolicy::by_frames(1)))
            .expect("healthy recorder owns the shared receipt"),
        ActionOutcome::AlreadyApplied
    );
    assert_eq!(
        reopened
            .stop(healthy, healthy_stop)
            .expect("healthy stop receipt was committed"),
        ActionOutcome::AlreadyApplied
    );
}

struct SegmentOpenFailingFactory;

impl WriterFactory for SegmentOpenFailingFactory {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>> {
        if path.extension().is_some_and(|extension| extension == "fms") {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected segment open failure",
            ));
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|file| Box::new(file) as Box<dyn DurableWriter>)
    }
}

#[derive(Clone, Copy)]
enum ManifestFailure {
    Write(usize),
    Sync(usize),
}

struct ManifestFrameFailingFactory {
    failure: ManifestFailure,
}

impl WriterFactory for ManifestFrameFailingFactory {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Box::new(ManifestFrameFailingWriter {
            file,
            manifest: path.file_name().is_some_and(|name| name == "manifest.fmr"),
            frame: 0,
            failure: self.failure,
        }))
    }
}

struct ManifestFrameFailingWriter {
    file: File,
    manifest: bool,
    frame: usize,
    failure: ManifestFailure,
}

impl Write for ManifestFrameFailingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.manifest && buffer.len() == 24 && buffer.starts_with(b"FMRC") {
            self.frame = self.frame.saturating_add(1);
            if matches!(self.failure, ManifestFailure::Write(frame) if self.frame == frame) {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "injected manifest frame failure",
                ));
            }
        }
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl DurableWriter for ManifestFrameFailingWriter {
    fn sync_all(&mut self) -> io::Result<()> {
        if self.manifest
            && matches!(self.failure, ManifestFailure::Sync(frame) if self.frame == frame)
        {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected manifest sync failure",
            ));
        }
        self.file.sync_all()
    }
}

fn assert_start_setup_failure_does_not_commit_receipt(name: &str, factory: Arc<dyn WriterFactory>) {
    let temp = TempDirectory::new(name);
    let recorder = id(39);
    let start_receipt = receipt(39);
    let (mut coordinator, _) = RecorderCoordinator::open_with_writer_factory(temp.path(), factory)
        .expect("open coordinator");

    coordinator
        .start(recorder, start_receipt, config(SegmentPolicy::default()))
        .expect_err("injected setup failure must fail start");
    assert!(
        coordinator
            .start(recorder, start_receipt, config(SegmentPolicy::default()),)
            .is_err(),
        "an in-process retry must not report the failed receipt as applied"
    );
    drop(coordinator);

    let (mut reopened, report) =
        RecorderCoordinator::open(temp.path()).expect("reconcile failed start");
    assert!(report.failures.is_empty());
    assert!(report.recovered[0].1.segments.is_empty());
    assert!(!format::segment_path(&recording_directory(temp.path(), recorder), 0).exists());
    assert_eq!(
        reopened
            .snapshot(recorder)
            .expect("recovered snapshot")
            .state,
        RecorderState::Stopped
    );
    assert_eq!(
        reopened
            .start(recorder, start_receipt, config(SegmentPolicy::default()),)
            .expect("retry failed start"),
        ActionOutcome::Applied
    );
}

#[test]
fn segment_open_failure_does_not_commit_start_receipt() {
    assert_start_setup_failure_does_not_commit_receipt(
        "start-segment-open-failure",
        Arc::new(SegmentOpenFailingFactory),
    );
}

#[test]
fn manifest_boundary_failure_does_not_commit_start_receipt() {
    assert_start_setup_failure_does_not_commit_receipt(
        "start-manifest-boundary-failure",
        Arc::new(ManifestFrameFailingFactory {
            failure: ManifestFailure::Write(1),
        }),
    );
}

#[test]
fn repair_validates_every_file_before_truncating_any_tail() {
    let temp = TempDirectory::new("repair-validate-first");
    let recorder = id(38);
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open(temp.path()).expect("open coordinator");
        coordinator
            .start(recorder, receipt(38), config(SegmentPolicy::default()))
            .expect("start recorder");
        coordinator
            .append(recorder, video(0, 16))
            .expect("append first segment");
        coordinator.rotate(recorder).expect("rotate recorder");
        coordinator
            .append(recorder, video(1, 16))
            .expect("append second segment");
    }

    let directory = recording_directory(temp.path(), recorder);
    let manifest = directory.join(format::MANIFEST_NAME);
    let first = format::segment_path(&directory, 0);
    let second = format::segment_path(&directory, 1);
    OpenOptions::new()
        .append(true)
        .open(&first)
        .expect("open first segment")
        .write_all(b"FMRC")
        .expect("write torn tail");
    let first_len = fs::metadata(&first).expect("first metadata").len();
    OpenOptions::new()
        .append(true)
        .open(&manifest)
        .expect("open manifest")
        .write_all(b"FMRC")
        .expect("write torn manifest tail");
    let manifest_len = fs::metadata(&manifest).expect("manifest metadata").len();

    let mut second_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&second)
        .expect("open second segment");
    second_file.seek(SeekFrom::End(-1)).expect("seek payload");
    let mut byte = [0];
    second_file
        .read_exact(&mut byte)
        .expect("read payload byte");
    second_file
        .seek(SeekFrom::End(-1))
        .expect("seek payload again");
    second_file
        .write_all(&[byte[0] ^ 0xff])
        .expect("corrupt payload");
    second_file.sync_all().expect("sync corruption");

    repair_recording(&directory).expect_err("later corruption must fail repair");
    assert_eq!(
        fs::metadata(first).expect("first metadata").len(),
        first_len
    );
    assert_eq!(
        fs::metadata(manifest).expect("manifest metadata").len(),
        manifest_len
    );
}

struct CreatingOpenFailingFactory {
    target: String,
}

impl WriterFactory for CreatingOpenFailingFactory {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>> {
        if path.to_string_lossy().contains(&self.target) {
            File::create(path)?.sync_all()?;
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected failure after creating segment",
            ));
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|file| Box::new(file) as Box<dyn DurableWriter>)
    }
}

#[test]
fn recovery_removes_uncommitted_rotation_artifact_without_skipping_index() {
    let temp = TempDirectory::new("rotation-orphan");
    let recorder = id(42);
    let target = format::segment_name(1);
    let factory = Arc::new(CreatingOpenFailingFactory { target });
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open_with_writer_factory(temp.path(), factory)
                .expect("open coordinator");
        coordinator
            .start(recorder, receipt(43), config(SegmentPolicy::default()))
            .expect("start recorder");
        coordinator
            .append(recorder, video(0, 16))
            .expect("append segment");
        coordinator
            .rotate(recorder)
            .expect_err("rotation fails after creating next segment");
    }

    let (reopened, report) =
        RecorderCoordinator::open(temp.path()).expect("recover rotation artifact");
    assert!(report.failures.is_empty());
    assert_eq!(report.recovered[0].1.segments.len(), 1);
    assert_eq!(
        reopened.snapshot(recorder).expect("snapshot").segment_index,
        Some(1)
    );
    assert!(format::segment_path(&recording_directory(temp.path(), recorder), 1).exists());
    assert!(!format::segment_path(&recording_directory(temp.path(), recorder), 2).exists());
}

#[test]
fn failed_reconciliation_does_not_append_segment_open() {
    let temp = TempDirectory::new("reconcile-segment-open-order");
    let recorder = id(43);
    let target = format::segment_name(1);
    let factory: Arc<dyn WriterFactory> = Arc::new(OpenFailingFactory {
        target: target.clone(),
    });
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open_with_writer_factory(temp.path(), Arc::clone(&factory))
                .expect("open coordinator");
        coordinator
            .start(recorder, receipt(44), config(SegmentPolicy::default()))
            .expect("start recorder");
        coordinator
            .append(recorder, video(0, 16))
            .expect("append segment");
        coordinator
            .rotate(recorder)
            .expect_err("close succeeds before next segment open fails");
    }

    let manifest = recording_directory(temp.path(), recorder).join(format::MANIFEST_NAME);
    let before = fs::metadata(&manifest).expect("manifest metadata").len();
    let (_, report) = RecorderCoordinator::open_with_writer_factory(temp.path(), factory)
        .expect("isolate reconciliation failure");
    assert_eq!(report.failures.len(), 1);
    assert_eq!(
        fs::metadata(manifest).expect("manifest metadata").len(),
        before
    );
}

#[test]
fn encoded_frame_limit_is_enforced_before_queue_acceptance() {
    let temp = TempDirectory::new("encoded-frame-limit");
    let recorder = id(44);
    let (mut coordinator, _) = RecorderCoordinator::open(temp.path()).expect("open coordinator");
    coordinator
        .start(
            recorder,
            receipt(45),
            RecorderConfig::new(QueueLimits::new(1, usize::MAX), SegmentPolicy::default()),
        )
        .expect("start recorder");
    let mut event = video(0, 1);
    let RecordEvent::Video { payload, .. } = &mut event else {
        unreachable!("video helper returns a video event");
    };
    payload.resize(format::MAX_ENCODED_FRAME_PAYLOAD + 1, 0);
    assert!(matches!(
        coordinator.enqueue(recorder, event),
        Err(EnqueueError::EventTooLarge(_))
    ));
}

#[test]
fn recorder_directory_sync_is_supported() {
    let temp = TempDirectory::new("directory-sync");
    format::sync_directory(temp.path()).expect("sync test directory");
}

#[test]
fn flush_sync_failure_retains_uncommitted_batch() {
    let temp = TempDirectory::new("flush-sync-failure");
    let recorder = id(46);
    let armed = Arc::new(AtomicBool::new(false));
    let factory = Arc::new(FailingFactory {
        target: format::segment_name(1),
        armed: Arc::clone(&armed),
        fail_sync: true,
    });
    let (mut coordinator, _) = RecorderCoordinator::open_with_writer_factory(temp.path(), factory)
        .expect("open coordinator");
    coordinator
        .start(recorder, receipt(46), config(SegmentPolicy::by_frames(2)))
        .expect("start recorder");

    for event in [video(0, 8), video(1, 9)] {
        coordinator
            .enqueue(recorder, event)
            .expect("enqueue first batch");
    }
    assert_eq!(coordinator.flush(recorder).expect("flush first batch"), 2);
    coordinator.rotate(recorder).expect("rotate after first batch");
    let directory = recording_directory(temp.path(), recorder);
    let first = format::scan_segment(&format::segment_path(&directory, 0))
        .expect("read durable first segment");
    assert_eq!((first.records, first.truncated_bytes), (2, 0));

    let second_batch = [video(2, 10), video(3, 11)];
    let queued_bytes = second_batch.iter().map(RecordEvent::queue_bytes).sum();
    let expected_path = temp.path().join("expected-second-segment.fms");
    let mut expected_file = File::create(&expected_path).expect("create expected segment");
    for event in &second_batch {
        let (kind, payload) = format::encoded_event(event).expect("encode expected event");
        format::write_frame(
            &mut expected_file,
            kind,
            &payload,
            format::MAX_ENCODED_FRAME_PAYLOAD,
        )
        .expect("write expected event");
    }
    drop(expected_file);
    let expected = fs::read(&expected_path).expect("read expected segment");
    fs::remove_file(expected_path).expect("remove expected segment");
    for event in second_batch {
        coordinator
            .enqueue(recorder, event)
            .expect("enqueue second batch");
    }
    armed.store(true, Ordering::SeqCst);

    coordinator
        .flush(recorder)
        .expect_err("segment batch sync must fail");

    let after = coordinator
        .snapshot(recorder)
        .expect("snapshot after flush");
    assert_eq!(
        (
            after.state,
            after.queued_events,
            after.queued_bytes,
            after.written_frames,
            after.written_bytes,
        ),
        (RecorderState::Failed, 2, queued_bytes, 0, 0)
    );
    let second_path = format::segment_path(&directory, 1);
    assert_eq!(
        fs::read(&second_path).expect("read failed second batch"),
        expected
    );
    assert!(matches!(
        coordinator.flush(recorder),
        Err(RecorderError::InvalidState {
            state: RecorderState::Failed,
            ..
        })
    ));
    assert_eq!(
        fs::read(second_path).expect("read second segment after rejected retry"),
        expected
    );
}

#[test]
fn manifest_sync_failure_does_not_publish_action() {
    #[derive(Clone, Copy)]
    enum Action {
        Start,
        Stop,
    }

    for (name, action, fail_frame) in [("start", Action::Start, 1), ("stop", Action::Stop, 3)] {
        let temp = TempDirectory::new(name);
        let recorder = id(47);
        let action_receipt = receipt(48);
        let factory = Arc::new(ManifestFrameFailingFactory {
            failure: ManifestFailure::Sync(fail_frame),
        });
        let (mut coordinator, _) =
            RecorderCoordinator::open_with_writer_factory(temp.path(), factory)
                .expect("open coordinator");
        if matches!(action, Action::Stop) {
            coordinator
                .start(recorder, receipt(47), config(SegmentPolicy::default()))
                .expect("start recorder before stop");
        }
        let mut apply = |action| match action {
            Action::Start => {
                coordinator.start(recorder, action_receipt, config(SegmentPolicy::default()))
            }
            Action::Stop => coordinator.stop(recorder, action_receipt),
        };
        apply(action).expect_err("manifest sync must fail");
        assert!(matches!(
            apply(action),
            Err(RecorderError::InvalidState {
                state: RecorderState::Failed,
                ..
            })
        ));
    }
}

struct FailingFactory {
    target: String,
    armed: Arc<AtomicBool>,
    fail_sync: bool,
}

impl WriterFactory for FailingFactory {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Box::new(FailingWriter {
            file,
            fail: path.to_string_lossy().contains(&self.target),
            armed: Arc::clone(&self.armed),
            fail_sync: self.fail_sync,
        }))
    }
}

struct FailingWriter {
    file: File,
    fail: bool,
    armed: Arc<AtomicBool>,
    fail_sync: bool,
}

impl Write for FailingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.fail && self.armed.load(Ordering::SeqCst) && !self.fail_sync {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected full disk",
            ))
        } else {
            self.file.write(buffer)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl DurableWriter for FailingWriter {
    fn sync_all(&mut self) -> io::Result<()> {
        if self.fail && self.armed.load(Ordering::SeqCst) && self.fail_sync {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected sync failure",
            ));
        }
        self.file.sync_all()
    }
}

#[test]
fn writer_failure_is_isolated_between_recorders() {
    let temp = TempDirectory::new("failure-isolation");
    let failed = id(40);
    let healthy = id(41);
    let armed = Arc::new(AtomicBool::new(false));
    let factory = Arc::new(FailingFactory {
        target: format!("recorder-{:032x}", failed.get().get()),
        armed: Arc::clone(&armed),
        fail_sync: false,
    });
    let (mut coordinator, _) = RecorderCoordinator::open_with_writer_factory(temp.path(), factory)
        .expect("open coordinator");
    coordinator
        .start(failed, receipt(40), config(SegmentPolicy::default()))
        .expect("start failing recorder before arming");
    coordinator
        .start(healthy, receipt(41), config(SegmentPolicy::default()))
        .expect("start healthy recorder");
    armed.store(true, Ordering::SeqCst);

    assert!(matches!(
        coordinator.append(failed, video(0, 8)),
        Err(AppendError::Recorder(_))
    ));
    assert_eq!(
        coordinator.snapshot(failed).expect("failed snapshot").state,
        RecorderState::Failed
    );
    coordinator
        .append(healthy, video(0, 8))
        .expect("healthy recorder remains writable");
    coordinator
        .stop(healthy, receipt(42))
        .expect("healthy recorder stops");
    assert_eq!(
        coordinator
            .snapshot(healthy)
            .expect("healthy snapshot")
            .state,
        RecorderState::Stopped
    );
}

#[test]
fn queue_bounds_reject_without_losing_event() {
    let temp = TempDirectory::new("queue");
    let recorder = id(50);
    let (mut coordinator, _) = RecorderCoordinator::open(temp.path()).expect("open coordinator");
    coordinator
        .start(
            recorder,
            receipt(50),
            RecorderConfig::new(QueueLimits::new(1, 1024), SegmentPolicy::default()),
        )
        .expect("start recorder");
    coordinator
        .enqueue(recorder, video(0, 8))
        .expect("first event fits");
    let rejected = coordinator
        .enqueue(recorder, video(1, 8))
        .expect_err("second event exceeds count bound");
    assert!(matches!(rejected, EnqueueError::QueueFull(_)));
    assert_eq!(rejected.into_event().payload_len(), 8);
    assert_eq!(coordinator.flush(recorder).expect("flush queue"), 1);

    let oversized = coordinator
        .enqueue(recorder, video(2, 900))
        .expect_err("metadata overhead exceeds byte bound");
    assert!(matches!(oversized, EnqueueError::EventTooLarge(_)));
}

#[test]
fn stop_and_action_receipts_are_idempotent_across_restart() {
    let temp = TempDirectory::new("receipts");
    let recorder = id(60);
    let start_receipt = receipt(60);
    let stop_receipt = receipt(61);
    {
        let (mut coordinator, _) =
            RecorderCoordinator::open(temp.path()).expect("open coordinator");
        assert_eq!(
            coordinator
                .start(recorder, start_receipt, config(SegmentPolicy::default()),)
                .expect("start recorder"),
            ActionOutcome::Applied
        );
        assert_eq!(
            coordinator
                .stop(recorder, stop_receipt)
                .expect("stop recorder"),
            ActionOutcome::Applied
        );
        assert_eq!(
            coordinator
                .stop(recorder, stop_receipt)
                .expect("repeat stop"),
            ActionOutcome::AlreadyApplied
        );
    }

    let (mut restarted, report) =
        RecorderCoordinator::open(temp.path()).expect("reconcile coordinator");
    assert!(report.failures.is_empty());
    assert_eq!(
        restarted
            .start(recorder, start_receipt, config(SegmentPolicy::by_frames(1)),)
            .expect("start receipt is not re-executed"),
        ActionOutcome::AlreadyApplied
    );
    assert_eq!(
        restarted
            .stop(recorder, stop_receipt)
            .expect("stop receipt is not re-executed"),
        ActionOutcome::AlreadyApplied
    );
    assert_eq!(
        restarted.snapshot(recorder).expect("snapshot").state,
        RecorderState::Stopped
    );
}
