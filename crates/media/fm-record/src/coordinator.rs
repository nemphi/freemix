use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    num::NonZeroU128,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    ActionOutcome, ActionReceiptId, AppendError, EnqueueError, RecordEvent, RecorderConfig,
    RecorderError, RecorderId, RecorderSnapshot, RecorderState, RecoveredSegment, RecoveryReport,
    format::{
        self, ActionKind, ManifestEntry, decode_manifest, encode_manifest, encoded_event,
        framed_len, scan_manifest, scan_segment, segment_path, write_frame,
    },
};

pub trait DurableWriter: Write {
    /// Flushes buffered data and metadata to durable storage.
    ///
    /// # Errors
    ///
    /// Returns the underlying storage or injected synchronization error.
    fn sync_all(&mut self) -> io::Result<()>;
}

impl DurableWriter for File {
    fn sync_all(&mut self) -> io::Result<()> {
        File::sync_all(self)
    }
}

pub trait WriterFactory: Send + Sync {
    /// Opens a durable append-only writer for `path`.
    ///
    /// # Errors
    ///
    /// Returns the underlying filesystem or injected writer error.
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StdWriterFactory;

impl WriterFactory for StdWriterFactory {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn DurableWriter>> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|file| Box::new(file) as Box<dyn DurableWriter>)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationFailure {
    pub recorder_id: Option<RecorderId>,
    pub directory: PathBuf,
    pub error: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconciliationReport {
    pub recovered: Vec<(RecorderId, RecoveryReport)>,
    pub failures: Vec<ReconciliationFailure>,
}

struct Recorder {
    id: RecorderId,
    directory: PathBuf,
    state: RecorderState,
    config: Option<RecorderConfig>,
    queue: VecDeque<RecordEvent>,
    queue_bytes: usize,
    manifest: Option<Box<dyn DurableWriter>>,
    segment: Option<Box<dyn DurableWriter>>,
    segment_index: Option<u64>,
    segment_frames: u64,
    segment_bytes: u64,
    failure: Option<String>,
}

impl Recorder {
    fn failed(id: RecorderId, directory: PathBuf, failure: String) -> Self {
        Self {
            id,
            directory,
            state: RecorderState::Failed,
            config: None,
            queue: VecDeque::new(),
            queue_bytes: 0,
            manifest: None,
            segment: None,
            segment_index: None,
            segment_frames: 0,
            segment_bytes: 0,
            failure: Some(failure),
        }
    }

    fn snapshot(&self) -> RecorderSnapshot {
        RecorderSnapshot {
            id: self.id,
            state: self.state,
            segment_index: self.segment_index,
            queued_events: self.queue.len(),
            queued_bytes: self.queue_bytes,
            written_frames: self.segment_frames,
            written_bytes: self.segment_bytes,
            failure: self.failure.clone(),
        }
    }
}

pub struct RecorderCoordinator {
    root: PathBuf,
    factory: Arc<dyn WriterFactory>,
    recorders: HashMap<RecorderId, Recorder>,
    receipts: HashMap<ActionReceiptId, (RecorderId, ActionKind)>,
}

impl RecorderCoordinator {
    /// Opens a coordinator and returns it without the optional reconciliation
    /// details. Use [`Self::open`] when recovery reporting is needed.
    ///
    /// # Errors
    ///
    /// Returns an error when the root directory cannot be accessed.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, RecorderError> {
        Self::open(root).map(|(coordinator, _)| coordinator)
    }

    /// Opens `root`, repairs torn final records, and reconciles recorder state.
    /// Corruption in one recorder is reported and does not prevent other
    /// recorders from being recovered.
    ///
    /// # Errors
    ///
    /// Returns an error when the root directory itself cannot be created or
    /// enumerated.
    pub fn open(root: impl Into<PathBuf>) -> Result<(Self, ReconciliationReport), RecorderError> {
        Self::open_with_writer_factory(root, Arc::new(StdWriterFactory))
    }

