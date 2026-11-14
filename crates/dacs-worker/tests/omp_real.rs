//! Contract checks of the OMP adapter against a real installed OMP binary.
//!
//! Run with `DACS_TEST_OMP=/path/to/omp cargo test -p dacs-worker --test omp_real -- --nocapture`.
//! Without `DACS_TEST_OMP` every test prints why it is skipped and returns.
//!
//! No model provider is ever contacted. Each OMP runs with a fresh temporary
//! `HOME` (no credentials, no owner profile), a cleared environment, and one
//! configured model whose endpoint is an in-process loopback stub that scripts
//! assistant turns. Every session asserts that stub model is selected before a
//! prompt is sent.
//!
//! `DACS_TEST_OMP_PIN` (default `18.20.0`, the team pin) selects the pin for the
//! version check. `DACS_TEST_OMP_DESKTOP=1` additionally runs the read-only
//! `computer.capabilities()` probe; it never sends desktop input.

use axum::{
    body::Bytes,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use dacs_worker::{
    OmpEvent, OmpRpc, OmpRpcError, check_pinned_version, installed_version, runtime,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Child, sync::mpsc, task::JoinHandle};

const EVENT_TIMEOUT: Duration = Duration::from_secs(90);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const ATTEMPT_TOOLS: [&str; 7] = [
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "glob",
    "web_search",
];
const NATIVE_SPAWNING: [&str; 4] = ["task", "wait", "ask", "browser"];
const DESKTOP_PROBE: &str =
    "console.log('DACS_DESKTOP_CAPS='+JSON.stringify(await computer.capabilities()));";

fn omp() -> Option<PathBuf> {
    match std::env::var_os("DACS_TEST_OMP") {
        None => {
            eprintln!("skipping real-OMP contract test: set DACS_TEST_OMP=/path/to/omp to run it");
            None
        }
        Some(path) => {
            let path = PathBuf::from(path);
            assert!(
                path.is_file(),
                "DACS_TEST_OMP={} is not a file",
                path.display()
            );
            Some(path)
        }
    }
}

#[derive(Clone)]
enum Step {
    Tool(&'static str, Value),
    Text(&'static str),
    Status(u16),
}

type StubState = (Arc<Vec<Step>>, Arc<Mutex<Vec<Value>>>);

/// Loopback OpenAI-compatible endpoint. A request's step is chosen by how many
/// tool results it already carries, so retries and replays stay deterministic.
struct Stub {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Stub {
    async fn start(script: Vec<Step>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(stub_turn))
            .with_state((Arc::new(script), requests.clone()));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            port,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn stub_turn(State((script, requests)): State<StubState>, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let tool_results = request["messages"]
        .as_array()
        .map_or(0, |m| m.iter().filter(|m| m["role"] == "tool").count());
    requests.lock().unwrap().push(request);
    let chunk = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"id":"stub","object":"chat.completion.chunk","created":0,"model":"null-model",
                   "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
        )
    };
    let mut sse = match &script[tool_results.min(script.len() - 1)] {
        Step::Status(code) => {
            return (
                StatusCode::from_u16(*code).unwrap(),
                axum::Json(json!({"error":{"message":"stub rejected the request","type":"invalid_request_error"}})),
            )
                .into_response();
        }
        Step::Tool(name, arguments) => {
            chunk(
                json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("call_{tool_results}"),"type":"function",
                       "function":{"name":name,"arguments":arguments.to_string()}}]}),
                Value::Null,
            ) + &chunk(json!({}), json!("tool_calls"))
        }
        Step::Text(text) => {
            chunk(json!({"role":"assistant","content":text}), Value::Null)
                + &chunk(json!({}), json!("stop"))
        }
    };
    sse += &format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"stub","object":"chat.completion.chunk","created":0,"model":"null-model","choices":[],
               "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})
    );
    ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
}

#[derive(Default)]
struct Launch {
    probe: bool,
    desktop: bool,
    /// Drop the managed `--tools` allowlist (overlay only).
    no_allowlist: bool,
    /// Drop the managed overlay.
    no_overlay: bool,
    /// Drop `--no-ui`, enabling OMP's RPC dialog requests.
    rpc_ui: bool,
    owner_config: Option<&'static str>,
    /// USD per million tokens of the stub model.
    cost_per_million: f64,
    events: Option<Value>,
}

