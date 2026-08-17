use std::{
    cell::RefCell,
    collections::VecDeque,
    error::Error,
    fmt, fs,
    io::{self, Read as _},
    path::PathBuf,
    time::Duration,
};

use turso::{Row, Value};

use crate::{
    ProjectStore, StoreError, StoredProject,
    journal_db::{Deadline, JournalDatabase, Transaction, column_blob, column_integer},
    store::sync_directory,
};

const JOURNAL_DIRECTORY: &str = "journal";
const DATABASE_NAME: &str = "journal.db";
const WRITE_AHEAD_LOG_NAME: &str = "journal.db-wal";
const SHARED_MEMORY_NAME: &str = "journal.db-shm";

/// Bytes of fixed record overhead accounted against
/// [`MAX_JOURNAL_RECORD_BYTES`]: sequence, base revision, revision, checksum.
const RECORD_OVERHEAD_BYTES: u64 = 8 + 8 + 8 + 4;

const CHECKPOINT_TABLE: &str = "journal_checkpoint";
const BATCH_TABLE: &str = "journal_batch";

const CREATE_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS journal_checkpoint (
    id INTEGER PRIMARY KEY,
    sequence INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    unapplied_bytes INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS journal_batch (
    sequence INTEGER PRIMARY KEY,
    base_revision INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    payload BLOB NOT NULL,
    checksum INTEGER NOT NULL
);
";

const SELECT_TABLE_SQL: &str =
    "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1";
const SELECT_CHECKPOINT_SQL: &str =
    "SELECT sequence, revision, unapplied_bytes FROM journal_checkpoint WHERE id = 1";
const SELECT_HEAD_SQL: &str =
    "SELECT sequence, revision FROM journal_batch ORDER BY sequence DESC LIMIT 1";
const SELECT_BATCHES_SQL: &str = "SELECT sequence, base_revision, revision, payload, checksum \
     FROM journal_batch WHERE sequence > ?1 ORDER BY sequence ASC";
const INSERT_BATCH_SQL: &str = "INSERT INTO journal_batch \
     (sequence, base_revision, revision, payload, checksum) VALUES (?1, ?2, ?3, ?4, ?5)";
const UPDATE_UNAPPLIED_BYTES_SQL: &str =
    "UPDATE journal_checkpoint SET unapplied_bytes = ?1 WHERE id = 1";
const DELETE_CHECKPOINT_SQL: &str = "DELETE FROM journal_checkpoint";
const INSERT_CHECKPOINT_SQL: &str = "INSERT INTO journal_checkpoint (id, sequence, revision, unapplied_bytes) \
     VALUES (1, ?1, ?2, ?3)";
const DELETE_APPLIED_SQL: &str = "DELETE FROM journal_batch WHERE sequence <= ?1";
const SELECT_UNVERIFIED_BATCHES_SQL: &str = "SELECT sequence, base_revision, revision, payload \
     FROM journal_batch WHERE sequence > ?1 ORDER BY sequence ASC";
const CHECKPOINT_LOG_SQL: &str = "PRAGMA wal_checkpoint(TRUNCATE)";

/// Bytes of write-ahead log header that precede the first frame.
const LOG_HEADER_BYTES: usize = 32;
/// Bytes of frame header that precede each logged page.
const LOG_FRAME_HEADER_BYTES: u64 = 24;
/// Write-ahead log magic, native and byte-swapped checksum variants.
const LOG_MAGIC: [u32; 2] = [0x377f_0682, 0x377f_0683];

/// Maximum encoded mutation batch record size (1 MiB).
pub const MAX_JOURNAL_RECORD_BYTES: u64 = 1024 * 1024;

/// Maximum number of batches that may sit between the durable checkpoint and
/// the journal head. Recovery must fit in memory, so a project that never
/// checkpoints is refused rather than allowed to grow without bound.
pub const MAX_UNAPPLIED_JOURNAL_BATCHES: u64 = 65_536;

/// Maximum total size of the batches that may sit between the durable
/// checkpoint and the journal head (128 MiB).
///
/// [`MAX_UNAPPLIED_JOURNAL_BATCHES`] alone bounds only how *many* records
/// recovery collects, and each may be [`MAX_JOURNAL_RECORD_BYTES`] long: the
/// count on its own permits 64 GiB in one `Vec`, which is an out-of-memory
/// abort before a show rather than a bounded refusal. Both bounds are enforced
/// when a batch is appended and again while recovery reads, so a journal that
/// can be written can always be read back.
pub const MAX_UNAPPLIED_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;

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

/// Something a scan observed about the journal that is not part of its normal
/// state and that an operator must be told about.
///
/// A scan that returns history the caller can trust still has to say what it
/// had to reconcile to get there; silence would turn a damaged or interrupted
/// journal into an ordinary short history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalObservation {
    /// The manifest is ahead of the durable checkpoint: a checkpoint was
    /// interrupted after the manifest was saved and before the journal
    /// recorded it. Batches through `sequence` are already applied and are not
    /// returned as unapplied history.
    IncompleteCheckpoint { sequence: u64, revision: u64 },
    /// The write-ahead log does not end on a frame boundary, so its last
    /// transaction was torn by a crash and the engine dropped it. History is
    /// therefore shorter than what was written.
    TornWriteAheadLog { bytes: u64, frame_bytes: u64 },
}

impl fmt::Display for JournalObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteCheckpoint { sequence, revision } => write!(
                formatter,
                "checkpoint through sequence {sequence} (revision {revision}) was interrupted after the manifest was saved"
            ),
            Self::TornWriteAheadLog { bytes, frame_bytes } => write!(
                formatter,
                "write-ahead log is {bytes} bytes, which is not a whole number of {frame_bytes}-byte frames after its {LOG_HEADER_BYTES}-byte header: its final transaction was torn and dropped"
            ),
        }
    }
}

/// Valid journal state discovered by a scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalScan {
    checkpoint_sequence: u64,
    checkpoint_revision: u64,
    batches: Vec<MutationBatch>,
    observations: Vec<JournalObservation>,
}

impl JournalScan {
    /// Sequence through which the manifest is known to be durable. This is the
    /// stored checkpoint unless a scan resolved an [`JournalObservation`], in
    /// which case it is the resolved position and the observation says why.
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

    /// Everything the scan had to reconcile or found damaged. Empty for an
    /// undamaged journal.
    #[must_use]
    pub fn observations(&self) -> &[JournalObservation] {
        &self.observations
    }
}

