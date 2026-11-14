//! Item 3: issued pre-auth keys are single-use, non-reusable, short-expiry and
//! tagged, and are issued only for an approved device whose consent matches
//! the named host and the current disclosure revision.

mod common;

use axum::http::StatusCode;
use common::*;
use dacs_net_contract::{CONTRACT_REVISION, EnrollmentGrant};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn create_requests(h: &Harness) -> Vec<Recorded> {
    h.hs.requests()
        .into_iter()
        .filter(|r| r.op == Some(Op::CreateKey))
        .collect()
}

#[tokio::test]
async fn headscale_is_asked_for_a_single_use_tagged_short_lived_key() {
    let h = Harness::start().await;
    let before = OffsetDateTime::now_utc();
    // Duplicate and unordered roles collapse to one sorted tag set.
    let req = enrollment_request("alice-pc", &["compile_server", "worker", "compile_server"]);
    let (s, body) = h.enroll(&req).await;
    let after = OffsetDateTime::now_utc();
    assert_eq!(s, StatusCode::OK, "{body}");
    let grant: EnrollmentGrant = serde_json::from_str(&body).unwrap();

    let creates = create_requests(&h);
    assert_eq!(creates.len(), 1);
    let sent: Value = serde_json::from_str(&creates[0].body).unwrap();
    assert_eq!(sent["reusable"], false, "{sent}");
    assert_eq!(sent["ephemeral"], false, "{sent}");
    assert_eq!(
        sent["aclTags"],
        serde_json::json!(["tag:dacs-worker", "tag:dacs-sccache-server"])
    );
    assert!(sent.get("user").is_none(), "tags-only key must not name a user: {sent}");
    let exp = OffsetDateTime::parse(sent["expiration"].as_str().unwrap(), &Rfc3339).unwrap();
    let ttl = time::Duration::minutes(15);
    assert!(exp >= before + ttl && exp <= after + ttl, "expiration {exp} not now+15m");
    assert_eq!(
        creates[0].authorization.as_deref(),
        Some(format!("Bearer {}", h.hs.api_key).as_str())
    );

    // The grant describes exactly that key.
    let key = h.hs.key(grant.auth_key_id).unwrap();
    assert_eq!(grant.auth_key.expose(), key.secret);
    assert!(!key.reusable && !key.ephemeral && !key.used);
    assert_eq!(grant.tags, vec!["tag:dacs-worker", "tag:dacs-sccache-server"]);
    assert_eq!(grant.tags, key.tags);
    assert_eq!(grant.expires_at, exp);
    assert_eq!(Some(grant.expires_at), key.expiration);
    assert_eq!(grant.contract_revision, CONTRACT_REVISION);
    assert_eq!(grant.device_id.0, "dev-alice-pc");
    assert_eq!(grant.host_name.as_str(), "alice-pc");
    assert_eq!(grant.login_server, LOGIN_SERVER);
    assert_eq!(grant.coordinator.overlay_url, OVERLAY_URL);
}

#[tokio::test]
async fn key_ttl_follows_configuration() {
    let h = Harness::start_with(settings_with_ttl(1)).await;
    let before = OffsetDateTime::now_utc();
    let (s, body) = h.enroll(&enrollment_request("ttl-pc", &["worker"])).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let grant: EnrollmentGrant = serde_json::from_str(&body).unwrap();
    let left = grant.expires_at - before;
    assert!(left <= time::Duration::seconds(61) && left >= time::Duration::seconds(59), "{left}");
    let (_, body) = h.op("GET", "/v1/contract", None).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["key_ttl_minutes"], 1);
}

#[tokio::test]
async fn each_grant_is_unique_and_its_key_admits_exactly_one_node() {
    let h = Harness::start().await;
    let (_, a) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    let (_, b) = h.enroll(&enrollment_request("bob-pc", &["worker"])).await;
    let a: EnrollmentGrant = serde_json::from_str(&a).unwrap();
    let b: EnrollmentGrant = serde_json::from_str(&b).unwrap();
    assert_ne!(a.auth_key_id, b.auth_key_id);
    assert_ne!(a.auth_key.expose(), b.auth_key.expose());

    h.hs.register(a.auth_key.expose(), "alice-os", "100.64.0.5").unwrap();
    assert_eq!(
        h.hs.register(a.auth_key.expose(), "intruder", "100.64.0.66").unwrap_err(),
        "authkey already used"
    );
    // Once past its expiry the key admits nobody.
    h.hs.lock()
        .keys
        .iter_mut()
        .find(|k| k.id == b.auth_key_id)
        .unwrap()
        .expiration = Some(OffsetDateTime::now_utc() - time::Duration::seconds(1));
    assert_eq!(
        h.hs.register(b.auth_key.expose(), "bob-os", "100.64.0.6").unwrap_err(),
        "authkey expired"
    );
    assert_eq!(h.hs.lock().nodes.len(), 1);
}

