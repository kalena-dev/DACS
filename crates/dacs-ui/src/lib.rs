//! Shared, non-hydrated Leptos views. Routes authenticate and authorize every POST,
//! verify the `csrf` field and Origin, then return 303 redirects. Never render secrets
//! in notices. GETs only render; no view performs enrollment, installation or execution.
use dacs_protocol::*;
use leptos::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};

mod chat;
#[cfg(feature = "native")]
pub mod native;
pub mod onboarding;
mod overview;
mod setup;
pub use chat::{THREAD_REFRESH_SECONDS, render_chat, render_chat_thread};
pub use overview::{REFRESH_SECONDS, render_dashboard};
pub use setup::render_setup;
pub const CSS: &str = include_str!("style.css");

pub(crate) fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).expect("DACS protocol values serialize to JSON")
}

pub(crate) fn hidden(name: &'static str, value: impl Into<String>) -> AnyView {
    let value: String = value.into();
    view! { <input type="hidden" name=name value=value/> }.into_any()
}

pub(crate) fn input(
    label: &str,
    name: &'static str,
    kind: &'static str,
    value: impl Into<String>,
    required: bool,
) -> AnyView {
    let value: String = value.into();
    view! { <label><span>{label.to_owned()}</span><input type=kind name=name value=value required=required/></label> }.into_any()
}

pub(crate) fn textarea(
    label: &str,
    name: &'static str,
    value: impl Into<String>,
    required: bool,
) -> AnyView {
    let value: String = value.into();
    view! { <label><span>{label.to_owned()}</span><textarea name=name rows="6" required=required spellcheck="false">{value}</textarea></label> }.into_any()
}

pub(crate) fn checkbox(label: &str, name: &'static str, required: bool) -> AnyView {
    view! { <label class="check"><input type="checkbox" name=name value="true" required=required/><span>{label.to_owned()}</span></label> }.into_any()
}

pub(crate) fn form(
    action: &'static str,
    csrf: &str,
    body: impl IntoView + 'static,
    submit: &str,
) -> AnyView {
    view! { <form method="post" action=action>{hidden("csrf", csrf)}{hidden("command_id", uuid::Uuid::new_v4().to_string())}{body}<button type="submit">{submit.to_owned()}</button></form> }.into_any()
}

fn command_button(command: Command, csrf: &str, label: &str) -> AnyView {
    form(
        "/ui/command",
        csrf,
        hidden("command", pretty(&command)),
        label,
    )
}

pub(crate) fn json_details(label: &str, value: &impl Serialize) -> AnyView {
    view! { <details><summary>{label.to_owned()}</summary><pre>{pretty(value)}</pre></details> }
        .into_any()
}

pub(crate) fn page(title: &str, notice: Option<&str>, body: impl IntoView) -> String {
    page_with(title, notice, None, body)
}

/// `refresh` makes the browser reload the page every that many seconds. It needs no
/// script, so it works under the `script-src 'none'` policy and in the native shell.
pub(crate) fn page_with(
    title: &str,
    notice: Option<&str>,
    refresh: Option<u32>,
    body: impl IntoView,
) -> String {
    let active = match title {
        "Dashboard" => "/",
        "Chat" => "/chat",
        "Manage" | "Candidate source" => "/manage",
        "Compiler pools" => "/builds",
        "Network" => "/network",
        "Local owner onboarding" => "/onboarding",
        "Local setup" => "/setup",
        "Sign in" => "/login",
        _ => "",
    };
    let navigation = [
        ("/", "Dashboard"),
        ("/chat", "Chat"),
        ("/manage", "Manage"),
        ("/builds", "Compiler pools"),
        ("/network", "Network"),
        ("/onboarding", "Join a project"),
        ("/setup", "Local setup"),
        ("/login", "Sign in"),
    ].into_iter().map(|(href, label)| view! {
        <a href=href aria-current=(href == active).then_some("page")>{label}</a>
    }).collect_view();
    let title = format!("{title} · DACS");
    let notice = notice.map(|text| view! { <p class="notice" role="status">{text.to_owned()}</p> });
    let footer = match refresh {
        Some(seconds) => format!("Live view: reloads every {seconds} s. Unknown and not-run states are not successes."),
        None => "State is a server snapshot. Refresh to see new events. Unknown and not-run states are not successes.".to_owned(),
    };
    let refresh = refresh.map(|seconds| view! { <meta http-equiv="refresh" content=seconds.to_string()/> });
    let html = view! {
        <html lang="en">
            <head><meta charset="utf-8"/><meta name="viewport" content="width=device-width, initial-scale=1"/>
                <meta name="referrer" content="strict-origin"/>{refresh}<title>{title}</title><link rel="stylesheet" href="/assets/dacs.css"/>
            </head>
            <body><a class="skip" href="#main">"Skip to content"</a>
                <header class="app-rail"><a class="brand" href="/">"DACS"</a><p class="rail-label">"Workspace"</p><nav aria-label="Primary">{navigation}</nav></header>
                <main id="main" tabindex="-1">{notice}{body}</main>
                <footer class="app-footer">{footer}</footer>
            </body>
        </html>
    }.to_html();
    format!("<!doctype html>{html}")
}

