//! Embedded turso database that backs the durable mutation journal.
//!
//! # Ownership
//!
//! turso takes a non-blocking exclusive `fcntl` lock on `journal.db` and on its
//! write-ahead log inside `Builder::build()`, for reads as much as for writes.
//! The journal is therefore a single-writer, daemon-owned resource: while one
//! process holds it open, every other process is refused. Two rules follow and
//! are enforced here and in [`crate::journal`]:
//!
//! - ordinary project inspection ([`crate::ProjectStore::load`]) never opens
//!   the database, so reading a project is never blocked by a running daemon;
//! - the database is opened lazily, used, and dropped inside a single journal
//!   operation, so the lock window is as short as the work itself.
//!
//! A refused lock is [`JournalError::Locked`], distinct from
//! [`JournalError::Database`], after a small bounded retry.
//!
//! # Deadline
//!
//! Every operation runs under a [`Deadline`]. It bounds what happens *between*
//! polls: lock retries, turso's busy-handler retries, and a completion that
//! never fires. It cannot bound a blocking syscall *inside* a poll —
//! `UnixIO::step` is a no-op and `pread`/`pwrite` block inline in the future —
//! so a disk that stalls in the kernel stalls this thread for as long as the
//! kernel takes. The deadline is a bound on waiting, not a cancellation.

use std::{
    future::Future,
    path::Path,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, Thread},
    time::{Duration, Instant},
};

use turso::{Builder, Connection, Database, Row, Value};

use crate::journal::JournalError;

/// Wall-clock budget for one complete journal database operation.
pub(crate) const JOURNAL_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

/// How long turso may wait for a competing writer before reporting `Busy`.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// `PRAGMA synchronous` value that fsyncs the write-ahead log on every commit.
const SYNCHRONOUS_FULL: i64 = 2;

/// `PRAGMA data_sync_retry` value that reports an fsync failure instead of
/// aborting the process.
const DATA_SYNC_RETRY_ON: i64 = 1;

/// Attempts to acquire the exclusive file lock before reporting
/// [`JournalError::Locked`].
const LOCK_ATTEMPTS: u32 = 4;

/// Pause between lock attempts, so a lock held across a short journal
/// operation in another process is waited out rather than reported.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(75);

/// How long [`block_on`] re-polls a woken future without pausing. turso wakes
/// its waker inline both when local IO completed during the poll and when the
/// busy handler wants a retry, so a longer streak is indistinguishable from a
/// lock fight that would otherwise burn a core.
const SPIN_WINDOW: Duration = Duration::from_millis(1);

/// Pause between polls once [`SPIN_WINDOW`] is exhausted.
const POLL_THROTTLE: Duration = Duration::from_micros(200);

/// Bounds one journal operation, including every turso future it drives.
pub(crate) struct Deadline {
    operation: &'static str,
    expires_at: Instant,
}

impl Deadline {
    pub(crate) fn new(operation: &'static str) -> Self {
        Self {
            operation,
            expires_at: Instant::now() + JOURNAL_OPERATION_TIMEOUT,
        }
    }

    pub(crate) const fn operation(&self) -> &'static str {
        self.operation
    }

    /// Time left in the budget, or `None` once it is spent.
    fn remaining(&self) -> Option<Duration> {
        self.expires_at.checked_duration_since(Instant::now())
    }

    fn expired(&self) -> JournalError {
        JournalError::Deadline {
            operation: self.operation,
            limit: JOURNAL_OPERATION_TIMEOUT,
        }
    }

    /// Classifies a turso failure so callers can distinguish a damaged database
    /// from one that is merely owned by another process.
    fn fault(&self, error: &turso::Error) -> JournalError {
        match error {
            turso::Error::Corrupt(message) | turso::Error::NotAdb(message) => {
                JournalError::CorruptDatabase {
                    operation: self.operation,
                    message: message.clone(),
                }
            }
            turso::Error::Busy(message) | turso::Error::BusySnapshot(message) => {
                JournalError::Locked {
                    operation: self.operation,
                    message: message.clone(),
                }
            }
            turso::Error::Constraint(message) => JournalError::Constraint {
                operation: self.operation,
                message: message.clone(),
            },
            turso::Error::Error(message) if is_locking_failure(message) => JournalError::Locked {
                operation: self.operation,
                message: message.clone(),
            },
            other => JournalError::Database {
                operation: self.operation,
                message: other.to_string(),
            },
        }
    }
}

