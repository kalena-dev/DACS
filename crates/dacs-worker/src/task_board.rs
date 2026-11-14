//! Small private task-board API used for the DACS implementation demonstration.
//! State lives in redb and survives API process restarts.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::{path::Path as FsPath, sync::Arc};
use thiserror::Error;
use uuid::Uuid;

const TASKS: TableDefinition<&str, &str> = TableDefinition::new("tasks");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    InProgress,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: TaskStatus,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CreateTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct EditTask {
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SetStatus {
    pub status: TaskStatus,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FilterTasks {
    pub status: Option<TaskStatus>,
    pub q: Option<String>,
}

#[derive(Debug, Error)]
pub enum BoardError {
    #[error("redb database error: {0}")]
    Db(#[from] redb::DatabaseError),
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("task not found")]
    NotFound,
    #[error("task title cannot be empty")]
    EmptyTitle,
    #[error("task encoding error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct TaskBoard {
    db: Arc<Database>,
}

impl TaskBoard {
    pub fn open(path: impl AsRef<FsPath>) -> Result<Self, BoardError> {
        let db = Database::create(path)?;
        let write = db.begin_write()?;
        {
            write.open_table(TASKS)?;
        }
        write.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    pub fn create(&self, input: CreateTask) -> Result<BoardTask, BoardError> {
        let title = input.title.trim().to_owned();
        if title.is_empty() {
            return Err(BoardError::EmptyTitle);
        }
        let now = now_ms();
        let task = BoardTask {
            id: Uuid::now_v7().to_string(),
            title,
            description: input.description,
            status: TaskStatus::Todo,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.put(&task)?;
        Ok(task)
    }

    pub fn edit(&self, id: &str, input: EditTask) -> Result<BoardTask, BoardError> {
        self.update(id, |task| {
            if let Some(title) = input.title {
                if title.trim().is_empty() {
                    return Err(BoardError::EmptyTitle);
                }
                task.title = title.trim().to_owned();
            }
            if let Some(description) = input.description {
                task.description = description;
            }
            Ok(())
        })
    }

    pub fn set_status(&self, id: &str, status: TaskStatus) -> Result<BoardTask, BoardError> {
        self.update(id, |task| {
            task.status = status;
            Ok(())
        })
    }

    pub fn list(&self, filter: FilterTasks) -> Result<Vec<BoardTask>, BoardError> {
        let read = self.db.begin_read()?;
        let table = read.open_table(TASKS)?;
        let needle = filter.q.as_deref().map(str::to_lowercase);
        let mut result = Vec::new();
        for item in table.iter()? {
            let (_, value) = item?;
            let task: BoardTask = serde_json::from_str(value.value())?;
            if filter
                .status
                .as_ref()
                .is_some_and(|status| status != &task.status)
            {
                continue;
            }
            if needle.as_ref().is_some_and(|q| {
                !task.title.to_lowercase().contains(q)
                    && !task.description.to_lowercase().contains(q)
            }) {
                continue;
            }
            result.push(task);
        }
        result.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(result)
    }

    pub fn delete(&self, id: &str) -> Result<(), BoardError> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(TASKS)?;
            if table.remove(id)?.is_none() {
                return Err(BoardError::NotFound);
            }
        }
        write.commit()?;
        Ok(())
    }

    // Read and modify under one write transaction: concurrent status/text edits
    // cannot overwrite each other or resurrect a concurrently deleted task.
    fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut BoardTask) -> Result<(), BoardError>,
    ) -> Result<BoardTask, BoardError> {
        let write = self.db.begin_write()?;
        let task = {
            let mut table = write.open_table(TASKS)?;
            let mut task: BoardTask = {
                let value = table.get(id)?.ok_or(BoardError::NotFound)?;
                serde_json::from_str(value.value())?
            };
            change(&mut task)?;
            task.updated_at_ms = now_ms().max(task.updated_at_ms);
            let encoded = serde_json::to_string(&task)?;
            table.insert(id, encoded.as_str())?;
            task
        };
        write.commit()?;
        Ok(task)
    }

    fn put(&self, task: &BoardTask) -> Result<(), BoardError> {
        let encoded = serde_json::to_string(task)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(TASKS)?;
            table.insert(task.id.as_str(), encoded.as_str())?;
        }
        write.commit()?;
        Ok(())
    }
}

