//! What it costs to keep a journal, over a whole show rather than one command.
//!
//! The journal was correct and unbounded: compaction deleted rows and left
//! every page they had been written to in the write-ahead log, and every append
//! opened the database again and paid to rebuild that log's index. Both are
//! invisible in a test that records a handful of batches and both decide what a
//! cut costs at command five thousand.

use std::{
    fs,
    num::NonZeroU128,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use fm_model::{Input, InputKind, MainMix, Project, ProjectSettings};
use fm_persistence::{
    JournalError, MutationBatch, ProjectPosition, ProjectStore, RuntimeRouting, StoreError,
    StoredProject,
};
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, PixelFormat, ProjectId,
    SampleFormat, SampleRate, ScanMode, VideoDimensions, VideoFormat,
};

const WRITE_AHEAD_LOG_NAME: &str = "journal.db-wal";

/// Commands between two manifest checkpoints in the daemon.
const BATCHES_PER_CHECKPOINT: u64 = 64;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestBundle(PathBuf);

impl TestBundle {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fm-journal-wal-{}-{}-{name}.freemix",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }

    fn store(&self) -> ProjectStore {
        let store = ProjectStore::new(self.0.clone()).unwrap();
        store.save(&manifest(0)).unwrap();
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
    let program = InputId::new(NonZeroU128::new(1).unwrap());
    let preview = InputId::new(NonZeroU128::new(2).unwrap());
    let mut domain = Project::new(
        ProjectId::new(NonZeroU128::new(7).unwrap()),
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

/// A batch whose revision is its sequence, so the manifest that checkpoints
/// through sequence `n` is simply the manifest at revision `n`.
fn batch(sequence: u64) -> MutationBatch {
    MutationBatch::new(sequence, sequence - 1, sequence, vec![0x5a; 400])
}

fn write_ahead_log_bytes(store: &ProjectStore) -> u64 {
    fs::metadata(store.journal_path().join(WRITE_AHEAD_LOG_NAME)).map_or(0, |file| file.len())
}

/// The write-ahead log must not be the one part of a bundle that only grows.
///
/// Compaction removed rows and never truncated the log they were written to, so
/// a bundle reused across shows carried every page of every command it had ever
/// served: the log reached megabytes while the database stayed at one page, it
/// survived a clean stop, and every open of it paid to rebuild its index. This
/// records and checkpoints far more than a show's worth of commands and demands
/// that the log end each cycle exactly where it ended the first one.
#[test]
fn the_write_ahead_log_stays_flat_across_many_checkpoints() {
    let bundle = TestBundle::new("flat-log");
    let store = bundle.store();
    let writer = store.open_journal_writer().unwrap();

    let mut sequence = 0;
    let mut settled = Vec::new();
    let mut peak = 0;
    for _ in 0..12 {
        for _ in 0..BATCHES_PER_CHECKPOINT {
            sequence += 1;
            writer.append_batch(&batch(sequence)).unwrap();
        }
        peak = peak.max(write_ahead_log_bytes(&store));
        writer
            .checkpoint_and_compact(&manifest(sequence), sequence)
            .unwrap();
        settled.push(write_ahead_log_bytes(&store));
    }

    assert_eq!(sequence, 768, "the run is longer than a show's worth");
    assert!(
        settled.iter().all(|bytes| *bytes == settled[0]),
        "the log must end every checkpoint where it ended the first one, got {settled:?}"
    );
    // The log is bounded by what one checkpoint interval writes, not by the
    // history behind it: 768 records of 400 bytes cannot fit in this.
    assert!(
        peak < 1024 * 1024,
        "the log peaked at {peak} bytes between checkpoints"
    );
    // Dropping the writer must not leave the growth behind either: the log
    // survived a clean SIGTERM before, larger than it started.
    drop(writer);
    assert_eq!(write_ahead_log_bytes(&store), settled[0]);
    assert_eq!(store.load().unwrap().position().revision, sequence);
    assert!(store.scan_journal().unwrap().batches().is_empty());
}

/// An append must go through the handle the writer already holds, not reopen
/// the database by path.
///
/// Every append used to open a fresh database — a stat, a lock, a write-ahead
/// log index rebuild, two pragma round trips and two schema queries — before
/// the transaction that was the actual work. The observable difference is that
/// a writer no longer needs the database to be reachable by name: with the file
/// renamed out from under it, the one-shot path cannot find it and the writer
/// carries on, because it never looks.
#[test]
fn the_held_writer_appends_without_reopening_the_database() {
    let bundle = TestBundle::new("no-reopen");
    let store = bundle.store();
    let writer = store.open_journal_writer().unwrap();
    writer.append_batch(&batch(1)).unwrap();

    let database = store.journal_database_path();
    let renamed = database.with_extension("db.renamed");
    fs::rename(&database, &renamed).unwrap();

    // The one-shot path resolves the database by name every time, so it now
    // finds a surviving write-ahead log with no database beside it.
    assert!(
        matches!(
            store.append_batch(&batch(2)),
            Err(StoreError::Journal(JournalError::MissingDatabase { .. }))
        ),
        "the one-shot path must reopen by path, or this proves nothing"
    );
    // The writer does not, so it is unaffected.
    writer.append_batch(&batch(2)).unwrap();
    writer.append_batch(&batch(3)).unwrap();

    fs::rename(&renamed, &database).unwrap();
    drop(writer);

    let scan = store.scan_journal().unwrap();
    let sequences: Vec<u64> = scan.batches().iter().map(MutationBatch::sequence).collect();
    assert_eq!(sequences, vec![1, 2, 3]);
    assert!(scan.observations().is_empty());
}
