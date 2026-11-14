use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex, mpsc, oneshot, watch},
};
use uuid::Uuid;

const MAX_FRAME: usize = 1024 * 1024;
const MAX_LOGICAL: usize = 64 * 1024 * 1024;
type Pending = Arc<Mutex<BTreeMap<String, oneshot::Sender<Result<Value, OmpRpcError>>>>>;

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
    #[error("OMP transport I/O failed: {0}")]
    Write(#[from] std::io::Error),
    #[error("invalid OMP frame: {0}")]
    Protocol(String),
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
    ToolCall {
        id: String,
        tool_call_id: String,
        name: String,
        arguments: Value,
    },
    ToolCancel {
        id: String,
    },
    PromptResult {
        id: Option<String>,
        status: String,
        error: Option<Value>,
        session_settled: bool,
    },
    Settled,
    Unknown {
        raw: Value,
    },
}

#[derive(Clone)]
pub struct OmpRpc {
    writer: Arc<Mutex<Option<ChildStdin>>>,
    pending: Pending,
    ready: watch::Receiver<Option<Value>>,
}

impl OmpRpc {
    /// The supervisor retains process ownership. The caller must also drain stderr.
    pub fn from_io(
        stdin: ChildStdin,
        stdout: ChildStdout,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<OmpEvent>) {
        let (events, receiver) = mpsc::channel(capacity.max(32));
        let (ready_tx, ready) = watch::channel(None);
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        tokio::spawn(read_loop(stdout, pending.clone(), events, ready_tx));
        (
            Self {
                writer: Arc::new(Mutex::new(Some(stdin))),
                pending,
                ready,
            },
            receiver,
        )
    }

    pub async fn initialize(&self) -> Result<Value, OmpRpcError> {
        let mut ready = self.ready.clone();
        let frame = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(frame) = ready.borrow().clone() {
                    return Ok::<Value, OmpRpcError>(frame);
                }
                ready.changed().await.map_err(|_| OmpRpcError::Closed)?;
            }
        })
        .await
        .map_err(|_| OmpRpcError::Timeout)??;
        if frame["protocolVersion"] != 1 {
            return Err(OmpRpcError::Protocol("unsupported ready version".into()));
        }
        if frame["supportedProtocolVersions"]
            .as_array()
            .is_some_and(|v| v.contains(&json!(2)))
        {
            self.request(
                "negotiate_protocol",
                json!({"protocolVersion":2}),
                Duration::from_secs(30),
            )
            .await?;
        }
        Ok(frame)
    }

    pub async fn request(
        &self,
        command: &str,
        fields: Value,
        timeout: Duration,
    ) -> Result<Value, OmpRpcError> {
        self.request_id(&Uuid::new_v4().to_string(), command, fields, timeout)
            .await
    }

    pub async fn request_id(
        &self,
        id: &str,
        command: &str,
        fields: Value,
        timeout: Duration,
    ) -> Result<Value, OmpRpcError> {
        let mut frame = fields
            .as_object()
            .cloned()
            .ok_or_else(|| OmpRpcError::Protocol("command fields must be an object".into()))?;
        frame.insert("type".into(), json!(command));
        frame.insert("id".into(), json!(id));
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.contains_key(id) {
                return Err(OmpRpcError::Protocol("duplicate request id".into()));
            }
            pending.insert(id.to_owned(), tx);
        }
        if let Err(error) = self.send(Value::Object(frame)).await {
            self.pending.lock().await.remove(id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Err(OmpRpcError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(id);
                Err(OmpRpcError::Timeout)
            }
        }
    }

    pub async fn respond_tool(&self, id: &str, result: Value) -> Result<(), OmpRpcError> {
        self.send(json!({"type":"host_tool_result","id":id,"result":{"content":[{"type":"text","text":result.to_string()}]}})).await
    }

    pub async fn reject_tool(&self, id: &str, reason: &str) -> Result<(), OmpRpcError> {
        self.send(json!({"type":"host_tool_result","id":id,"isError":true,"result":{"content":[{"type":"text","text":reason}]}})).await
    }

    pub async fn close(&self) -> Result<(), OmpRpcError> {
        if let Some(mut stdin) = self.writer.lock().await.take() {
            stdin.shutdown().await?;
        }
        Ok(())
    }

    async fn send(&self, frame: Value) -> Result<(), OmpRpcError> {
        let bytes = serde_json::to_vec(&frame)?;
        if bytes.len() >= MAX_FRAME {
            return Err(OmpRpcError::Protocol(
                "inbound command exceeds physical frame limit".into(),
            ));
        }
        let mut writer = self.writer.lock().await;
        let stdin = writer.as_mut().ok_or(OmpRpcError::Closed)?;
        stdin.write_all(&bytes).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }
}