pub fn router(board: TaskBoard) -> Router {
    Router::new()
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/{id}", put(edit_task).delete(delete_task))
        .route("/api/tasks/{id}/status", post(set_status))
        .with_state(board)
}

async fn list_tasks(
    State(board): State<TaskBoard>,
    Query(filter): Query<FilterTasks>,
) -> Result<Json<Vec<BoardTask>>, ApiError> {
    Ok(Json(board.list(filter)?))
}
async fn create_task(
    State(board): State<TaskBoard>,
    Json(input): Json<CreateTask>,
) -> Result<(StatusCode, Json<BoardTask>), ApiError> {
    Ok((StatusCode::CREATED, Json(board.create(input)?)))
}
async fn edit_task(
    State(board): State<TaskBoard>,
    Path(id): Path<String>,
    Json(input): Json<EditTask>,
) -> Result<Json<BoardTask>, ApiError> {
    Ok(Json(board.edit(&id, input)?))
}
async fn set_status(
    State(board): State<TaskBoard>,
    Path(id): Path<String>,
    Json(input): Json<SetStatus>,
) -> Result<Json<BoardTask>, ApiError> {
    Ok(Json(board.set_status(&id, input.status)?))
}
async fn delete_task(
    State(board): State<TaskBoard>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    board.delete(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

struct ApiError(BoardError);
impl From<BoardError> for ApiError {
    fn from(value: BoardError) -> Self {
        Self(value)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match self.0 {
            BoardError::NotFound => StatusCode::NOT_FOUND,
            BoardError::EmptyTitle => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.0.to_string()).into_response()
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

    #[test]
    fn empty_board_and_all_mutations_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.redb");
        let board = TaskBoard::open(&path).unwrap();
        assert!(board.list(FilterTasks::default()).unwrap().is_empty());
        assert!(matches!(board.delete("missing"), Err(BoardError::NotFound)));
        let task = board
            .create(CreateTask {
                title: "  First  ".into(),
                description: "needle".into(),
            })
            .unwrap();
        assert_eq!(task.title, "First");
        board
            .edit(
                &task.id,
                EditTask {
                    title: Some("Updated".into()),
                    description: Some("New detail".into()),
                },
            )
            .unwrap();
        board.set_status(&task.id, TaskStatus::InProgress).unwrap();
        assert!(matches!(
            board.edit(
                &task.id,
                EditTask {
                    title: Some("  ".into()),
                    description: Some("must roll back".into()),
                }
            ),
            Err(BoardError::EmptyTitle)
        ));
        drop(board);
        let board = TaskBoard::open(&path).unwrap();
        let tasks = board
            .list(FilterTasks {
                status: Some(TaskStatus::InProgress),
                q: Some("NEW DETAIL".into()),
            })
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Updated");
        assert_eq!(tasks[0].created_at_ms, task.created_at_ms);
        assert!(
            board
                .list(FilterTasks {
                    status: Some(TaskStatus::Done),
                    q: None
                })
                .unwrap()
                .is_empty()
        );
        board.delete(&task.id).unwrap();
        assert!(matches!(
            board.set_status(&task.id, TaskStatus::Done),
            Err(BoardError::NotFound)
        ));
        drop(board);
        assert!(
            TaskBoard::open(&path)
                .unwrap()
                .list(FilterTasks::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn simultaneous_status_and_text_updates_preserve_both_changes() {
        let dir = tempfile::tempdir().unwrap();
        let board = TaskBoard::open(dir.path().join("board.redb")).unwrap();
        let task = board
            .create(CreateTask {
                title: "Original".into(),
                description: String::new(),
            })
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                board
                    .edit(
                        &task.id,
                        EditTask {
                            title: Some("Edited".into()),
                            description: None,
                        },
                    )
                    .unwrap();
            });
            scope.spawn(|| {
                barrier.wait();
                board.set_status(&task.id, TaskStatus::Done).unwrap();
            });
            barrier.wait();
        });
        let tasks = board.list(FilterTasks::default()).unwrap();
        assert_eq!(tasks[0].title, "Edited");
        assert_eq!(tasks[0].status, TaskStatus::Done);
    }
}