pub fn render_login(error: Option<&str>, csrf: &str) -> String {
    page(
        "Sign in",
        error,
        view! {
            <section class="narrow"><h1>"Sign in to this coordinator"</h1>
                <p>"Use your coordinator or device token. Browser and native app use the same server session. Tokens are never passed in the URL."</p>
                {form("/session", csrf, view! { <label><span>"Access token"</span><input type="password" name="token" autocomplete="current-password" required=true/></label> }, "Sign in")}
                <p>"Signing in does not consent to local administrator access, start a worker or join a network."</p>
            </section>
        },
    )
}

pub fn render_network(status: &Value) -> String {
    let configured = match status.get("configured").and_then(Value::as_bool) {
        Some(true) => "Network adapter configured",
        Some(false) => "Network adapter not configured",
        None => "Network configuration status unavailable",
    };
    page(
        "Network",
        None,
        view! {
            <section><h1>"Network membership"</h1><p>{configured}</p>
                <p>"Configuration and issued grants do not prove membership. Device records show the latest observed membership or an unresolved operation."</p>
                {json_details("Network state and device records", status)}
                <p><a href="/onboarding">"Open local owner onboarding"</a></p>
            </section>
        },
    )
}

pub fn render_compiler_pools(pools: &[dacs_protocol::workflow::PoolOwnership]) -> String {
    let entries = pools.iter().map(|pool| view! {
        <article><h2>{pool.pool_id.clone()}</h2>
            <p>{if pool.released { "All owned execution reconciled; reservation released" } else { "Reservation held until build and every compiler service stop" }}</p>
            <dl><dt>"Project"</dt><dd>{pool.project_id.clone()}</dd>
                <dt>"Build attempt"</dt><dd>{pool.holder_attempt_id.clone()}</dd>
                <dt>"Physical compiler hosts"</dt><dd>{pool.physical_hosts.join(", ")}</dd>
                <dt>"Service attempts"</dt><dd>{pool.service_attempt_ids.join(", ")}</dd></dl>
        </article>
    }).collect_view();
    page(
        "Compiler pools",
        None,
        view! {
            <h1>"Compiler pool reservations"</h1>
            <p>"Reservations describe ownership, not successful remote compilation. Workflow tasks link to immutable result evidence."</p>
            {empty(pools.is_empty(), "No compiler pool has been reserved.")}{entries}
            <p><a href="/builds">"Refresh reservations"</a></p>
        },
    )
}

