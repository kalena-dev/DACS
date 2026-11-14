//! `Journal::open_read_only` must never create or modify anything in the
//! directory it reads, and must explain a missing or uninitialised journal.

use dacs_worker::{Journal, JournalError};
use std::{collections::BTreeSet, path::Path};

fn entries(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn unavailable(result: Result<Journal, JournalError>) -> String {
    match result {
        Err(JournalError::Unavailable(message)) => message,
        Err(other) => panic!("expected Unavailable, got {other}"),
        Ok(_) => panic!("expected Unavailable, got a journal"),
    }
}

#[test]
fn missing_directory_is_reported_and_not_created() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("absent");
    let message = unavailable(Journal::open_read_only(dir.join("worker.sqlite")));
    assert!(message.contains("does not exist"), "{message}");
    assert!(message.contains("absent"), "{message}");
    assert!(!dir.exists());
    assert!(entries(root.path()).is_empty());
}

#[test]
fn empty_directory_is_reported_and_left_empty() {
    let dir = tempfile::tempdir().unwrap();
    let message = unavailable(Journal::open_read_only(dir.path().join("worker.sqlite")));
    assert!(message.contains("no worker state"), "{message}");
    assert!(entries(dir.path()).is_empty());
}

#[test]
fn uninitialised_file_is_reported_and_left_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.sqlite");
    std::fs::write(&path, b"").unwrap();
    let message = unavailable(Journal::open_read_only(&path));
    assert!(message.contains("not an initialised"), "{message}");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(
        entries(dir.path()),
        BTreeSet::from(["worker.sqlite".into()])
    );
}

#[test]
fn reads_a_closed_journal_without_writing_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.sqlite");
    {
        let journal = Journal::open(&path).unwrap();
        journal.set_value("local_status", &"seen").unwrap();
    }
    let before = entries(dir.path());
    let bytes = std::fs::read(&path).unwrap();
    {
        let journal = Journal::open_read_only(&path).unwrap();
        assert_eq!(
            journal.get_value::<String>("local_status").unwrap(),
            Some("seen".into())
        );
        assert_eq!(journal.get_value::<String>("absent").unwrap(), None);
        assert!(journal.set_value("local_status", &"changed").is_err());
    }
    assert_eq!(entries(dir.path()), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn sees_values_a_live_writer_has_not_checkpointed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.sqlite");
    let writer = Journal::open(&path).unwrap();
    writer.set_value("local_status", &1_u64).unwrap();
    {
        let reader = Journal::open_read_only(&path).unwrap();
        assert_eq!(reader.get_value::<u64>("local_status").unwrap(), Some(1));
        writer.set_value("local_status", &2_u64).unwrap();
        assert_eq!(reader.get_value::<u64>("local_status").unwrap(), Some(2));
        assert!(reader.set_value("local_status", &3_u64).is_err());
    }
    // The reader leaves the writer's WAL in place and the writer keeps working.
    writer.set_value("local_status", &4_u64).unwrap();
    assert_eq!(writer.get_value::<u64>("local_status").unwrap(), Some(4));
}

#[test]
fn writer_started_during_a_read_keeps_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.sqlite");
    drop(Journal::open(&path).unwrap());
    let reader = Journal::open_read_only(&path).unwrap();
    let writer = Journal::open(&path).unwrap();
    writer.set_value("local_status", &7_u64).unwrap();
    assert_eq!(reader.get_value::<u64>("local_status").unwrap(), Some(7));
    drop(reader);
    assert_eq!(writer.get_value::<u64>("local_status").unwrap(), Some(7));
    drop(writer);
    let reopened = Journal::open_read_only(&path).unwrap();
    assert_eq!(reopened.get_value::<u64>("local_status").unwrap(), Some(7));
}
