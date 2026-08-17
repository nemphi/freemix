//! Embedded turso database that backs the durable mutation journal.
//!
//! turso exposes an asynchronous API, but a local database performs its IO
//! inline: the future does the work inside `poll` and then reports
//! [`Poll::Pending`] without arranging a wake-up. A parking executor therefore
//! deadlocks, so every future is driven by [`block_on`], a polling loop that is
//! bounded by an explicit [`Deadline`]. A future that stops making progress
//! becomes a typed error instead of hanging a live show.

use std::{
    future::Future,
    path::Path,
    task::{Context, Poll, Waker},
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

    fn exceeded(&self) -> Option<JournalError> {
        (Instant::now() >= self.expires_at).then_some(JournalError::Deadline {
            operation: self.operation,
            limit: JOURNAL_OPERATION_TIMEOUT,
        })
    }

    /// Classifies a turso failure so callers can distinguish a damaged database
    /// from a transient one.
    fn fault(&self, error: &turso::Error) -> JournalError {
        match error {
            turso::Error::Corrupt(message) | turso::Error::NotAdb(message) => {
                JournalError::CorruptDatabase {
                    operation: self.operation,
                    message: message.clone(),
                }
            }
            other => JournalError::Database {
                operation: self.operation,
                message: other.to_string(),
            },
        }
    }
}

/// Drives a local turso future to completion on the calling thread.
///
/// A no-op waker is correct here precisely because turso never schedules one
/// for a local database: progress happens during `poll`, so the loop must
/// re-poll rather than park. The deadline turns a future that stops making
/// progress into an error instead of an unbounded spin.
fn block_on<F: Future>(future: F, deadline: &Deadline) -> Result<F::Output, JournalError> {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return Ok(value),
            Poll::Pending => {
                if let Some(error) = deadline.exceeded() {
                    return Err(error);
                }
                std::thread::yield_now();
            }
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
pub(crate) struct JournalDatabase {
    connection: Connection,
    _database: Database,
}

impl JournalDatabase {
    /// Opens (creating if absent) the journal database and replays its
    /// write-ahead log, so the connection observes the last committed state.
    pub(crate) fn open(path: &Path, deadline: &Deadline) -> Result<Self, JournalError> {
        let location = path
            .to_str()
            .ok_or_else(|| JournalError::UnsupportedDatabasePath(path.to_path_buf()))?;
        let database = run(Builder::new_local(location).build(), deadline)?;
        let connection = database.connect().map_err(|error| deadline.fault(&error))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| deadline.fault(&error))?;
        let opened = Self {
            connection,
            _database: database,
        };
        // A commit must reach stable storage before it is acknowledged. Confirm
        // the engine honoured that rather than assuming it: a journal that
        // silently ran at a weaker sync level would lose a live show's work.
        run(
            opened.connection.pragma_update("synchronous", "FULL"),
            deadline,
        )?;
        let reported = opened
            .query("PRAGMA synchronous", Vec::new(), deadline, |row| {
                column_integer(row, 0)
            })?
            .first()
            .copied();
        if reported != Some(SYNCHRONOUS_FULL) {
            return Err(JournalError::DurabilityUnavailable { reported });
        }
        Ok(opened)
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