/// Headscale answered a single-use/tagged/short-lived request with a key
/// that is not. netadmin must refuse it (502), not hand out its secret, and
/// expire it so it cannot be used.
async fn assert_refused_and_expired(tamper: fn(&mut Key), what: &str) {
    let h = Harness::start().await;
    h.hs.lock().tamper_key = Some(tamper);
    let (s, body) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{what}: {body}");
    assert_eq!(api_error(&body).code, "headscale_unavailable", "{what}");
    assert_no_secrets(&body, &h.hs.secrets(), what);
    let key = h.hs.lock().keys[0].clone();
    assert!(
        key.expiration.is_some_and(|e| e <= OffsetDateTime::now_utc()),
        "{what}: refused key {} left usable (expiration {:?})",
        key.id,
        key.expiration
    );
    let records = h.audit_records();
    assert!(
        !records.iter().any(|r| r["op"] == "enroll"),
        "{what}: refusal audited as a successful enrollment"
    );
    let refused: Vec<_> = records.iter().filter(|r| r["op"] == "enroll_refused").collect();
    assert_eq!(refused.len(), 1, "{what}: refusal not audited");
    assert_eq!(refused[0]["detail"]["auth_key_id"], key.id);
    assert_eq!(refused[0]["detail"]["key_expired"], true);
    assert_no_secrets(&h.audit(), &h.hs.secrets(), what);
}

#[tokio::test]
async fn failed_cleanup_of_a_refused_key_is_reported_and_audited_for_reconciliation() {
    let h = Harness::start().await;
    h.hs.lock().force_reusable = true;
    h.hs.fault(Op::ExpireKey, Fault::Status(500, "database is locked".into()));
    let (s, body) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{body}");
    assert_no_secrets(&body, &h.hs.secrets(), "refusal body");
    let key = h.hs.lock().keys[0].clone();
    assert!(key.expiration > Some(OffsetDateTime::now_utc()), "the mock really kept it live");
    let records = h.audit_records();
    let refused: Vec<_> = records.iter().filter(|r| r["op"] == "enroll_refused").collect();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["detail"]["auth_key_id"], key.id);
    assert_eq!(refused[0]["detail"]["key_expired"], false, "cleanup claimed but failed");
    assert_eq!(refused[0]["detail"]["device_id"], "dev-alice-pc");
    assert_no_secrets(&h.audit(), &h.hs.secrets(), "audit");
}

#[tokio::test]
async fn reusable_key_is_refused_and_expired() {
    assert_refused_and_expired(|k| k.reusable = true, "reusable").await;
}

#[tokio::test]
async fn ephemeral_key_is_refused_and_expired() {
    assert_refused_and_expired(|k| k.ephemeral = true, "ephemeral").await;
}

#[tokio::test]
async fn key_without_secret_is_refused_and_expired() {
    assert_refused_and_expired(|k| k.secret.clear(), "empty secret").await;
}

#[tokio::test]
async fn key_with_other_tags_is_refused_and_expired() {
    assert_refused_and_expired(
        |k| k.tags = vec!["tag:dacs-worker".into(), "tag:dacs-infra".into()],
        "extra infra tag",
    )
    .await;
    assert_refused_and_expired(|k| k.tags.clear(), "untagged").await;
}

#[tokio::test]
async fn key_outliving_the_requested_ttl_is_refused_and_expired() {
    assert_refused_and_expired(
        |k| k.expiration = Some(OffsetDateTime::now_utc() + time::Duration::days(90)),
        "90-day key",
    )
    .await;
    assert_refused_and_expired(|k| k.expiration = None, "non-expiring key").await;
}

