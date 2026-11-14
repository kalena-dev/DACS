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
        db.busy_timeout(std::time::Duration::from_secs(5))?;
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
            );
            CREATE TABLE IF NOT EXISTS values_store(key TEXT PRIMARY KEY,body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS outbound_receipts(id TEXT PRIMARY KEY,body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sequences(attempt TEXT PRIMARY KEY,value INTEGER NOT NULL);")?;
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
        if !command.task_id.is_empty()
            && latest.is_some_and(|generation| command.generation < generation)
        {
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
        let mut db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let body = serde_json::to_string(result)?;
        let prior: Option<String> = tx
            .query_row(
                "SELECT body FROM results WHERE result_id=?1",
                [&result.result_id],
                |r| r.get(0),
            )
            .optional()?;
        if prior.as_ref().is_some_and(|old| old != &body) {
            return Err(JournalError::DuplicateMismatch);
        }
        tx.execute("INSERT OR IGNORE INTO results(result_id,task_id,attempt_id,generation,sequence,body,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![result.result_id, result.task_id, result.attempt_id, result.generation, result.sequence, body, now_ms()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_results(&self, limit: usize) -> Result<Vec<PendingResult>, JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let mut stmt =
            db.prepare("SELECT body FROM results WHERE state='pending' ORDER BY rowid LIMIT ?1")?;
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

    pub fn get_value<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let value: Option<String> = db
            .query_row("SELECT body FROM values_store WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        value
            .map(|body| serde_json::from_str(&body).map_err(Into::into))
            .transpose()
    }

    pub fn set_value<T: Serialize>(&self, key: &str, value: &T) -> Result<(), JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        db.execute("INSERT INTO values_store(key,body) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body", params![key, serde_json::to_string(value)?])?;
        Ok(())
    }

    pub fn set_values(&self, values: &[(&str, serde_json::Value)]) -> Result<(), JournalError> {
        let mut db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, value) in values {
            tx.execute("INSERT INTO values_store(key,body) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body",params![key,value.to_string()])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn receipt(&self, id: &str) -> Result<Option<serde_json::Value>, JournalError> {
        let db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let value: Option<String> = db
            .query_row(
                "SELECT body FROM outbound_receipts WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        value
            .map(|body| serde_json::from_str(&body).map_err(Into::into))
            .transpose()
    }

    pub fn complete_result(
        &self,
        id: &str,
        response: &serde_json::Value,
    ) -> Result<(), JournalError> {
        let mut db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO outbound_receipts(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![id, response.to_string()])?;
        tx.execute(
            "UPDATE results SET state='acknowledged' WHERE result_id=?1",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Sequence allocation and enqueue are one transaction: a crash cannot leave
    /// an allocated event missing from the ordered outbox.
    pub fn event(
        &self,
        task: &str,
        attempt: &str,
        generation: u64,
        minimum: u64,
        event: dacs_protocol::WorkerEvent,
    ) -> Result<String, JournalError> {
        self.event_checkpoint(task, attempt, generation, minimum, event, None)
    }

    pub fn event_checkpoint(
        &self,
        task: &str,
        attempt: &str,
        generation: u64,
        minimum: u64,
        event: dacs_protocol::WorkerEvent,
        checkpoint: Option<(&str, serde_json::Value)>,
    ) -> Result<String, JournalError> {
        let mut db = self.db.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: u64 = tx
            .query_row(
                "SELECT value FROM sequences WHERE attempt=?1",
                [attempt],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let sequence = prior.max(minimum) + 1;
        let id = uuid::Uuid::new_v4().to_string();
        let envelope = dacs_protocol::CommandEnvelope {
            protocol_version: dacs_protocol::PROTOCOL_VERSION,
            command_id: id.clone(),
            command: dacs_protocol::Command::ReportEvent {
                attempt_id: attempt.into(),
                generation,
                sequence,
                event,
            },
        };
        let result = PendingResult {
            result_id: id.clone(),
            task_id: task.into(),
            attempt_id: attempt.into(),
            generation,
            sequence,
            payload: serde_json::to_value(envelope)?,
        };
        tx.execute("INSERT INTO sequences(attempt,value) VALUES(?1,?2) ON CONFLICT(attempt) DO UPDATE SET value=excluded.value",params![attempt,sequence])?;
        tx.execute("INSERT INTO results(result_id,task_id,attempt_id,generation,sequence,body,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id,task,attempt,generation,sequence,serde_json::to_string(&result)?,now_ms()])?;
        if let Some((key, value)) = checkpoint {
            tx.execute("INSERT INTO values_store(key,body) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body",params![key,value.to_string()])?;
        }
        tx.commit()?;
        Ok(id)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(id: &str, task: &str, generation: u64) -> PendingCommand {
        PendingCommand {
            command_id: id.into(),
            task_id: task.into(),
            attempt_id: format!("attempt-{generation}"),
            generation,
            payload: json!({"pin":"source"}),
        }
    }

    #[test]
    fn receipts_reject_changed_duplicates_and_stale_generations() {
        let journal = Journal::in_memory().unwrap();
        let command = command("first", "task", 3);
        assert!(journal.receive(&command).unwrap());
        assert!(!journal.receive(&command).unwrap());
        let mut changed = command.clone();
        changed.payload = json!({"pin":"different"});
        assert!(matches!(
            journal.receive(&changed),
            Err(JournalError::DuplicateMismatch)
        ));
        let mut stale = command.clone();
        stale.command_id = "stale".into();
        stale.generation = 2;
        assert!(matches!(
            journal.receive(&stale),
            Err(JournalError::StaleGeneration)
        ));
    }

    #[test]
    fn mailbox_generations_do_not_fence_unrelated_tasks() {
        let journal = Journal::in_memory().unwrap();
        assert!(journal.receive(&command("mail-high", "", 12)).unwrap());
        assert!(journal.receive(&command("mail-low", "", 1)).unwrap());
    }

    #[test]
    fn event_order_and_checkpoint_survive_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.sqlite");
        {
            let journal = Journal::open(&path).unwrap();
            journal
                .event_checkpoint(
                    "t",
                    "a",
                    2,
                    4,
                    dacs_protocol::WorkerEvent::Started,
                    Some(("offset", json!(123))),
                )
                .unwrap();
            journal
                .event("t", "a", 2, 0, dacs_protocol::WorkerEvent::Stopped)
                .unwrap();
        }
        let journal = Journal::open(&path).unwrap();
        let results = journal.pending_results(10).unwrap();
        assert_eq!(
            results.iter().map(|r| r.sequence).collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(journal.get_value::<u64>("offset").unwrap(), Some(123));
        let mut changed = results[0].clone();
        changed.payload = json!({"different":true});
        assert!(matches!(
            journal.append_result(&changed),
            Err(JournalError::DuplicateMismatch)
        ));
        journal
            .complete_result(&results[0].result_id, &json!({"ok":true}))
            .unwrap();
        assert_eq!(journal.pending_results(10).unwrap().len(), 1);
        assert_eq!(
            journal.receipt(&results[0].result_id).unwrap().unwrap()["ok"],
            true
        );
    }
}
