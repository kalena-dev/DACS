use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    InProgress,
    Done,
}

impl TaskStatus {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "todo" => Some(Self::Todo),
            "in_progress" => Some(Self::InProgress),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash)]
pub struct BoardTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: TaskStatus,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub fn list_url(query: &str, status: Option<TaskStatus>) -> String {
    let mut params = form_urlencoded::Serializer::new(String::new());
    if !query.is_empty() {
        params.append_pair("q", query);
    }
    if let Some(status) = status {
        params.append_pair("status", status.wire());
    }
    let query = params.finish();
    if query.is_empty() {
        "/api/tasks".to_owned()
    } else {
        format!("/api/tasks?{query}")
    }
}

#[cfg(target_arch = "wasm32")]
mod transport {
    use super::*;
    use gloo_net::http::{Request, RequestBuilder};
    use serde_json::json;

    pub enum Mutation {
        Create {
            title: String,
            description: String,
        },
        Edit {
            id: String,
            title: String,
            description: String,
        },
        Status {
            id: String,
            status: TaskStatus,
        },
        Delete {
            id: String,
        },
    }

    impl Mutation {
        pub fn edits_form(&self) -> bool {
            matches!(self, Self::Create { .. } | Self::Edit { .. })
        }

        pub fn success_message(&self) -> &'static str {
            match self {
                Self::Create { .. } => "Task created and saved.",
                Self::Edit { .. } => "Task changes saved.",
                Self::Status { .. } => "Task status saved.",
                Self::Delete { .. } => "Task deleted.",
            }
        }

        pub async fn execute(self) -> Result<(), String> {
            let (builder, body, deleting) = match self {
                Self::Create { title, description } => (
                    Request::post("/api/tasks"),
                    Some(json!({ "title": title, "description": description })),
                    false,
                ),
                Self::Edit {
                    id,
                    title,
                    description,
                } => (
                    Request::put(&format!("/api/tasks/{}", path_segment(&id))),
                    Some(json!({ "title": title, "description": description })),
                    false,
                ),
                Self::Status { id, status } => (
                    Request::post(&format!("/api/tasks/{}/status", path_segment(&id))),
                    Some(json!({ "status": status })),
                    false,
                ),
                Self::Delete { id } => (
                    Request::delete(&format!("/api/tasks/{}", path_segment(&id))),
                    None,
                    true,
                ),
            };
            let request = if let Some(body) = body {
                builder
                    .header("Content-Type", "application/json")
                    .body(body.to_string())
            } else {
                builder.build()
            }
            .map_err(|error| format!("Could not prepare request: {error}"))?;
            let response = request.send().await.map_err(|error| {
                format!("Network request failed: {error}. The change may have reached the server; refresh before repeating it.")
            })?;
            let status = response.status();
            let text = response.text().await.map_err(|error| {
                format!("Could not read server response: {error}. Refresh to check whether the change was saved.")
            })?;
            if !(200..300).contains(&status) {
                return Err(format!("Server returned HTTP {status}: {text}"));
            }
            if !deleting {
                serde_json::from_str::<BoardTask>(&text).map_err(|error| {
                    format!(
                        "Unexpected saved-task response: {error}. Refresh to check server state."
                    )
                })?;
            }
            Ok(())
        }
    }

    fn path_segment(id: &str) -> String {
        form_urlencoded::byte_serialize(id.as_bytes()).collect()
    }

    async fn get(builder: RequestBuilder) -> Result<String, String> {
        let response = builder
            .send()
            .await
            .map_err(|error| format!("Cannot reach the task-board server: {error}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| format!("Could not read server response: {error}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("Server returned HTTP {status}: {text}"));
        }
        Ok(text)
    }

    pub async fn list(query: &str, status: Option<TaskStatus>) -> Result<Vec<BoardTask>, String> {
        let text =
            get(Request::get(&list_url(query, status)).header("Cache-Control", "no-cache")).await?;
        serde_json::from_str(&text)
            .map_err(|error| format!("Unexpected task-list response: {error}"))
    }
}

#[cfg(target_arch = "wasm32")]
pub use transport::{Mutation, list};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_matches_backend_wire_values() {
        for status in [TaskStatus::Todo, TaskStatus::InProgress, TaskStatus::Done] {
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.wire())
            );
            assert_eq!(TaskStatus::parse(status.wire()), Some(status));
        }
        assert_eq!(TaskStatus::parse("in-progress"), None);
    }

    #[test]
    fn filters_use_backend_query_names_and_preserve_text() {
        assert_eq!(list_url("", None), "/api/tasks");
        let url = list_url("A&B + café?", Some(TaskStatus::InProgress));
        let values: Vec<_> = form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            values,
            vec![
                ("q".to_owned(), "A&B + café?".to_owned()),
                ("status".to_owned(), "in_progress".to_owned()),
            ]
        );
    }

    #[test]
    fn reads_complete_backend_task() {
        let task: BoardTask = serde_json::from_str(r#"{"id":"task-1","title":"Saved task","description":"Persistent description","status":"in_progress","created_at_ms":1,"updated_at_ms":2}"#).unwrap();
        assert_eq!(task.status, TaskStatus::InProgress);
        assert_eq!(task.description, "Persistent description");
        assert_eq!(task.updated_at_ms, 2);
    }
}
