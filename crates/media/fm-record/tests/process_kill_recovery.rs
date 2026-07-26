use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    num::{NonZeroU32, NonZeroU64, NonZeroU128},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fm_frame::{
    ClockDomainId, CodecConfigGeneration, CodecId, EncodedPacket, EncodedPacketMetadata,
    MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp, PacketFlags,
    SequenceNumber, StreamId,
};
use fm_record::{
    ActionOutcome, ActionReceiptId, DurableWriter, QueueLimits, RecordEvent, RecorderConfig,
    RecorderCoordinator, RecorderId, RecorderState, SegmentPolicy, WriterFactory,
};
use fm_types::{MediaTimestamp, PixelFormat, TimeBase, VideoDimensions};

const CHILD_ROOT: &str = "FM_RECORD_TORN_WRITE_CHILD_ROOT";
const FRAME_HEADER_BYTES: u64 = 24;
const TORN_PAYLOAD_BYTES: u64 = 7;
const TORN_RECORD_BYTES: u64 = FRAME_HEADER_BYTES + TORN_PAYLOAD_BYTES;
const CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "fm-record-process-kill-{}-{nonce}-{counter}",
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

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "child already reaped"))?
            .try_wait()?;
        if status.is_some() {
            self.child = None;
        }
        Ok(status)
    }

    fn terminate_and_reap(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        if let Err(error) = self.child.as_mut().expect("live child").kill()
            && error.kind() != io::ErrorKind::InvalidInput
        {
            self.abandon();
            return Err(error);
        }

        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(error) => {
                    self.abandon();
                    return Err(error);
                }
            }
            if Instant::now() >= deadline {
                self.abandon();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out reaping killed child",
                ));
            }
            thread::sleep(CHILD_POLL_INTERVAL);
        }
    }

    fn abandon(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let _ = child.try_wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() && self.terminate_and_reap(CHILD_CLEANUP_TIMEOUT).is_err() {
            self.abandon();
        }
    }
}

fn id(value: u128) -> RecorderId {
    RecorderId::new(NonZeroU128::new(value).expect("nonzero recorder id"))
}

fn receipt(value: u128) -> ActionReceiptId {
    ActionReceiptId::new(NonZeroU128::new(value).expect("nonzero receipt id"))
}

fn video(sequence: u64) -> RecordEvent {
    let time_base = TimeBase::new(1, 30).expect("valid time base");
    let original_timestamp = OriginalTimestamp::new(
        MediaTimestamp::new(i64::try_from(sequence).expect("small sequence")),
        time_base,
    );
    let timing = MediaTiming::new(
        original_timestamp,
        NormalizedTimestamp::from_nanos(
            i64::try_from(sequence * 33_333_333).expect("small timestamp"),
        ),
        NormalizedDuration::from_nanos(33_333_333).expect("nonzero duration"),
        ClockDomainId::new(NonZeroU128::new(1).expect("nonzero clock")),
        SequenceNumber::new(sequence),
    )
    .expect("valid timing");
    let metadata = EncodedPacketMetadata::new(
        CodecId::new("video/h264").expect("valid codec"),
        CodecConfigGeneration::new(NonZeroU64::new(1).expect("nonzero generation")),
        StreamId::new(NonZeroU32::new(1).expect("nonzero stream")),
        None,
        timing,
        original_timestamp,
        PacketFlags::RANDOM_ACCESS,
    )
    .expect("matching timestamps");
    let packet =
        EncodedPacket::from_bytes(metadata, vec![u8::try_from(sequence).unwrap_or(255); 64])
            .expect("nonempty payload");
    RecordEvent::video(
        packet,
        VideoDimensions::new(1920, 1080).expect("valid dimensions"),
        PixelFormat::Nv12,
    )
    .expect("video event")
}

struct TornWriteFactory {
    armed: Arc<AtomicBool>,
    marker: PathBuf,
}

impl WriterFactory for TornWriteFactory {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Box::new(TornWriter {
            file,
            armed: Arc::clone(&self.armed),
            marker: self.marker.clone(),
            segment: path.extension().is_some_and(|extension| extension == "fms"),
            torn_header_written: false,
            torn: false,
        }))
    }
}

struct TornWriter {
    file: File,
    armed: Arc<AtomicBool>,
    marker: PathBuf,
    segment: bool,
    torn_header_written: bool,
    torn: bool,
}