/// The durable checkpoint row: the position the manifest is known to hold, plus
/// the size of the batches recorded after it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Checkpoint {
    sequence: u64,
    revision: u64,
    unapplied_bytes: u64,
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
    /// Directory inside the bundle that holds the journal database.
    #[must_use]
    pub fn journal_path(&self) -> PathBuf {
        self.root().join(JOURNAL_DIRECTORY)
    }

    /// The embedded journal database file itself.
    ///
    /// The database is owned by whichever process has it open: turso locks it
    /// exclusively even to read. Nothing outside this module opens it, and
    /// [`ProjectStore::load`] never does, so inspecting a project is never
    /// blocked by a running daemon.
    #[must_use]
    pub fn journal_database_path(&self) -> PathBuf {
        self.journal_path().join(DATABASE_NAME)
    }

    /// Appends one immutable checksummed mutation batch.
    ///
    /// Sequences must increase by exactly one, `base_revision` must equal the
    /// preceding durable revision, and `revision` must increase. The head is
    /// read and the row inserted inside a single write transaction, so the cost
    /// does not grow with the number of retained batches, and a refused batch
    /// leaves the journal byte-for-byte unchanged. The commit is durable before
    /// this call returns. FNV-1a 32-bit is stored beside every row only to
    /// detect accidental corruption, not for security.
    ///
    /// This is a write path, so it creates the journal when the project has
    /// never journalled before, and it is refused while another process owns
    /// the journal.
    ///
    /// # Errors
    ///
    /// Returns journal consistency, size-limit, manifest, database, lock,
    /// deadline, or filesystem errors.
    pub fn append_batch(&self, batch: &MutationBatch) -> Result<(), StoreError> {
        let deadline = Deadline::new("append_batch");
        // Refuse an oversized record before creating or touching the journal.
        let size = record_size(&batch.payload);
        enforce_record_size(size)?;
        let database = self.open_journal(&deadline)?;

        let transaction = Transaction::begin(&database, &deadline).map_err(StoreError::Journal)?;
        let checkpoint = read_checkpoint(&database, &deadline)?
            .ok_or(StoreError::Journal(JournalError::MalformedCheckpoint))?;
        let head = read_head(&database, &deadline)?;
        // A row that survives below the checkpoint must not set the expected
        // sequence: `scan_journal` only ever returns rows above the checkpoint,
        // so a batch acknowledged there would be durable but invisible.
        let (base_sequence, base_revision) = match head {
            Some((sequence, revision)) if sequence > checkpoint.sequence => (sequence, revision),
            _ => (checkpoint.sequence, checkpoint.revision),
        };
        let expected_sequence = base_sequence
            .checked_add(1)
            .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?;
        validate_batch(batch, expected_sequence, base_revision)?;
        enforce_unapplied_limit(checkpoint.sequence, batch.sequence)?;
        let unapplied_bytes = checkpoint.unapplied_bytes.saturating_add(size);
        enforce_unapplied_bytes(unapplied_bytes)?;

        database
            .execute_prepared(
                INSERT_BATCH_SQL,
                vec![
                    sql_u64(batch.sequence)?,
                    sql_u64(batch.base_revision)?,
                    sql_u64(batch.revision)?,
                    Value::Blob(batch.payload.clone()),
                    Value::Integer(i64::from(checksum(batch))),
                ],
                &deadline,
            )
            .map_err(StoreError::Journal)?;
        database
            .execute_prepared(
                UPDATE_UNAPPLIED_BYTES_SQL,
                vec![sql_u64(unapplied_bytes)?],
                &deadline,
            )
            .map_err(StoreError::Journal)?;
        transaction.commit(&deadline).map_err(StoreError::Journal)?;
        Ok(())
    }

    /// Reads the durable checkpoint and every batch recorded after it.
    ///
    /// Opening the database replays its write-ahead log, so a process that
    /// crashed mid-append observes the last committed state and never a partial
    /// record. Every returned batch must round-trip its stored checksum and
    /// continue the sequence and revision chain; anything else is an error, so
    /// a damaged journal never yields partial history. The manifest itself
    /// remains readable through [`ProjectStore::load`] in that case.
    ///
    /// This is a read path: it creates nothing. An existing database whose
    /// tables or checkpoint row are missing is damage, not an empty journal,
    /// and is reported as such rather than laundered into "nothing to recover".
    /// Whatever a scan has to reconcile is reported through
    /// [`JournalScan::observations`].
    ///
    /// # Errors
    ///
    /// Returns an error for a missing database file whose write-ahead log
    /// survives, missing tables, a missing or malformed checkpoint, a manifest
    /// that disagrees with the journal, a corrupt or unreadable database, a
    /// checksum mismatch, a sequence or revision gap, a size limit, a lock held
    /// by another process, a deadline overrun, or a filesystem failure.
    pub fn scan_journal(&self) -> Result<JournalScan, StoreError> {
        let manifest_revision = self.load()?.position().revision;
        let database_path = self.journal_database_path();
        if !self.journal_is_initialised()? {
            self.require_no_orphaned_log()?;
            return Ok(JournalScan::empty(manifest_revision));
        }
        let deadline = Deadline::new("scan_journal");
        let database =
            JournalDatabase::open(&database_path, &deadline).map_err(StoreError::Journal)?;
        let observations = self.probe_write_ahead_log()?;
        Self::read_scan(&database, manifest_revision, observations, &deadline)
    }

    /// Brings the journal database to a consistent, writable state.
    ///
    /// Opening recovers the write-ahead log; this additionally creates the
    /// journal when it is absent, resolves an interrupted checkpoint, discards
    /// any batch the checkpoint already covers, and republishes the checkpoint
    /// row so the next append continues from the manifest's revision. Anything
    /// it had to reconcile is reported through [`JournalScan::observations`].
    ///
    /// # Errors
    ///
    /// Returns any scan, database, lock, deadline, or filesystem error without
    /// discarding unapplied batches.
    pub fn recover_journal(&self) -> Result<JournalScan, StoreError> {
        let deadline = Deadline::new("recover_journal");
        let database = self.open_journal(&deadline)?;
        let observations = self.probe_write_ahead_log()?;
        let manifest_revision = self.load()?.position().revision;
        let scan = Self::read_scan(&database, manifest_revision, observations, &deadline)?;
        let resolved = Checkpoint {
            sequence: scan.checkpoint_sequence,
            revision: scan.checkpoint_revision,
            unapplied_bytes: retained_bytes(&scan.batches),
        };
        let transaction = Transaction::begin(&database, &deadline).map_err(StoreError::Journal)?;
        write_checkpoint(&database, resolved, &deadline)?;
        database
            .execute(
                DELETE_APPLIED_SQL,
                vec![sql_u64(resolved.sequence)?],
                &deadline,
            )
            .map_err(StoreError::Journal)?;
        transaction.commit(&deadline).map_err(StoreError::Journal)?;
        // Unconditionally, not only for a torn tail. A recovery is the one
        // moment nothing is in flight, and a bundle that has served a show
        // arrives here with a log holding every page every command ever
        // touched: leaving it means the next show starts slower than the last
        // one ended and never recovers. Truncating also erases a torn tail that
        // this recovery has already reported, instead of shadowing every later
        // scan with damage that has been dealt with.
        truncate_write_ahead_log(&database, &deadline)?;
        Ok(scan)
    }

    /// Discards every batch the durable checkpoint does not cover.
    ///
    /// This is an operator's escape hatch, not a recovery path: the batches it
    /// destroys are acknowledged commands, so it hands them back for the caller
    /// to name before anything else happens to them. Their checksums are *not*
    /// verified on the way out, because the reason to reach for this is usually
    /// that one of them no longer verifies and every other path refuses the
    /// bundle for it.
    ///
    /// The checkpoint row is republished at the manifest's revision, so a
    /// journal abandoned in the middle of an interrupted checkpoint agrees with
    /// the manifest afterwards and the next append continues above every
    /// sequence ever written here.
    ///
    /// # Errors
    ///
    /// Returns a missing or malformed checkpoint, a size limit, a manifest,
    /// database, lock, deadline, or filesystem error.
    pub fn abandon_unapplied_batches(&self) -> Result<AbandonedJournal, StoreError> {
        let deadline = Deadline::new("abandon_unapplied_batches");
        let manifest_revision = self.load()?.position().revision;
        let database = self.open_journal(&deadline)?;
        let transaction = Transaction::begin(&database, &deadline).map_err(StoreError::Journal)?;
        let checkpoint = read_checkpoint(&database, &deadline)?
            .ok_or(StoreError::Journal(JournalError::MalformedCheckpoint))?;
        let batches = read_unverified_batches(&database, checkpoint.sequence, &deadline)?;
        let sequence = batches
            .last()
            .map_or(checkpoint.sequence, MutationBatch::sequence)
            .max(checkpoint.sequence);
        write_checkpoint(
            &database,
            Checkpoint {
                sequence,
                revision: manifest_revision,
                unapplied_bytes: 0,
            },
            &deadline,
        )?;
        database
            .execute(DELETE_APPLIED_SQL, vec![sql_u64(sequence)?], &deadline)
            .map_err(StoreError::Journal)?;
        transaction.commit(&deadline).map_err(StoreError::Journal)?;
        truncate_write_ahead_log(&database, &deadline)?;
        Ok(AbandonedJournal {
            checkpoint_sequence: sequence,
            batches,
        })
    }

    /// Opens the journal for the life of the process that serves this project.
    ///
    /// Creates the journal when the project has never journalled, then
    /// validates the schema, the durability pragmas and the recorded history
    /// once and keeps the database open. See [`JournalWriter`] for why.
    ///
    /// # Errors
    ///
    /// Returns anything [`ProjectStore::scan_journal`] returns, plus the
    /// creation errors of a project that has never journalled.
    pub fn open_journal_writer(&self) -> Result<JournalWriter<'_>, StoreError> {
        let deadline = Deadline::new("open_journal_writer");
        let database = self.open_journal(&deadline)?;
        let observations = self.probe_write_ahead_log()?;
        let manifest_revision = self.load()?.position().revision;
        let scan = Self::read_scan(&database, manifest_revision, observations, &deadline)?;
        Ok(JournalWriter {
            store: self,
            database,
            state: RefCell::new(WriterState {
                checkpoint: Checkpoint {
                    sequence: scan.checkpoint_sequence,
                    revision: scan.checkpoint_revision,
                    unapplied_bytes: retained_bytes(&scan.batches),
                },
                head_sequence: scan
                    .batches
                    .last()
                    .map_or(scan.checkpoint_sequence, MutationBatch::sequence),
                head_revision: scan
                    .batches
                    .last()
                    .map_or(scan.checkpoint_revision, MutationBatch::revision),
                unapplied: scan
                    .batches
                    .iter()
                    .map(|batch| record_size(&batch.payload))
                    .collect(),
            }),
        })
    }

    /// Durably saves a manifest, then advances the checkpoint and discards the
    /// batches it includes.
    ///
    /// The manifest rename and directory sync complete before the checkpoint
    /// moves. Advancing the checkpoint and discarding the applied batches then
    /// happen inside a single database transaction, so that pair is
    /// all-or-nothing.
    ///
    /// A crash between the two leaves the manifest at the new revision while
    /// the checkpoint still names the old one and the applied batches are still
    /// present. Those batches are *not* replayable against the older
    /// checkpoint: applying them again would repeat mutations the manifest
    /// already contains. A scan detects that window instead — the manifest
    /// revision matches a retained batch — reports
    /// [`JournalObservation::IncompleteCheckpoint`], and treats that batch as
    /// the checkpoint, so recovery completes the interrupted checkpoint rather
    /// than either app refusing to open the project.
    ///
    /// # Errors
    ///
    /// Returns an error unless `applied_through_sequence` exists (or is the
    /// current checkpoint) and its revision exactly matches the manifest, plus
    /// any validation, database, lock, deadline, or filesystem error.
    pub fn checkpoint_and_compact(
        &self,
        project: &StoredProject,
        applied_through_sequence: u64,
    ) -> Result<CompactionReport, StoreError> {
        let deadline = Deadline::new("checkpoint_and_compact");
        let database = self.open_journal(&deadline)?;
        let observations = self.probe_write_ahead_log()?;
        let manifest_revision = self.load()?.position().revision;
        let scan = Self::read_scan(&database, manifest_revision, observations, &deadline)?;
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
        let retained = retained_bytes(
            scan.batches
                .iter()
                .filter(|batch| batch.sequence > applied_through_sequence),
        );

        self.save(project)?;
        let transaction = Transaction::begin(&database, &deadline).map_err(StoreError::Journal)?;
        write_checkpoint(
            &database,
            Checkpoint {
                sequence: applied_through_sequence,
                revision: applied_revision,
                unapplied_bytes: retained,
            },
            &deadline,
        )?;
        let removed = database
            .execute(
                DELETE_APPLIED_SQL,
                vec![sql_u64(applied_through_sequence)?],
                &deadline,
            )
            .map_err(StoreError::Journal)?;
        transaction.commit(&deadline).map_err(StoreError::Journal)?;
        // The rows are gone, but their pages are still in the write-ahead log
        // and stay there until something folds them back. A journal that
        // compacts and never truncates grows for the life of the bundle.
        truncate_write_ahead_log(&database, &deadline)?;
        Ok(CompactionReport {
            applied_through_sequence,
            removed_records: usize::try_from(removed).unwrap_or(usize::MAX),
        })
    }

    /// Opens the journal, creating the directory, schema, and initial
    /// checkpoint row only when the project has never journalled before.
    ///
    /// This is the only path that creates anything, and even here creation is
    /// confined to a journal that does not exist yet: an initialised database
    /// whose schema has gone missing is damage, and recreating the schema over
    /// it would report a clean journal where work was lost.
    fn open_journal(&self, deadline: &Deadline) -> Result<JournalDatabase, StoreError> {
        let database_path = self.journal_database_path();
        if self.journal_is_initialised()? {
            let database =
                JournalDatabase::open(&database_path, deadline).map_err(StoreError::Journal)?;
            require_table(&database, CHECKPOINT_TABLE, deadline)?;
            require_table(&database, BATCH_TABLE, deadline)?;
            return Ok(database);
        }

        self.require_no_orphaned_log()?;
        // Reading the manifest first keeps a missing project from leaving a
        // half-created journal directory behind.
        let revision = self.load()?.position().revision;
        let journal = self.journal_path();
        if !journal.try_exists().map_err(StoreError::Io)? {
            fs::create_dir_all(&journal).map_err(StoreError::Io)?;
            sync_directory(self.root())?;
        }
        let database =
            JournalDatabase::open(&database_path, deadline).map_err(StoreError::Journal)?;
        database
            .execute_batch(CREATE_SCHEMA_SQL, deadline)
            .map_err(StoreError::Journal)?;
        let transaction = Transaction::begin(&database, deadline).map_err(StoreError::Journal)?;
        write_checkpoint(
            &database,
            Checkpoint {
                sequence: 0,
                revision,
                unapplied_bytes: 0,
            },
            deadline,
        )?;
        transaction.commit(deadline).map_err(StoreError::Journal)?;
        // Make the new database file's directory entry durable too.
        sync_directory(&journal)?;
        Ok(database)
    }

    /// Whether a journal database has ever been written here.
    ///
    /// A zero-length file is an initialisation that crashed before its first
    /// commit: nothing was ever recorded in it, so it may be initialised
    /// again. Anything longer has held committed state.
    fn journal_is_initialised(&self) -> Result<bool, StoreError> {
        match fs::metadata(self.journal_database_path()) {
            Ok(metadata) => Ok(metadata.len() > 0),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    /// Refuses a journal whose database file is gone or empty while its
    /// write-ahead log or shared-memory sidecar survives. That is a deleted or
    /// half-restored journal, not a project that has never journalled, and
    /// initialising over it would report "nothing to recover" on top of lost
    /// work.
    fn require_no_orphaned_log(&self) -> Result<(), StoreError> {
        let journal = self.journal_path();
        for sidecar in [WRITE_AHEAD_LOG_NAME, SHARED_MEMORY_NAME] {
            let path = journal.join(sidecar);
            if path.try_exists().map_err(StoreError::Io)? {
                return Err(StoreError::Journal(JournalError::MissingDatabase {
                    database: self.journal_database_path(),
                    sidecar: path,
                }));
            }
        }
        Ok(())
    }

    /// Reports a write-ahead log that does not end on a frame boundary.
    ///
    /// turso writes a 32-byte header followed by frames of
    /// `24 + page_size` bytes and stops recovery at the last whole, checksummed
    /// frame, so a crash mid-write silently shortens history. Measuring the
    /// file is the one cheap witness of that: it runs while this process holds
    /// the exclusive lock, so no other writer can be mid-frame. A tail lost on
    /// an exact frame boundary leaves no trace here and is only caught when the
    /// manifest depends on it.
    fn probe_write_ahead_log(&self) -> Result<Vec<JournalObservation>, StoreError> {
        let path = self.journal_path().join(WRITE_AHEAD_LOG_NAME);
        let size = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(StoreError::Io(error)),
        };
        if size == 0 {
            return Ok(Vec::new());
        }
        let header_bytes = LOG_HEADER_BYTES as u64;
        if size < header_bytes {
            return Ok(vec![JournalObservation::TornWriteAheadLog {
                bytes: size,
                frame_bytes: 0,
            }]);
        }
        let mut header = [0_u8; LOG_HEADER_BYTES];
        fs::File::open(&path)
            .and_then(|mut file| file.read_exact(&mut header))
            .map_err(StoreError::Io)?;
        let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let page_bytes = u64::from(u32::from_be_bytes([
            header[8], header[9], header[10], header[11],
        ]));
        // An unrecognised header is not a torn tail; the database open reports
        // whatever is actually wrong with it.
        if !LOG_MAGIC.contains(&magic) || !(512..=65_536).contains(&page_bytes) {
            return Ok(Vec::new());
        }
        let frame_bytes = LOG_FRAME_HEADER_BYTES + page_bytes;
        if (size - header_bytes).is_multiple_of(frame_bytes) {
            return Ok(Vec::new());
        }
        Ok(vec![JournalObservation::TornWriteAheadLog {
            bytes: size,
            frame_bytes,
        }])
    }

    /// Reads the journal's state, reconciling it against the manifest.
    ///
    /// The manifest is the durable applied state and the journal is the
    /// write-ahead record of what comes after it, so the checkpoint revision,
    /// the manifest revision and the journal head must be ordered. Anything
    /// else means one of them lost history, which is reported rather than
    /// returned as a short but plausible-looking scan.
    fn read_scan(
        database: &JournalDatabase,
        manifest_revision: u64,
        mut observations: Vec<JournalObservation>,
        deadline: &Deadline,
    ) -> Result<JournalScan, StoreError> {
        require_table(database, CHECKPOINT_TABLE, deadline)?;
        require_table(database, BATCH_TABLE, deadline)?;
        let checkpoint = read_checkpoint(database, deadline)?
            .ok_or(StoreError::Journal(JournalError::MalformedCheckpoint))?;
        if checkpoint.revision > manifest_revision {
            return Err(StoreError::Journal(JournalError::ManifestBehindJournal {
                manifest_revision,
                checkpoint_revision: checkpoint.revision,
            }));
        }
        let head = read_head(database, deadline)?;
        if let Some((sequence, _)) = head {
            enforce_unapplied_limit(checkpoint.sequence, sequence)?;
        }
        enforce_unapplied_bytes(checkpoint.unapplied_bytes)?;

        // The byte budget is enforced as rows arrive, so a journal that grew
        // past it is refused instead of collected into memory first.
        let mut collected = 0_u64;
        let mut batches = database
            .query(
                SELECT_BATCHES_SQL,
                vec![sql_u64(checkpoint.sequence)?],
                deadline,
                |row| {
                    let batch = decode_batch(row)?;
                    collected = collected.saturating_add(record_size(&batch.payload));
                    if collected > MAX_UNAPPLIED_JOURNAL_BYTES {
                        return Err(JournalError::UnappliedByteLimit {
                            unapplied: collected,
                            maximum: MAX_UNAPPLIED_JOURNAL_BYTES,
                        });
                    }
                    Ok(batch)
                },
            )
            .map_err(StoreError::Journal)?;
        let mut expected_sequence = checkpoint
            .sequence
            .checked_add(1)
            .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?;
        let mut expected_revision = checkpoint.revision;
        for batch in &batches {
            validate_batch(batch, expected_sequence, expected_revision)?;
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?;
            expected_revision = batch.revision;
        }

        let mut resolved = checkpoint;
        if manifest_revision > checkpoint.revision {
            if batches.is_empty() {
                // Nothing unapplied is recorded, so the journal cannot disagree
                // with a manifest that moved on without it: the manifest is the
                // durable state and the checkpoint row is merely stale.
                resolved.revision = manifest_revision;
            } else if let Some(applied) = batches
                .iter()
                .position(|batch| batch.revision == manifest_revision)
            {
                resolved = Checkpoint {
                    sequence: batches[applied].sequence,
                    revision: manifest_revision,
                    unapplied_bytes: 0,
                };
                observations.push(JournalObservation::IncompleteCheckpoint {
                    sequence: resolved.sequence,
                    revision: resolved.revision,
                });
                batches.drain(..=applied);
            } else {
                // The manifest contains a revision the journal has no record
                // of: the journal lost history the manifest depends on.
                return Err(StoreError::Journal(JournalError::JournalBehindManifest {
                    manifest_revision,
                    journal_revision: expected_revision,
                }));
            }
        }
        Ok(JournalScan {
            checkpoint_sequence: resolved.sequence,
            checkpoint_revision: resolved.revision,
            batches,
            observations,
        })
    }
}