#[derive(Default)]
struct Decoder {
    chunk: Option<(String, usize, usize, usize, Vec<u8>)>,
}

impl Decoder {
    fn decode(&mut self, frame: Value) -> Result<Option<Value>, OmpRpcError> {
        let fail = |s: &str| OmpRpcError::Protocol(s.into());
        if frame["type"] != "rpc_chunk" {
            if self.chunk.is_some() {
                return Err(fail("interrupted chunk sequence"));
            }
            return Ok(Some(frame));
        }
        let id = frame["chunkId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| fail("missing chunk ID"))?;
        let index = frame["index"]
            .as_u64()
            .ok_or_else(|| fail("invalid chunk index"))? as usize;
        let count = frame["count"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= MAX_LOGICAL as u64)
            .ok_or_else(|| fail("invalid chunk count"))? as usize;
        let size = frame["byteLength"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= MAX_LOGICAL as u64)
            .ok_or_else(|| fail("invalid logical frame size"))? as usize;
        if self.chunk.is_none() {
            if index != 0 {
                return Err(fail("chunk sequence did not start at zero"));
            }
            self.chunk = Some((id.to_owned(), 0, count, size, Vec::new()));
        }
        let (expected_id, next, expected_count, expected_size, bytes) =
            self.chunk.as_mut().expect("initialized chunk");
        if id != expected_id
            || index != *next
            || count != *expected_count
            || size != *expected_size
            || index >= count
        {
            return Err(fail("interleaved or inconsistent chunk sequence"));
        }
        let segment = base64::engine::general_purpose::STANDARD
            .decode(
                frame["data"]
                    .as_str()
                    .ok_or_else(|| fail("missing chunk data"))?,
            )
            .map_err(|_| fail("invalid chunk base64"))?;
        if bytes.len() + segment.len() > size {
            return Err(fail("chunk exceeds declared size"));
        }
        bytes.extend_from_slice(&segment);
        *next += 1;
        if *next < count {
            return Ok(None);
        }
        let (_, _, _, size, bytes) = self.chunk.take().expect("complete chunk");
        if bytes.len() != size {
            return Err(fail("chunk size mismatch"));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| fail("invalid UTF-8"))?;
        let value: Value = serde_json::from_str(text)?;
        if !value.is_object() || value["type"] == "rpc_chunk" {
            return Err(fail("invalid logical frame"));
        }
        Ok(Some(value))
    }
}

async fn read_loop(
    stdout: ChildStdout,
    pending: Pending,
    events: mpsc::Sender<OmpEvent>,
    ready: watch::Sender<Option<Value>>,
) {
    let outcome: Result<(), OmpRpcError> = async {
        let mut reader = BufReader::new(stdout);
        let mut decoder = Decoder::default();
        let mut line = Vec::new();
        let mut seen_ready = false;
        loop {
            line.clear();
            // fill_buf bounds allocation even if a broken process omits newlines.
            loop {
                let available = reader.fill_buf().await?;
                if available.is_empty() {
                    if !line.is_empty() || decoder.chunk.is_some() {
                        return Err(OmpRpcError::Protocol("truncated frame".into()));
                    }
                    return Ok(());
                }
                let count = available
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(available.len(), |n| n + 1);
                if line.len() + count > MAX_FRAME {
                    return Err(OmpRpcError::Protocol("physical frame too large".into()));
                }
                let complete = available[count - 1] == b'\n';
                line.extend_from_slice(&available[..count]);
                reader.consume(count);
                if complete {
                    break;
                }
            }
            let Some(value) = decoder.decode(serde_json::from_slice(&line)?)? else {
                continue;
            };
            if !seen_ready {
                if value["type"] != "ready" {
                    return Err(OmpRpcError::Protocol("first frame is not ready".into()));
                }
                seen_ready = true;
                let _ = ready.send(Some(value));
                continue;
            }
            if value["type"] == "response" {
                if let Some(id) = value["id"].as_str() {
                    if let Some(tx) = pending.lock().await.remove(id) {
                        let response = if value["success"] == true {
                            Ok(value.get("data").cloned().unwrap_or(Value::Null))
                        } else {
                            Err(OmpRpcError::Remote(value["error"].to_string()))
                        };
                        let _ = tx.send(response);
                        continue;
                    }
                }
            }
            // Overflow fails the session instead of dropping a submission or
            // blocking command responses behind an undrained event queue.
            events
                .try_send(classify_event(value))
                .map_err(|e| OmpRpcError::Protocol(format!("event consumer unavailable: {e}")))?;
        }
    }
    .await;
    if let Err(error) = outcome {
        tracing::error!(%error, "OMP transport failed");
    }
    for (_, tx) in std::mem::take(&mut *pending.lock().await) {
        let _ = tx.send(Err(OmpRpcError::Closed));
    }
}

fn classify_event(value: Value) -> OmpEvent {
    match value["type"].as_str().unwrap_or("") {
        "host_tool_call" => OmpEvent::ToolCall {
            id: value["id"].as_str().unwrap_or("").into(),
            tool_call_id: value["toolCallId"].as_str().unwrap_or("").into(),
            name: value["toolName"].as_str().unwrap_or("").into(),
            arguments: value["arguments"].clone(),
        },
        "host_tool_cancel" => OmpEvent::ToolCancel {
            id: value["targetId"].as_str().unwrap_or("").into(),
        },
        "prompt_result" => OmpEvent::PromptResult {
            id: value["id"].as_str().map(str::to_owned),
            status: value["status"].as_str().unwrap_or("").into(),
            error: value.get("error").cloned(),
            session_settled: value["sessionSettled"] == true,
        },
        "session_settled" => OmpEvent::Settled,
        _ => OmpEvent::Unknown { raw: value },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_host_protocol_is_flat() {
        assert!(
            matches!(classify_event(json!({"type":"host_tool_call","id":"h","toolCallId":"t","toolName":"dacs_submit","arguments":{"x":1}})),
            OmpEvent::ToolCall { id, tool_call_id, name, arguments } if id == "h" && tool_call_id == "t" && name == "dacs_submit" && arguments["x"] == 1)
        );
        assert!(
            matches!(classify_event(json!({"type":"prompt_result","status":"error","error":{"code":"cost_cap"},"sessionSettled":true})),
            OmpEvent::PromptResult { error: Some(e), .. } if e["code"] == "cost_cap")
        );
    }

    #[test]
    fn chunks_reject_interleaving_and_wrong_sizes() {
        let mut decoder = Decoder::default();
        assert!(decoder.decode(json!({"type":"rpc_chunk","chunkId":"a","index":0,"count":2,"byteLength":2,"data":"ew=="})).unwrap().is_none());
        assert!(decoder.decode(json!({"type":"response"})).is_err());
        let mut decoder = Decoder::default();
        assert!(decoder.decode(json!({"type":"rpc_chunk","chunkId":"a","index":0,"count":1,"byteLength":3,"data":"e30="})).is_err());
    }

    #[test]
    fn chunks_decode_losslessly() {
        let bytes = br#"{"type":"session_settled"}"#;
        let frame = json!({"type":"rpc_chunk","chunkId":"a","index":0,"count":1,"byteLength":bytes.len(),"data":base64::engine::general_purpose::STANDARD.encode(bytes)});
        assert_eq!(
            Decoder::default().decode(frame).unwrap().unwrap()["type"],
            "session_settled"
        );
    }
}