struct Session {
    child: Child,
    rpc: OmpRpc,
    events: mpsc::Receiver<OmpEvent>,
    seen: Vec<OmpEvent>,
    stderr: JoinHandle<Vec<u8>>,
    exit: Option<std::process::ExitStatus>,
    work: tempfile::TempDir,
    _home: tempfile::TempDir,
}

impl Session {
    async fn start(omp: &Path, port: u16, launch: Launch) -> Self {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let agent = home.path().join(".omp").join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(home.path().join("tmp")).unwrap();
        std::fs::write(
            agent.join("models.yml"),
            format!(
                "providers:\n  dacs-null:\n    baseUrl: http://127.0.0.1:{port}/v1\n    api: openai-completions\n    auth: none\n    models:\n      - id: null-model\n        name: DACS loopback stub\n        cost:\n          input: {c}\n          output: {c}\n          cacheRead: 0\n          cacheWrite: 0\n",
                c = launch.cost_per_million
            ),
        )
        .unwrap();
        if let Some(config) = launch.owner_config {
            // Stands in for the owner's own OMP profile settings.
            std::fs::write(agent.join("config.yml"), config).unwrap();
        }
        let state = home.path().join("attempt");
        std::fs::create_dir_all(&state).unwrap();
        let overlay = state.join("managed.yml");
        std::fs::write(&overlay, runtime::managed_overlay(launch.desktop)).unwrap();
        let mut args = runtime::managed_omp_args(
            &overlay,
            &state.join("session"),
            None,
            launch.probe,
            launch.desktop,
        );
        if launch.no_allowlist {
            let at = args.iter().position(|a| a == "--tools").unwrap();
            args.drain(at..at + 2);
        }
        if launch.no_overlay {
            let at = args.iter().position(|a| a == "--config").unwrap();
            args.drain(at..at + 2);
        }
        if launch.rpc_ui {
            args.retain(|a| a != "--no-ui");
        }
        let mut child = tokio::process::Command::new(omp)
            .args(&args)
            .current_dir(work.path())
            .env_clear()
            .env("HOME", home.path())
            .env("TMPDIR", home.path().join("tmp"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn OMP");
        let mut stderr_pipe = child.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut bytes).await;
            bytes
        });
        let (rpc, events) = OmpRpc::from_io(
            child.stdin.take().unwrap(),
            child.stdout.take().unwrap(),
            64,
        );
        let mut session = Self {
            child,
            rpc,
            events,
            seen: Vec::new(),
            stderr,
            exit: None,
            work,
            _home: home,
        };
        let ready = match session.rpc.initialize().await {
            Ok(ready) => ready,
            Err(error) => panic!("OMP RPC startup failed: {error}; {}", session.stop().await),
        };
        // The adapter accepts protocol 1 and negotiates 2 when advertised.
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["protocolVersion"], 1);
        assert!(
            ready["supportedProtocolVersions"]
                .as_array()
                .is_some_and(|v| v.contains(&json!(2))),
            "{ready}"
        );
        session
            .request(
                "set_event_filter",
                json!({"events":launch.events.unwrap_or(json!(["agent_end","notice"])),"messageUpdates":"delta"}),
            )
            .await;
        // Safety gate: only the loopback stub model may be selected.
        let state = session.request("get_state", json!({})).await;
        assert_eq!(
            state["model"]["provider"], "dacs-null",
            "{}",
            state["model"]
        );
        assert_eq!(
            state["model"]["baseUrl"],
            format!("http://127.0.0.1:{port}/v1")
        );
        session
    }

    async fn request(&self, command: &str, fields: Value) -> Value {
        match self.rpc.request(command, fields, REQUEST_TIMEOUT).await {
            Ok(value) => value,
            Err(error) => panic!("{command} failed: {error}"),
        }
    }

    async fn tools(&self) -> Vec<String> {
        names(&self.request("get_state", json!({})).await["dumpTools"])
    }

    async fn wait_for(&mut self, what: &str, matches: impl Fn(&OmpEvent) -> bool) -> OmpEvent {
        let events = &mut self.events;
        let seen = &mut self.seen;
        let found = tokio::time::timeout(EVENT_TIMEOUT, async {
            while let Some(event) = events.recv().await {
                seen.push(event.clone());
                if matches(&event) {
                    return Some(event);
                }
            }
            None
        })
        .await;
        match found {
            Ok(Some(event)) => event,
            Ok(None) => panic!("OMP transport closed before {what}; {}", self.stop().await),
            Err(_) => panic!("timed out waiting for {what}; {}", self.stop().await),
        }
    }

    async fn prompt_result(&mut self) -> OmpEvent {
        self.wait_for("prompt_result", |e| {
            matches!(e, OmpEvent::PromptResult { .. })
        })
        .await
    }

    /// Closes stdin and reports how the process ended plus its stderr.
    async fn stop(&mut self) -> String {
        let _ = self.rpc.close().await;
        let status = match tokio::time::timeout(Duration::from_secs(20), self.child.wait()).await {
            Ok(status) => {
                self.exit = status.as_ref().ok().copied();
                format!("{status:?}")
            }
            Err(_) => {
                let _ = self.child.kill().await;
                "killed after 20s".into()
            }
        };
        let stderr = tokio::time::timeout(Duration::from_secs(5), &mut self.stderr)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        format!(
            "exit {status}; stderr: {}",
            String::from_utf8_lossy(&stderr)
        )
    }
}