/// Every coordinator control and the full record of state. The dashboard links here.
pub fn render_manage(snapshot: &Snapshot, notice: Option<&str>, csrf: &str) -> String {
    let projects = snapshot
        .projects
        .iter()
        .map(|p| project_view(p, csrf))
        .collect_view();
    let tasks = snapshot
        .tasks
        .iter()
        .map(|t| task_view(t, csrf))
        .collect_view();
    let workers = snapshot
        .workers
        .iter()
        .map(|w| worker_view(w, csrf))
        .collect_view();
    let attempts = snapshot
        .attempts
        .iter()
        .map(|a| attempt_view(a, csrf))
        .collect_view();
    let candidates = snapshot
        .candidates
        .iter()
        .map(|c| candidate_view(c, csrf))
        .collect_view();
    let publications = snapshot.publications.iter().map(|p| view! {
        <tr><td><code>{p.id.clone()}</code></td><td>{p.task_id.clone()}</td><td>{p.target_branch.clone()}</td>
            <td>{p.expected_head.clone().unwrap_or_else(|| "Empty target".into())}</td><td><code>{p.candidate_sha.clone()}</code></td><td>{p.state.clone()}</td></tr>
    }).collect_view();
    let events = snapshot.events.iter().rev().map(|e| view! {
        <article class="event"><h3>{format!("#{} · {}", e.sequence, e.kind)}</h3><p>{format!("Unix time {} · project {}", e.at, e.project_id.as_deref().unwrap_or("global"))}</p>{json_details("Event payload", &e.payload)}</article>
    }).collect_view();
    let messages = snapshot.messages.iter().map(|message| view! {
        <article><h3>{format!("#{} · {}", message.sequence, message.kind)}</h3><p>{format!("Message {} · worker {} · attempt {} · generation {}", message.id, message.worker_id, message.attempt_id.as_deref().unwrap_or("not attempt-scoped"), message.generation.map(|g| g.to_string()).unwrap_or_else(|| "not attempt-scoped".into()))}</p>{json_details("Durable message payload", &message.payload)}</article>
    }).collect_view();
    let usage = snapshot.usage.iter().map(|record| view! {
        <tr><td>{record.attempt_id.clone()}</td><td>{record.worker_id.clone()}</td><td>{if record.estimated { "Estimated" } else { "Measured / reported" }}</td><td>{record.input_tokens}</td><td>{record.output_tokens}</td><td>{record.cost_usd.map(|cost| format!("${cost:.6}")).unwrap_or_else(|| "Unknown".into())}</td><td>{record.at}</td></tr>
    }).collect_view();
    let runtime = snapshot.runtime_status.iter().map(|status| {
        let boolean = |value: Option<bool>| match value { Some(true) => "Reported ready", Some(false) => "Reported not ready", None => "Unknown / not reported" };
        let queued = snapshot.messages.iter().filter(|m| m.worker_id == status.worker_id && m.kind == "pause_desktop").count();
        view! {
            <article><h3>{format!("Runtime: {}", status.worker_id)}</h3>
                <dl><dt>"Provider authentication"</dt><dd>{boolean(status.provider_authenticated)}</dd><dt>"Network readiness"</dt><dd>{boolean(status.network_ready)}</dd><dt>"Desktop session"</dt><dd>{status.desktop_session.clone().unwrap_or_else(|| "Unknown / not reported".into())}</dd><dt>"Desktop control"</dt><dd>{if status.desktop_paused { "Reported paused" } else if status.desktop_active { "Reported ACTIVE" } else { "Reported inactive" }}</dd><dt>"Observed (Unix seconds)"</dt><dd>{status.observed_at}</dd><dt>"Pause requests in message history"</dt><dd>{queued}" — a queued request is not proof of application"</dd></dl>
                {(!status.desktop_paused).then(|| command_button(Command::RequestDesktopPause { worker_id: status.worker_id.clone() }, csrf, "Request desktop pause"))}
                <p>"Only a fresh worker report confirms a paused desktop. This coordinator request is not the owner's local pause control or local permission consent."</p>
            </article>
        }
    }).collect_view();
    page(
        "Manage",
        notice,
        view! {
            <div class="heading"><h1>"Manage coordinator"</h1><div class="actions"><a class="button" href="/manage">"Refresh state"</a>{form("/ui/logout", csrf, (), "Sign out")}</div></div>
            <nav class="sections" aria-label="Manage sections"><a href="#tasks">"Tasks and DAG"</a><a href="#workers">"Workers"</a><a href="#projects">"Projects"</a><a href="#attempts">"Attempts"</a><a href="#candidates">"Candidates"</a><a href="#publications">"Publication"</a><a href="#messages">"Messages"</a><a href="#events">"Events"</a><a href="#commands">"Commands"</a></nav>
            <aside class="warning"><details><summary>"Full-trust execution · elevated restrictions are not a sandbox"</summary><p>"Elevated restrictions are advisory, not a sandbox. Coordinator approval is separate from local owner consent, network membership, installed requirements and manual OMP login. Local runtime/desktop control and network readiness are unavailable until their adapters report them. No remote pause button is represented as local consent."</p></details></aside>
            <section id="tasks"><h2>{format!("Tasks and dependency graph ({})", snapshot.tasks.len())}</h2><p>"Dependency edges below point from a task to its prerequisites. Only accepted, integrated outputs release dependants."</p>{empty(snapshot.tasks.is_empty(), "No tasks have been created.")}{tasks}
                <details><summary>"Create task with pinned contract"</summary>
                    <p>"Choose a project below for its current contract and policy revisions. Supply instructions, source commit and resources. Dependencies and capabilities are JSON string arrays; effects are workspace or external."</p>
                    {snapshot.projects.iter().map(|p| {
                        let spec = json!({"project_id":p.id,"title":"","instructions":"","source_commit":"","dependencies":[],"required_capabilities":[],"resources":{"agent_slots":1,"cpu_millis":0,"memory_mb":0,"disk_mb":0},"effect_class":"workspace","lease_seconds":300,"max_runtime_seconds":3600,"contract_revision":p.contract_revision,"policy_revision":p.policy_revision,"max_retries":0});
                        view! { <details><summary>{format!("Task for {}", p.name)}</summary>{form("/ui/create-task", csrf, textarea("TaskSpec JSON (review all resource limits)", "spec", pretty(&spec), true), "Create task")}</details> }
                    }).collect_view()}
                </details>
            </section>
            <section id="workers"><h2>{format!("Workers ({})", snapshot.workers.len())}</h2><p>"Reported capacity and held reservations are not measured usage. Reservations on unreachable workers remain held until reconciliation. Heartbeat time alone never permits a retry."</p>{empty(snapshot.workers.is_empty(), "No enrolled workers.")}{workers}<h3>"Runtime and desktop observations"</h3>{empty(snapshot.runtime_status.is_empty(), "Runtime, provider login, network and desktop status: unknown; no adapter observations supplied.")}{runtime}</section>
            <section id="projects"><h2>{format!("Projects ({})", snapshot.projects.len())}</h2>{empty(snapshot.projects.is_empty(), "No authorized projects.")}{projects}
                <details><summary>"Create project"</summary>{form("/ui/create-project", csrf, view! {
                    {input("Name", "name", "text", "", true)}{input("Integration target branch", "target_branch", "text", "main", true)}
                    {input("Maximum tasks", "max_tasks", "number", "", true)}{input("Maximum delegation depth", "max_depth", "number", "", true)}
                }, "Create project")}</details>
            </section>
            <section id="attempts"><h2>{format!("Attempts ({})", snapshot.attempts.len())}</h2>{empty(snapshot.attempts.is_empty(), "No attempts have been admitted.")}{attempts}<p>"CPU utilization: unknown; no CPU telemetry supplied. A settled OMP session is not an accepted result."</p>
                <h3>"Usage observations"</h3>{empty(snapshot.usage.is_empty(), "Token usage and cost: unknown; no usage telemetry supplied.")}
                <div class="table-scroll"><table><caption>"Per-attempt usage records; estimates are not measurements"</caption><thead><tr><th scope="col">"Attempt"</th><th scope="col">"Worker"</th><th scope="col">"Basis"</th><th scope="col">"Input tokens"</th><th scope="col">"Output tokens"</th><th scope="col">"Cost USD"</th><th scope="col">"Observed (Unix seconds)"</th></tr></thead><tbody>{usage}</tbody></table></div>
            </section>
            <section id="candidates"><h2>{format!("Candidates and validation ({})", snapshot.candidates.len())}</h2>{empty(snapshot.candidates.is_empty(), "No submitted candidates; validation not run.")}{candidates}</section>
            <section id="publications"><h2>"Publication and reconciliation"</h2>{empty(snapshot.publications.is_empty(), "No publication intents recorded.")}<div class="table-scroll"><table><caption>"Durable publication intents; Git and SQLite are not an atomic transaction"</caption><thead><tr><th scope="col">"Intent"</th><th scope="col">"Task"</th><th scope="col">"Branch"</th><th scope="col">"Expected head"</th><th scope="col">"Candidate"</th><th scope="col">"State"</th></tr></thead><tbody>{publications}</tbody></table></div></section>
            <section id="messages"><h2>"Messages"</h2><p>"Messages and acknowledgments are durable commands. Delivery evidence appears in the event history; a send acknowledgment is not task completion."</p>
                {empty(snapshot.messages.is_empty(), "No visible durable messages.")}{messages}
                <details><summary>"Send a durable message"</summary>
                {form("/ui/message", csrf, view! {
                    {input("Project ID", "project_id", "text", "", true)}{input("Recipient task ID", "recipient_task_id", "text", "", true)}
                    {input("Unique message ID (reuse only for exact replay)", "message_id", "text", "", true)}{input("Message kind", "kind", "text", "", true)}
                    {textarea("Body (JSON)", "body", "", true)}
                }, "Send message")}
                </details>
            </section>
            <section id="events"><h2>{format!("Event history ({})", snapshot.events.len())}</h2>{empty(snapshot.events.is_empty(), "No recorded events.")}{events}</section>
            <section id="commands"><details class="advanced"><summary>"Advanced protocol controls"</summary><p>"Submit a typed protocol Command. Authorization and current generation checks apply on the server. Worker heartbeat, delegation, candidate submission, reconciliation and acknowledgments require the corresponding device identity; an administrator must not impersonate local owner approval."</p>
                {form("/ui/command", csrf, textarea("Command JSON", "command", "", true), "Submit command")}
                <details><summary>"Supported command types"</summary><p>"create_project, set_requirements, create_invitation, revoke_invitation, approve_worker, revoke_worker, set_draining, heartbeat, create_task, admit_task, delegate, report_event, submit_candidate, review_candidate, cancel_task, abandon_attempt, reconcile_worker, invalidate_contract, send_message, acknowledge_messages, report_runtime_status, request_desktop_pause"</p></details>
            </details></section>
        },
    )
}