/// A refused file lock reaches this crate as an untyped `turso::Error::Error`:
/// turso maps every `LimboError` it has no code for through `Display`, and
/// `LimboError::LockingError` renders with this prefix. Matching it here keeps
/// the one unavoidable string comparison in a single place, next to the reason
/// it is unavoidable.
fn is_locking_failure(message: &str) -> bool {
    message.starts_with("Locking error:")
}

/// Wakes the thread parked inside [`block_on`].
struct BlockingWaker {
    thread: Thread,
    woken: AtomicBool,
}

impl Wake for BlockingWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Drives a turso future to completion on the calling thread.
///
/// turso registers the waker on every completion and wakes it — inline when a
/// local read already finished during the poll, from the busy handler when a
/// lock must be retried — so this parks instead of spinning. Because a woken
/// busy handler re-arms with no delay of its own, a streak of immediate wakes
/// longer than [`SPIN_WINDOW`] is throttled: waiting for a lock must not cost a
/// core in the middle of a show.
fn block_on<F: Future>(future: F, deadline: &Deadline) -> Result<F::Output, JournalError> {
    let state = Arc::new(BlockingWaker {
        thread: thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker = Waker::from(Arc::clone(&state));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    let started = Instant::now();
    loop {
        // Arm before polling so a wake raised during the poll is observed.
        state.woken.store(false, Ordering::Release);
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return Ok(value);
        }
        let Some(remaining) = deadline.remaining() else {
            return Err(deadline.expired());
        };
        if state.woken.swap(false, Ordering::Acquire) {
            if started.elapsed() > SPIN_WINDOW {
                thread::sleep(POLL_THROTTLE.min(remaining));
            }
        } else {
            thread::park_timeout(remaining);
        }
    }
}

fn run<T>(
    future: impl Future<Output = turso::Result<T>>,
    deadline: &Deadline,
) -> Result<T, JournalError> {
    block_on(future, deadline)?.map_err(|error| deadline.fault(&error))
}

/// An open connection to a project's journal database.
///
/// Held only for the duration of one journal operation: dropping it closes the
/// files and releases the exclusive lock.
pub(crate) struct JournalDatabase {
    connection: Connection,
    _database: Database,
}

impl JournalDatabase {
    /// Opens the journal database, replaying its write-ahead log so the
    /// connection observes the last committed state.
    ///
    /// turso creates the file when it is absent; callers that must not create
    /// one check for it first. A lock held by another process is retried a few
    /// times and then reported as [`JournalError::Locked`].
    pub(crate) fn open(path: &Path, deadline: &Deadline) -> Result<Self, JournalError> {
        let location = path
            .to_str()
            .ok_or_else(|| JournalError::UnsupportedDatabasePath(path.to_path_buf()))?;
        let mut attempt = 1;
        loop {
            let error = match Self::connect(location, deadline) {
                Ok(database) => return Ok(database),
                Err(error) => error,
            };
            let retry = matches!(error, JournalError::Locked { .. })
                && attempt < LOCK_ATTEMPTS
                && deadline
                    .remaining()
                    .is_some_and(|remaining| remaining > LOCK_RETRY_DELAY);
            if !retry {
                return Err(error);
            }
            attempt += 1;
            thread::sleep(LOCK_RETRY_DELAY);
        }
    }

    fn connect(location: &str, deadline: &Deadline) -> Result<Self, JournalError> {
        let database = run(Builder::new_local(location).build(), deadline)?;
        let connection = database.connect().map_err(|error| deadline.fault(&error))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| deadline.fault(&error))?;
        let opened = Self {
            connection,
            _database: database,
        };
        // A commit must reach stable storage before it is acknowledged, and a
        // failed fsync must come back as an error. turso panics the process on
        // an fsync failure while `data_sync_retry` is off, which would abort a
        // running show. Confirm both settings rather than assuming them.
        opened.set_pragma("synchronous", "FULL", SYNCHRONOUS_FULL, deadline)?;
        opened.set_pragma("data_sync_retry", "ON", DATA_SYNC_RETRY_ON, deadline)?;
        Ok(opened)
    }

    /// Applies a pragma and reads it back, refusing the database when the
    /// engine did not honour it.
    fn set_pragma(
        &self,
        pragma: &'static str,
        value: &str,
        expected: i64,
        deadline: &Deadline,
    ) -> Result<(), JournalError> {
        run(self.connection.pragma_update(pragma, value), deadline)?;
        let reported = self
            .query(&format!("PRAGMA {pragma}"), Vec::new(), deadline, |row| {
                column_integer(row, 0)
            })?
            .first()
            .copied();
        if reported == Some(expected) {
            Ok(())
        } else {
            Err(JournalError::DurabilityUnavailable { pragma, reported })
        }
    }

    pub(crate) fn execute_batch(&self, sql: &str, deadline: &Deadline) -> Result<(), JournalError> {
        run(self.connection.execute_batch(sql), deadline)
    }

    pub(crate) fn execute(
        &self,
        sql: &str,
        parameters: Vec<Value>,
        deadline: &Deadline,
    ) -> Result<u64, JournalError> {
        run(self.connection.execute(sql, parameters), deadline)
    }

    /// Executes a prepared statement, reusing the connection's statement cache.
    pub(crate) fn execute_prepared(
        &self,
        sql: &str,
        parameters: Vec<Value>,
        deadline: &Deadline,
    ) -> Result<u64, JournalError> {
        let mut statement = run(self.connection.prepare_cached(sql), deadline)?;
        run(statement.execute(parameters), deadline)
    }

    /// Runs `sql` and maps every row. The row set is always driven to
    /// completion or abandoned with an error, never left half-read.
    pub(crate) fn query<T>(
        &self,
        sql: &str,
        parameters: Vec<Value>,
        deadline: &Deadline,
        mut map: impl FnMut(&Row) -> Result<T, JournalError>,
    ) -> Result<Vec<T>, JournalError> {
        let mut statement = run(self.connection.prepare_cached(sql), deadline)?;
        let mut rows = run(statement.query(parameters), deadline)?;
        let mut mapped = Vec::new();
        while let Some(row) = run(rows.next(), deadline)? {
            mapped.push(map(&row)?);
        }
        Ok(mapped)
    }
}