/// Everything [`ProjectStore::abandon_unapplied_batches`] destroyed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbandonedJournal {
    checkpoint_sequence: u64,
    batches: Vec<MutationBatch>,
}

impl AbandonedJournal {
    /// The sequence the journal is now checkpointed at.
    #[must_use]
    pub const fn checkpoint_sequence(&self) -> u64 {
        self.checkpoint_sequence
    }

    /// The discarded batches, oldest first, exactly as they were stored.
    #[must_use]
    pub fn batches(&self) -> &[MutationBatch] {
        &self.batches
    }
}

/// A process's open, exclusive handle to a project's journal.
///
/// [`ProjectStore::append_batch`] opens a database, checks its schema and its
/// durability pragmas, writes one row and closes it again. That is right for a
/// one-shot offline write and wrong for the write that stands between an
/// operator's cut and its acknowledgement: opening rebuilds the write-ahead log
/// index, so the cost of a command grows with everything recorded before it and
/// keeps growing for as long as the bundle is used. This module already states
/// that the journal is a single-writer, process-owned resource; a writer is
/// that ownership made explicit. It is opened once, its schema, pragmas and
/// history are validated once, and it holds the exclusive lock for as long as
/// it lives, so an append is a transaction and nothing else.
pub struct JournalWriter<'store> {
    store: &'store ProjectStore,
    database: JournalDatabase,
    state: RefCell<WriterState>,
}

