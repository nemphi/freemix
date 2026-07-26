use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use crate::{ProjectStore, StoreError, StoredProject, store::sync_directory};

const JOURNAL_DIRECTORY: &str = "journal";
const CHECKPOINT_NAME: &str = "checkpoint";
const RECORD_MAGIC: &[u8; 8] = b"FMJRNL01";
const CHECKPOINT_MAGIC: &[u8; 8] = b"FMJCHK01";
const RECORD_HEADER_BYTES: usize = 8 + 8 + 8 + 8 + 4;
const CHECKSUM_BYTES: usize = 4;
const CHECKPOINT_BYTES: usize = 8 + 8 + 8 + CHECKSUM_BYTES;

/// Maximum encoded mutation batch record size (1 MiB).
pub const MAX_JOURNAL_RECORD_BYTES: u64 = 1024 * 1024;

/// An immutable, passive mutation batch. Persistence never executes payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationBatch {
    sequence: u64,
    base_revision: u64,
    revision: u64,
    payload: Vec<u8>,
}

impl MutationBatch {
    #[must_use]
    pub fn new(sequence: u64, base_revision: u64, revision: u64, payload: Vec<u8>) -> Self {
        Self {
            sequence,
            base_revision,
            revision,
            payload,
        }
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn base_revision(&self) -> u64 {
        self.base_revision
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Valid journal state discovered by a scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalScan {
    checkpoint_sequence: u64,
    checkpoint_revision: u64,
    batches: Vec<MutationBatch>,
    ignored_torn_paths: Vec<PathBuf>,
}

impl JournalScan {
    #[must_use]
    pub const fn checkpoint_sequence(&self) -> u64 {
        self.checkpoint_sequence
    }

    #[must_use]
    pub const fn checkpoint_revision(&self) -> u64 {
        self.checkpoint_revision
    }

    #[must_use]
    pub fn batches(&self) -> &[MutationBatch] {
        &self.batches
    }

    #[must_use]
    pub fn ignored_torn_paths(&self) -> &[PathBuf] {
        &self.ignored_torn_paths
    }
}

/// Summary of a durable manifest checkpoint and journal compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionReport {
    applied_through_sequence: u64,
    removed_records: usize,
}

impl CompactionReport {
    #[must_use]
    pub const fn applied_through_sequence(&self) -> u64 {
        self.applied_through_sequence
    }

    #[must_use]
    pub const fn removed_records(&self) -> usize {
        self.removed_records
    }
}

impl ProjectStore {
    #[must_use]
    pub fn journal_path(&self) -> PathBuf {
        self.root().join(JOURNAL_DIRECTORY)
    }

    /// Atomically appends one immutable checksummed mutation batch.
    ///
    /// Sequences must increase by exactly one, `base_revision` must equal the
    /// preceding durable revision, and `revision` must increase. FNV-1a 32-bit
    /// is used only to detect accidental corruption, not for security.
    ///
    /// # Errors
    ///
    /// Returns journal consistency, size-limit, manifest, or filesystem errors.
    pub fn append_batch(&self, batch: &MutationBatch) -> Result<(), StoreError> {
        self.ensure_journal()?;
        let scan = self.scan_journal()?;
        if !scan.ignored_torn_paths.is_empty() {
            return Err(StoreError::Journal(JournalError::TornRecordPending));
        }
        let expected_sequence = scan
            .batches
            .last()
            .map_or(scan.checkpoint_sequence, MutationBatch::sequence)
            .checked_add(1)
            .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?;
        let expected_revision = scan
            .batches
            .last()
            .map_or(scan.checkpoint_revision, MutationBatch::revision);
        validate_batch(batch, expected_sequence, expected_revision)?;
        let bytes = encode_record(batch)?;

        let journal = self.journal_path();
        let final_path = journal.join(record_name(batch.sequence));
        let temp_path = journal.join(temp_name(batch.sequence));
        let mut guard = TempRecordGuard(Some(temp_path.clone()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(StoreError::Io)?;
        file.write_all(&bytes).map_err(StoreError::Io)?;
        file.sync_all().map_err(StoreError::Io)?;
        drop(file);
        rename_no_replace(&temp_path, &final_path)?;
        guard.0 = None;
        sync_directory(&journal)?;
        Ok(())
    }

    /// Scans valid batches after the durable checkpoint without mutating disk.
    ///
    /// A single torn final record or append temp is reported and ignored. Any
    /// non-final tear, sequence/revision gap, malformed record, or checksum
    /// mismatch is rejected.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid checkpoint, corrupt/non-final record,
    /// sequence or revision gap, unexpected entry, or filesystem failure.
    pub fn scan_journal(&self) -> Result<JournalScan, StoreError> {
        let journal = self.journal_path();
        if !journal.try_exists().map_err(StoreError::Io)? {
            let project = self.load()?;
            return Ok(JournalScan {
                checkpoint_sequence: 0,
                checkpoint_revision: project.position().revision,
                batches: Vec::new(),
                ignored_torn_paths: Vec::new(),
            });
        }
        let JournalEntries {
            records,
            mut temps,
            checkpoint_temps,
        } = read_journal_entries(&journal)?;
        let checkpoint_path = journal.join(CHECKPOINT_NAME);
        let checkpoint = if checkpoint_path.try_exists().map_err(StoreError::Io)? {
            read_checkpoint(&checkpoint_path)?
        } else if records.is_empty() && temps.is_empty() {
            (0, self.load()?.position().revision)
        } else {
            return Err(StoreError::Journal(JournalError::MalformedCheckpoint));
        };

        let mut expected_sequence = checkpoint
            .0
            .checked_add(1)
            .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?;
        let mut expected_revision = checkpoint.1;
        let mut batches = Vec::new();
        let mut ignored = checkpoint_temps;
        for (index, (file_sequence, path)) in records.iter().enumerate() {
            if *file_sequence <= checkpoint.0 {
                let batch = read_record(path, false)?.ok_or_else(|| {
                    StoreError::Journal(JournalError::MalformedRecord(path.clone()))
                })?;
                if batch.sequence != *file_sequence {
                    return Err(StoreError::Journal(JournalError::FilenameMismatch {
                        path: path.clone(),
                        encoded: batch.sequence,
                    }));
                }
                continue;
            }
            if *file_sequence != expected_sequence {
                return Err(StoreError::Journal(JournalError::SequenceGap {
                    expected: expected_sequence,
                    found: *file_sequence,
                }));
            }
            let is_final = index + 1 == records.len() && temps.is_empty();
            let Some(batch) = read_record(path, is_final)? else {
                if !ignored.is_empty() {
                    return Err(StoreError::Journal(JournalError::NonFinalTornRecord(
                        path.clone(),
                    )));
                }
                ignored.push(path.clone());
                break;
            };
            if batch.sequence != *file_sequence {
                return Err(StoreError::Journal(JournalError::FilenameMismatch {
                    path: path.clone(),
                    encoded: batch.sequence,
                }));
            }
            validate_batch(&batch, expected_sequence, expected_revision)?;
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?;
            expected_revision = batch.revision;
            batches.push(batch);
        }

        if !temps.is_empty() {
            let is_pending_append = temps[0].0 == expected_sequence;
            let is_linked_append = temps[0].0.checked_add(1) == Some(expected_sequence)
                && records.iter().any(|(sequence, _)| *sequence == temps[0].0);
            if !ignored.is_empty() || temps.len() != 1 || !(is_pending_append || is_linked_append) {
                return Err(StoreError::Journal(JournalError::NonFinalTornRecord(
                    temps[0].1.clone(),
                )));
            }
            ignored.push(temps.remove(0).1);
        }
        Ok(JournalScan {
            checkpoint_sequence: checkpoint.0,
            checkpoint_revision: checkpoint.1,
            batches,
            ignored_torn_paths: ignored,
        })
    }

    /// Removes only the torn final paths that [`ProjectStore::scan_journal`]
    /// proved safe to ignore, returning the pre-cleanup scan.
    ///
    /// # Errors
    ///
    /// Returns any scan or filesystem error without removing valid records.
    pub fn recover_journal(&self) -> Result<JournalScan, StoreError> {
        let scan = self.scan_journal()?;
        for path in &scan.ignored_torn_paths {
            fs::remove_file(path).map_err(StoreError::Io)?;
        }
        let checkpoint = self.journal_path().join(CHECKPOINT_NAME);
        if !checkpoint.try_exists().map_err(StoreError::Io)? {
            write_checkpoint(
                &self.journal_path(),
                scan.checkpoint_sequence,
                scan.checkpoint_revision,
            )?;
        }
        let removed_applied =
            remove_applied_records(&self.journal_path(), scan.checkpoint_sequence)?;
        if !scan.ignored_torn_paths.is_empty() || removed_applied != 0 {
            sync_directory(&self.journal_path())?;
        }
        Ok(scan)
    }

    /// Durably saves a manifest, then removes journal records it includes.
    ///
    /// The manifest rename and directory sync complete before the checkpoint
    /// advances, and the checkpoint is durable before records are removed.
    ///
    /// # Errors
    ///
    /// Returns an error unless `applied_through_sequence` exists (or is the
    /// current checkpoint) and its revision exactly matches the manifest.
    pub fn checkpoint_and_compact(
        &self,
        project: &StoredProject,
        applied_through_sequence: u64,
    ) -> Result<CompactionReport, StoreError> {
        self.ensure_journal()?;
        let scan = self.scan_journal()?;
        if !scan.ignored_torn_paths.is_empty() {
            return Err(StoreError::Journal(JournalError::TornRecordPending));
        }
        let applied_revision = if applied_through_sequence == scan.checkpoint_sequence {
            scan.checkpoint_revision
        } else {
            scan.batches
                .iter()
                .find(|batch| batch.sequence == applied_through_sequence)
                .map(MutationBatch::revision)
                .ok_or(StoreError::Journal(JournalError::UnknownCheckpoint(
                    applied_through_sequence,
                )))?
        };
        if project.position().revision != applied_revision {
            return Err(StoreError::Journal(JournalError::CheckpointRevision {
                sequence: applied_through_sequence,
                expected: applied_revision,
                found: project.position().revision,
            }));
        }

        self.save(project)?;
        write_checkpoint(
            &self.journal_path(),
            applied_through_sequence,
            applied_revision,
        )?;
        let removed = remove_applied_records(&self.journal_path(), applied_through_sequence)?;
        if removed != 0 {
            sync_directory(&self.journal_path())?;
        }
        Ok(CompactionReport {
            applied_through_sequence,
            removed_records: removed,
        })
    }

    fn ensure_journal(&self) -> Result<(), StoreError> {
        let journal = self.journal_path();
        if journal.try_exists().map_err(StoreError::Io)? {
            let checkpoint = journal.join(CHECKPOINT_NAME);
            if !checkpoint.try_exists().map_err(StoreError::Io)? {
                let scan = self.scan_journal()?;
                if !scan.ignored_torn_paths.is_empty() {
                    return Err(StoreError::Journal(JournalError::TornRecordPending));
                }
                write_checkpoint(&journal, scan.checkpoint_sequence, scan.checkpoint_revision)?;
            }
            return Ok(());
        }
        let revision = self.load()?.position().revision;
        fs::create_dir(&journal).map_err(StoreError::Io)?;
        sync_directory(self.root())?;
        write_checkpoint(&journal, 0, revision)
    }
}

fn validate_batch(
    batch: &MutationBatch,
    expected_sequence: u64,
    expected_revision: u64,
) -> Result<(), StoreError> {
    if batch.sequence != expected_sequence {
        return Err(StoreError::Journal(JournalError::SequenceGap {
            expected: expected_sequence,
            found: batch.sequence,
        }));
    }
    let next_revision = expected_revision.checked_add(1);
    if batch.base_revision != expected_revision || Some(batch.revision) != next_revision {
        return Err(StoreError::Journal(JournalError::RevisionGap {
            sequence: batch.sequence,
            expected_base: expected_revision,
            found_base: batch.base_revision,
            found_revision: batch.revision,
        }));
    }
    Ok(())
}

struct JournalEntries {
    records: Vec<(u64, PathBuf)>,
    temps: Vec<(u64, PathBuf)>,
    checkpoint_temps: Vec<PathBuf>,
}

fn read_journal_entries(journal: &Path) -> Result<JournalEntries, StoreError> {
    let mut records = Vec::new();
    let mut temps = Vec::new();
    let mut checkpoint_temps = Vec::new();
    for entry in fs::read_dir(journal).map_err(StoreError::Io)? {
        let entry = entry.map_err(StoreError::Io)?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| StoreError::Journal(JournalError::UnexpectedEntry(entry.path())))?;
        if name == CHECKPOINT_NAME {
            continue;
        }
        if name.starts_with(&format!(".{CHECKPOINT_NAME}.tmp-")) {
            checkpoint_temps.push(entry.path());
            continue;
        }
        if let Some(sequence) = parse_record_name(name) {
            records.push((sequence, entry.path()));
        } else if let Some(sequence) = parse_temp_name(name) {
            temps.push((sequence, entry.path()));
        } else {
            return Err(StoreError::Journal(JournalError::UnexpectedEntry(
                entry.path(),
            )));
        }
    }
    records.sort_by_key(|(sequence, _)| *sequence);
    temps.sort_by_key(|(sequence, _)| *sequence);
    if checkpoint_temps.len() > 1 {
        return Err(StoreError::Journal(JournalError::NonFinalTornRecord(
            checkpoint_temps.remove(0),
        )));
    }
    Ok(JournalEntries {
        records,
        temps,
        checkpoint_temps,
    })
}

fn encode_record(batch: &MutationBatch) -> Result<Vec<u8>, StoreError> {
    let payload_length = u32::try_from(batch.payload.len()).map_err(|_| {
        StoreError::Journal(JournalError::RecordTooLarge {
            size: u64::MAX,
            maximum: MAX_JOURNAL_RECORD_BYTES,
        })
    })?;
    let size = RECORD_HEADER_BYTES + batch.payload.len() + CHECKSUM_BYTES;
    if size as u64 > MAX_JOURNAL_RECORD_BYTES {
        return Err(StoreError::Journal(JournalError::RecordTooLarge {
            size: size as u64,
            maximum: MAX_JOURNAL_RECORD_BYTES,
        }));
    }
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(RECORD_MAGIC);
    bytes.extend_from_slice(&batch.sequence.to_le_bytes());
    bytes.extend_from_slice(&batch.base_revision.to_le_bytes());
    bytes.extend_from_slice(&batch.revision.to_le_bytes());
    bytes.extend_from_slice(&payload_length.to_le_bytes());
    bytes.extend_from_slice(&batch.payload);
    bytes.extend_from_slice(&fnv1a32(&bytes).to_le_bytes());
    Ok(bytes)
}

fn read_record(path: &Path, allow_torn: bool) -> Result<Option<MutationBatch>, StoreError> {
    let size = fs::metadata(path).map_err(StoreError::Io)?.len();
    if size > MAX_JOURNAL_RECORD_BYTES {
        return Err(StoreError::Journal(JournalError::RecordTooLarge {
            size,
            maximum: MAX_JOURNAL_RECORD_BYTES,
        }));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
    File::open(path)
        .and_then(|file| {
            file.take(MAX_JOURNAL_RECORD_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(StoreError::Io)?;
    if bytes.len() as u64 > MAX_JOURNAL_RECORD_BYTES {
        return Err(StoreError::Journal(JournalError::RecordTooLarge {
            size: bytes.len() as u64,
            maximum: MAX_JOURNAL_RECORD_BYTES,
        }));
    }
    if bytes.len() < RECORD_HEADER_BYTES + CHECKSUM_BYTES {
        return torn_or_error(path, allow_torn);
    }
    if &bytes[..8] != RECORD_MAGIC {
        return Err(StoreError::Journal(JournalError::MalformedRecord(
            path.to_path_buf(),
        )));
    }
    let payload_length =
        u32::from_le_bytes(bytes[32..36].try_into().expect("fixed slice")) as usize;
    let expected_length = RECORD_HEADER_BYTES
        .checked_add(payload_length)
        .and_then(|value| value.checked_add(CHECKSUM_BYTES))
        .ok_or(StoreError::Journal(JournalError::RecordTooLarge {
            size: u64::MAX,
            maximum: MAX_JOURNAL_RECORD_BYTES,
        }))?;
    if bytes.len() < expected_length {
        if bytes.len() >= RECORD_HEADER_BYTES + CHECKSUM_BYTES {
            let possible_checksum = u32::from_le_bytes(
                bytes[bytes.len() - CHECKSUM_BYTES..]
                    .try_into()
                    .expect("fixed slice"),
            );
            if possible_checksum == fnv1a32(&bytes[..bytes.len() - CHECKSUM_BYTES]) {
                return Err(StoreError::Journal(JournalError::MalformedRecord(
                    path.to_path_buf(),
                )));
            }
        }
        return torn_or_error(path, allow_torn);
    }
    if bytes.len() != expected_length {
        return Err(StoreError::Journal(JournalError::MalformedRecord(
            path.to_path_buf(),
        )));
    }
    let checksum = u32::from_le_bytes(
        bytes[expected_length - CHECKSUM_BYTES..]
            .try_into()
            .expect("fixed slice"),
    );
    let actual = fnv1a32(&bytes[..expected_length - CHECKSUM_BYTES]);
    if checksum != actual {
        return Err(StoreError::Journal(JournalError::ChecksumMismatch {
            path: path.to_path_buf(),
            expected: checksum,
            actual,
        }));
    }
    Ok(Some(MutationBatch {
        sequence: read_u64(&bytes, 8),
        base_revision: read_u64(&bytes, 16),
        revision: read_u64(&bytes, 24),
        payload: bytes[RECORD_HEADER_BYTES..expected_length - CHECKSUM_BYTES].to_vec(),
    }))
}

fn torn_or_error(path: &Path, allow_torn: bool) -> Result<Option<MutationBatch>, StoreError> {
    if allow_torn {
        Ok(None)
    } else {
        Err(StoreError::Journal(JournalError::NonFinalTornRecord(
            path.to_path_buf(),
        )))
    }
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed slice"))
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn record_name(sequence: u64) -> String {
    format!("{sequence:020}.batch")
}

fn parse_record_name(name: &str) -> Option<u64> {
    name.strip_suffix(".batch")
        .filter(|digits| digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))?
        .parse()
        .ok()
}

fn temp_name(sequence: u64) -> String {
    format!(
        ".{sequence:020}.batch.tmp-{}-{}",
        std::process::id(),
        crate::store::next_temp_sequence()
    )
}

fn parse_temp_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix('.')?;
    let (digits, _) = rest.split_once(".batch.tmp-")?;
    if digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

fn rename_no_replace(from: &Path, to: &Path) -> Result<(), StoreError> {
    fs::hard_link(from, to).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            StoreError::Journal(JournalError::RecordExists(to.to_path_buf()))
        } else {
            StoreError::Io(error)
        }
    })?;
    fs::remove_file(from).map_err(StoreError::Io)
}

fn remove_applied_records(journal: &Path, through_sequence: u64) -> Result<usize, StoreError> {
    let mut removed = 0;
    for entry in fs::read_dir(journal).map_err(StoreError::Io)? {
        let entry = entry.map_err(StoreError::Io)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if parse_record_name(&name).is_some_and(|sequence| sequence <= through_sequence) {
            fs::remove_file(entry.path()).map_err(StoreError::Io)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn write_checkpoint(journal: &Path, sequence: u64, revision: u64) -> Result<(), StoreError> {
    let mut bytes = Vec::with_capacity(CHECKPOINT_BYTES);
    bytes.extend_from_slice(CHECKPOINT_MAGIC);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&revision.to_le_bytes());
    bytes.extend_from_slice(&fnv1a32(&bytes).to_le_bytes());
    let temp = journal.join(format!(
        ".{CHECKPOINT_NAME}.tmp-{}-{}",
        std::process::id(),
        crate::store::next_temp_sequence()
    ));
    let mut guard = TempRecordGuard(Some(temp.clone()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(StoreError::Io)?;
    file.write_all(&bytes).map_err(StoreError::Io)?;
    file.sync_all().map_err(StoreError::Io)?;
    drop(file);
    fs::rename(&temp, journal.join(CHECKPOINT_NAME)).map_err(StoreError::Io)?;
    guard.0 = None;
    sync_directory(journal)
}

fn read_checkpoint(path: &Path) -> Result<(u64, u64), StoreError> {
    let bytes = fs::read(path).map_err(StoreError::Io)?;
    if bytes.len() != CHECKPOINT_BYTES || &bytes[..8] != CHECKPOINT_MAGIC {
        return Err(StoreError::Journal(JournalError::MalformedCheckpoint));
    }
    let checksum = u32::from_le_bytes(bytes[24..28].try_into().expect("fixed slice"));
    if checksum != fnv1a32(&bytes[..24]) {
        return Err(StoreError::Journal(JournalError::MalformedCheckpoint));
    }
    Ok((read_u64(&bytes, 8), read_u64(&bytes, 16)))
}

struct TempRecordGuard(Option<PathBuf>);

impl Drop for TempRecordGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
pub enum JournalError {
    RecordTooLarge {
        size: u64,
        maximum: u64,
    },
    SequenceGap {
        expected: u64,
        found: u64,
    },
    RevisionGap {
        sequence: u64,
        expected_base: u64,
        found_base: u64,
        found_revision: u64,
    },
    ChecksumMismatch {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },
    FilenameMismatch {
        path: PathBuf,
        encoded: u64,
    },
    MalformedRecord(PathBuf),
    NonFinalTornRecord(PathBuf),
    UnexpectedEntry(PathBuf),
    RecordExists(PathBuf),
    UnknownCheckpoint(u64),
    CheckpointRevision {
        sequence: u64,
        expected: u64,
        found: u64,
    },
    MalformedCheckpoint,
    TornRecordPending,
    SequenceOverflow,
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecordTooLarge { size, maximum } => write!(
                formatter,
                "journal record is {size} bytes, exceeding the {maximum}-byte maximum"
            ),
            Self::SequenceGap { expected, found } => write!(
                formatter,
                "journal sequence gap: expected {expected}, found {found}"
            ),
            Self::RevisionGap {
                sequence,
                expected_base,
                found_base,
                found_revision,
            } => write!(
                formatter,
                "journal batch {sequence} revision is not monotonic: expected base {expected_base}, found base {found_base} and revision {found_revision}"
            ),
            Self::ChecksumMismatch { path, .. } => write!(
                formatter,
                "journal checksum mismatch in `{}`",
                path.display()
            ),
            Self::FilenameMismatch { path, encoded } => write!(
                formatter,
                "journal file `{}` encodes sequence {encoded}",
                path.display()
            ),
            Self::MalformedRecord(path) => {
                write!(formatter, "malformed journal record `{}`", path.display())
            }
            Self::NonFinalTornRecord(path) => write!(
                formatter,
                "torn journal record is not final: `{}`",
                path.display()
            ),
            Self::UnexpectedEntry(path) => {
                write!(formatter, "unexpected journal entry `{}`", path.display())
            }
            Self::RecordExists(path) => write!(
                formatter,
                "journal record already exists: `{}`",
                path.display()
            ),
            Self::UnknownCheckpoint(sequence) => write!(
                formatter,
                "journal sequence {sequence} cannot be checkpointed"
            ),
            Self::CheckpointRevision {
                sequence,
                expected,
                found,
            } => write!(
                formatter,
                "checkpoint {sequence} requires manifest revision {expected}, found {found}"
            ),
            Self::MalformedCheckpoint => formatter.write_str("malformed journal checkpoint"),
            Self::TornRecordPending => {
                formatter.write_str("recover the torn final journal record before writing")
            }
            Self::SequenceOverflow => formatter.write_str("journal sequence overflow"),
        }
    }
}

impl Error for JournalError {}
