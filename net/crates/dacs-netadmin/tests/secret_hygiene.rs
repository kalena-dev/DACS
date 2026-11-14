//! Item 2: Headscale administration credentials (the API key) and pre-auth
//! key material never appear in responses, errors, audit records or logs.
//! The only place a pre-auth key may appear is the `auth_key` field of the
//! one successful `EnrollmentGrant` it was issued for.

mod common;

use axum::http::StatusCode;
use common::*;
use dacs_net_contract::{EnrollmentGrant, Secret};
use dacs_netadmin::headscale::{HeadscaleApi, HeadscaleClient};
use serde_json::{Value, json};

/// Check a response body. A successful grant may carry its own key in
/// `auth_key`; everything else in it, and every other body, must be clean.
fn scan_response(h: &Harness, what: &str, status: StatusCode, body: &str, extra: &[String]) {
    let mut secrets = h.hs.secrets();
    secrets.extend_from_slice(extra);
    secrets.push(TOKEN.to_owned());
    if status == StatusCode::OK && body.contains("\"auth_key\"") {
        let mut v: Value = serde_json::from_str(body).unwrap();
        let grant: EnrollmentGrant = serde_json::from_value(v.clone()).unwrap();
        let issued = h.hs.key(grant.auth_key_id).expect("grant names a real key");
        assert_eq!(
            grant.auth_key.expose(),
            issued.secret,
            "{what}: grant key mismatch"
        );
        v.as_object_mut().unwrap().remove("auth_key");
        assert_no_secrets(&v.to_string(), &secrets, what);
    } else {
        assert_no_secrets(body, &secrets, what);
    }
}