/// What a writer knows about the journal it owns.
///
/// The writer is the journal's only writer, so what it last committed is what
/// the journal holds: re-reading the checkpoint row and the head before every
/// append would only confirm what this already says. It advances after a commit
/// succeeds and never before, so a refused or failed write leaves it describing
/// the durable state exactly as the rolled-back transaction does.
struct WriterState {
    checkpoint: Checkpoint,
    head_sequence: u64,
    head_revision: u64,
    /// Record sizes of the batches above the checkpoint, oldest first.
    unapplied: VecDeque<u64>,
}

/// What compacting through a sequence would leave behind.
struct AppliedCheckpoint {
    revision: u64,
    retained_bytes: u64,
    removed_records: usize,
}

impl WriterState {
    /// The revision a retained `sequence` carries, and what compacting through
    /// it would leave.
    ///
    /// Sequences increase by exactly one and every batch moves the revision by
    /// exactly one — both enforced on the way in — so the revision at a
    /// retained sequence follows from the checkpoint and there is nothing to
    /// look up.
    fn applied_checkpoint(&self, sequence: u64) -> Result<AppliedCheckpoint, StoreError> {
        if sequence < self.checkpoint.sequence || sequence > self.head_sequence {
            return Err(StoreError::Journal(JournalError::UnknownCheckpoint(
                sequence,
            )));
        }
        let applied = sequence - self.checkpoint.sequence;
        let removed_records =
            usize::try_from(applied).map_err(|_| StoreError::Journal(JournalError::SequenceOverflow))?;
        Ok(AppliedCheckpoint {
            revision: self
                .checkpoint
                .revision
                .checked_add(applied)
                .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?,
            retained_bytes: self
                .unapplied
                .iter()
                .skip(removed_records)
                .fold(0, |total, size| total.saturating_add(*size)),
            removed_records,
        })
    }
}