fn empty(show: bool, message: &str) -> Option<AnyView> {
    show.then(|| view! { <p class="muted">{message.to_owned()}</p> }.into_any())
}

fn project_view(project: &Project, csrf: &str) -> AnyView {
    view! { <article><h3>{project.name.clone()}</h3><p><code>{project.id.clone()}</code>{format!(" · target {} · contract {} · policy {} · limits {} tasks / depth {}", project.target_branch, project.contract_revision, project.policy_revision, project.max_tasks, project.max_depth)}</p>
        {json_details("Current versioned requirements", &project.requirements)}
        <details><summary>"Edit project requirements"</summary><p>"Changing requirements does not silently install or upgrade tools. Each owner must review the new local plan."</p>
            {form("/ui/requirements", csrf, view! { {hidden("project_id", &project.id)}{textarea("Requirements manifest JSON", "requirements", pretty(&project.requirements), true)} }, "Save requirements")}
        </details>
        <details><summary>"Create named-host invitation"</summary>{form("/ui/invitation", csrf, view! {
            {hidden("project_id", &project.id)}{input("Inviting host name shown to the owner", "host_name", "text", "", true)}{input("Expiry from now (seconds)", "expires_in_seconds", "number", "", true)}
        }, "Create invitation")}</details>
        <details><summary>"Invalidate project contract"</summary><p class="warning">"Invalidates outstanding work under the old revision. Publication already in progress must reconcile first."</p>{command_button(Command::InvalidateContract { project_id: project.id.clone() }, csrf, "Invalidate contract")}</details>
    </article> }.into_any()
}