impl Write for TornWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.segment && self.armed.load(Ordering::SeqCst) && !self.torn {
            if !self.torn_header_written {
                self.file.write_all(buffer)?;
                self.torn_header_written = true;
                return Ok(buffer.len());
            }
            let written = usize::try_from(TORN_PAYLOAD_BYTES)
                .unwrap_or(7)
                .min(buffer.len());
            self.file.write_all(&buffer[..written])?;
            self.file.sync_all()?;
            let marker = File::create(&self.marker)?;
            marker.sync_all()?;
            self.torn = true;
            loop {
                thread::park();
            }
        }
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl DurableWriter for TornWriter {
    fn sync_all(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[test]
fn subprocess_child_writes_torn_tail() {
    let Some(root) = env::var_os(CHILD_ROOT).map(PathBuf::from) else {
        return;
    };
    let armed = Arc::new(AtomicBool::new(false));
    let factory = Arc::new(TornWriteFactory {
        armed: Arc::clone(&armed),
        marker: root.join("torn-write-ready"),
    });
    let (mut coordinator, report) =
        RecorderCoordinator::open_with_writer_factory(&root, factory).expect("open child recorder");
    assert!(report.failures.is_empty());
    let recorder = id(1);
    coordinator
        .start(
            recorder,
            receipt(1),
            RecorderConfig::new(QueueLimits::new(8, 1024 * 1024), SegmentPolicy::default()),
        )
        .expect("start child recorder");
    coordinator
        .append(recorder, video(0))
        .expect("append first durable record");
    coordinator
        .append(recorder, video(1))
        .expect("append second durable record");
    armed.store(true, Ordering::SeqCst);

    coordinator
        .append(recorder, video(2))
        .expect("parent kills child during this append");
    panic!("torn append unexpectedly completed");
}

#[test]
fn process_kill_repairs_torn_tail_and_resumes_active_recording() {
    let temp = TempDirectory::new();
    let marker = temp.path().join("torn-write-ready");
    let child = Command::new(env::current_exe().expect("locate integration test binary"))
        .arg("--exact")
        .arg("subprocess_child_writes_torn_tail")
        .arg("--nocapture")
        .env(CHILD_ROOT, temp.path())
        .spawn()
        .expect("spawn torn-write child");
    let mut child = ChildGuard::new(child);

    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() {
        if let Some(status) = child.try_wait().expect("poll child") {
            panic!("torn-write child exited before blocking: {status}");
        }
        if Instant::now() >= deadline {
            child
                .terminate_and_reap(CHILD_CLEANUP_TIMEOUT)
                .expect("terminate timed-out torn-write child");
            panic!("timed out waiting for torn write");
        }
        thread::sleep(CHILD_POLL_INTERVAL);
    }

    let status = child
        .terminate_and_reap(CHILD_CLEANUP_TIMEOUT)
        .expect("kill and reap child during partial write");
    assert!(!status.success(), "killed child unexpectedly succeeded");

    let recorder = id(1);
    let start_receipt = receipt(1);
    let stop_receipt = receipt(2);
    let (mut reopened, report) =
        RecorderCoordinator::open(temp.path()).expect("repair and reopen recording");
    assert!(report.failures.is_empty());
    let (_, recovery) = report
        .recovered
        .iter()
        .find(|(recovered, _)| *recovered == recorder)
        .expect("recorder was recovered");
    assert_eq!(recovery.segments.len(), 1);
    assert_eq!(recovery.segments[0].records, 2);
    assert_eq!(recovery.segments[0].truncated_bytes, TORN_RECORD_BYTES);
    assert_eq!(
        reopened
            .snapshot(recorder)
            .expect("recovered snapshot")
            .state,
        RecorderState::Recording
    );
    assert_eq!(
        reopened
            .start(
                recorder,
                start_receipt,
                RecorderConfig::new(QueueLimits::new(1, 1), SegmentPolicy::by_frames(1)),
            )
            .expect("deduplicate durable start receipt"),
        ActionOutcome::AlreadyApplied
    );

    reopened
        .append(recorder, video(3))
        .expect("append after recovery");
    assert_eq!(
        reopened
            .stop(recorder, stop_receipt)
            .expect("stop recovered recorder"),
        ActionOutcome::Applied
    );
    assert_eq!(
        reopened
            .stop(recorder, stop_receipt)
            .expect("deduplicate stop receipt"),
        ActionOutcome::AlreadyApplied
    );
    drop(reopened);

    let (mut finalized, report) =
        RecorderCoordinator::open(temp.path()).expect("reopen finalized recorder");
    assert!(report.failures.is_empty());
    let (_, recovery) = report
        .recovered
        .iter()
        .find(|(recovered, _)| *recovered == recorder)
        .expect("finalized recorder was recovered");
    assert_eq!(recovery.segments.len(), 1);
    assert_eq!(recovery.segments[0].records, 3);
    assert_eq!(recovery.segments[0].truncated_bytes, 0);
    assert_eq!(
        finalized.snapshot(recorder).expect("final snapshot").state,
        RecorderState::Stopped
    );
    assert_eq!(
        finalized
            .stop(recorder, stop_receipt)
            .expect("deduplicate stop after restart"),
        ActionOutcome::AlreadyApplied
    );
}
