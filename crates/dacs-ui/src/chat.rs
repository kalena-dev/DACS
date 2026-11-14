//! Direct chat between the coordinator's administrator and one device's agent.
//!
//! The page is two documents so it works without script: the outer page holds the
//! message form and never reloads by itself, so typing is not interrupted; the
//! conversation is a framed document that reloads every `THREAD_REFRESH_SECONDS`.
use crate::overview::{ago, clock, in_contact};
use crate::{hidden, page_with};
use dacs_protocol::*;
use leptos::prelude::*;

/// Seconds between reloads of the framed conversation.
pub const THREAD_REFRESH_SECONDS: u32 = 2;

/// Devices the administrator may chat with: approved and not revoked.
fn reachable(snapshot: &Snapshot) -> impl Iterator<Item = &WorkerRecord> {
    snapshot.workers.iter().filter(|w| w.approved && !w.revoked)
}

/// The requested device when it can be chatted with, otherwise the first that can.
fn selected<'a>(snapshot: &'a Snapshot, requested: Option<&str>) -> Option<&'a WorkerRecord> {
    reachable(snapshot)
        .find(|w| Some(w.id.as_str()) == requested)
        .or_else(|| reachable(snapshot).next())
}

fn contact_note(worker: &WorkerRecord, now: i64) -> &'static str {
    if !in_contact(worker, now) {
        "No recent contact: messages wait until its worker runs"
    } else if worker.draining {
        "In contact but draining: messages wait until its worker is started again"
    } else {
        "In contact"
    }
}

/// `now` is the coordinator clock in Unix seconds.
pub fn render_chat(
    snapshot: &Snapshot,
    requested: Option<&str>,
    notice: Option<&str>,
    csrf: &str,
    now: i64,
) -> String {
    let Some(worker) = selected(snapshot, requested) else {
        return page_with(
            "Chat",
            notice,
            None,
            view! {
                <div class="heading"><h1>"Chat"</h1></div>
                <p>"No approved device to chat with yet. Approve one in "<a href="/manage#workers">"Manage"</a>"."</p>
            },
        );
    };
    let name = worker.registration.name.clone();
    let others = (reachable(snapshot).count() > 1).then(|| {
        reachable(snapshot)
            .map(|w| {
                view! { <a class="button" href=format!("/chat?worker={}", w.id) aria-current=(w.id == worker.id).then_some("page")>{w.registration.name.clone()}</a> }
            })
            .collect_view()
    });
    let dot = if in_contact(worker, now) && !worker.draining {
        "dot ok"
    } else {
        "dot"
    };
    page_with(
        "Chat",
        notice,
        None,
        view! {
            <div class="heading"><h1>"Chat"</h1><div class="actions">{others}</div></div>
            <p>"Message the agent on a device directly. It answers from one ongoing conversation on that device, outside any task; each message is one agent turn there, with the device owner's tools and login."</p>
            <ul class="rows"><li>
                <span class=dot aria-hidden="true"></span>
                <span class="name">{name.clone()}</span>
                <span class="doing">{contact_note(worker, now)}</span>
                <span class="seen" title=clock(worker.last_seen)>{format!("seen {}", ago(now, worker.last_seen))}</span>
            </li></ul>
            <iframe class="chat-frame" src=format!("/chat/thread?worker={}", worker.id) title=format!("Conversation with {name}")></iframe>
            <form class="chat-form" method="post" action="/ui/chat">
                {hidden("csrf", csrf)}{hidden("command_id", uuid::Uuid::new_v4().to_string())}{hidden("worker_id", worker.id.clone())}
                <label><span>{format!("Message to {name}")}</span><input type="text" name="text" required autofocus maxlength="4000" autocomplete="off"/></label>
                <button type="submit">"Send"</button>
            </form>
        },
    )
}

fn entry(class: &'static str, at: i64, now: i64, route: String, text: String) -> AnyView {
    view! { <li class=class>
        <span class="when" title=clock(at)>{ago(now, at)}</span>
        <span class="route">{route}</span>
        <span class="what">{text}</span>
    </li> }
    .into_any()
}