fn names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .expect("tool list")
        .iter()
        .map(|t| {
            t["name"]
                .as_str()
                .or_else(|| t["function"]["name"].as_str())
                .or_else(|| t.as_str())
                .expect("tool name")
                .to_owned()
        })
        .collect()
}

fn dacs_tools() -> Vec<String> {
    names(&runtime::host_tools())
}

fn set<I: IntoIterator<Item = S>, S: Into<String>>(items: I) -> BTreeSet<String> {
    items.into_iter().map(Into::into).collect()
}

fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_startup_state_and_concurrent_correlation() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![Step::Text("unused")]).await;
    let mut session = Session::start(&omp, stub.port, Launch::default()).await;

    let state = session.request("get_state", json!({})).await;
    for key in ["sessionId", "isSettled", "hasPendingAsyncWork", "dumpTools"] {
        assert!(state.get(key).is_some(), "get_state lacks {key}: {state}");
    }
    assert_eq!(state["isSettled"], true);
    assert_eq!(state["hasPendingAsyncWork"], false);

    // 48 concurrent requests of three kinds; each response must reach its own
    // caller. Their combined output far exceeds a pipe buffer, so this also
    // requires continuous draining of OMP's stdout.
    let mut calls = Vec::new();
    for n in 0..48 {
        let rpc = session.rpc.clone();
        let command = ["get_state", "get_session_stats", "get_available_commands"][n % 3];
        calls.push(tokio::spawn(async move {
            (
                command,
                rpc.request(command, json!({}), REQUEST_TIMEOUT).await,
            )
        }));
    }
    for call in calls {
        let (command, response) = call.await.unwrap();
        let response = response.unwrap_or_else(|e| panic!("{command}: {e}"));
        match command {
            "get_state" => assert!(response["dumpTools"].is_array(), "{command}"),
            "get_session_stats" => assert!(response["tokens"].is_object(), "{command}"),
            _ => assert!(response["commands"].is_array(), "{command}"),
        }
    }

    // A failure response stays correlated and keeps OMP's message.
    match session
        .rpc
        .request("dacs_not_a_command", json!({}), REQUEST_TIMEOUT)
        .await
    {
        Err(OmpRpcError::Remote { message, .. }) => {
            assert!(message.contains("dacs_not_a_command"), "{message}")
        }
        other => panic!("unexpected result for an unknown command: {other:?}"),
    }
    // Caller-chosen IDs are echoed back.
    let response = session
        .rpc
        .request_id(
            "dacs-fixed-id",
            "get_session_stats",
            json!({}),
            REQUEST_TIMEOUT,
        )
        .await
        .unwrap();
    assert!(response["tokens"].is_object());

    // Closing stdin ends OMP cleanly.
    let report = session.stop().await;
    assert!(session.exit.is_some_and(|s| s.success()), "{report}");
    assert!(stub.requests().is_empty(), "no model turn was requested");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_profile_registers_dacs_tools_and_omits_native_spawning() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![Step::Text("unused")]).await;
    let dacs = dacs_tools();
    assert_eq!(
        set(dacs.clone()),
        set([
            "dacs_delegate",
            "dacs_workflow",
            "dacs_message",
            "dacs_submit",
            "dacs_pending"
        ])
    );
    for (probe, desktop) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut session = Session::start(
            &omp,
            stub.port,
            Launch {
                probe,
                desktop,
                ..Launch::default()
            },
        )
        .await;
        let mut expected = if probe {
            set(Vec::<String>::new())
        } else {
            set(ATTEMPT_TOOLS)
        };
        if desktop {
            expected.insert("eval".into());
        }
        let builtin = session.tools().await;
        assert_eq!(
            set(builtin.clone()),
            expected,
            "probe={probe} desktop={desktop}"
        );
        if !probe {
            let registered = session
                .request("set_host_tools", json!({"tools":runtime::host_tools()}))
                .await;
            assert_eq!(names(&registered["toolNames"]), dacs);
            let tools = session.tools().await;
            let mut with_dacs = expected.clone();
            with_dacs.extend(dacs.iter().cloned());
            assert_eq!(
                set(tools.clone()),
                with_dacs,
                "probe={probe} desktop={desktop}"
            );
            for native in NATIVE_SPAWNING {
                assert!(
                    !tools.iter().any(|t| t == native),
                    "{native} offered: {tools:?}"
                );
            }
        }
        match session
            .rpc
            .request("get_subagents", json!({}), REQUEST_TIMEOUT)
            .await
        {
            Ok(value) => assert_eq!(value["subagents"], json!([]), "{value}"),
            // OMP refuses the query when no subagent bus exists at all.
            Err(OmpRpcError::Remote { message, .. }) => {
                assert!(
                    message.contains("Subagent event bus is unavailable"),
                    "{message}"
                )
            }
            Err(error) => panic!("get_subagents: {error}"),
        }
        eprintln!("probe={probe} desktop={desktop}: OMP reports {builtin:?} before host tools");
        session.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_overlay_alone_removes_native_task_tool() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![Step::Text("unused")]).await;
    // Control: OMP's own defaults offer native spawning.
    let mut defaults = Session::start(
        &omp,
        stub.port,
        Launch {
            no_allowlist: true,
            no_overlay: true,
            ..Launch::default()
        },
    )
    .await;
    let default_tools = defaults.tools().await;
    defaults.stop().await;
    assert!(
        default_tools.iter().any(|t| t == "task"),
        "control: OMP defaults no longer include task: {default_tools:?}"
    );
    // The managed overlay without the allowlist still removes them.
    let mut overlay = Session::start(
        &omp,
        stub.port,
        Launch {
            no_allowlist: true,
            ..Launch::default()
        },
    )
    .await;
    let overlay_tools = overlay.tools().await;
    overlay.stop().await;
    eprintln!("defaults {default_tools:?}; managed overlay only {overlay_tools:?}");
    for native in ["task", "wait"] {
        assert!(
            !overlay_tools.iter().any(|t| t == native),
            "{native} survives the managed overlay: {overlay_tools:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_tool_call_round_trip_reaches_the_model() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![
        Step::Tool("dacs_pending", json!({})),
        Step::Text("done"),
    ])
    .await;
    let mut session = Session::start(&omp, stub.port, Launch::default()).await;
    session
        .request("set_host_tools", json!({"tools":runtime::host_tools()}))
        .await;
    session
        .request("prompt", json!({"message":"check obligations"}))
        .await;
    let call = session
        .wait_for("host_tool_call", |e| matches!(e, OmpEvent::ToolCall { .. }))
        .await;
    let OmpEvent::ToolCall {
        id,
        tool_call_id,
        name,
        arguments,
    } = call
    else {
        unreachable!()
    };
    assert_eq!(name, "dacs_pending");
    assert_eq!(arguments, json!({}));
    assert!(!id.is_empty() && !tool_call_id.is_empty());
    session
        .rpc
        .respond_tool(&id, json!({"pending_children":["dacs-test-child"]}))
        .await
        .unwrap();
    let result = session.prompt_result().await;
    let OmpEvent::PromptResult { status, .. } = &result else {
        unreachable!()
    };
    assert_eq!(status, "completed", "{result:?}");
    assert!(result.cost_capped().is_none());
    session
        .wait_for("session_settled", |e| matches!(e, OmpEvent::Settled))
        .await;
    assert!(
        session
            .seen
            .iter()
            .any(|e| matches!(e, OmpEvent::Unknown { raw } if raw["type"] == "agent_end")),
        "agent_end was not forwarded"
    );
    session.stop().await;

    // What the model actually received: the allowlist plus DACS tools, and the
    // host tool result.
    let requests = stub.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    let mut offered = set(ATTEMPT_TOOLS);
    offered.extend(dacs_tools());
    assert_eq!(set(names(&requests[0]["tools"])), offered);
    let tool_message = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool result sent to the model");
    assert!(
        text_of(tool_message).contains("dacs-test-child"),
        "{tool_message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_cancels_a_pending_host_tool_call() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![
        Step::Tool("dacs_pending", json!({})),
        Step::Text("done"),
    ])
    .await;
    let mut session = Session::start(&omp, stub.port, Launch::default()).await;
    session
        .request("set_host_tools", json!({"tools":runtime::host_tools()}))
        .await;
    session
        .request("prompt", json!({"message":"check obligations"}))
        .await;
    let OmpEvent::ToolCall { id, .. } = session
        .wait_for("host_tool_call", |e| matches!(e, OmpEvent::ToolCall { .. }))
        .await
    else {
        unreachable!()
    };
    session.request("abort", json!({})).await;
    let target = id.clone();
    session
        .wait_for(
            "host_tool_cancel",
            move |e| matches!(e, OmpEvent::ToolCancel { id } if *id == target),
        )
        .await;
    let result = session.prompt_result().await;
    let OmpEvent::PromptResult { status, .. } = &result else {
        unreachable!()
    };
    assert_eq!(status, "aborted", "{result:?}");
    session.stop().await;
    assert_eq!(stub.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_cost_cap_is_reported_distinctly() {
    let Some(omp) = omp() else { return };
    // 12 tokens at $100k per million is $1.20, above the owner's $0.50 cap.
    let stub = Stub::start(vec![
        Step::Tool("dacs_pending", json!({})),
        Step::Text("done"),
    ])
    .await;
    let mut session = Session::start(
        &omp,
        stub.port,
        Launch {
            owner_config: Some("session:\n  maxCost: 0.5\n"),
            cost_per_million: 100_000.0,
            ..Launch::default()
        },
    )
    .await;
    session
        .request("set_host_tools", json!({"tools":runtime::host_tools()}))
        .await;
    session
        .request("prompt", json!({"message":"check obligations"}))
        .await;
    let OmpEvent::ToolCall { id, .. } = session
        .wait_for("host_tool_call", |e| matches!(e, OmpEvent::ToolCall { .. }))
        .await
    else {
        unreachable!()
    };
    session
        .rpc
        .respond_tool(&id, json!({"pending":[]}))
        .await
        .unwrap();
    let result = session.prompt_result().await;
    let capped = result
        .cost_capped()
        .unwrap_or_else(|| panic!("not reported as cost-capped: {result:?}"))
        .clone();
    assert!(
        capped["message"]
            .as_str()
            .unwrap_or("")
            .contains("Cost cap"),
        "{capped}"
    );
    assert_eq!(capped["retryable"], false);
    assert!(session.seen.iter().any(
        |e| matches!(e, OmpEvent::Unknown { raw } if raw["type"] == "notice" && raw["source"] == "cost-gate")
    ));
    let stats = session.request("get_session_stats", json!({})).await;
    assert!(stats["cost"].as_f64().unwrap() > 0.5, "{stats}");
    // A later prompt in the capped session is refused the same way.
    session.request("prompt", json!({"message":"again"})).await;
    let again = session.prompt_result().await;
    assert!(again.cost_capped().is_some(), "{again:?}");
    session.stop().await;
    // The capped request was never sent.
    assert_eq!(stub.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_failure_is_not_reported_as_cost_cap() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![Step::Status(400)]).await;
    let mut session = Session::start(&omp, stub.port, Launch::default()).await;
    session
        .request("set_auto_retry", json!({"enabled":false}))
        .await;
    session.request("prompt", json!({"message":"hello"})).await;
    let result = session.prompt_result().await;
    let OmpEvent::PromptResult { status, error, .. } = &result else {
        unreachable!()
    };
    assert_eq!(status, "error", "{result:?}");
    assert!(
        error.as_ref().is_some_and(|e| e.get("code").is_none()),
        "{result:?}"
    );
    assert!(result.cost_capped().is_none());
    session.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_dialogs_are_declined_instead_of_left_pending() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![
        Step::Tool("bash", json!({"command":"echo approved > approved.txt"})),
        Step::Text("done"),
    ])
    .await;
    // The worker's `--no-ui` launch never receives dialogs; with OMP's RPC UI
    // enabled and an owner `always-ask` policy, tool approval is a blocking
    // `select` that the adapter must answer.
    let mut session = Session::start(
        &omp,
        stub.port,
        Launch {
            rpc_ui: true,
            owner_config: Some("tools:\n  approvalMode: always-ask\n"),
            ..Launch::default()
        },
    )
    .await;
    session
        .request("prompt", json!({"message":"write a file"}))
        .await;
    let result = session.prompt_result().await;
    let OmpEvent::PromptResult { status, .. } = &result else {
        unreachable!()
    };
    assert_eq!(status, "completed", "{result:?}");
    let dialog = session
        .seen
        .iter()
        .find_map(|e| match e {
            OmpEvent::Unknown { raw }
                if raw["type"] == "extension_ui_request" && raw["method"] == "select" =>
            {
                Some(raw.clone())
            }
            _ => None,
        })
        .expect("no approval dialog was forwarded");
    assert!(
        dialog["title"].as_str().unwrap_or("").contains("bash"),
        "{dialog}"
    );
    assert!(!session.work.path().join("approved.txt").exists());
    session.stop().await;
    let requests = stub.requests();
    let denied = requests
        .last()
        .and_then(|r| r["messages"].as_array())
        .and_then(|m| m.iter().find(|m| m["role"] == "tool"))
        .map(text_of)
        .unwrap_or_default();
    assert!(denied.contains("denied"), "{denied}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_always_ask_refuses_tools_in_managed_headless_mode() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![
        Step::Tool("bash", json!({"command":"echo approved > approved.txt"})),
        Step::Text("done"),
    ])
    .await;
    let mut session = Session::start(
        &omp,
        stub.port,
        Launch {
            owner_config: Some("tools:\n  approvalMode: always-ask\n"),
            ..Launch::default()
        },
    )
    .await;
    session
        .request("prompt", json!({"message":"write a file"}))
        .await;
    let result = session.prompt_result().await;
    assert!(matches!(&result, OmpEvent::PromptResult { status, .. } if status == "completed"));
    assert!(!session.seen.iter().any(
        |e| matches!(e, OmpEvent::Unknown { raw } if raw["type"] == "extension_ui_request" && raw["method"] == "select")
    ));
    assert!(!session.work.path().join("approved.txt").exists());
    session.stop().await;
    let refusal = stub
        .requests()
        .last()
        .and_then(|r| r["messages"].as_array().cloned())
        .and_then(|m| m.into_iter().find(|m| m["role"] == "tool"))
        .map(|m| text_of(&m))
        .unwrap_or_default();
    assert!(refusal.contains("no interactive UI"), "{refusal}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_response_is_reassembled_from_rpc_chunks() {
    let Some(omp) = omp() else { return };
    let stub = Stub::start(vec![Step::Text("unused")]).await;
    let mut session = Session::start(&omp, stub.port, Launch::default()).await;
    // A host command must fit one 1 MiB line; the get_state reply then exceeds
    // it and arrives as a protocol-2 rpc_chunk sequence.
    let description = "x".repeat(1_045_000);
    session
        .request(
            "set_host_tools",
            json!({"tools":[{"name":"dacs_large","description":description,"loadMode":"essential",
                             "parameters":{"type":"object","properties":{}}}]}),
        )
        .await;
    let state = session.request("get_state", json!({})).await;
    assert!(serde_json::to_vec(&state).unwrap().len() > 1024 * 1024);
    let large = state["dumpTools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "dacs_large")
        .unwrap();
    assert_eq!(large["description"].as_str().unwrap().len(), 1_045_000);
    // The transport stays usable after the chunk sequence.
    session.request("get_session_stats", json!({})).await;
    // Over-limit host frames are refused locally rather than truncated.
    let too_large = "x".repeat(1024 * 1024);
    assert!(matches!(
        session
            .rpc
            .request(
                "set_host_tools",
                json!({"tools":[{"name":"x","description":too_large,"parameters":{}}]}),
                REQUEST_TIMEOUT
            )
            .await,
        Err(OmpRpcError::Protocol(_))
    ));
    session.request("get_session_stats", json!({})).await;
    session.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_version_is_compared_with_the_pin() {
    let Some(omp) = omp() else { return };
    let home = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(&omp)
        .arg("--version")
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let installed = installed_version(&text)
        .unwrap_or_else(|| panic!("unreadable --version output {text:?}"))
        .to_owned();
    assert_eq!(check_pinned_version(&text, &installed).unwrap(), installed);
    let pin = std::env::var("DACS_TEST_OMP_PIN").unwrap_or_else(|_| "18.20.0".into());
    match check_pinned_version(&text, &pin) {
        Ok(_) => {
            assert_eq!(installed, pin.trim_start_matches('v'));
            eprintln!("installed OMP {installed} matches pin {pin}");
        }
        Err(error) => {
            assert_ne!(installed, pin.trim_start_matches('v'));
            assert!(
                error.contains(&installed) && error.contains(&pin),
                "{error}"
            );
            eprintln!("installed OMP {installed} does not match pin {pin}: {error}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_probe_reports_permission_state_read_only() {
    let Some(omp) = omp() else { return };
    if std::env::var_os("DACS_TEST_OMP_DESKTOP").is_none_or(|v| v != "1") {
        eprintln!("skipping desktop capability probe: set DACS_TEST_OMP_DESKTOP=1 to run it");
        return;
    }
    let stub = Stub::start(vec![
        Step::Tool("eval", json!({"language":"js","code":DESKTOP_PROBE})),
        Step::Text("READY"),
    ])
    .await;
    let mut session = Session::start(
        &omp,
        stub.port,
        Launch {
            probe: true,
            desktop: true,
            events: Some(json!([
                "agent_end",
                "notice",
                "tool_execution_start",
                "tool_execution_end"
            ])),
            ..Launch::default()
        },
    )
    .await;
    session
        .request("prompt", json!({"message":"Readiness check only."}))
        .await;
    let OmpEvent::Unknown { raw } = session
        .wait_for(
            "tool_execution_end",
            |e| matches!(e, OmpEvent::Unknown { raw } if raw["type"] == "tool_execution_end"),
        )
        .await
    else {
        unreachable!()
    };
    session.prompt_result().await;
    session.stop().await;
    assert_ne!(raw["isError"], true, "{raw}");
    let text = raw["result"]["content"][0]["text"].as_str().unwrap_or("");
    let caps: Value = text
        .lines()
        .find_map(|l| {
            l.split_once("DACS_DESKTOP_CAPS=")
                .map(|(_, j)| j.trim().to_owned())
        })
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_else(|| panic!("no capability report in {text}"));
    for key in [
        "capture",
        "input",
        "capturePermission",
        "inputPermission",
        "screenLocked",
    ] {
        assert!(caps.get(key).is_some(), "{key} missing from {caps}");
    }
    eprintln!("OMP desktop capabilities: {caps}");
}