fn task_view(task: &TaskRecord, csrf: &str) -> AnyView {
    let dependencies = task
        .spec
        .dependencies
        .iter()
        .map(|id| view! { <li><code>{id.clone()}</code></li> })
        .collect_view();
    let terminal = matches!(
        task.state,
        TaskState::Accepted | TaskState::Cancelled | TaskState::Failed
    );
    view! { <article><h3>{task.spec.title.clone()}<span class="badge">{format!("{:?}", task.state)}</span></h3>
        <p><code>{task.id.clone()}</code>{format!(" · project {} · generation {} · {:?} effects", task.spec.project_id, task.generation, task.spec.effect_class)}</p>
        {task.blocked_reason.as_ref().map(|reason| view! { <p class="warning">{reason.clone()}</p> })}
        <details><summary>"Task details and prerequisites"</summary>
        <dl><dt>"Pinned source"</dt><dd><code>{task.spec.source_commit.clone()}</code></dd><dt>"Integrated revision"</dt><dd>{task.integrated_sha.clone().unwrap_or_else(|| "Not integrated".into())}</dd><dt>"Blocked reason"</dt><dd>{task.blocked_reason.clone().unwrap_or_else(|| "None recorded (not a readiness guarantee)".into())}</dd><dt>"Parent"</dt><dd>{task.parent_task_id.clone().unwrap_or_else(|| "Root task".into())}</dd><dt>"Current attempt"</dt><dd>{task.attempt_id.clone().unwrap_or_else(|| "Not admitted".into())}</dd><dt>"Cancellation"</dt><dd>{if task.cancel_requested { "Requested; await terminal outcome" } else { "Not requested" }}</dd></dl>
        <h4>"Prerequisites"</h4>{empty(task.spec.dependencies.is_empty(), "No prerequisite edges.")}<ul>{dependencies}</ul>
        {json_details("Task instructions, resources and pinned revisions", &task.spec)}
        </details>
        {task.workflow_result.as_ref().map(|result| view! {
            <section><h4>"Workflow result"</h4>
                <p>{if result.success { "Recorded successful result" } else { "Recorded failed result" }}</p>
                <p><a href=format!("/api/projects/{}/artifacts/{}", task.spec.project_id, result.artifact_digest)>"Download immutable result evidence"</a></p>
                {checks("Checks reported by the worker", &result.evidence)}
            </section>
        })}
        <div class="actions">{(!terminal).then(|| command_button(Command::AdmitTask { task_id: task.id.clone(), worker_id: None }, csrf, "Request admission"))}
            {(!terminal && !task.cancel_requested).then(|| command_button(Command::CancelTask { task_id: task.id.clone() }, csrf, "Request cancellation"))}</div>
    </article> }.into_any()
}