/// An explicit write transaction that rolls back unless it is committed.
pub(crate) struct Transaction<'database> {
    database: &'database JournalDatabase,
    open: bool,
}

impl<'database> Transaction<'database> {
    pub(crate) fn begin(
        database: &'database JournalDatabase,
        deadline: &Deadline,
    ) -> Result<Self, JournalError> {
        database.execute("BEGIN IMMEDIATE", Vec::new(), deadline)?;
        Ok(Self {
            database,
            open: true,
        })
    }

    pub(crate) fn commit(mut self, deadline: &Deadline) -> Result<(), JournalError> {
        self.database.execute("COMMIT", Vec::new(), deadline)?;
        self.open = false;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if self.open {
            let _ = self.database.execute(
                "ROLLBACK",
                Vec::new(),
                &Deadline::new("journal transaction rollback"),
            );
        }
    }
}

/// Reads a non-null integer column, rejecting any other stored type.
pub(crate) fn column_integer(row: &Row, index: usize) -> Result<i64, JournalError> {
    match row.get_value(index) {
        Ok(Value::Integer(value)) => Ok(value),
        _ => Err(JournalError::MalformedColumn { index }),
    }
}

/// Reads a non-null blob column, rejecting any other stored type.
pub(crate) fn column_blob(row: &Row, index: usize) -> Result<Vec<u8>, JournalError> {
    match row.get_value(index) {
        Ok(Value::Blob(value)) => Ok(value),
        _ => Err(JournalError::MalformedColumn { index }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused file lock must be [`JournalError::Locked`], not the generic
    /// database error a caller cannot act on.
    #[test]
    fn a_refused_file_lock_is_typed_as_locked() {
        let deadline = Deadline::new("test");
        let locked = deadline.fault(&turso::Error::Error(
            "Locking error: Failed locking file '/show.freemix/journal/journal.db'. \
             File is locked by another process"
                .to_owned(),
        ));
        assert!(matches!(locked, JournalError::Locked { .. }));
        assert!(matches!(
            deadline.fault(&turso::Error::Busy("database is locked".to_owned())),
            JournalError::Locked { .. }
        ));
        assert!(matches!(
            deadline.fault(&turso::Error::Error(
                "no such table: journal_batch".to_owned()
            )),
            JournalError::Database { .. }
        ));
    }
}