impl JournalWriter<'_> {
    /// The newest sequence this journal holds, applied or not.
    #[must_use]
    pub fn head_sequence(&self) -> u64 {
        self.state.borrow().head_sequence
    }

    /// Appends one immutable checksummed mutation batch.
    ///
    /// The contract is [`ProjectStore::append_batch`]'s — sequences increase by
    /// exactly one, `base_revision` must equal the preceding durable revision,
    /// `revision` must be the next one, and the commit is durable before this
    /// returns — and none of it re-reads the journal or re-checks the schema:
    /// this writer owns the journal, so the head it validates against is the
    /// one it last committed. A refused or failed batch leaves the journal and
    /// this writer byte-for-byte unchanged.
    ///
    /// # Errors
    ///
    /// Returns journal consistency, size-limit, database, lock, deadline, or
    /// filesystem errors.
    pub fn append_batch(&self, batch: &MutationBatch) -> Result<(), StoreError> {
        let deadline = Deadline::new("append_batch");
        let size = record_size(&batch.payload);
        enforce_record_size(size)?;
        let mut state = self.state.borrow_mut();
        let expected_sequence = state
            .head_sequence
            .checked_add(1)
            .ok_or(StoreError::Journal(JournalError::SequenceOverflow))?;
        validate_batch(batch, expected_sequence, state.head_revision)?;
        enforce_unapplied_limit(state.checkpoint.sequence, batch.sequence)?;
        let unapplied_bytes = state.checkpoint.unapplied_bytes.saturating_add(size);
        enforce_unapplied_bytes(unapplied_bytes)?;

        let transaction =
            Transaction::begin(&self.database, &deadline).map_err(StoreError::Journal)?;
        self.database
            .execute_prepared(
                INSERT_BATCH_SQL,
                vec![
                    sql_u64(batch.sequence)?,
                    sql_u64(batch.base_revision)?,
                    sql_u64(batch.revision)?,
                    Value::Blob(batch.payload.clone()),
                    Value::Integer(i64::from(checksum(batch))),
                ],
                &deadline,
            )
            .map_err(StoreError::Journal)?;
        self.database
            .execute_prepared(
                UPDATE_UNAPPLIED_BYTES_SQL,
                vec![sql_u64(unapplied_bytes)?],
                &deadline,
            )
            .map_err(StoreError::Journal)?;
        transaction.commit(&deadline).map_err(StoreError::Journal)?;

        state.checkpoint.unapplied_bytes = unapplied_bytes;
        state.head_sequence = batch.sequence;
        state.head_revision = batch.revision;
        state.unapplied.push_back(size);
        Ok(())
    }

    /// Durably saves a manifest, then advances the checkpoint, discards the
    /// batches it includes and truncates the write-ahead log.
    ///
    /// The ordering and the crash windows are
    /// [`ProjectStore::checkpoint_and_compact`]'s. The truncation is what keeps
    /// the log — and therefore the cost of every later command — the same size
    /// after ten thousand commands as after ten.
    ///
    /// # Errors
    ///
    /// Returns an error unless `applied_through_sequence` is the current
    /// checkpoint or a batch recorded after it and its revision is exactly the
    /// manifest's, plus any database, lock, deadline, or filesystem error.
    pub fn checkpoint_and_compact(
        &self,
        project: &StoredProject,
        applied_through_sequence: u64,
    ) -> Result<CompactionReport, StoreError> {
        let deadline = Deadline::new("checkpoint_and_compact");
        let mut state = self.state.borrow_mut();
        let applied = state.applied_checkpoint(applied_through_sequence)?;
        if project.position().revision != applied.revision {
            return Err(StoreError::Journal(JournalError::CheckpointRevision {
                sequence: applied_through_sequence,
                expected: applied.revision,
                found: project.position().revision,
            }));
        }
        let checkpoint = Checkpoint {
            sequence: applied_through_sequence,
            revision: applied.revision,
            unapplied_bytes: applied.retained_bytes,
        };

        self.store.save(project)?;
        let transaction =
            Transaction::begin(&self.database, &deadline).map_err(StoreError::Journal)?;
        write_checkpoint(&self.database, checkpoint, &deadline)?;
        let removed = self
            .database
            .execute(
                DELETE_APPLIED_SQL,
                vec![sql_u64(applied_through_sequence)?],
                &deadline,
            )
            .map_err(StoreError::Journal)?;
        transaction.commit(&deadline).map_err(StoreError::Journal)?;
        state.checkpoint = checkpoint;
        state.unapplied.drain(..applied.removed_records);
        drop(state);

        truncate_write_ahead_log(&self.database, &deadline)?;
        Ok(CompactionReport {
            applied_through_sequence,
            removed_records: usize::try_from(removed).unwrap_or(usize::MAX),
        })
    }
}

/// Folds the write-ahead log back into the database and truncates it.
///
/// Compaction deletes rows; it does not shorten the log those deletions were
/// written to. Without this the log is the one part of a bundle that only ever
/// grows, and every journal open pays to rebuild its index.
fn truncate_write_ahead_log(
    database: &JournalDatabase,
    deadline: &Deadline,
) -> Result<(), StoreError> {
    database
        .query(CHECKPOINT_LOG_SQL, Vec::new(), deadline, |_| Ok(()))
        .map(|_| ())
        .map_err(StoreError::Journal)
}

/// Reads the batches above `checkpoint_sequence` without verifying them.
///
/// Only [`ProjectStore::abandon_unapplied_batches`] uses this: everything else
/// must refuse a batch whose bytes no longer round-trip rather than return it.
fn read_unverified_batches(
    database: &JournalDatabase,
    checkpoint_sequence: u64,
    deadline: &Deadline,
) -> Result<Vec<MutationBatch>, StoreError> {
    let mut collected = 0_u64;
    database
        .query(
            SELECT_UNVERIFIED_BATCHES_SQL,
            vec![sql_u64(checkpoint_sequence)?],
            deadline,
            |row| {
                let payload = column_blob(row, 3)?;
                collected = collected.saturating_add(record_size(&payload));
                if collected > MAX_UNAPPLIED_JOURNAL_BYTES {
                    return Err(JournalError::UnappliedByteLimit {
                        unapplied: collected,
                        maximum: MAX_UNAPPLIED_JOURNAL_BYTES,
                    });
                }
                Ok(MutationBatch {
                    sequence: row_u64(row, 0)?,
                    base_revision: row_u64(row, 1)?,
                    revision: row_u64(row, 2)?,
                    payload,
                })
            },
        )
        .map_err(StoreError::Journal)
}