/// The framed conversation with one device. Entries are rendered newest first and
/// laid out bottom-up, so the view opens at the latest message.
pub fn render_chat_thread(snapshot: &Snapshot, requested: Option<&str>, now: i64) -> String {
    let worker = selected(snapshot, requested);
    let name = worker.map_or_else(|| "device".to_owned(), |w| w.registration.name.clone());
    let chats: Vec<&ChatMessage> = snapshot
        .chats
        .iter()
        .filter(|c| worker.is_some_and(|w| w.id == c.worker_id))
        .collect();
    let delivered = |chat: &ChatMessage| {
        snapshot
            .messages
            .iter()
            .zip(&snapshot.deliveries)
            .any(|(message, delivery)| message.id == chat.id && delivery.acknowledged)
    };
    let entries = chats
        .iter()
        .rev()
        .map(|chat| {
            let answer = match (&chat.reply, chat.failed) {
                (Some(reply), false) => Some(entry(
                    "say in",
                    chat.replied_at.unwrap_or(0),
                    now,
                    name.clone(),
                    reply.clone(),
                )),
                (Some(reason), true) => Some(entry(
                    "say failed",
                    chat.replied_at.unwrap_or(0),
                    now,
                    format!("{name} · no answer"),
                    reason.clone(),
                )),
                (None, _) if delivered(chat) => Some(entry(
                    "say note",
                    chat.sent_at,
                    now,
                    name.clone(),
                    "Working on an answer…".to_owned(),
                )),
                (None, _) => None,
            };
            let receipt = if delivered(chat) {
                view! { <span class="receipt">"Received by agent"</span> }
            } else {
                view! { <span class="receipt pending">"Not yet received"</span> }
            };
            view! {
                {answer}
                <li class="say out">
                    <span class="when" title=clock(chat.sent_at)>{ago(now, chat.sent_at)}</span>
                    <span class="route">{format!("You → {name}")}</span>
                    <span class="what">{chat.text.clone()}</span>
                    {receipt}
                </li>
            }
        })
        .collect_view();
    let empty = chats.is_empty().then(|| {
        view! { <li class="say note"><span class="what">"No messages yet. Send the first one below."</span></li> }
    });
    let html = view! {
        <html lang="en">
            <head><meta charset="utf-8"/><meta name="referrer" content="strict-origin"/>
                <meta http-equiv="refresh" content=THREAD_REFRESH_SECONDS.to_string()/>
                <title>{format!("Conversation with {name} · DACS")}</title><link rel="stylesheet" href="/assets/dacs.css"/>
            </head>
            <body class="chat-thread"><ol class="chat-log">{entries}{empty}</ol></body>
        </html>
    }
    .to_html();
    format!("<!doctype html>{html}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> Snapshot {
        let worker = |id: &str, name: &str| WorkerRecord {
            id: id.into(),
            project_id: "p".into(),
            registration: WorkerRegistration {
                name: name.into(),
                physical_host_id: id.into(),
                os: "windows".into(),
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
        };
        let chat = |id: &str, worker: &str, text: &str, reply: Option<(&str, bool)>| ChatMessage {
            id: id.into(),
            worker_id: worker.into(),
            text: text.into(),
            sent_at: 990,
            reply: reply.map(|(text, _)| text.into()),
            failed: reply.is_some_and(|(_, failed)| failed),
            replied_at: reply.map(|_| 995),
        };
        let message = |sequence: i64, id: &str| ControlMessage {
            sequence,
            id: id.into(),
            worker_id: "b".into(),
            attempt_id: None,
            generation: None,
            kind: "chat".into(),
            payload: json!({}),
        };
        let delivery = |sequence: i64, acknowledged: bool| MessageDelivery {
            sequence,
            queued_at: 990,
            acknowledged,
        };
        Snapshot {
            workers: vec![worker("b", "machine-b"), worker("c", "machine-c")],
            chats: vec![
                chat("one", "b", "first <script>bad</script>", Some(("answer one", false))),
                chat("two", "b", "second", Some(("provider login expired", true))),
                chat("three", "b", "third", None),
                chat("four", "b", "fourth", None),
                chat("other", "c", "for machine-c only", None),
            ],
            messages: vec![message(1, "one"), message(2, "two"), message(3, "three"), message(4, "four")],
            deliveries: vec![delivery(1, true), delivery(2, true), delivery(3, true), delivery(4, false)],
            ..Snapshot::default()
        }
    }

    #[test]
    fn thread_shows_one_device_newest_first_with_each_message_state() {
        let html = render_chat_thread(&snapshot(), Some("b"), 1000);
        assert!(!html.contains("<script"));
        assert!(!html.contains("for machine-c only"));
        assert!(html.contains("http-equiv=\"refresh\" content=\"2\""));
        let at = |needle: &str| html.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        // Newest first; within one exchange the answer precedes its message.
        assert!(at("fourth") < at("third") && at("third") < at("second"));
        assert!(at("answer one") < at("first "));
        assert!(html.contains("machine-b · no answer") && html.contains("provider login expired"));
        // Delivered without an answer yet is in progress; undelivered is not.
        assert_eq!(html.matches("Working on an answer").count(), 1);
        assert_eq!(html.matches("Not yet received").count(), 1);
        assert!(at("Not yet received") < at("Working on an answer"));
    }

    #[test]
    fn page_targets_the_requested_device_and_falls_back_to_the_first() {
        let page = render_chat(&snapshot(), Some("c"), None, "csrf", 1000);
        assert!(!page.contains("<script"));
        assert!(!page.contains("http-equiv"));
        assert!(page.contains("src=\"/chat/thread?worker=c\""));
        assert!(page.contains("name=\"worker_id\" value=\"c\""));
        let fallback = render_chat(&snapshot(), Some("revoked-or-unknown"), None, "csrf", 1000);
        assert!(fallback.contains("src=\"/chat/thread?worker=b\""));
        let none = render_chat(&Snapshot::default(), None, None, "csrf", 1000);
        assert!(none.contains("No approved device") && !none.contains("<form"));
    }
}