#[tokio::test]
async fn harmless_differences_in_the_returned_key_are_accepted() {
    let h = Harness::start().await;
    // Reordered tags and an expiry truncated to whole seconds are the same key.
    h.hs.lock().tamper_key = Some(|k| {
        k.tags.reverse();
        k.expiration = k.expiration.map(|e| e.replace_nanosecond(0).unwrap());
    });
    let (s, body) = h
        .enroll(&enrollment_request("alice-pc", &["worker", "coordinator"]))
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

// ---- consent and approval gate issuance -----------------------------------

fn set(v: &mut Value, path: &[&str], value: Value) {
    let mut cur = v;
    for p in &path[..path.len() - 1] {
        cur = &mut cur[*p];
    }
    cur[path[path.len() - 1]] = value;
}

fn remove(v: &mut Value, path: &[&str]) {
    let mut cur = v;
    for p in &path[..path.len() - 1] {
        cur = &mut cur[*p];
    }
    cur.as_object_mut().unwrap().remove(path[path.len() - 1]);
}

#[tokio::test]
async fn no_key_is_issued_without_matching_consent_and_approval() {
    let h = Harness::start().await;
    let future = (OffsetDateTime::now_utc() + time::Duration::hours(1))
        .format(&Rfc3339)
        .unwrap();
    type Edit = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Edit, StatusCode)> = vec![
        ("consent for another host", Box::new(|v| set(v, &["consent", "accepted_host"], "bob-pc".into())), StatusCode::BAD_REQUEST),
        ("consent for another device", Box::new(|v| set(v, &["consent", "device_id"], "dev-bob-pc".into())), StatusCode::BAD_REQUEST),
        ("approval for another device", Box::new(|v| set(v, &["approval", "device_id"], "dev-bob-pc".into())), StatusCode::BAD_REQUEST),
        ("request for another device", Box::new(|v| set(v, &["device_id"], "dev-bob-pc".into())), StatusCode::BAD_REQUEST),
        ("request for another host", Box::new(|v| set(v, &["host_name"], "bob-pc".into())), StatusCode::BAD_REQUEST),
        ("stale disclosure", Box::new(|v| set(v, &["consent", "disclosure_revision"], "host-admin-access/0".into())), StatusCode::BAD_REQUEST),
        ("future disclosure", Box::new(|v| set(v, &["consent", "disclosure_revision"], "host-admin-access/2".into())), StatusCode::BAD_REQUEST),
        ("empty disclosure", Box::new(|v| set(v, &["consent", "disclosure_revision"], "".into())), StatusCode::BAD_REQUEST),
        ("no accepting owner", Box::new(|v| set(v, &["consent", "accepted_by"], "  ".into())), StatusCode::BAD_REQUEST),
        ("no approver", Box::new(|v| set(v, &["approval", "approved_by"], "".into())), StatusCode::BAD_REQUEST),
        ("no authorization record", Box::new(|v| set(v, &["approval", "authorization_ref"], " ".into())), StatusCode::BAD_REQUEST),
        ("consent dated in the future", { let f = future.clone(); Box::new(move |v| set(v, &["consent", "accepted_at"], f.clone().into())) }, StatusCode::BAD_REQUEST),
        ("approval dated in the future", { let f = future.clone(); Box::new(move |v| set(v, &["approval", "approved_at"], f.clone().into())) }, StatusCode::BAD_REQUEST),
        ("no roles", Box::new(|v| set(v, &["roles"], serde_json::json!([]))), StatusCode::BAD_REQUEST),
        ("guest naming itself", Box::new(|v| set(v, &["host_kind"], serde_json::json!({"kind": "guest", "physical_host": "dev-alice-pc"}))), StatusCode::BAD_REQUEST),
        ("guest naming nobody", Box::new(|v| set(v, &["host_kind"], serde_json::json!({"kind": "guest", "physical_host": " "}))), StatusCode::BAD_REQUEST),
        ("infra role", Box::new(|v| set(v, &["roles"], serde_json::json!(["worker", "infra"]))), StatusCode::FORBIDDEN),
        ("approval missing", Box::new(|v| remove(v, &["approval"])), StatusCode::UNPROCESSABLE_ENTITY),
        ("approval null", Box::new(|v| set(v, &["approval"], Value::Null)), StatusCode::UNPROCESSABLE_ENTITY),
        ("consent missing", Box::new(|v| remove(v, &["consent"])), StatusCode::UNPROCESSABLE_ENTITY),
        ("disclosure revision missing", Box::new(|v| remove(v, &["consent", "disclosure_revision"])), StatusCode::UNPROCESSABLE_ENTITY),
        ("accepted host missing", Box::new(|v| remove(v, &["consent", "accepted_host"])), StatusCode::UNPROCESSABLE_ENTITY),
        ("invalid host name", Box::new(|v| { set(v, &["host_name"], "Alice PC".into()); set(v, &["consent", "accepted_host"], "Alice PC".into()); }), StatusCode::UNPROCESSABLE_ENTITY),
        ("unknown role", Box::new(|v| set(v, &["roles"], serde_json::json!(["admin"]))), StatusCode::UNPROCESSABLE_ENTITY),
    ];
    for (what, edit, expected) in &cases {
        let mut req = enrollment_request("alice-pc", &["worker"]);
        edit(&mut req);
        let (s, body) = h.enroll(&req).await;
        assert_eq!(s, *expected, "{what}: {body}");
        let err = api_error(&body);
        let code = if *expected == StatusCode::FORBIDDEN { "forbidden" } else { "bad_request" };
        assert_eq!(err.code, code, "{what}");
        if *what == "stale disclosure" {
            assert!(err.message.contains("disclosure"), "{}", err.message);
        }
    }
    assert!(
        h.hs.mutations().is_empty(),
        "a refused request reached Headscale: {:?}",
        h.hs.mutations()
    );
    assert!(h.hs.lock().keys.is_empty());
    assert!(h.audit().is_empty(), "refused requests were audited as issued");

    // The unmodified request is accepted, so the refusals above were caused by
    // the edits alone.
    let (s, body) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn host_name_still_registered_is_refused_before_issuing() {
    let h = Harness::start().await;
    let (_, body) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    let grant: EnrollmentGrant = serde_json::from_str(&body).unwrap();
    let node = h.hs.register(grant.auth_key.expose(), "alice-pc", "100.64.0.5").unwrap();
    h.hs.clear_requests();
    let (s, body) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    let err = api_error(&body);
    assert_eq!(err.code, "conflict");
    assert!(err.message.contains(&node.to_string()), "{}", err.message);
    assert!(h.hs.mutations().is_empty());
    assert_eq!(h.hs.lock().keys.len(), 1);
}