fn resources(label: &str, r: &Resources) -> AnyView {
    view! { <p class="resources"><strong>{label.to_owned()}</strong><span>{format!("{} agent slots", r.agent_slots)}</span><span>{format!("{} CPU millicores", r.cpu_millis)}</span><span>{format!("{} MiB memory", r.memory_mb)}</span><span>{format!("{} MiB disk", r.disk_mb)}</span></p> }.into_any()
}

fn worker_view(worker: &WorkerRecord, csrf: &str) -> AnyView {
    let r = &worker.registration;
    view! { <article><h3>{r.name.clone()}</h3><p><code>{worker.id.clone()}</code>{format!(" · project {} · physical host {} · {}/{}", worker.project_id, r.physical_host_id, r.os, r.architecture)}</p>
        <dl><dt>"Host approval"</dt><dd>{if worker.revoked { "Revoked" } else if worker.approved { "Approved" } else { "Pending" }}</dd><dt>"Local owner consent"</dt><dd>{if r.owner_consented { format!("Reported for {}", r.consent_host) } else { "Not consented".into() }}</dd><dt>"Admission"</dt><dd>{if worker.draining { "Draining; admitted work retained" } else { "Not draining; other gates still apply" }}</dd><dt>"Last heartbeat (Unix seconds)"</dt><dd>{worker.last_seen}</dd><dt>"OMP version"</dt><dd>{r.omp_version.clone()}</dd><dt>"Capabilities"</dt><dd>{r.capabilities.join(", ")}</dd></dl>
        {resources("Reported total capacity", &r.capacity)}{resources("Held reservations", &worker.reserved)}
        <div class="actions">{(!worker.approved && !worker.revoked).then(|| command_button(Command::ApproveWorker { worker_id: worker.id.clone() }, csrf, "Approve device"))}
            {(!worker.revoked).then(|| command_button(Command::SetDraining { worker_id: worker.id.clone(), draining: !worker.draining }, csrf, if worker.draining { "Resume admission" } else { "Drain worker" }))}
            {(!worker.revoked).then(|| command_button(Command::RevokeWorker { worker_id: worker.id.clone() }, csrf, "Revoke device"))}</div>
    </article> }.into_any()
}

fn attempt_view(attempt: &AttemptRecord, csrf: &str) -> AnyView {
    view! { <article><h3><code>{attempt.id.clone()}</code><span class="badge">{attempt.state.clone()}</span></h3>
        <p>{format!("Task {} · worker {} · generation {} · last event {}", attempt.task_id, attempt.worker_id, attempt.generation, attempt.last_sequence)}</p>
        <dl><dt>"Started / lease expiry (Unix seconds)"</dt><dd>{format!("{} / {}", attempt.started_at, attempt.lease_expires_at)}</dd><dt>"OMP session"</dt><dd>{if attempt.settled { "Settled; obligations may remain" } else { "Not settled" }}</dd><dt>"Stop reason"</dt><dd>{attempt.stop_reason.clone().unwrap_or_else(|| "None recorded".into())}</dd><dt>"Pending children"</dt><dd>{if attempt.pending_children.is_empty() { "None recorded".into() } else { attempt.pending_children.join(", ") }}</dd></dl>
        {resources("Attempt reservation", &attempt.resources)}
        <details><summary>"Explicitly abandon uncertain attempt"</summary><p class="warning">"Abandonment is not proof that an unreachable process stopped. External effects require human investigation; held reservations need worker reconciliation."</p>
            {form("/ui/command", csrf, textarea("AbandonAttempt JSON: enter an investigation reason", "command", pretty(&Command::AbandonAttempt { attempt_id: attempt.id.clone(), reason: String::new() }), true), "Abandon attempt")}
        </details>
    </article> }.into_any()
}

fn checks(title: &str, evidence: &[CheckEvidence]) -> AnyView {
    view! { <h4>{title.to_owned()}</h4>{empty(evidence.is_empty(), "Not run / no evidence supplied.")}
        <ul>{evidence.iter().map(|e| view! { <li><strong>{e.name.clone()}</strong>{if e.passed { " — passed" } else { " — failed" }}<p><code>{e.command.clone()}</code></p><p>{format!("Source {} · artifact {}", e.source_sha, e.artifact_digest.as_deref().unwrap_or("not supplied"))}</p></li> }).collect_view()}</ul>
    }.into_any()
}