impl JournalScan {
    /// A project that has never journalled: the manifest is the whole state.
    const fn empty(manifest_revision: u64) -> Self {
        Self {
            checkpoint_sequence: 0,
            checkpoint_revision: manifest_revision,
            batches: Vec::new(),
            observations: Vec::new(),
        }
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

fn decode_batch(row: &Row) -> Result<MutationBatch, JournalError> {
    let sequence = row_u64(row, 0)?;
    let base_revision = row_u64(row, 1)?;
    let revision = row_u64(row, 2)?;
    let payload = column_blob(row, 3)?;
    let stored = u32::try_from(column_integer(row, 4)?)
        .map_err(|_| JournalError::MalformedColumn { index: 4 })?;
    let size = record_size(&payload);
    if size > MAX_JOURNAL_RECORD_BYTES {
        return Err(JournalError::RecordTooLarge {
            size,
            maximum: MAX_JOURNAL_RECORD_BYTES,
        });
    }
    let batch = MutationBatch {
        sequence,
        base_revision,
        revision,
        payload,
    };
    let actual = checksum(&batch);
    if stored != actual {
        return Err(JournalError::ChecksumMismatch {
            sequence,
            expected: stored,
            actual,
        });
    }
    Ok(batch)
}

/// Refuses a database that is missing a table this journal defines.
///
/// Read paths never create the schema: an existing database without it has
/// lost its schema, and creating it there would turn that damage into an
/// empty, apparently healthy journal.
fn require_table(
    database: &JournalDatabase,
    table: &'static str,
    deadline: &Deadline,
) -> Result<(), StoreError> {
    let found = database
        .query(
            SELECT_TABLE_SQL,
            vec![Value::Text(table.to_owned())],
            deadline,
            |row| column_integer(row, 0),
        )
        .map_err(StoreError::Journal)?
        .first()
        .copied()
        .unwrap_or_default();
    if found > 0 {
        return Ok(());
    }
    Err(StoreError::Journal(JournalError::CorruptDatabase {
        operation: deadline.operation(),
        message: format!("table `{table}` is missing"),
    }))
}

fn read_checkpoint(
    database: &JournalDatabase,
    deadline: &Deadline,
) -> Result<Option<Checkpoint>, StoreError> {
    let rows = database
        .query(SELECT_CHECKPOINT_SQL, Vec::new(), deadline, |row| {
            Ok(Checkpoint {
                sequence: row_u64(row, 0)?,
                revision: row_u64(row, 1)?,
                unapplied_bytes: row_u64(row, 2)?,
            })
        })
        .map_err(StoreError::Journal)?;
    Ok(rows.into_iter().next())
}

fn read_head(
    database: &JournalDatabase,
    deadline: &Deadline,
) -> Result<Option<(u64, u64)>, StoreError> {
    let rows = database
        .query(SELECT_HEAD_SQL, Vec::new(), deadline, |row| {
            Ok((row_u64(row, 0)?, row_u64(row, 1)?))
        })
        .map_err(StoreError::Journal)?;
    Ok(rows.into_iter().next())
}

fn write_checkpoint(
    database: &JournalDatabase,
    checkpoint: Checkpoint,
    deadline: &Deadline,
) -> Result<(), StoreError> {
    database
        .execute(DELETE_CHECKPOINT_SQL, Vec::new(), deadline)
        .map_err(StoreError::Journal)?;
    database
        .execute(
            INSERT_CHECKPOINT_SQL,
            vec![
                sql_u64(checkpoint.sequence)?,
                sql_u64(checkpoint.revision)?,
                sql_u64(checkpoint.unapplied_bytes)?,
            ],
            deadline,
        )
        .map_err(StoreError::Journal)?;
    Ok(())
}

fn retained_bytes<'batch>(batches: impl IntoIterator<Item = &'batch MutationBatch>) -> u64 {
    batches.into_iter().fold(0, |total, batch| {
        total.saturating_add(record_size(&batch.payload))
    })
}

fn row_u64(row: &Row, index: usize) -> Result<u64, JournalError> {
    u64::try_from(column_integer(row, index)?).map_err(|_| JournalError::MalformedColumn { index })
}

fn sql_u64(value: u64) -> Result<Value, StoreError> {
    i64::try_from(value)
        .map(Value::Integer)
        .map_err(|_| StoreError::Journal(JournalError::OutOfRange { value }))
}

fn record_size(payload: &[u8]) -> u64 {
    RECORD_OVERHEAD_BYTES.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX))
}

fn enforce_record_size(size: u64) -> Result<(), StoreError> {
    if size > MAX_JOURNAL_RECORD_BYTES {
        return Err(StoreError::Journal(JournalError::RecordTooLarge {
            size,
            maximum: MAX_JOURNAL_RECORD_BYTES,
        }));
    }
    Ok(())
}

fn enforce_unapplied_limit(checkpoint_sequence: u64, head_sequence: u64) -> Result<(), StoreError> {
    let unapplied = head_sequence.saturating_sub(checkpoint_sequence);
    if unapplied > MAX_UNAPPLIED_JOURNAL_BATCHES {
        return Err(StoreError::Journal(JournalError::UnappliedBatchLimit {
            unapplied,
            maximum: MAX_UNAPPLIED_JOURNAL_BATCHES,
        }));
    }
    Ok(())
}

fn enforce_unapplied_bytes(unapplied: u64) -> Result<(), StoreError> {
    if unapplied > MAX_UNAPPLIED_JOURNAL_BYTES {
        return Err(StoreError::Journal(JournalError::UnappliedByteLimit {
            unapplied,
            maximum: MAX_UNAPPLIED_JOURNAL_BYTES,
        }));
    }
    Ok(())
}

fn checksum(batch: &MutationBatch) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for field in [batch.sequence, batch.base_revision, batch.revision] {
        hash = fnv1a32(hash, &field.to_le_bytes());
    }
    fnv1a32(hash, &batch.payload)
}

fn fnv1a32(mut hash: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
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
        sequence: u64,
        expected: u32,
        actual: u32,
    },
    MalformedColumn {
        index: usize,
    },
    MalformedCheckpoint,
    /// The manifest holds a revision the journal has no record of.
    JournalBehindManifest {
        manifest_revision: u64,
        journal_revision: u64,
    },
    /// The checkpoint claims the manifest holds work the manifest does not.
    ManifestBehindJournal {
        manifest_revision: u64,
        checkpoint_revision: u64,
    },
    /// The database file is gone while a sidecar of it survives.
    MissingDatabase {
        database: PathBuf,
        sidecar: PathBuf,
    },
    UnknownCheckpoint(u64),
    CheckpointRevision {
        sequence: u64,
        expected: u64,
        found: u64,
    },
    UnappliedBatchLimit {
        unapplied: u64,
        maximum: u64,
    },
    UnappliedByteLimit {
        unapplied: u64,
        maximum: u64,
    },
    OutOfRange {
        value: u64,
    },
    SequenceOverflow,
    UnsupportedDatabasePath(PathBuf),
    DurabilityUnavailable {
        pragma: &'static str,
        reported: Option<i64>,
    },
    CorruptDatabase {
        operation: &'static str,
        message: String,
    },
    /// Another process owns the journal database.
    Locked {
        operation: &'static str,
        message: String,
    },
    /// The database refused a write that this module's own checks should have
    /// refused first: an internal invariant, not an operator error.
    Constraint {
        operation: &'static str,
        message: String,
    },
    Database {
        operation: &'static str,
        message: String,
    },
    Deadline {
        operation: &'static str,
        limit: Duration,
    },
}

