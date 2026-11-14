//! The dashboard: a read-only live view of agents, tasks and the conversation between
//! the coordinator and its agents. Every control lives on the Manage page.
use crate::{form, json_details, page_with};
use dacs_protocol::*;
use leptos::prelude::*;
use serde_json::Value;
use std::collections::HashMap;

/// Seconds between reloads of the live dashboard.
pub const REFRESH_SECONDS: u32 = 5;
/// A worker heard from within this window counts as in contact: twice the longest
/// poll interval a worker accepts.
const CONTACT_SECONDS: i64 = 60;
/// Conversation entries rendered; the full history stays on the Manage page.
const FEED_LIMIT: usize = 100;

struct Names<'a> {
    workers: HashMap<&'a str, &'a str>,
    tasks: HashMap<&'a str, &'a TaskRecord>,
    attempts: HashMap<&'a str, &'a AttemptRecord>,
}

impl<'a> Names<'a> {
    fn new(snapshot: &'a Snapshot) -> Self {
        Self {
            workers: snapshot
                .workers
                .iter()
                .map(|w| (w.id.as_str(), w.registration.name.as_str()))
                .collect(),
            tasks: snapshot.tasks.iter().map(|t| (t.id.as_str(), t)).collect(),
            attempts: snapshot
                .attempts
                .iter()
                .map(|a| (a.id.as_str(), a))
                .collect(),
        }
    }
    fn worker(&self, id: &str) -> String {
        match self.workers.get(id) {
            Some(name) if !name.is_empty() => (*name).to_owned(),
            _ => format!("agent {id}"),
        }
    }
    fn task(&self, id: &str) -> String {
        self.tasks
            .get(id)
            .map_or_else(|| format!("task {id}"), |t| quote(&t.spec.title))
    }
    fn attempt_worker(&self, attempt_id: &str) -> Option<String> {
        self.attempts
            .get(attempt_id)
            .map(|a| self.worker(&a.worker_id))
    }
    fn attempt_task(&self, attempt_id: &str) -> String {
        self.attempts.get(attempt_id).map_or_else(
            || format!("attempt {attempt_id}"),
            |a| self.task(&a.task_id),
        )
    }
}

fn quote(title: &str) -> String {
    format!("“{title}”")
}

fn clip(text: &str) -> String {
    let mut out: String = text.chars().take(240).collect();
    if text.chars().nth(240).is_some() {
        out.push('…');
    }
    out
}