fn candidate_view(candidate: &CandidateRecord, csrf: &str) -> AnyView {
    let submission = &candidate.submission;
    let review = candidate.decision.as_ref().map(|d| view! {
        <h4>"Coordinator decision"</h4><p>{if d.approve { "Approved for the recorded exact SHA" } else { "Rejected" }}</p><p>{d.reason.clone()}</p><p><code>{d.candidate_sha.clone()}</code></p>
        {checks("Coordinator checks", &d.checks_run)}<h4>"Explicitly skipped checks"</h4>{empty(d.skipped_checks.is_empty(), "None listed; absence is not evidence of execution.")}<ul>{d.skipped_checks.iter().map(|s| view! { <li>{s.clone()}</li> }).collect_view()}</ul>
    });
    view! { <article><h3><code>{candidate.id.clone()}</code></h3><p>{format!("Task {} · attempt {} · generation {} · contract {} · policy {}", submission.task_id, submission.attempt_id, submission.generation, submission.contract_revision, submission.policy_revision)}</p>
        <dl><dt>"Submitted commit"</dt><dd><code>{submission.commit_sha.clone()}</code></dd><dt>"Prepared integration SHA"</dt><dd>{candidate.integration_sha.clone().unwrap_or_else(|| "Not prepared".into())}</dd><dt>"Expected target head"</dt><dd>{candidate.expected_head.clone().unwrap_or_else(|| "Not recorded / empty target".into())}</dd><dt>"Publication"</dt><dd>{candidate.publication_id.clone().unwrap_or_else(|| "Not published".into())}</dd></dl>
        <p><a href=format!("/candidates/{}", candidate.id)>"Inspect source diff"</a></p>
        {checks("Submitted check evidence", &submission.evidence)}<h4>"Unresolved findings"</h4>{empty(submission.unresolved.is_empty(), "None reported (not proof of correctness).")}<ul>{submission.unresolved.iter().map(|u| view! { <li>{u.clone()}</li> }).collect_view()}</ul>
        {review}{empty(candidate.decision.is_none(), "Not reviewed.")}
        <div class="actions">{form("/ui/stage", csrf, hidden("candidate_id", &candidate.id), "Prepare against current target")}</div>
        {candidate.integration_sha.as_ref().map(|sha| view! {
            <details><summary>"Record exact-SHA review"</summary><p>"Record only checks actually run against this integration SHA; list every skipped required check explicitly."</p>
                {form("/ui/review", csrf, view! {
                    {hidden("candidate_id", &candidate.id)}{hidden("candidate_sha", sha)}
                    <label><span>"Decision"</span><select name="approve" required=true><option value="">"Select decision"</option><option value="false">"Reject"</option><option value="true">"Approve exact integration SHA"</option></select></label>
                    {textarea("Reason", "reason", "", true)}{textarea("Checks actually run (CheckEvidence JSON array)", "checks_run", "[]", true)}{textarea("Skipped checks (one per line)", "skipped_checks", "", false)}
                }, "Record review")}
            </details>
        })}
        {candidate.decision.as_ref().filter(|d| d.approve).filter(|d| candidate.integration_sha.as_ref() == Some(&d.candidate_sha)).filter(|_| candidate.publication_id.is_none()).map(|_| form("/ui/publish", csrf, hidden("candidate_id", &candidate.id), "Publish approved exact SHA"))}
    </article> }.into_any()
}

pub fn render_candidate(candidate: &CandidateRecord, diff: &Value, csrf: &str) -> String {
    page(
        "Candidate source",
        None,
        view! {
            <h1>"Candidate source"</h1>
            {candidate_view(candidate, csrf)}
            <section><h2>"Source comparison"</h2>
                <p>{format!("Base: {} · compared commit: {}", diff["base_sha"].as_str().unwrap_or("unknown"), diff["integration_sha"].as_str().or_else(|| diff["commit_sha"].as_str()).unwrap_or("unknown"))}</p>
                {diff["truncated"].as_bool().unwrap_or(false).then(|| view! { <p class="warning">"The source comparison exceeds the display limit. Export the source bundle to inspect the full change before approving."</p> })}
                <pre>{diff["stat"].as_str().unwrap_or("").to_owned()}</pre>
                <pre>{diff["diff"].as_str().unwrap_or("").to_owned()}</pre>
            </section>
        },
    )
}