    /// Opens a coordinator using an injectable append-writer factory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root directory cannot be accessed.
    pub fn open_with_writer_factory(
        root: impl Into<PathBuf>,
        factory: Arc<dyn WriterFactory>,
    ) -> Result<(Self, ReconciliationReport), RecorderError> {
        let root = root.into();
        format::create_dir_all_durable(&root)?;
        let mut coordinator = Self {
            root: root.clone(),
            factory,
            recorders: HashMap::new(),
            receipts: HashMap::new(),
        };
        let mut report = ReconciliationReport::default();
        let entries = fs::read_dir(&root).map_err(|error| RecorderError::io(&root, error))?;
        let mut directories = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| RecorderError::io(&root, error))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| RecorderError::io(&path, error))?;
            if file_type.is_dir() {
                directories.push(path);
            }
        }
        directories.sort();

        for directory in directories {
            let Some(id) = recorder_id_from_directory(&directory) else {
                continue;
            };
            match coordinator.reconcile_recorder(id, &directory) {
                Ok(recovery) => report.recovered.push((id, recovery)),
                Err(error) => {
                    let message = error.to_string();
                    report.failures.push(ReconciliationFailure {
                        recorder_id: Some(id),
                        directory: directory.clone(),
                        error: message.clone(),
                    });
                    coordinator
                        .recorders
                        .insert(id, Recorder::failed(id, directory, message));
                }
            }
        }
        Ok((coordinator, report))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn snapshot(&self, id: RecorderId) -> Option<RecorderSnapshot> {
        self.recorders.get(&id).map(Recorder::snapshot)
    }

    #[must_use]
    pub fn snapshots(&self) -> Vec<RecorderSnapshot> {
        let mut snapshots = self
            .recorders
            .values()
            .map(Recorder::snapshot)
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
    }

    /// Starts a recorder after setting up its segment and durably storing the
    /// action receipt and config.
    ///
    /// # Errors
    ///
    /// Returns a state, receipt, configuration, or I/O error. A write error
    /// moves only this recorder to [`RecorderState::Failed`].
    pub fn start(
        &mut self,
        id: RecorderId,
        receipt: ActionReceiptId,
        config: RecorderConfig,
    ) -> Result<ActionOutcome, RecorderError> {
        config.validate()?;
        if let Some(outcome) = self.receipt_outcome(id, receipt, ActionKind::Start)? {
            return Ok(outcome);
        }
        if let Some(recorder) = self.recorders.get(&id)
            && recorder.state != RecorderState::Stopped
        {
            return Err(RecorderError::InvalidState {
                id,
                state: recorder.state,
                operation: "start",
            });
        }

        let directory = self.recorder_directory(id);
        format::create_dir_all_durable(&directory)?;
        let manifest_path = directory.join(format::MANIFEST_NAME);
        let mut manifest = self
            .factory
            .open_append(&manifest_path)
            .map_err(|error| RecorderError::io(&manifest_path, error))?;
        if let Err(error) = manifest.sync_all() {
            let error = RecorderError::io(&manifest_path, error);
            self.mark_or_insert_failed(id, directory, &error);
            return Err(error);
        }
        format::sync_directory(&directory)?;
        let index = self
            .recorders
            .get(&id)
            .and_then(|recorder| recorder.segment_index)
            .map_or(0, |index| index.saturating_add(1));
        let segment_path = segment_path(&directory, index);
        let mut segment = match self.factory.open_append(&segment_path) {
            Ok(segment) => segment,
            Err(error) => {
                let error = RecorderError::io(&segment_path, error);
                self.mark_or_insert_failed(id, directory, &error);
                return Err(error);
            }
        };
        if let Err(error) = segment.sync_all() {
            let error = RecorderError::io(&segment_path, error);
            self.mark_or_insert_failed(id, directory, &error);
            return Err(error);
        }
        if let Err(error) = format::sync_directory(&directory) {
            self.mark_or_insert_failed(id, directory, &error);
            return Err(error);
        }
        if let Err(error) = write_manifest_frame(
            manifest.as_mut(),
            &manifest_path,
            ManifestEntry::Start {
                receipt,
                config,
                index,
            },
        ) {
            self.mark_or_insert_failed(id, directory, &error);
            return Err(error);
        }

        self.recorders.insert(
            id,
            Recorder {
                id,
                directory,
                state: RecorderState::Recording,
                config: Some(config),
                queue: VecDeque::with_capacity(config.queue.max_events),
                queue_bytes: 0,
                manifest: Some(manifest),
                segment: Some(segment),
                segment_index: Some(index),
                segment_frames: 0,
                segment_bytes: 0,
                failure: None,
            },
        );
        self.receipts.insert(receipt, (id, ActionKind::Start));
        Ok(ActionOutcome::Applied)
    }

    /// Adds an event to the recorder's non-blocking bounded queue.
    ///
    /// # Errors
    ///
    /// Returns ownership of the event if the recorder is unavailable or either
    /// queue bound would be exceeded.
    pub fn enqueue(&mut self, id: RecorderId, event: RecordEvent) -> Result<(), EnqueueError> {
        let Some(recorder) = self.recorders.get_mut(&id) else {
            return Err(EnqueueError::UnknownRecorder(Box::new(event)));
        };
        if recorder.state != RecorderState::Recording {
            return Err(EnqueueError::NotRecording {
                state: recorder.state,
                event: Box::new(event),
            });
        }
        let Some(config) = recorder.config else {
            return Err(EnqueueError::NotRecording {
                state: recorder.state,
                event: Box::new(event),
            });
        };
        if format::validate_event(&event).is_err() {
            return Err(EnqueueError::EventTooLarge(Box::new(event)));
        }
        let event_bytes = event.queue_bytes();
        if event_bytes > config.queue.max_bytes {
            return Err(EnqueueError::EventTooLarge(Box::new(event)));
        }
        if recorder.queue.len() == config.queue.max_events
            || recorder.queue_bytes.saturating_add(event_bytes) > config.queue.max_bytes
        {
            return Err(EnqueueError::QueueFull(Box::new(event)));
        }
        recorder.queue.push_back(event);
        recorder.queue_bytes += event_bytes;
        Ok(())
    }

    /// Enqueues and durably writes one event.
    ///
    /// # Errors
    ///
    /// Returns queue or recorder errors through [`AppendError`].
    pub fn append(&mut self, id: RecorderId, event: RecordEvent) -> Result<(), AppendError> {
        self.enqueue(id, event)?;
        self.flush(id)?;
        Ok(())
    }

    /// Durably drains all currently queued events in FIFO order.
    ///
    /// # Errors
    ///
    /// A write or rotation error marks this recorder failed.
    pub fn flush(&mut self, id: RecorderId) -> Result<usize, RecorderError> {
        let state = self
            .recorders
            .get(&id)
            .ok_or(RecorderError::UnknownRecorder(id))?
            .state;
        if state != RecorderState::Recording {
            return Err(RecorderError::InvalidState {
                id,
                state,
                operation: "flush",
            });
        }
        let mut written = 0;
        loop {
            let (batch, batch_frames, batch_bytes, queue_bytes, path) = {
                let recorder = self
                    .recorders
                    .get(&id)
                    .ok_or(RecorderError::UnknownRecorder(id))?;
                if recorder.queue.is_empty() {
                    break;
                }
                let config = recorder.config.ok_or(RecorderError::InvalidState {
                    id,
                    state: recorder.state,
                    operation: "flush",
                })?;
                let mut batch = Vec::new();
                let mut frames = recorder.segment_frames;
                let mut bytes = recorder.segment_bytes;
                let mut batch_frames = 0;
                let mut batch_bytes = 0;
                let mut queue_bytes = 0;
                for event in &recorder.queue {
                    let (kind, payload) = encoded_event(event)?;
                    let record_bytes = framed_len(payload.len())?;
                    let counts_as_frame = event.counts_as_frame();
                    if should_rotate(
                        config.segments,
                        frames,
                        bytes,
                        record_bytes,
                        counts_as_frame,
                    ) {
                        break;
                    }
                    frames = frames.saturating_add(u64::from(counts_as_frame));
                    bytes = bytes.saturating_add(record_bytes);
                    batch_frames += u64::from(counts_as_frame);
                    batch_bytes += record_bytes;
                    queue_bytes += event.queue_bytes();
                    batch.push((kind, payload));
                }
                let index = recorder.segment_index.ok_or(RecorderError::InvalidState {
                    id,
                    state: recorder.state,
                    operation: "flush",
                })?;
                (
                    batch,
                    batch_frames,
                    batch_bytes,
                    queue_bytes,
                    segment_path(&recorder.directory, index),
                )
            };
            if batch.is_empty() {
                if let Err(error) = self.rotate_inner(id) {
                    self.mark_failed(id, &error);
                    return Err(error);
                }
                continue;
            }
            let commit_result = {
                let recorder = self
                    .recorders
                    .get_mut(&id)
                    .ok_or(RecorderError::UnknownRecorder(id))?;
                let segment = recorder
                    .segment
                    .as_mut()
                    .ok_or(RecorderError::InvalidState {
                        id,
                        state: recorder.state,
                        operation: "flush",
                    })?;
                batch
                    .iter()
                    .try_for_each(|(kind, payload)| {
                        write_frame(
                            segment.as_mut(),
                            *kind,
                            payload,
                            format::MAX_ENCODED_FRAME_PAYLOAD,
                        )
                        .map(drop)
                    })
                    .and_then(|()| segment.sync_all())
            };
            if let Err(error) = commit_result {
                let error = RecorderError::io(path, error);
                self.mark_failed(id, &error);
                return Err(error);
            }
            let recorder = self
                .recorders
                .get_mut(&id)
                .ok_or(RecorderError::UnknownRecorder(id))?;
            for _ in 0..batch.len() {
                recorder.queue.pop_front();
            }
            recorder.queue_bytes = recorder.queue_bytes.saturating_sub(queue_bytes);
            recorder.segment_bytes = recorder.segment_bytes.saturating_add(batch_bytes);
            recorder.segment_frames = recorder.segment_frames.saturating_add(batch_frames);
            written += batch.len();
        }
        Ok(written)
    }

    /// Closes the current segment and opens the next one, including when the
    /// current segment is empty.
    ///
    /// # Errors
    ///
    /// Returns a state or I/O error and marks only this recorder failed on I/O
    /// failure.
    pub fn rotate(&mut self, id: RecorderId) -> Result<(), RecorderError> {
        self.flush(id)?;
        if let Err(error) = self.rotate_inner(id) {
            self.mark_failed(id, &error);
            return Err(error);
        }
        Ok(())
    }

    /// Flushes, closes, and durably stores a stop action receipt. Repeating the
    /// same receipt is a no-op, including after process restart.
    ///
    /// # Errors
    ///
    /// Returns a receipt, state, flush, or I/O error.
    pub fn stop(
        &mut self,
        id: RecorderId,
        receipt: ActionReceiptId,
    ) -> Result<ActionOutcome, RecorderError> {
        if let Some(outcome) = self.receipt_outcome(id, receipt, ActionKind::Stop)? {
            return Ok(outcome);
        }
        let state = self
            .recorders
            .get(&id)
            .ok_or(RecorderError::UnknownRecorder(id))?
            .state;
        if state == RecorderState::Failed {
            return Err(RecorderError::InvalidState {
                id,
                state,
                operation: "stop",
            });
        }
        if state == RecorderState::Recording {
            self.flush(id)?;
            if let Err(error) = self.close_segment(id) {
                self.mark_failed(id, &error);
                return Err(error);
            }
        }

        let (manifest_path, mut manifest) = {
            let recorder = self
                .recorders
                .get_mut(&id)
                .ok_or(RecorderError::UnknownRecorder(id))?;
            let path = recorder.directory.join(format::MANIFEST_NAME);
            let writer = match recorder.manifest.take() {
                Some(writer) => writer,
                None => self
                    .factory
                    .open_append(&path)
                    .map_err(|error| RecorderError::io(&path, error))?,
            };
            (path, writer)
        };
        if let Err(error) = write_manifest_frame(
            manifest.as_mut(),
            &manifest_path,
            ManifestEntry::Stop { receipt },
        ) {
            self.mark_failed(id, &error);
            return Err(error);
        }
        self.receipts.insert(receipt, (id, ActionKind::Stop));
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or(RecorderError::UnknownRecorder(id))?;
        recorder.state = RecorderState::Stopped;
        recorder.manifest = None;
        recorder.segment = None;
        recorder.queue.clear();
        recorder.queue_bytes = 0;
        Ok(ActionOutcome::Applied)
    }

    fn reconcile_recorder(
        &mut self,
        id: RecorderId,
        directory: &Path,
    ) -> Result<RecoveryReport, RecorderError> {
        let report = repair_recording(directory)?;
        let manifest_path = directory.join(format::MANIFEST_NAME);
        let manifest_scan = scan_manifest(&manifest_path)?;
        let mut config = None;
        let mut state = RecorderState::Stopped;
        let mut latest_open = None;
        let mut closed = HashMap::new();
        let mut staged_receipts = HashMap::new();
        for frame in &manifest_scan.frames {
            match decode_manifest(frame, &manifest_path)? {
                ManifestEntry::Start {
                    receipt,
                    config: started_config,
                    index,
                } => {
                    self.stage_receipt(&mut staged_receipts, receipt, id, ActionKind::Start)?;
                    config = Some(started_config);
                    state = RecorderState::Recording;
                    latest_open = Some(index);
                }
                ManifestEntry::Stop { receipt } => {
                    self.stage_receipt(&mut staged_receipts, receipt, id, ActionKind::Stop)?;
                    state = RecorderState::Stopped;
                }
                ManifestEntry::SegmentOpen { index } => latest_open = Some(index),
                ManifestEntry::SegmentClose {
                    index,
                    frames,
                    bytes,
                } => {
                    closed.insert(index, (frames, bytes));
                }
            }
        }

        let mut segment_index = report.segments.iter().map(|segment| segment.index).max();
        let mut segment_frames = 0;
        let mut segment_bytes = 0;
        let mut manifest = None;
        let mut segment = None;
        if state == RecorderState::Recording {
            let recorder_config = config.ok_or(RecorderError::Corrupt {
                path: manifest_path.clone(),
                offset: 0,
                reason: "recording state has no start config",
            })?;
            let active_index = latest_open
                .filter(|index| !closed.contains_key(index))
                .unwrap_or_else(|| segment_index.map_or(0, |index| index.saturating_add(1)));
            let active_path = segment_path(directory, active_index);
            let mut segment_writer = self
                .factory
                .open_append(&active_path)
                .map_err(|error| RecorderError::io(&active_path, error))?;
            segment_writer
                .sync_all()
                .map_err(|error| RecorderError::io(&active_path, error))?;
            format::sync_directory(directory)?;
            let active_scan = scan_segment(&active_path)?;
            segment_frames = active_scan.media_frames;
            segment_bytes = active_scan.valid_bytes;
            let mut manifest_writer = self
                .factory
                .open_append(&manifest_path)
                .map_err(|error| RecorderError::io(&manifest_path, error))?;
            if latest_open != Some(active_index) || closed.contains_key(&active_index) {
                write_manifest_frame(
                    manifest_writer.as_mut(),
                    &manifest_path,
                    ManifestEntry::SegmentOpen {
                        index: active_index,
                    },
                )?;
            }
            segment = Some(segment_writer);
            manifest = Some(manifest_writer);
            segment_index = Some(active_index);
            config = Some(recorder_config);
        }

        let recorder = Recorder {
            id,
            directory: directory.to_path_buf(),
            state,
            config,
            queue: VecDeque::new(),
            queue_bytes: 0,
            manifest,
            segment,
            segment_index,
            segment_frames,
            segment_bytes,
            failure: None,
        };
        self.receipts.extend(staged_receipts);
        self.recorders.insert(id, recorder);
        Ok(report)
    }

    fn receipt_outcome(
        &self,
        id: RecorderId,
        receipt: ActionReceiptId,
        kind: ActionKind,
    ) -> Result<Option<ActionOutcome>, RecorderError> {
        match self.receipts.get(&receipt) {
            Some(&(stored_id, stored_kind)) if stored_id == id && stored_kind == kind => {
                Ok(Some(ActionOutcome::AlreadyApplied))
            }
            Some(_) => Err(RecorderError::ReceiptConflict(receipt)),
            None => Ok(None),
        }
    }

    fn stage_receipt(
        &self,
        staged: &mut HashMap<ActionReceiptId, (RecorderId, ActionKind)>,
        receipt: ActionReceiptId,
        id: RecorderId,
        kind: ActionKind,
    ) -> Result<(), RecorderError> {
        let action = (id, kind);
        if staged
            .get(&receipt)
            .or_else(|| self.receipts.get(&receipt))
            .is_some_and(|stored| *stored != action)
        {
            return Err(RecorderError::ReceiptConflict(receipt));
        }
        staged.insert(receipt, action);
        Ok(())
    }

    fn rotate_inner(&mut self, id: RecorderId) -> Result<(), RecorderError> {
        self.close_segment(id)?;
        let (directory, next_index, manifest_path) = {
            let recorder = self
                .recorders
                .get(&id)
                .ok_or(RecorderError::UnknownRecorder(id))?;
            if recorder.state != RecorderState::Recording {
                return Err(RecorderError::InvalidState {
                    id,
                    state: recorder.state,
                    operation: "rotate",
                });
            }
            let next = recorder
                .segment_index
                .and_then(|index| index.checked_add(1))
                .ok_or(RecorderError::FormatLimit("segment index exhausted"))?;
            (
                recorder.directory.clone(),
                next,
                recorder.directory.join(format::MANIFEST_NAME),
            )
        };
        let path = segment_path(&directory, next_index);
        let mut segment = self
            .factory
            .open_append(&path)
            .map_err(|error| RecorderError::io(&path, error))?;
        segment
            .sync_all()
            .map_err(|error| RecorderError::io(&path, error))?;
        format::sync_directory(&directory)?;
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or(RecorderError::UnknownRecorder(id))?;
        let manifest = recorder
            .manifest
            .as_mut()
            .ok_or(RecorderError::InvalidState {
                id,
                state: recorder.state,
                operation: "rotate",
            })?;
        write_manifest_frame(
            manifest.as_mut(),
            &manifest_path,
            ManifestEntry::SegmentOpen { index: next_index },
        )?;
        recorder.segment = Some(segment);
        recorder.segment_index = Some(next_index);
        recorder.segment_frames = 0;
        recorder.segment_bytes = 0;
        Ok(())
    }

    fn close_segment(&mut self, id: RecorderId) -> Result<(), RecorderError> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or(RecorderError::UnknownRecorder(id))?;
        let index = recorder.segment_index.ok_or(RecorderError::InvalidState {
            id,
            state: recorder.state,
            operation: "close segment",
        })?;
        if let Some(segment) = recorder.segment.as_mut() {
            segment.sync_all().map_err(|error| {
                RecorderError::io(segment_path(&recorder.directory, index), error)
            })?;
        }
        let manifest_path = recorder.directory.join(format::MANIFEST_NAME);
        let manifest = recorder
            .manifest
            .as_mut()
            .ok_or(RecorderError::InvalidState {
                id,
                state: recorder.state,
                operation: "close segment",
            })?;
        write_manifest_frame(
            manifest.as_mut(),
            &manifest_path,
            ManifestEntry::SegmentClose {
                index,
                frames: recorder.segment_frames,
                bytes: recorder.segment_bytes,
            },
        )?;
        recorder.segment = None;
        Ok(())
    }

    fn recorder_directory(&self, id: RecorderId) -> PathBuf {
        self.root.join(format!("recorder-{:032x}", id.get().get()))
    }

    fn mark_failed(&mut self, id: RecorderId, error: &RecorderError) {
        if let Some(recorder) = self.recorders.get_mut(&id) {
            recorder.state = RecorderState::Failed;
            recorder.failure = Some(error.to_string());
            recorder.manifest = None;
            recorder.segment = None;
        }
    }

    fn mark_or_insert_failed(&mut self, id: RecorderId, directory: PathBuf, error: &RecorderError) {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.recorders.entry(id) {
            entry.insert(Recorder::failed(id, directory, error.to_string()));
        } else {
            self.mark_failed(id, error);
        }
    }
}

