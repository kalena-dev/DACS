//! Small private task-board API used for the DACS implementation demonstration.
//! State lives in redb and survives API process restarts.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
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
        Ok(Self {
            db: Arc::new(Database::create(path)?),
        })
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
        let mut task = self.get(id)?.ok_or(BoardError::NotFound)?;
        if let Some(title) = input.title {
            if title.trim().is_empty() {
                return Err(BoardError::EmptyTitle);
            }
            task.title = title.trim().to_owned();
        }
        if let Some(description) = input.description {
            task.description = description;
        }
        task.updated_at_ms = now_ms();
        self.put(&task)?;
        Ok(task)
    }

    pub fn set_status(&self, id: &str, status: TaskStatus) -> Result<BoardTask, BoardError> {
        let mut task = self.get(id)?.ok_or(BoardError::NotFound)?;
        task.status = status;
        task.updated_at_ms = now_ms();
        self.put(&task)?;
        Ok(task)
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

    fn get(&self, id: &str) -> Result<Option<BoardTask>, BoardError> {
        let read = self.db.begin_read()?;
        let table = read.open_table(TASKS)?;
        table
            .get(id)?
            .map(|v| Ok(serde_json::from_str(v.value())?))
            .transpose()
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