pub fn render_invitation(
    invitation: &Invitation,
    coordinator_name: &str,
    notice: Option<&str>,
    _csrf: &str,
) -> String {
    page(
        "Invitation review",
        notice,
        view! {
            <h1>"Review invitation — not enrolled"</h1><p>{format!("Coordinator: {coordinator_name}")}</p>
            <aside class="warning"><h2>{format!("Inviting host: {}", invitation.host_name)}</h2><p>"Accepting local execution can give this named host administrator-level access using your identity, profile and interactive desktop. Elevated restrictions are advisory, not a sandbox. Review and approve locally before any installation, network change or execution."</p></aside>
            <dl><dt>"Project"</dt><dd>{invitation.project_id.clone()}</dd><dt>"Invitation status"</dt><dd>{if invitation.revoked { "Revoked — cannot enroll" } else { "Expiry and eligibility checked by the server at enrollment" }}</dd><dt>"Expires (Unix seconds)"</dt><dd>{invitation.expires_at}</dd></dl>
            <h2>"Host requirements"</h2>{json_details("Review exact versioned manifest", &invitation.requirements)}
            <p>"Tool detection: not run on this device. Required, already-present, missing and incompatible states appear only after authenticated local detection and planning. All eight optional catalogue categories remain available in local setup. Cargo owns project crate dependencies."</p>
            <h2>"Separate consent and readiness gates"</h2><ol><li>"Open authenticated local setup on this PC and review the named host, exact invitation requirements and plan digest."</li><li>"Explicitly approve selected tools and administrator access locally. Declined required tools disable affected capabilities."</li><li>"Individual host approval, network enrollment and manual worker launch are separate operations. Changing Tailscale control servers may displace an existing login."</li><li>"Complete pinned OMP readiness and manual provider login locally; no provider credentials are sent to this page."</li></ol>
            <p class="notice">"Opening this URL enrolls nothing, executes nothing, grants no access and starts no worker. The local runtime/network adapters must complete their own gates; this page does not claim they exist."</p>
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_is_password_post_without_token_value() {
        let html = render_login(None, "csrf-secret");
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("action=\"/session\""));
        assert!(html.contains("type=\"password\""));
        assert!(html.contains("name=\"csrf\""));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn hostile_notice_and_csrf_are_escaped() {
        let html = render_login(
            Some("<script>alert(1)</script>"),
            "\"/><script>bad</script>",
        );
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn empty_snapshot_does_not_claim_execution() {
        let html = render_manage(&Snapshot::default(), None, "csrf");
        assert!(html.contains("No attempts have been admitted."));
        assert!(html.contains("validation not run"));
        assert!(html.contains("unknown; no usage telemetry supplied"));
    }

    #[test]
    fn invitation_never_embeds_its_secret_or_enrollment_action() {
        let invite = Invitation {
            token: "never-echo-this".into(),
            project_id: "project".into(),
            host_name: "<b>host</b>".into(),
            expires_at: 1,
            requirements: json!({}),
            revoked: true,
        };
        let html = render_invitation(&invite, "coordinator", None, "csrf");
        assert!(!html.contains("never-echo-this"));
        assert!(!html.contains("<b>host</b>"));
        assert!(!html.contains("<form"));
        assert!(html.contains("Revoked"));
    }

    #[test]
    fn telemetry_distinguishes_estimate_and_pending_pause() {
        let mut snapshot = Snapshot::default();
        snapshot.usage.push(UsageRecord {
            attempt_id: "attempt".into(),
            worker_id: "worker".into(),
            input_tokens: 41,
            output_tokens: 7,
            cost_usd: None,
            estimated: true,
            at: 3,
        });
        snapshot.runtime_status.push(WorkerRuntimeStatus {
            worker_id: "worker".into(),
            provider_authenticated: None,
            network_ready: Some(false),
            desktop_session: Some("local-session".into()),
            desktop_active: true,
            desktop_paused: false,
            observed_at: 4,
        });
        snapshot.messages.push(ControlMessage {
            sequence: 1,
            id: "message".into(),
            worker_id: "worker".into(),
            attempt_id: None,
            generation: None,
            kind: "pause_desktop".into(),
            payload: json!({"note":"<script>bad</script>"}),
        });
        let html = render_manage(&snapshot, None, "csrf");
        assert!(html.contains("Estimated"));
        assert!(html.contains("Reported ACTIVE"));
        assert!(html.contains("request_desktop_pause"));
        assert!(html.contains("not proof of application"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("Reported paused"));
        for form in html.split("<form ").skip(1) {
            let form = form.split("</form>").next().unwrap();
            assert!(form.contains("name=\"csrf\""), "mutation form missing CSRF");
        }
    }

    #[test]
    fn confirmed_pause_has_no_duplicate_pause_button() {
        let mut snapshot = Snapshot::default();
        snapshot.runtime_status.push(WorkerRuntimeStatus {
            worker_id: "worker".into(),
            provider_authenticated: Some(true),
            network_ready: Some(true),
            desktop_session: None,
            desktop_active: false,
            desktop_paused: true,
            observed_at: 4,
        });
        let html = render_manage(&snapshot, None, "csrf");
        assert!(html.contains("Reported paused"));
        assert!(!html.contains(">Request desktop pause</button>"));
    }
}