fn humanize(kind: &str) -> String {
    let text = kind.replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

pub(crate) fn ago(now: i64, at: i64) -> String {
    if at <= 0 {
        return "time unknown".into();
    }
    match (now - at).max(0) {
        0..5 => "just now".into(),
        age @ 5..60 => format!("{age} s ago"),
        age @ 60..3600 => format!("{} min ago", age / 60),
        age @ 3600..86400 => format!("{} h ago", age / 3600),
        age => format!("{} d ago", age / 86400),
    }
}

pub(crate) fn clock(at: i64) -> String {
    if at <= 0 {
        return "Time not recorded".into();
    }
    let second = at.rem_euclid(86400);
    format!(
        "{:02}:{:02}:{:02} UTC",
        second / 3600,
        second % 3600 / 60,
        second % 60
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    ToAgent,
    FromAgent,
    Note,
}

struct Entry<'a> {
    at: i64,
    /// Order within one second: a message the coordinator queued precedes reports.
    tie: (u8, i64),
    direction: Direction,
    agent: Option<String>,
    text: String,
    received: Option<bool>,
    payload: &'a Value,
}

fn message_text(message: &ControlMessage, names: &Names) -> String {
    let p = &message.payload;
    let task = || {
        message
            .attempt_id
            .as_deref()
            .map(|id| names.attempt_task(id))
    };
    match message.kind.as_str() {
        "assignment" => format!(
            "Assigned task {}",
            p["task"]["spec"]["title"]
                .as_str()
                .map(quote)
                .or_else(task)
                .unwrap_or_default()
        ),
        "stop" => format!(
            "Stop {}: {}",
            task().unwrap_or_else(|| "work".into()),
            clip(p["reason"].as_str().unwrap_or("no reason recorded"))
        ),
        "pause_desktop" => "Pause desktop control".into(),
        "chat" => format!("Chat: {}", clip(p["text"].as_str().unwrap_or_default())),
        "user_message" => {
            let body = match &p["body"] {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            format!(
                "Message ({}): {}",
                p["kind"].as_str().unwrap_or("untyped"),
                clip(&body)
            )
        }
        "child_result" => format!(
            "Subtask {} ended: {}",
            names.task(p["task_id"].as_str().unwrap_or_default()),
            p["state"].as_str().unwrap_or("state unknown")
        ),
        other => humanize(other),
    }
}

fn worker_event_text(event: &Value, task: &str) -> String {
    let message = || clip(event["message"].as_str().unwrap_or_default());
    match event["type"].as_str().unwrap_or_default() {
        "started" => format!("Started {task}"),
        "progress" => message(),
        "usage" => format!(
            "Used {} input and {} output tokens{}",
            event["input_tokens"].as_u64().unwrap_or(0),
            event["output_tokens"].as_u64().unwrap_or(0),
            if event["estimated"].as_bool().unwrap_or(false) {
                " (estimated)"
            } else {
                ""
            }
        ),
        "settled" => format!("Agent session settled on {task}"),
        "workflow_completed" => format!(
            "Reported workflow {} for {task}",
            if event["success"].as_bool().unwrap_or(false) {
                "success"
            } else {
                "failure"
            }
        ),
        "compiler_telemetry" => format!(
            "Compiler telemetry: {} records",
            event["records"].as_array().map_or(0, Vec::len)
        ),
        "failed" => format!("Failed {task}: {}", message()),
        "cost_capped" => format!("Cost cap reached on {task}: {}", message()),
        "stopped" => format!("Stopped {task}"),
        other => humanize(other),
    }
}

fn status_text(status: &Value) -> String {
    let provider = match status["provider_authenticated"].as_bool() {
        Some(true) => "provider signed in",
        Some(false) => "provider not signed in",
        None => "provider login unknown",
    };
    let network = match status["network_ready"].as_bool() {
        Some(true) => "network ready",
        Some(false) => "network not ready",
        None => "network unknown",
    };
    let desktop = if status["desktop_paused"].as_bool().unwrap_or(false) {
        "desktop paused"
    } else if status["desktop_active"].as_bool().unwrap_or(false) {
        "desktop active"
    } else {
        "desktop inactive"
    };
    format!("Status: {provider} · {network} · {desktop}")
}

/// Who or what a coordinator-side event concerns, from the record it carries.
fn subject(p: &Value, names: &Names) -> Option<String> {
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    text(&p["spec"]["title"])
        .map(|title| quote(&title))
        .or_else(|| text(&p["registration"]["name"]))
        .or_else(|| text(&p["name"]))
        .or_else(|| text(&p["host_name"]))
        .or_else(|| text(&p["worker_id"]).map(|id| names.worker(&id)))
        .or_else(|| {
            text(&p["task_id"])
                .or_else(|| text(&p["submission"]["task_id"]))
                .or_else(|| text(&p["publication"]["task_id"]))
                .map(|id| names.task(&id))
        })
}

/// `None` hides an event: either the queued message already shows it, or it repeats
/// the status the same worker last reported.
fn describe(
    event: &Event,
    names: &Names,
    statuses: &mut HashMap<String, Value>,
) -> Option<(Direction, Option<String>, String)> {
    let p = &event.payload;
    let field = |key: &str| p[key].as_str().unwrap_or_default();
    let from_attempt = |attempt_id: &str, text: String| {
        Some((
            Direction::FromAgent,
            names.attempt_worker(attempt_id),
            text,
        ))
    };
    match event.kind.as_str() {
        "attempt_admitted" | "desktop_pause_requested" => None,
        "worker_event" => {
            let attempt = field("attempt_id");
            from_attempt(
                attempt,
                worker_event_text(&p["event"], &names.attempt_task(attempt)),
            )
        }
        "candidate_submitted" => {
            let attempt = p["submission"]["attempt_id"].as_str().unwrap_or_default();
            from_attempt(
                attempt,
                format!(
                    "Submitted a result for review: {}",
                    names.attempt_task(attempt)
                ),
            )
        }
        "worker_stop_reconciled" => {
            let attempt = field("attempt_id");
            from_attempt(
                attempt,
                format!("Confirmed {} stopped", names.attempt_task(attempt)),
            )
        }
        "child_admitted" => from_attempt(
            field("parent_attempt_id"),
            format!("Delegated subtask {}", names.task(field("child_task_id"))),
        ),
        "runtime_status" => {
            let worker = field("worker_id");
            let mut status = p.clone();
            if let Some(fields) = status.as_object_mut() {
                fields.remove("observed_at");
            }
            if statuses.get(worker) == Some(&status) {
                return None;
            }
            statuses.insert(worker.to_owned(), status);
            Some((
                Direction::FromAgent,
                Some(names.worker(worker)),
                status_text(p),
            ))
        }
        "chat_replied" => Some((
            Direction::FromAgent,
            Some(names.worker(field("worker_id"))),
            if p["failed"] == true {
                format!("Chat not answered: {}", clip(field("reply")))
            } else {
                format!("Chat answer: {}", clip(field("reply")))
            },
        )),
        "enrollment_pending" => Some((
            Direction::FromAgent,
            Some(names.worker(field("worker_id"))),
            "Asked to join; waiting for approval".into(),
        )),
        kind => Some((
            Direction::Note,
            None,
            match subject(p, names) {
                Some(subject) => format!("{} — {subject}", humanize(kind)),
                None => humanize(kind),
            },
        )),
    }
}

/// Queued messages and recorded events as one conversation, newest first.
fn conversation<'a>(snapshot: &'a Snapshot, names: &Names) -> Vec<Entry<'a>> {
    let deliveries: HashMap<i64, &MessageDelivery> = snapshot
        .deliveries
        .iter()
        .map(|d| (d.sequence, d))
        .collect();
    let mut entries: Vec<Entry<'a>> = snapshot
        .messages
        .iter()
        .map(|message| {
            let delivery = deliveries.get(&message.sequence);
            Entry {
                at: delivery.map_or(0, |d| d.queued_at),
                tie: (0, message.sequence),
                direction: Direction::ToAgent,
                agent: Some(names.worker(&message.worker_id)),
                text: message_text(message, names),
                received: delivery.map(|d| d.acknowledged),
                payload: &message.payload,
            }
        })
        .collect();
    let mut statuses = HashMap::new();
    for event in &snapshot.events {
        if let Some((direction, agent, text)) = describe(event, names, &mut statuses) {
            entries.push(Entry {
                at: event.at,
                tie: (1, event.sequence),
                direction,
                agent,
                text,
                received: None,
                payload: &event.payload,
            });
        }
    }
    entries.sort_by_key(|entry| std::cmp::Reverse((entry.at, entry.tie)));
    entries
}