impl fmt::Display for JournalError {
    #[allow(clippy::too_many_lines)]
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
            Self::ChecksumMismatch { sequence, .. } => write!(
                formatter,
                "journal batch {sequence} does not match its stored checksum"
            ),
            Self::MalformedColumn { index } => write!(
                formatter,
                "journal database column {index} holds an unexpected value"
            ),
            Self::MalformedCheckpoint => formatter.write_str("malformed journal checkpoint"),
            Self::JournalBehindManifest {
                manifest_revision,
                journal_revision,
            } => write!(
                formatter,
                "journal history ends at revision {journal_revision} but the manifest holds revision {manifest_revision}: the journal lost committed history"
            ),
            Self::ManifestBehindJournal {
                manifest_revision,
                checkpoint_revision,
            } => write!(
                formatter,
                "journal checkpoint is at revision {checkpoint_revision} but the manifest holds revision {manifest_revision}: the manifest lost committed history"
            ),
            Self::MissingDatabase { database, sidecar } => write!(
                formatter,
                "journal database `{}` is missing while `{}` survives: the journal was deleted or restored incompletely",
                database.display(),
                sidecar.display()
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
            Self::UnappliedBatchLimit { unapplied, maximum } => write!(
                formatter,
                "journal holds {unapplied} unapplied batches, exceeding the {maximum} maximum; checkpoint the project"
            ),
            Self::UnappliedByteLimit { unapplied, maximum } => write!(
                formatter,
                "journal holds {unapplied} unapplied bytes, exceeding the {maximum}-byte maximum; checkpoint the project"
            ),
            Self::OutOfRange { value } => {
                write!(
                    formatter,
                    "journal value {value} is out of the stored range"
                )
            }
            Self::SequenceOverflow => formatter.write_str("journal sequence overflow"),
            Self::UnsupportedDatabasePath(path) => write!(
                formatter,
                "journal database path `{}` is not valid UTF-8",
                path.display()
            ),
            Self::DurabilityUnavailable { pragma, reported } => write!(
                formatter,
                "journal database would not commit durably (PRAGMA {pragma} reported {})",
                reported.map_or_else(|| "nothing".to_owned(), |value| value.to_string())
            ),
            Self::CorruptDatabase { operation, message } => write!(
                formatter,
                "journal database is corrupt during {operation}: {message}"
            ),
            Self::Locked { operation, message } => write!(
                formatter,
                "journal database is owned by another process during {operation}: a running freemixd normally holds it, and offline access is refused while it does; stop the daemon and retry ({message})"
            ),
            Self::Constraint { operation, message } => write!(
                formatter,
                "journal database refused a write during {operation}: {message}"
            ),
            Self::Database { operation, message } => {
                write!(
                    formatter,
                    "journal database error during {operation}: {message}"
                )
            }
            Self::Deadline { operation, limit } => write!(
                formatter,
                "journal {operation} exceeded its {} ms deadline",
                limit.as_millis()
            ),
        }
    }
}