#[tokio::test]
async fn lifecycle_and_failure_paths_never_leak_credentials() {
    let logs = LogCapture::default();
    let _guard = logs.install();
    let h = Harness::start().await;
    let canary_auth = format!("hskey-auth-{}", random_hex(24));
    let canary_api = format!("hskey-api-{}", random_hex(24));
    let extra = vec![canary_auth.clone(), canary_api.clone()];
    let mut seen = Vec::new();
    macro_rules! step {
        ($what:expr, $call:expr) => {{
            let (status, body) = $call;
            scan_response(&h, $what, status, &body, &extra);
            seen.push(($what, status));
            (status, body)
        }};
    }

    // Happy path: enroll, join, query (rename), list, compiler endpoints.
    let alice = enrollment_request("alice-pc", &["worker", "compile_server"]);
    let (s, body) = step!("enroll alice", h.enroll(&alice).await);
    assert_eq!(s, StatusCode::OK, "{body}");
    let grant: EnrollmentGrant = serde_json::from_str(&body).unwrap();
    let node =
        h.hs.register(grant.auth_key.expose(), "DESKTOP-OS", "100.64.0.5")
            .unwrap();
    let q = membership_query("dev-alice-pc", "alice-pc", Some(grant.auth_key_id), None);
    let (s, _) = step!("membership alice", h.membership(&q).await);
    assert_eq!(s, StatusCode::OK);
    step!("nodes", h.op("GET", "/v1/nodes", None).await);
    step!(
        "compiler endpoints",
        h.op("GET", "/v1/compiler-endpoints", None).await
    );
    step!("contract", h.op("GET", "/v1/contract", None).await);

    // An unused grant for a second device.
    let bob = enrollment_request("bob-pc", &["worker"]);
    let (s, body) = step!("enroll bob", h.enroll(&bob).await);
    assert_eq!(s, StatusCode::OK);
    let bob_grant: EnrollmentGrant = serde_json::from_str(&body).unwrap();

    // Upstream errors that echo the Authorization header (API key).
    for (op, what) in [
        (Op::ListNodes, "enroll with echoing list"),
        (Op::CreateKey, "enroll with echoing create"),
    ] {
        h.hs.fault(op, Fault::EchoAuth);
        let (s, body) = step!(
            what,
            h.enroll(&enrollment_request("carol-pc", &["worker"])).await
        );
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{what}: {body}");
        assert_eq!(api_error(&body).code, "headscale_unavailable");
    }
    h.hs.fault(Op::ListKeys, Fault::EchoAuth);
    let (s, _) = step!(
        "membership with echoing key list",
        h.membership(&membership_query(
            "dev-bob-pc",
            "bob-pc",
            Some(bob_grant.auth_key_id),
            None
        ))
        .await
    );
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    h.hs.fault(Op::ExpireKey, Fault::EchoAuth);
    let (s, _) = step!(
        "revoke with echoing expire",
        h.revoke(&revocation("dev-bob-pc", Some(bob_grant.auth_key_id), None))
            .await
    );
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    h.hs.fault(Op::DeleteNode, Fault::EchoAuth);
    let (s, _) = step!(
        "revoke with echoing delete",
        h.revoke(&revocation(
            "dev-alice-pc",
            Some(grant.auth_key_id),
            Some(node)
        ))
        .await
    );
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    h.hs.fault(Op::RenameNode, Fault::EchoAuth);
    let (s, _) = step!(
        "membership with echoing rename",
        h.membership(&membership_query(
            "dev-alice-pc",
            "alice-renamed",
            None,
            Some(node)
        ))
        .await
    );
    assert_eq!(s, StatusCode::OK, "rename failure is only a warning");
    for op in [Op::ListNodes] {
        h.hs.fault(op, Fault::EchoAuth);
        step!(
            "nodes with echoing list",
            h.op("GET", "/v1/nodes", None).await
        );
        h.hs.fault(op, Fault::EchoAuth);
        step!(
            "compiler endpoints with echoing list",
            h.op("GET", "/v1/compiler-endpoints", None).await
        );
    }
    h.hs.fault(Op::Health, Fault::EchoAuth);
    step!(
        "health with echoing upstream",
        h.call("GET", "/v1/health", None, None).await
    );

    // Malformed upstream success bodies that carry key-shaped material.
    h.hs.fault(
        Op::CreateKey,
        Fault::ApplyThenBody(json!({"preAuthKey": canary_auth})),
    );
    let (s, _) = step!(
        "enroll with malformed create body",
        h.enroll(&enrollment_request("dave-pc", &["worker"])).await
    );
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    h.hs.fault(
        Op::CreateKey,
        Fault::ApplyThenBody(
            json!({"preAuthKey": {"id": "77", "key": canary_auth, "aclTags": canary_api}}),
        ),
    );
    let (s, _) = step!(
        "enroll with mistyped create body",
        h.enroll(&enrollment_request("dave-pc", &["worker"])).await
    );
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    h.hs.fault(
        Op::ListNodes,
        Fault::ApplyThenBody(json!({"nodes": canary_api})),
    );
    let (s, _) = step!(
        "nodes with malformed list body",
        h.op("GET", "/v1/nodes", None).await
    );
    assert_eq!(s, StatusCode::BAD_GATEWAY);

    // Headscale hands back a reusable key: refused, and its secret not returned.
    h.hs.lock().force_reusable = true;
    let (s, _) = step!(
        "enroll with reusable key",
        h.enroll(&enrollment_request("erin-pc", &["worker"])).await
    );
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    h.hs.lock().force_reusable = false;

    // Rejected and invalid requests.
    step!(
        "unauthorized enroll",
        h.call(
            "POST",
            "/v1/enrollments",
            Some("Bearer nope"),
            Some(alice.clone())
        )
        .await
    );
    let mut bad = enrollment_request("frank-pc", &["worker"]);
    bad["consent"]["accepted_host"] = "other-pc".into();
    step!("mismatched consent", h.enroll(&bad).await);

    // Finally, revoke successfully.
    let (s, _) = step!(
        "revoke alice",
        h.revoke(&revocation(
            "dev-alice-pc",
            Some(grant.auth_key_id),
            Some(node)
        ))
        .await
    );
    assert_eq!(s, StatusCode::OK);
    let (s, _) = step!(
        "revoke bob",
        h.revoke(&revocation("dev-bob-pc", Some(bob_grant.auth_key_id), None))
            .await
    );
    assert_eq!(s, StatusCode::OK);

    let mut all = h.hs.secrets();
    all.extend(extra.iter().cloned());
    all.push(TOKEN.to_owned());
    let audit = h.audit();
    assert!(
        audit.contains("\"op\":\"enroll\""),
        "audit has enroll records"
    );
    assert!(
        audit.contains("\"op\":\"revoke\""),
        "audit has revoke records"
    );
    assert_no_secrets(&audit, &all, "audit log");
    let log_text = logs.text();
    assert!(
        log_text.contains("dacs_netadmin"),
        "logs were captured: {seen:?}"
    );
    assert_no_secrets(&log_text, &all, "tracing output");
}

