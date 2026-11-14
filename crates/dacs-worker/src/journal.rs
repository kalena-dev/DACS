use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("message id was already received with a different command body")]
    DuplicateMismatch,
    #[error("attempt generation is stale")]
    StaleGeneration,
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("journal mutex was poisoned")]
    Poisoned,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCommand {
    pub command_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub generation: u64,
    /// Full assignment/command including source, policy and contract pins.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingResult {
    pub result_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub payload: serde_json::Value,
}

/// SQLite-backed command inbox, generation fence and result outbox.
pub struct Journal {
    db: Mutex<Connection>,
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, JournalError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(db: Connection) -> Result<Self, JournalError> {
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS commands(
                command_id TEXT PRIMARY KEY, body TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'received', received_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS attempts(
                task_id TEXT NOT NULL, attempt_id TEXT NOT NULL, generation INTEGER NOT NULL,
                state TEXT NOT NULL, updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY(task_id, attempt_id)
            );
            CREATE TABLE IF NOT EXISTS results(
                result_id TEXT PRIMARY KEY, task_id TEXT NOT NULL, attempt_id TEXT NOT NULL,
                generation INTEGER NOT NULL, sequence INTEGER NOT NULL, body TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'pending', created_at_ms INTEGER NOT NULL
            );")?;
        Ok(Self { db: Mutex::new(db) })
    }

    /// Durably receive a command before acknowledging it. Duplicate ids must have
    /// byte-equivalent canonical JSON bodies; a mismatch is rejected.
    pub fn receive(&self, command: &PendingCommand) -> Result<bool, JournalError> {
        let body = serde_json::to_string(command)?;
        let mut db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = tx
            .query_row(
                "SELECT body FROM commands WHERE command_id=?1",
                [&command.command_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            if existing != body {
                return Err(JournalError::DuplicateMismatch);
            }
            tx.commit()?;
            return Ok(false);
        }
        let latest: Option<u64> = tx
            .query_row(
                "SELECT MAX(generation) FROM attempts WHERE task_id=?1",
                [&command.task_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if latest.is_some_and(|generation| command.generation < generation) {
            return Err(JournalError::StaleGeneration);
        }
        tx.execute("INSERT INTO commands(command_id,body,state,received_at_ms) VALUES(?1,?2,'received',?3)", params![command.command_id, body, now_ms()])?;
        tx.execute("INSERT INTO attempts(task_id,attempt_id,generation,state,updated_at_ms) VALUES(?1,?2,?3,'admitted',?4)
            ON CONFLICT(task_id,attempt_id) DO UPDATE SET generation=excluded.generation,state='admitted',updated_at_ms=excluded.updated_at_ms",
            params![command.task_id, command.attempt_id, command.generation, now_ms()])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn mark_command(&self, command_id: &str, state: &str) -> Result<(), JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        db.execute(
            "UPDATE commands SET state=?2 WHERE command_id=?1",
            params![command_id, state],
        )?;
        Ok(())
    }

    /// Append an event/result to the durable outbox before attempting transport.
    pub fn append_result(&self, result: &PendingResult) -> Result<(), JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let body = serde_json::to_string(result)?;
        db.execute("INSERT OR IGNORE INTO results(result_id,task_id,attempt_id,generation,sequence,body,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![result.result_id, result.task_id, result.attempt_id, result.generation, result.sequence, body, now_ms()])?;
        Ok(())
    }

    pub fn pending_results(&self, limit: usize) -> Result<Vec<PendingResult>, JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let mut stmt = db.prepare("SELECT body FROM results WHERE state='pending' ORDER BY created_at_ms,result_id LIMIT ?1")?;
        let rows = stmt.query_map([limit as u64], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn acknowledge_result(&self, result_id: &str) -> Result<(), JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        db.execute(
            "UPDATE results SET state='acknowledged' WHERE result_id=?1",
            [result_id],
        )?;
        Ok(())
    }

    /// Rehydrate admitted commands after a process restart. Runtime reconciliation
    /// decides whether a process is still alive before marking it running.
    pub fn pending_commands(&self) -> Result<Vec<PendingCommand>, JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let mut stmt = db.prepare("SELECT body FROM commands WHERE state IN ('received','admitted','running') ORDER BY received_at_ms")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
