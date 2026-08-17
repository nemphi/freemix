//! Where an accepted command becomes durable.
//!
//! The daemon appends every accepted command to the bundle's mutation journal
//! and waits for that append to be durable *before* the acceptance reaches the
//! client and before any runtime realization is published. The manifest is no
//! longer rewritten per command; it is rewritten at a checkpoint, which folds
//! the recorded mutations into it and compacts the journal.

use std::cell::Cell;

use fm_persistence::{MutationBatch, ProjectStore, StoredProject};
use fm_protocol::{CommandMessage, WireMessage, decode_line, encode_line};

use crate::{AppFailure, AppResult};

/// Mutation batches allowed between two manifest checkpoints.
///
/// A count, not an interval or an idle frame: the count is the quantity the
/// journal itself bounds (`MAX_UNAPPLIED_JOURNAL_BATCHES`,
/// `MAX_UNAPPLIED_JOURNAL_BYTES`) and the quantity crash recovery pays for, so
/// bounding it here is the same bound stated once at the point that controls
/// it. It also needs no timer thread and reads no wall clock, so a checkpoint
/// lands at the same place in a show as in a test. Sixty-four commands is far
/// below both journal limits — each command is at most one 64 KiB protocol
/// line — while amortising the manifest rewrite 64:1 during a show.
const MAX_BATCHES_PER_CHECKPOINT: u64 = 64;

/// Bytes of submission time that precede the encoded command in a payload.
const SUBMITTED_AT_BYTES: usize = 8;

/// Makes an accepted command durable.
pub(crate) trait DurableStore {
    /// Records the mutation from `previous` to `updated`.
    ///
    /// Returns only once that mutation will survive loss of power. Everything
    /// that can fail happens before that point, so a caller that sees an error
    /// must refuse the command rather than acknowledge it.
    fn record(
        &self,
        command: &CommandMessage,
        now_millis: u64,
        previous: &StoredProject,
        updated: &StoredProject,
    ) -> AppResult<()>;

    /// Folds every recorded mutation into the manifest and compacts the
    /// journal. `project` must hold the revision of the newest recorded
    /// mutation, which is what the journal checkpoint is validated against.
    fn checkpoint(&self, project: &StoredProject) -> AppResult<()>;
}

/// The live journal of a served project.
///
/// The daemon is the journal's only writer, so it can track the head sequence
/// instead of re-reading it: `head` is the sequence the journal was recovered
/// to at startup plus everything appended since.
pub(crate) struct DurableJournal<'store> {
    store: &'store ProjectStore,
    head: Cell<u64>,
    pending: Cell<u64>,
}

impl<'store> DurableJournal<'store> {
    /// `head` is the sequence startup recovery left the journal checkpointed
    /// at, so the manifest and the checkpoint agree when the first command
    /// arrives.
    pub(crate) const fn new(store: &'store ProjectStore, head: u64) -> Self {
        Self {
            store,
            head: Cell::new(head),
            pending: Cell::new(0),
        }
    }

    /// Folds anything recorded since the last checkpoint into the manifest.
    ///
    /// Nothing recorded means the manifest already holds `project`, so this
    /// does not rewrite it.
    pub(crate) fn settle(&self, project: &StoredProject) -> AppResult<()> {
        if self.pending.get() == 0 {
            return Ok(());
        }
        self.checkpoint(project)
    }
}

impl DurableStore for DurableJournal<'_> {
    fn record(
        &self,
        command: &CommandMessage,
        now_millis: u64,
        previous: &StoredProject,
        updated: &StoredProject,
    ) -> AppResult<()> {
        let Some(revision) = single_revision_step(previous, updated) else {
            // The command advanced no revision, so there is no mutation batch
            // to append: a refused command records only its receipt, and a
            // journal batch must move the revision by exactly one. The manifest
            // is the only place that receipt can become durable.
            return self.checkpoint(updated);
        };
        // Checkpoint *before* the append, never after. Everything that can fail
        // then fails while the command is still refusable; a checkpoint after a
        // durable append would have to choose between reporting its failure and
        // telling the truth about a command that already survives a crash.
        if self.pending.get() >= MAX_BATCHES_PER_CHECKPOINT {
            self.checkpoint(previous)?;
        }
        let sequence = self
            .head
            .get()
            .checked_add(1)
            .ok_or_else(|| AppFailure("journal sequence overflow".into()))?;
        self.store.append_batch(&MutationBatch::new(
            sequence,
            previous.position().revision,
            revision,
            encode_mutation(command, now_millis)?,
        ))?;
        self.head.set(sequence);
        self.pending.set(self.pending.get().saturating_add(1));
        Ok(())
    }

    fn checkpoint(&self, project: &StoredProject) -> AppResult<()> {
        // `checkpoint_and_compact` saves the manifest, then advances the
        // checkpoint and discards the applied batches in one transaction, and
        // refuses a manifest whose revision is not the one it is checkpointing.
        self.store
            .checkpoint_and_compact(project, self.head.get())?;
        self.pending.set(0);
        Ok(())
    }
}

