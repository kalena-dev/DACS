use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::{Mutex, mpsc, oneshot},
};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum OmpRpcError {
    #[error("OMP RPC transport ended")]
    Closed,
    #[error("OMP RPC request timed out")]
    Timeout,
    #[error("OMP returned an error: {0}")]
    Remote(String),
    #[error("invalid OMP JSONL: {0}")]
    Json(#[from] serde_json::Error),
    #[error("OMP stdin write failed: {0}")]
    Write(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OmpToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OmpEvent {
    Response {
        id: String,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        error: Option<Value>,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    },
    Message {
        role: String,
        #[serde(default)]
        content: Value,
    },
    Session {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        state: Option<String>,
    },
    Unknown {
        raw: Value,
    },
}

#[derive(Clone)]
pub struct OmpRpc {
    writer: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<BTreeMap<String, oneshot::Sender<Result<Value, OmpRpcError>>>>>,
    events: mpsc::Sender<OmpEvent>,
}

impl OmpRpc {
    /// Attach to an already launched OMP JSONL process and start draining stdout.
    pub fn attach(
        child: &mut Child,
        event_capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<OmpEvent>), OmpRpcError> {
        let stdin = child.stdin.take().ok_or(OmpRpcError::Closed)?;
        let stdout = child.stdout.take().ok_or(OmpRpcError::Closed)?;
        let (tx, rx) = mpsc::channel(event_capacity.max(1));
        let pending = Arc::new(Mutex::new(BTreeMap::<
            String,
            oneshot::Sender<Result<Value, OmpRpcError>>,
        >::new()));
        let rpc = Self {
            writer: Arc::new(Mutex::new(stdin)),
            pending: pending.clone(),
            events: tx.clone(),
        };
        tokio::spawn(read_loop(stdout, pending, tx));
        Ok((rpc, rx))
    }

    /// Send one correlated JSONL request. A response timeout never kills the OMP
    /// session; pending DACS obligations remain in the journal independently.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, OmpRpcError> {
        let id = Uuid::new_v4().to_string();
        let request = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let line = serde_json::to_vec(&request)?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let write_result = async {
            let mut writer = self.writer.lock().await;
            writer.write_all(&line).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await
        }
        .await;
        if let Err(error) = write_result {
            self.pending.lock().await.remove(&id);
            return Err(OmpRpcError::Write(error));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(OmpRpcError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(OmpRpcError::Timeout)
            }
        }
    }

    /// Respond to an OMP-issued tool invocation after DACS admission and local
    /// execution. The original invocation id is preserved for OMP correlation.
    pub async fn respond_tool(
        &self,
        invocation_id: &str,
        result: Value,
    ) -> Result<(), OmpRpcError> {
        self.send_notification(
            "tool_result",
            serde_json::json!({"id":invocation_id,"result":result}),
        )
        .await
    }

    pub async fn reject_tool(&self, invocation_id: &str, reason: &str) -> Result<(), OmpRpcError> {
        self.send_notification(
            "tool_error",
            serde_json::json!({"id":invocation_id,"error":{"message":reason}}),
        )
        .await
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), OmpRpcError> {
        let line = serde_json::to_vec(
            &serde_json::json!({"jsonrpc":"2.0","method":method,"params":params}),
        )?;
        let mut writer = self.writer.lock().await;
        writer.write_all(&line).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    pub fn event_sender(&self) -> mpsc::Sender<OmpEvent> {
        self.events.clone()
    }
}

async fn read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<BTreeMap<String, oneshot::Sender<Result<Value, OmpRpcError>>>>>,
    events: mpsc::Sender<OmpEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error=%e, "discarding malformed OMP JSONL");
                continue;
            }
        };
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            if let Some(tx) = pending.lock().await.remove(id) {
                let response = if let Some(error) = value.get("error") {
                    Err(OmpRpcError::Remote(error.to_string()))
                } else {
                    Ok(value.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(response);
                continue;
            }
        }
        let event = classify_event(value);
        if events.send(event).await.is_err() {
            break;
        }
    }
    let mut pending = pending.lock().await;
    for (_, tx) in std::mem::take(&mut *pending) {
        let _ = tx.send(Err(OmpRpcError::Closed));
    }
}

fn classify_event(value: Value) -> OmpEvent {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| value.get("method").and_then(Value::as_str))
        .unwrap_or("");
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match kind {
        "tool_call" | "$/toolCall" => OmpEvent::ToolCall {
            id,
            name: params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            arguments: params.get("arguments").cloned().unwrap_or(Value::Null),
        },
        "message" | "$/message" => OmpEvent::Message {
            role: params
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("assistant")
                .to_owned(),
            content: params.get("content").cloned().unwrap_or(Value::Null),
        },
        "session" | "$/session" => OmpEvent::Session {
            session_id: params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            state: params
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        _ => OmpEvent::Unknown { raw: value },
    }
}