#[tokio::test]
async fn headscale_client_errors_do_not_carry_the_api_key() {
    let hs = MockHeadscale::start().await;
    let client = HeadscaleClient::new(&hs.url(), Secret::new(hs.api_key.clone())).unwrap();
    let secrets = hs.secrets();
    for op in [
        Op::Health,
        Op::ListNodes,
        Op::ListKeys,
        Op::ExpireKey,
        Op::DeleteNode,
        Op::CreateKey,
        Op::RenameNode,
    ] {
        hs.fault(op, Fault::EchoAuth);
    }
    let errors = vec![
        client.health().await.unwrap_err(),
        client.list_nodes().await.unwrap_err(),
        client.list_keys().await.unwrap_err(),
        client.expire_key(1).await.unwrap_err(),
        client.delete_node(1).await.unwrap_err(),
        client
            .create_tagged_key(&["tag:dacs-worker".into()], time::OffsetDateTime::now_utc())
            .await
            .unwrap_err(),
        client.rename_node(1, "x").await.unwrap_err(),
    ];
    // The mock really did receive the key, and really did echo it.
    assert!(
        hs.requests()
            .iter()
            .all(|r| r.authorization.as_deref() == Some(&*format!("Bearer {}", hs.api_key)))
    );
    for e in errors {
        assert!(
            matches!(
                e,
                dacs_netadmin::headscale::HeadscaleError::Status { status: 500, .. }
            ),
            "{e}"
        );
        assert_no_secrets(&e.to_string(), &secrets, "HeadscaleError Display");
        assert_no_secrets(&format!("{e:?}"), &secrets, "HeadscaleError Debug");
    }
}

#[tokio::test]
async fn wrong_api_key_is_reported_without_echoing_either_key() {
    let hs = MockHeadscale::start().await;
    let wrong = format!("hskey-api-{}", random_hex(24));
    let client = HeadscaleClient::new(&hs.url(), Secret::new(wrong.clone())).unwrap();
    let e = client.list_nodes().await.unwrap_err();
    assert!(
        matches!(
            e,
            dacs_netadmin::headscale::HeadscaleError::Status { status: 401, .. }
        ),
        "{e}"
    );
    let mut secrets = hs.secrets();
    secrets.push(wrong);
    assert_no_secrets(&e.to_string(), &secrets, "401 error");
}

#[test]
fn grant_debug_output_redacts_the_auth_key() {
    let v = json!({
        "contract_revision": dacs_net_contract::CONTRACT_REVISION,
        "device_id": "dev-a",
        "host_name": "a",
        "login_server": LOGIN_SERVER,
        "auth_key": "hskey-auth-supersecretvalue",
        "auth_key_id": 1,
        "tags": ["tag:dacs-worker"],
        "expires_at": "2026-11-14T10:15:00Z",
        "coordinator": {"bootstrap_url": BOOTSTRAP_URL, "overlay_url": OVERLAY_URL}
    });
    let grant: EnrollmentGrant = serde_json::from_value(v).unwrap();
    assert!(!format!("{grant:?}").contains("supersecretvalue"));
    assert!(!format!("{grant:#?}").contains("supersecretvalue"));
}