/// Replay reruns commands that are already durable, so it records nothing.
pub(crate) struct ReplayedMutations;

impl DurableStore for ReplayedMutations {
    fn record(
        &self,
        _command: &CommandMessage,
        _now_millis: u64,
        _previous: &StoredProject,
        _updated: &StoredProject,
    ) -> AppResult<()> {
        Ok(())
    }

    fn checkpoint(&self, _project: &StoredProject) -> AppResult<()> {
        Ok(())
    }
}

/// The revision a single journal batch would carry, if this is one.
///
/// The journal defines a batch as exactly one revision step. A command that
/// moved the revision by anything else — a refusal, which moves it by none —
/// has no batch to append.
fn single_revision_step(previous: &StoredProject, updated: &StoredProject) -> Option<u64> {
    let next = previous.position().revision.checked_add(1)?;
    (updated.position().revision == next).then_some(next)
}

/// Encodes the accepted command and the instant it was submitted.
///
/// The submission time is part of the mutation, not context: it decides
/// deadline and receipt outcomes, so replaying the command without it would
/// not reproduce the state the operator was told about.
fn encode_mutation(command: &CommandMessage, now_millis: u64) -> AppResult<Vec<u8>> {
    let line = encode_line(&WireMessage::Command(command.clone()))?;
    let mut payload = Vec::with_capacity(SUBMITTED_AT_BYTES + line.len());
    payload.extend_from_slice(&now_millis.to_le_bytes());
    payload.extend_from_slice(line.as_bytes());
    Ok(payload)
}

/// Decodes what [`encode_mutation`] wrote.
///
/// # Errors
///
/// Returns an error for a payload this daemon did not write, which is damage
/// rather than history and is never applied.
pub(crate) fn decode_mutation(payload: &[u8]) -> AppResult<(CommandMessage, u64)> {
    let (submitted_at, line) = payload
        .split_at_checked(SUBMITTED_AT_BYTES)
        .ok_or_else(|| {
            AppFailure(format!(
                "journal mutation is {} bytes, shorter than its {SUBMITTED_AT_BYTES}-byte header",
                payload.len()
            ))
        })?;
    let now_millis = u64::from_le_bytes(
        submitted_at
            .try_into()
            .expect("the payload was split at eight bytes"),
    );
    let line = std::str::from_utf8(line)
        .map_err(|error| AppFailure(format!("journal mutation is not valid UTF-8: {error}")))?;
    match decode_line(line)? {
        WireMessage::Command(command) => Ok((command, now_millis)),
        _ => Err(AppFailure("journal mutation does not hold a command".into()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use fm_protocol::{CURRENT_PROTOCOL_VERSION, CommandPayload};

    /// The recorded mutation must come back exactly, including the submission
    /// time: replay that guessed the time would not reproduce deadline and
    /// receipt outcomes.
    #[test]
    fn a_recorded_mutation_round_trips_with_its_submission_time() {
        let command = CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            id: "round-trip".into(),
            idempotency_key: "round-trip-key".into(),
            expected_revision: Some(7),
            deadline_ms: Some(1_500),
            payload: CommandPayload::Fade { duration_frames: 4 },
        };

        let payload = encode_mutation(&command, 1_700_000_000_123).unwrap();
        assert_eq!(
            decode_mutation(&payload).unwrap(),
            (command, 1_700_000_000_123)
        );
        assert!(decode_mutation(&payload[..4]).is_err());
        assert!(decode_mutation(&payload[..SUBMITTED_AT_BYTES]).is_err());
    }
}