fn entry_view(entry: &Entry, now: i64) -> AnyView {
    let agent = entry
        .agent
        .clone()
        .unwrap_or_else(|| "unknown agent".into());
    let (class, route) = match entry.direction {
        Direction::ToAgent => ("say out", format!("Coordinator → {agent}")),
        Direction::FromAgent => ("say in", format!("{agent} → Coordinator")),
        Direction::Note => ("say note", "Coordinator".to_owned()),
    };
    let receipt = entry.received.map(|received| {
        if received {
            view! { <span class="receipt">"Received by agent"</span> }
        } else {
            view! { <span class="receipt pending">"Not yet received"</span> }
        }
    });
    view! { <li class=class>
        <span class="when" title=clock(entry.at)>{ago(now, entry.at)}</span>
        <span class="route">{route}</span>
        <span class="what">{entry.text.clone()}</span>
        {receipt}
        {json_details("Details", entry.payload)}
    </li> }
    .into_any()
}

/// Attempts the worker still holds: the agent has work it has not finished or handed in.
fn working_on<'a>(worker: &WorkerRecord, snapshot: &'a Snapshot) -> Vec<&'a TaskRecord> {
    snapshot
        .attempts
        .iter()
        .filter(|a| {
            a.worker_id == worker.id
                && matches!(a.state.as_str(), "admitted" | "executing" | "waiting")
        })
        .filter_map(|a| {
            snapshot
                .tasks
                .iter()
                .find(|t| t.id == a.task_id && t.attempt_id.as_deref() == Some(a.id.as_str()))
        })
        .collect()
}