fn should_rotate(
    policy: crate::SegmentPolicy,
    frames: u64,
    bytes: u64,
    next_bytes: u64,
    counts_as_frame: bool,
) -> bool {
    let frame_limit = counts_as_frame && policy.max_frames.is_some_and(|limit| frames >= limit);
    let byte_limit = bytes > 0
        && policy
            .max_bytes
            .is_some_and(|limit| bytes.saturating_add(next_bytes) > limit);
    frame_limit || byte_limit
}

fn write_manifest_frame(
    writer: &mut dyn DurableWriter,
    path: &Path,
    entry: ManifestEntry,
) -> Result<(), RecorderError> {
    let (kind, payload) = encode_manifest(entry);
    write_frame(writer, kind, &payload, format::MAX_MANIFEST_PAYLOAD)
        .and_then(|_| writer.sync_all())
        .map_err(|error| RecorderError::io(path, error))
}

/// Repairs a recording directory by truncating only incomplete final records.
/// Any invalid header or checksum is returned as corruption without truncation.
///
/// # Errors
///
/// Returns an I/O or corruption error. A missing manifest is an error.
pub fn repair_recording(directory: impl AsRef<Path>) -> Result<RecoveryReport, RecorderError> {
    let directory = directory.as_ref();
    let manifest_path = directory.join(format::MANIFEST_NAME);
    let manifest = scan_manifest(&manifest_path)?;
    let mut committed_segments = HashSet::new();
    for frame in &manifest.frames {
        match decode_manifest(frame, &manifest_path)? {
            ManifestEntry::Start { index, .. } | ManifestEntry::SegmentOpen { index } => {
                committed_segments.insert(index);
            }
            ManifestEntry::Stop { .. } | ManifestEntry::SegmentClose { .. } => {}
        }
    }
    let entries = fs::read_dir(directory).map_err(|error| RecorderError::io(directory, error))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| RecorderError::io(directory, error))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| RecorderError::io(&path, error))?;
        let name = entry.file_name();
        let Some(index) = name.to_str().and_then(format::parse_segment_name) else {
            continue;
        };
        if !file_type.is_file() {
            return Err(RecorderError::Corrupt {
                path,
                offset: 0,
                reason: "segment path is not a regular file",
            });
        }
        paths.push((index, path));
    }
    paths.sort_by_key(|(index, _)| *index);
    let mut segments = Vec::with_capacity(paths.len());
    let mut truncations = Vec::new();
    let mut abandoned = Vec::new();
    for (index, path) in paths {
        let scan = scan_segment(&path)?;
        if !committed_segments.contains(&index) {
            if scan.valid_bytes == 0 && scan.truncated_bytes == 0 {
                abandoned.push(path);
                continue;
            }
            return Err(RecorderError::Corrupt {
                path,
                offset: 0,
                reason: "unreferenced segment contains data",
            });
        }
        if scan.truncated_bytes > 0 {
            truncations.push((path.clone(), scan.valid_bytes));
        }
        segments.push(RecoveredSegment {
            index,
            records: scan.records,
            bytes: scan.valid_bytes,
            truncated_bytes: scan.truncated_bytes,
        });
    }
    if manifest.truncated_bytes > 0 {
        truncations.push((manifest_path, manifest.valid_bytes));
    }
    for (path, length) in truncations {
        format::truncate_file(&path, length)?;
    }
    for path in &abandoned {
        fs::remove_file(path).map_err(|error| RecorderError::io(path, error))?;
    }
    if !abandoned.is_empty() {
        format::sync_directory(directory)?;
    }
    Ok(RecoveryReport {
        manifest_records: manifest.frames.len().try_into().unwrap_or(u64::MAX),
        manifest_truncated_bytes: manifest.truncated_bytes,
        segments,
    })
}

fn recorder_id_from_directory(directory: &Path) -> Option<RecorderId> {
    let name = directory.file_name()?.to_str()?;
    let value = u128::from_str_radix(name.strip_prefix("recorder-")?, 16).ok()?;
    NonZeroU128::new(value).map(RecorderId::new)
}