impl Error for JournalError {}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    use fm_model::{Input, InputKind, MainMix, Project, ProjectSettings};
    use fm_types::{
        AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, PixelFormat, ProjectId,
        SampleFormat, SampleRate, ScanMode, VideoDimensions, VideoFormat,
    };

    use crate::{ProjectPosition, RuntimeRouting};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestBundle(PathBuf);

    impl TestBundle {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "fm-journal-{}-{}-{name}.freemix",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }

        fn store(&self, revision: u64) -> ProjectStore {
            let store = ProjectStore::new(self.0.clone()).unwrap();
            store.save(&manifest(revision)).unwrap();
            store
        }
    }

    impl Drop for TestBundle {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn manifest(revision: u64) -> StoredProject {
        let frame_rate = FrameRate::new(60, 1).unwrap();
        let settings = ProjectSettings {
            frame_rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(1_920, 1_080).unwrap(),
                frame_rate,
                pixel_format: PixelFormat::Nv12,
                scan: ScanMode::Progressive,
                color: ColorMetadata::default(),
            },
            audio: AudioFormat {
                sample_rate: SampleRate::new(48_000).unwrap(),
                sample_format: SampleFormat::F32,
                channels: ChannelLayout::stereo(),
            },
        };
        let program = InputId::new(std::num::NonZeroU128::new(1).unwrap());
        let preview = InputId::new(std::num::NonZeroU128::new(2).unwrap());
        let mut domain = Project::new(
            ProjectId::new(std::num::NonZeroU128::new(7).unwrap()),
            "Journal",
            settings,
        );
        for (id, name) in [(program, "Program"), (preview, "Preview")] {
            domain.add_input(Input {
                id,
                name: name.to_owned(),
                kind: InputKind::Color,
                required_capabilities: Vec::new(),
            });
        }
        domain.set_main_mix(MainMix::new(program, preview));
        StoredProject::from_project(
            domain,
            RuntimeRouting {
                desired_program_id: Some(program),
                realized_program_id: Some(program),
                desired_preview_id: Some(preview),
                realized_preview_id: Some(preview),
            },
            ProjectPosition {
                revision,
                ..ProjectPosition::default()
            },
            Vec::new(),
        )
        .unwrap()
    }

    /// A stored batch whose bytes no longer round-trip must be rejected instead
    /// of returned as history.
    #[test]
    fn scan_rejects_a_batch_whose_stored_bytes_do_not_round_trip() {
        let bundle = TestBundle::new("tamper");
        let store = bundle.store(4);
        store
            .append_batch(&MutationBatch::new(1, 4, 5, b"original".to_vec()))
            .unwrap();

        let deadline = Deadline::new("test");
        let database = JournalDatabase::open(&store.journal_database_path(), &deadline).unwrap();
        database
            .execute(
                "UPDATE journal_batch SET payload = ?1 WHERE sequence = 1",
                vec![Value::Blob(b"tampered".to_vec())],
                &deadline,
            )
            .unwrap();
        drop(database);

        assert!(matches!(
            store.scan_journal(),
            Err(StoreError::Journal(JournalError::ChecksumMismatch {
                sequence: 1,
                ..
            }))
        ));
    }

    /// A checkpoint left behind by a crash between the durable checkpoint and
    /// its cleanup is discarded by recovery, never replayed twice.
    #[test]
    fn recovery_discards_batches_already_covered_by_the_checkpoint() {
        let bundle = TestBundle::new("recover");
        let store = bundle.store(1);
        store
            .append_batch(&MutationBatch::new(1, 1, 2, b"one".to_vec()))
            .unwrap();
        store
            .append_batch(&MutationBatch::new(2, 2, 3, b"two".to_vec()))
            .unwrap();

        // Save the manifest and advance the checkpoint alone, as a crash before
        // the cleanup that follows them would leave it.
        store.save(&manifest(2)).unwrap();
        let deadline = Deadline::new("test");
        let database = JournalDatabase::open(&store.journal_database_path(), &deadline).unwrap();
        write_checkpoint(
            &database,
            Checkpoint {
                sequence: 1,
                revision: 2,
                unapplied_bytes: 0,
            },
            &deadline,
        )
        .unwrap();
        drop(database);

        let scan = store.scan_journal().unwrap();
        assert_eq!(scan.checkpoint_sequence(), 1);
        assert_eq!(scan.batches().len(), 1);
        assert_eq!(scan.batches()[0].sequence(), 2);

        let recovered = store.recover_journal().unwrap();
        assert_eq!(recovered.batches().len(), 1);
        assert_eq!(recovered.batches()[0].sequence(), 2);
        assert_eq!(store.scan_journal().unwrap().batches().len(), 1);
    }

    /// A journal whose write-ahead log was damaged must never be read as a
    /// clean, empty journal, and the read path and the recovery path must say
    /// the same thing about it. Creating the schema while reading turned
    /// exactly this damage into "nothing to recover".
    #[test]
    fn a_damaged_write_ahead_log_is_never_read_as_a_clean_journal() {
        let bundle = TestBundle::new("flipped-frame");
        let store = bundle.store(0);
        for sequence in 1..=5 {
            store
                .append_batch(&MutationBatch::new(
                    sequence,
                    sequence - 1,
                    sequence,
                    b"committed".to_vec(),
                ))
                .unwrap();
        }

        // Flip one bit inside the first logged frame.
        let log = store.journal_path().join(WRITE_AHEAD_LOG_NAME);
        let mut bytes = fs::read(&log).unwrap();
        let frame = LOG_HEADER_BYTES + usize::try_from(LOG_FRAME_HEADER_BYTES).unwrap();
        bytes[frame] ^= 0x01;
        fs::write(&log, bytes).unwrap();

        let scanned = store.scan_journal();
        let recovered = store.recover_journal();
        assert!(
            scanned.is_err(),
            "a damaged journal must not scan clean: {:?}",
            scanned.map(|scan| scan.batches().len())
        );
        assert!(
            recovered.is_err(),
            "a damaged journal must not recover clean: {:?}",
            recovered.map(|scan| scan.batches().len())
        );
        // The last consistent state stays readable through the manifest.
        assert_eq!(store.load().unwrap().position().revision, 0);
    }

    /// Deleting or emptying the database while its write-ahead log survives is
    /// a damaged journal, not a project that never journalled.
    #[test]
    fn a_database_deleted_under_its_log_is_reported() {
        let bundle = TestBundle::new("orphan-log");
        let store = bundle.store(0);
        store
            .append_batch(&MutationBatch::new(1, 0, 1, b"one".to_vec()))
            .unwrap();
        assert!(
            store
                .journal_path()
                .join(WRITE_AHEAD_LOG_NAME)
                .try_exists()
                .unwrap()
        );

        for damage in [
            |path: &PathBuf| fs::write(path, b"").unwrap(),
            |path: &PathBuf| fs::remove_file(path).unwrap(),
        ] {
            damage(&store.journal_database_path());
            assert!(matches!(
                store.scan_journal(),
                Err(StoreError::Journal(JournalError::MissingDatabase { .. }))
            ));
            assert!(matches!(
                store.recover_journal(),
                Err(StoreError::Journal(JournalError::MissingDatabase { .. }))
            ));
        }
    }

    /// Holding the journal database open must not stop anything from reading
    /// the project: inspection never touches the journal, so a running daemon
    /// never locks an operator out of the manifest.
    #[test]
    fn an_open_journal_database_does_not_block_loading_the_project() {
        let bundle = TestBundle::new("open-while-loading");
        let store = bundle.store(3);
        store
            .append_batch(&MutationBatch::new(1, 3, 4, b"one".to_vec()))
            .unwrap();

        let deadline = Deadline::new("test");
        let held = JournalDatabase::open(&store.journal_database_path(), &deadline).unwrap();
        assert_eq!(store.load().unwrap().position().revision, 3);
        assert_eq!(
            ProjectStore::new(store.root().to_path_buf())
                .unwrap()
                .load()
                .unwrap()
                .position()
                .revision,
            3
        );
        drop(held);
    }

    /// A crash between the manifest save and the checkpoint transaction leaves
    /// the manifest ahead of the checkpoint. The batches it already contains
    /// must not come back as unapplied history, and neither app may refuse the
    /// project: recovery finishes the interrupted checkpoint.
    #[test]
    fn an_interrupted_checkpoint_is_reported_and_completed() {
        let bundle = TestBundle::new("interrupted");
        let store = bundle.store(4);
        store
            .append_batch(&MutationBatch::new(1, 4, 5, b"one".to_vec()))
            .unwrap();
        store
            .append_batch(&MutationBatch::new(2, 5, 6, b"two".to_vec()))
            .unwrap();
        // `checkpoint_and_compact` saves the manifest first; crash here.
        store.save(&manifest(5)).unwrap();

        let scan = store.scan_journal().unwrap();
        assert_eq!(
            scan.observations(),
            [JournalObservation::IncompleteCheckpoint {
                sequence: 1,
                revision: 5
            }]
        );
        assert_eq!(scan.checkpoint_sequence(), 1);
        assert_eq!(scan.checkpoint_revision(), 5);
        let sequences: Vec<u64> = scan.batches().iter().map(MutationBatch::sequence).collect();
        assert_eq!(sequences, vec![2]);

        let recovered = store.recover_journal().unwrap();
        assert_eq!(recovered.checkpoint_sequence(), 1);
        assert_eq!(recovered.batches().len(), 1);
        // The resolution is durable, so the next scan has nothing left to
        // reconcile and the batch it already applied is gone.
        let settled = store.scan_journal().unwrap();
        assert!(settled.observations().is_empty());
        assert_eq!(settled.checkpoint_sequence(), 1);
        assert_eq!(settled.batches().len(), 1);
    }

    /// A journal whose history no longer reaches the manifest is reported
    /// instead of returned as a short, healthy-looking scan.
    #[test]
    fn a_journal_behind_the_manifest_is_reported() {
        let bundle = TestBundle::new("short-history");
        let store = bundle.store(4);
        store
            .append_batch(&MutationBatch::new(1, 4, 5, b"one".to_vec()))
            .unwrap();
        store
            .append_batch(&MutationBatch::new(2, 5, 6, b"two".to_vec()))
            .unwrap();
        store.save(&manifest(6)).unwrap();

        // Lose the batch the manifest was saved for, as a torn write-ahead log
        // tail does.
        let deadline = Deadline::new("test");
        let database = JournalDatabase::open(&store.journal_database_path(), &deadline).unwrap();
        database
            .execute(
                "DELETE FROM journal_batch WHERE sequence = 2",
                Vec::new(),
                &deadline,
            )
            .unwrap();
        drop(database);

        assert!(matches!(
            store.scan_journal(),
            Err(StoreError::Journal(JournalError::JournalBehindManifest {
                manifest_revision: 6,
                journal_revision: 5
            }))
        ));
    }

    /// A write-ahead log that does not end on a frame boundary lost its final
    /// transaction. The scan says so instead of returning the shortened
    /// history in silence.
    #[test]
    fn a_torn_write_ahead_log_tail_is_reported() {
        let bundle = TestBundle::new("torn-log");
        let store = bundle.store(0);
        for sequence in 1..=3 {
            store
                .append_batch(&MutationBatch::new(
                    sequence,
                    sequence - 1,
                    sequence,
                    b"payload".to_vec(),
                ))
                .unwrap();
        }

        let log = store.journal_path().join(WRITE_AHEAD_LOG_NAME);
        let bytes = fs::metadata(&log).unwrap().len();
        assert!(
            bytes > LOG_HEADER_BYTES as u64,
            "the log holds committed frames"
        );
        fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .unwrap()
            .set_len(bytes - 7)
            .unwrap();

        let observations = store.scan_journal().unwrap().observations().to_vec();
        assert!(
            observations.iter().any(|observation| matches!(
                observation,
                JournalObservation::TornWriteAheadLog { .. }
            )),
            "expected a torn log observation, got {observations:?}"
        );
        // Recovery rewrites the log from its header, so the damage is reported
        // while it is there and not forever afterwards.
        store.recover_journal().unwrap();
        assert!(store.scan_journal().unwrap().observations().is_empty());
    }

    /// Recovery memory is bounded by bytes, not only by record count: the count
    /// cap alone allows tens of gigabytes into one `Vec`.
    #[test]
    fn unapplied_batches_are_bounded_by_bytes() {
        assert!(
            MAX_UNAPPLIED_JOURNAL_BATCHES.saturating_mul(MAX_JOURNAL_RECORD_BYTES)
                > MAX_UNAPPLIED_JOURNAL_BYTES,
            "the count cap alone must not be the memory bound"
        );
        assert!(enforce_unapplied_bytes(MAX_UNAPPLIED_JOURNAL_BYTES).is_ok());
        assert!(matches!(
            enforce_unapplied_bytes(MAX_UNAPPLIED_JOURNAL_BYTES + 1),
            Err(StoreError::Journal(JournalError::UnappliedByteLimit {
                maximum: MAX_UNAPPLIED_JOURNAL_BYTES,
                ..
            }))
        ));

        // The bound is enforced against a running total the journal keeps, so
        // an append is refused before the journal grows past what recovery can
        // read back.
        let bundle = TestBundle::new("byte-budget");
        let store = bundle.store(0);
        let payload = vec![0xd7_u8; 4096];
        store
            .append_batch(&MutationBatch::new(1, 0, 1, payload.clone()))
            .unwrap();
        store
            .append_batch(&MutationBatch::new(2, 1, 2, payload.clone()))
            .unwrap();

        let deadline = Deadline::new("test");
        let database = JournalDatabase::open(&store.journal_database_path(), &deadline).unwrap();
        let checkpoint = read_checkpoint(&database, &deadline).unwrap().unwrap();
        drop(database);
        assert_eq!(checkpoint.unapplied_bytes, 2 * record_size(&payload));
    }
}