pub(crate) fn in_contact(worker: &WorkerRecord, now: i64) -> bool {
    worker.approved && !worker.revoked && now - worker.last_seen <= CONTACT_SECONDS
}

fn agent_view(worker: &WorkerRecord, snapshot: &Snapshot, now: i64) -> AnyView {
    let working = working_on(worker, snapshot);
    let contact = in_contact(worker, now);
    let (dot, doing) = if worker.revoked {
        ("dot", "Revoked".to_owned())
    } else if !worker.approved {
        ("dot warn", "Waiting for your approval".to_owned())
    } else if !working.is_empty() {
        (
            if contact { "dot ok" } else { "dot" },
            format!(
                "Working on {}",
                working
                    .iter()
                    .map(|t| quote(&t.spec.title))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    } else {
        (if contact { "dot ok" } else { "dot" }, "Idle".to_owned())
    };
    let status = snapshot
        .runtime_status
        .iter()
        .find(|s| s.worker_id == worker.id);
    let mut notes = Vec::new();
    if worker.approved && !worker.revoked && !contact {
        notes.push("no recent contact");
    }
    if worker.approved && !worker.revoked && worker.draining {
        notes.push("draining");
    }
    if status.is_some_and(|s| s.desktop_paused) {
        notes.push("desktop paused");
    }
    if status.is_some_and(|s| s.provider_authenticated == Some(false)) {
        notes.push("provider not signed in");
    }
    let doing = if notes.is_empty() {
        doing
    } else {
        format!("{doing} · {}", notes.join(" · "))
    };
    let review = (!worker.approved && !worker.revoked)
        .then(|| view! { <a href="/manage#workers">"Review"</a> });
    view! { <li>
        <span class=dot aria-hidden="true"></span>
        <span class="name">{worker.registration.name.clone()}</span>
        <span class="doing">{doing}</span>{review}
        <span class="seen" title=clock(worker.last_seen)>{format!("seen {}", ago(now, worker.last_seen))}</span>
    </li> }
    .into_any()
}

fn task_view(task: &TaskRecord, names: &Names) -> AnyView {
    let (label, class) = match task.state {
        TaskState::Waiting => ("Waiting", "badge"),
        TaskState::Ready => ("Ready", "badge"),
        TaskState::Executing => ("Running", "badge ok"),
        TaskState::Validating => ("In review", "badge warn"),
        TaskState::Publishing => ("Publishing", "badge ok"),
        TaskState::Accepted => ("Accepted", "badge ok"),
        TaskState::Failed => ("Failed", "badge bad"),
        TaskState::Cancelled => ("Cancelled", "badge"),
    };
    let mut notes: Vec<String> = Vec::new();
    if let Some(agent) = task
        .attempt_id
        .as_deref()
        .and_then(|id| names.attempt_worker(id))
    {
        notes.push(agent);
    }
    if let Some(reason) = &task.blocked_reason {
        notes.push(clip(reason));
    }
    if task.cancel_requested && task.state != TaskState::Cancelled {
        notes.push("cancellation requested".into());
    }
    view! { <li>
        <span class="name">{task.spec.title.clone()}</span>
        <span class=class>{label}</span>
        <span class="doing">{notes.join(" · ")}</span>
    </li> }
    .into_any()
}

/// `now` is the coordinator clock in Unix seconds. `live` reloads the page every
/// `REFRESH_SECONDS`; a paused page stays still so open details can be read.
pub fn render_dashboard(snapshot: &Snapshot, csrf: &str, now: i64, live: bool) -> String {
    let names = Names::new(snapshot);
    let agents = snapshot
        .workers
        .iter()
        .map(|w| agent_view(w, snapshot, now))
        .collect_view();
    let tasks = snapshot
        .tasks
        .iter()
        .map(|t| task_view(t, &names))
        .collect_view();
    let entries = conversation(snapshot, &names);
    let total = entries.len();
    let feed = entries
        .iter()
        .take(FEED_LIMIT)
        .map(|e| entry_view(e, now))
        .collect_view();
    let count = |state: TaskState| snapshot.tasks.iter().filter(|t| t.state == state).count();
    let enrolled = snapshot.workers.iter().filter(|w| !w.revoked).count();
    let contact = snapshot
        .workers
        .iter()
        .filter(|w| in_contact(w, now))
        .count();
    let attention = snapshot
        .workers
        .iter()
        .filter(|w| !w.approved && !w.revoked)
        .count()
        + count(TaskState::Validating)
        + count(TaskState::Failed);
    let live_control = if live {
        view! { <span class="live">{format!("Live · updates every {REFRESH_SECONDS} s")}</span><a class="button" href="/?live=off">"Pause"</a> }.into_any()
    } else {
        view! { <span class="live paused">"Paused"</span><a class="button" href="/">"Resume live"</a> }.into_any()
    };
    page_with(
        "Dashboard",
        None,
        live.then_some(REFRESH_SECONDS),
        view! {
            <div class="heading"><h1>"Dashboard"</h1><div class="actions">{live_control}<a class="button" href="/manage">"Manage"</a>{form("/ui/logout", csrf, (), "Sign out")}</div></div>
            <dl class="stats">
                <div><dt>"Agents in contact"</dt><dd>{format!("{contact} of {enrolled}")}</dd></div>
                <div><dt>"Tasks running"</dt><dd>{count(TaskState::Executing)}</dd></div>
                <div><dt>"Tasks accepted"</dt><dd>{format!("{} of {}", count(TaskState::Accepted), snapshot.tasks.len())}</dd></div>
                <div><dt>"Needs attention"</dt><dd>{attention}{(attention > 0).then(|| view! { " · "<a href="/manage">"open Manage"</a> })}</dd></div>
            </dl>
            <div class="overview">
                <div>
                    <section id="agents"><h2>"Agents"</h2>
                        {snapshot.workers.is_empty().then(|| view! { <p class="muted">"No agents have joined. Create an invitation in "<a href="/manage#projects">"Manage"</a>"."</p> })}
                        <ul class="rows">{agents}</ul>
                    </section>
                    <section id="tasks"><h2>"Tasks"</h2>
                        {snapshot.tasks.is_empty().then(|| view! { <p class="muted">"No tasks yet. Create one in "<a href="/manage#tasks">"Manage"</a>"."</p> })}
                        <ul class="rows">{tasks}</ul>
                    </section>
                </div>
                <section id="conversation"><h2>"Conversation"</h2>
                    <p>"Newest first. What the coordinator sent to each agent and what each agent reported back."</p>
                    {(total == 0).then(|| view! { <p class="muted">"Nothing has been sent or reported yet."</p> })}
                    <ol class="feed">{feed}</ol>
                    {(total > FEED_LIMIT).then(|| view! { <p class="muted">{format!("Showing the latest {FEED_LIMIT} of {total}. ")}<a href="/manage#events">"Full history"</a></p> })}
                </section>
            </div>
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> Snapshot {
        let mut snapshot = Snapshot::default();
        snapshot.workers.push(WorkerRecord {
            id: "w1".into(),
            project_id: "p".into(),
            registration: WorkerRegistration {
                name: "laptop-b".into(),
                physical_host_id: "host".into(),
                os: "linux".into(),
                architecture: "x86_64".into(),
                capabilities: Vec::new(),
                capacity: Resources::default(),
                omp_version: "1".into(),
                policy_revision: 1,
                owner_consented: true,
                consent_host: "host".into(),
            },
            approved: true,
            revoked: false,
            draining: false,
            last_seen: 1000,
            reserved: Resources::default(),
        });
        snapshot.attempts.push(AttemptRecord {
            id: "a1".into(),
            task_id: "t1".into(),
            worker_id: "w1".into(),
            generation: 1,
            state: "executing".into(),
            lease_expires_at: 2000,
            started_at: 990,
            settled: false,
            pending_children: Vec::new(),
            last_sequence: 1,
            stop_reason: None,
            resources: Resources::default(),
        });
        for (sequence, kind, acknowledged) in [(1, "assignment", true), (2, "stop", false)] {
            snapshot.messages.push(ControlMessage {
                sequence,
                id: format!("m{sequence}"),
                worker_id: "w1".into(),
                attempt_id: Some("a1".into()),
                generation: Some(1),
                kind: kind.into(),
                payload: json!({"task":{"spec":{"title":"Build parser"}},"reason":"<script>bad</script>"}),
            });
            snapshot.deliveries.push(MessageDelivery {
                sequence,
                queued_at: 990 + sequence,
                acknowledged,
            });
        }
        let status = |sequence, at| Event {
            sequence,
            at,
            project_id: Some("p".into()),
            kind: "runtime_status".into(),
            payload: json!({"worker_id":"w1","provider_authenticated":true,"network_ready":true,"desktop_active":false,"desktop_paused":false,"observed_at":at}),
        };
        snapshot.events.extend([
            status(1, 980),
            status(2, 985),
            Event {
                sequence: 3,
                at: 991,
                project_id: Some("p".into()),
                kind: "worker_event".into(),
                payload: json!({"attempt_id":"a1","generation":1,"sequence":1,"event":{"type":"progress","message":"compiling"}}),
            },
        ]);
        snapshot
    }

    #[test]
    fn conversation_attributes_direction_receipt_and_order() {
        let html = render_dashboard(&snapshot(), "csrf", 1000, true);
        let sent = html.find("Coordinator → laptop-b").expect("outgoing route");
        let assigned = html
            .find("Assigned task “Build parser”")
            .expect("assignment");
        let reported = html.find("laptop-b → Coordinator").expect("incoming route");
        let progress = html.find("compiling").expect("progress text");
        // Newest first: the stop (992), then progress and the assignment (both 991,
        // the queued message being the older of the two).
        assert!(sent < reported && reported < progress && progress < assigned);
        assert_eq!(html.matches("Received by agent").count(), 1);
        assert_eq!(html.matches("Not yet received").count(), 1);
        assert_eq!(
            html.matches("Status: provider signed in").count(),
            1,
            "an unchanged status report is not repeated"
        );
        assert!(!html.contains("<script>"));
        for form in html.split("<form ").skip(1) {
            let form = form.split("</form>").next().unwrap();
            assert!(form.contains("name=\"csrf\""), "mutation form missing CSRF");
        }
    }

    #[test]
    fn only_the_live_dashboard_reloads_itself() {
        let live = render_dashboard(&Snapshot::default(), "csrf", 1000, true);
        assert!(live.contains("http-equiv=\"refresh\" content=\"5\""));
        let paused = render_dashboard(&Snapshot::default(), "csrf", 1000, false);
        assert!(!paused.contains("http-equiv"));
        assert!(paused.contains("Resume live"));
    }
}
