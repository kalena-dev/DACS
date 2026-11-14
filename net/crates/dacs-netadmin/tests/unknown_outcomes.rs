//! Item 4: an operation whose Headscale outcome is unknown (the request may
//! have applied but the reply was lost) is never reported as success, and a
//! retry reports the real state. Revocation is idempotent. A mismatched
//! node/key pair is refused before anything is changed.

mod common;

use axum::http::StatusCode;
use common::*;
use dacs_net_contract::{EnrollmentGrant, MembershipState, NetworkMembership, RevocationOutcome};
use time::OffsetDateTime;

async fn grant(h: &Harness, host: &str) -> EnrollmentGrant {
    let (s, body) = h.enroll(&enrollment_request(host, &["worker"])).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Enroll `host` and register its device; returns (grant, node id).
async fn joined(h: &Harness, host: &str, ip: &str) -> (EnrollmentGrant, u64) {
    let g = grant(h, host).await;
    let node = h.hs.register(g.auth_key.expose(), host, ip).unwrap();
    (g, node)
}

fn outcome(status: StatusCode, body: &str) -> RevocationOutcome {
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(body).unwrap()
}

fn audit_ops(h: &Harness) -> Vec<String> {
    h.audit_records()
        .iter()
        .map(|r| r["op"].as_str().unwrap().to_owned())
        .collect()
}

fn key_usable(h: &Harness, id: u64) -> bool {
    let k = h.hs.key(id).unwrap();
    !k.used && k.expiration.is_some_and(|e| e > OffsetDateTime::now_utc())
}

// ---- issuance ---------------------------------------------------------------

#[tokio::test]
async fn lost_create_reply_is_502_and_a_retry_issues_a_fresh_key() {
    let h = Harness::start().await;
    h.hs.fault(Op::CreateKey, Fault::ApplyThenDrop);
    let req = enrollment_request("alice-pc", &["worker"]);
    let (s, body) = h.enroll(&req).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(api_error(&body).code, "headscale_unavailable");
    assert!(!body.contains("auth_key"), "no grant in an unknown outcome");
    assert!(
        !audit_ops(&h).contains(&"enroll".to_owned()),
        "unknown issuance audited as issued"
    );

    // The key was created upstream; nobody received its secret. It is single
    // use, tagged and dies at the requested TTL.
    let orphan = h.hs.lock().keys[0].clone();
    assert!(!orphan.reusable && !orphan.ephemeral && !orphan.used);
    assert_eq!(orphan.tags, vec!["tag:dacs-worker"]);
    assert!(orphan.expiration.unwrap() <= OffsetDateTime::now_utc() + time::Duration::minutes(15));

    // The coordinator retries the same approved request.
    let (s, body) = h.enroll(&req).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let g: EnrollmentGrant = serde_json::from_str(&body).unwrap();
    assert_ne!(g.auth_key_id, orphan.id);
    assert_ne!(g.auth_key.expose(), orphan.secret);
    assert_eq!(audit_ops(&h), vec!["enroll"]);
    let (_, body) = h
        .membership(&membership_query(
            "dev-alice-pc",
            "alice-pc",
            Some(g.auth_key_id),
            None,
        ))
        .await;
    let m: NetworkMembership = serde_json::from_str(&body).unwrap();
    assert!(
        matches!(m.state, MembershipState::KeyIssued { auth_key_id, .. } if auth_key_id == g.auth_key_id)
    );
}

#[tokio::test]
async fn create_and_list_failures_before_applying_issue_nothing() {
    let h = Harness::start().await;
    h.hs.fault(Op::CreateKey, Fault::Status(503, "unavailable".into()));
    let (s, _) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    h.hs.fault(Op::ListNodes, Fault::DropBeforeApply);
    let (s, _) = h.enroll(&enrollment_request("alice-pc", &["worker"])).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert!(h.hs.lock().keys.is_empty());
    assert_eq!(
        h.hs.requests()
            .iter()
            .filter(|r| r.op == Some(Op::CreateKey))
            .count(),
        1,
        "a failed host-name check must stop before creating a key"
    );
    assert!(h.audit().is_empty());
}

// ---- revocation: lost replies, no-ops and retries ---------------------------

#[tokio::test]
async fn lost_expire_reply_is_not_success_and_retry_reports_the_expired_key() {
    let h = Harness::start().await;
    let g = grant(&h, "bob-pc").await;
    let req = revocation("dev-bob-pc", Some(g.auth_key_id), None);

    h.hs.fault(Op::ExpireKey, Fault::ApplyThenDrop);
    let (s, body) = h.revoke(&req).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{body}");
    assert!(
        !key_usable(&h, g.auth_key_id),
        "the expiry did apply upstream"
    );
    assert!(
        !audit_ops(&h).contains(&"revoke".to_owned()),
        "unknown revocation audited"
    );

    h.hs.clear_requests();
    let out = {
        let (s, body) = h.revoke(&req).await;
        outcome(s, &body)
    };
    assert!(
        out.key_expired,
        "retry must report the key as expired: {out:?}"
    );
    assert_eq!(out.node_deleted, None);
    assert!(out.verified_absent, "{out:?}");
    assert!(
        h.hs.mutations().is_empty(),
        "retry re-applied: {:?}",
        h.hs.mutations()
    );
}

#[tokio::test]
async fn expire_that_did_not_apply_is_never_verified() {
    let h = Harness::start().await;
    let g = grant(&h, "bob-pc").await;
    let req = revocation("dev-bob-pc", Some(g.auth_key_id), None);

    // Headscale answers 200 {} but the key stays live.
    h.hs.fault(Op::ExpireKey, Fault::NoopOk);
    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert!(
        key_usable(&h, g.auth_key_id),
        "the mock really kept the key live"
    );
    assert!(
        !out.verified_absent,
        "a still-usable key was reported revoked: {out:?}"
    );
    assert!(!out.key_expired, "{out:?}");

    // A retry against a working Headscale completes it.
    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert!(out.key_expired && out.verified_absent, "{out:?}");
    assert!(!key_usable(&h, g.auth_key_id));
}

#[tokio::test]
async fn lost_delete_reply_is_not_success_and_retry_verifies_absence() {
    let h = Harness::start().await;
    let (g, node) = joined(&h, "alice-pc", "100.64.0.5").await;
    let req = revocation("dev-alice-pc", Some(g.auth_key_id), Some(node));

    h.hs.fault(Op::DeleteNode, Fault::ApplyThenDrop);
    let (s, body) = h.revoke(&req).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{body}");
    assert!(h.hs.node(node).is_none(), "the delete did apply upstream");
    assert!(!audit_ops(&h).contains(&"revoke".to_owned()));

    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert_eq!(
        out.node_deleted, None,
        "already gone, not deleted by this call"
    );
    assert!(!out.key_expired, "a used key needs no expiry");
    assert!(out.verified_absent, "{out:?}");
    let (_, body) = h
        .membership(&membership_query(
            "dev-alice-pc",
            "alice-pc",
            Some(g.auth_key_id),
            Some(node),
        ))
        .await;
    let m: NetworkMembership = serde_json::from_str(&body).unwrap();
    assert!(matches!(m.state, MembershipState::Revoked { .. }), "{m:?}");
}

#[tokio::test]
async fn failed_delete_is_not_success_and_retry_deletes() {
    let h = Harness::start().await;
    let (g, node) = joined(&h, "alice-pc", "100.64.0.5").await;
    let req = revocation("dev-alice-pc", Some(g.auth_key_id), Some(node));
    h.hs.fault(
        Op::DeleteNode,
        Fault::Status(500, "database is locked".into()),
    );
    let (s, _) = h.revoke(&req).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert!(h.hs.node(node).is_some());
    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert_eq!(out.node_deleted, Some(node));
    assert!(out.verified_absent);
}

#[tokio::test]
async fn delete_that_did_not_apply_is_never_verified() {
    let h = Harness::start().await;
    let (g, node) = joined(&h, "alice-pc", "100.64.0.5").await;
    let req = revocation("dev-alice-pc", Some(g.auth_key_id), Some(node));
    h.hs.fault(Op::DeleteNode, Fault::NoopOk);
    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert!(h.hs.node(node).is_some());
    assert!(
        !out.verified_absent,
        "a remaining node was reported absent: {out:?}"
    );
    let (s, body) = h.revoke(&req).await;
    assert!(outcome(s, &body).verified_absent);
    assert!(h.hs.node(node).is_none());
}

#[tokio::test]
async fn failed_verification_is_not_success_and_retry_reconciles() {
    let h = Harness::start().await;
    let (g, node) = joined(&h, "alice-pc", "100.64.0.5").await;
    let req = revocation("dev-alice-pc", Some(g.auth_key_id), Some(node));
    // The pre-mutation listing works; the verification listing fails.
    h.hs.fault(Op::ListNodes, Fault::Pass);
    h.hs.fault(Op::ListNodes, Fault::DropBeforeApply);
    let (s, body) = h.revoke(&req).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{body}");
    assert!(h.hs.node(node).is_none(), "deleted, but unverified");
    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert!(out.verified_absent && out.node_deleted.is_none(), "{out:?}");
}

#[tokio::test]
async fn device_joining_during_revocation_is_not_verified_absent() {
    let h = Harness::start().await;
    let g = grant(&h, "bob-pc").await;
    let req = revocation("dev-bob-pc", Some(g.auth_key_id), None);
    // The device registers with its key just before the expiry lands.
    h.hs.fault(
        Op::ExpireKey,
        Fault::Before(|s| {
            let secret = s.keys.iter().find(|k| !k.used).unwrap().secret.clone();
            s.register(&secret, "bob-pc", "100.64.0.6").unwrap();
        }),
    );
    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert!(
        !out.verified_absent,
        "a node that joined mid-revocation was missed: {out:?}"
    );
    assert_eq!(h.hs.lock().nodes.len(), 1);

    let (s, body) = h.revoke(&req).await;
    let out = outcome(s, &body);
    assert!(out.verified_absent && out.node_deleted.is_some(), "{out:?}");
    assert!(h.hs.lock().nodes.is_empty());
}

// ---- idempotency --------------------------------------------------------------

#[tokio::test]
async fn revocation_is_idempotent() {
    let h = Harness::start().await;
    let (alice, node) = joined(&h, "alice-pc", "100.64.0.5").await;
    let bob = grant(&h, "bob-pc").await;
    let by_both = revocation("dev-alice-pc", Some(alice.auth_key_id), Some(node));
    let by_key = revocation("dev-alice-pc", Some(alice.auth_key_id), None);
    let by_node = revocation("dev-alice-pc", None, Some(node));
    let unused = revocation("dev-bob-pc", Some(bob.auth_key_id), None);

    let (s, body) = h.revoke(&by_both).await;
    let first = outcome(s, &body);
    assert_eq!(first.node_deleted, Some(node));
    assert!(first.verified_absent && !first.key_expired);
    let (s, body) = h.revoke(&unused).await;
    let first_unused = outcome(s, &body);
    assert!(first_unused.key_expired && first_unused.verified_absent);

    h.hs.clear_requests();
    for _ in 0..2 {
        for req in [&by_both, &by_key, &by_node] {
            let (s, body) = h.revoke(req).await;
            let out = outcome(s, &body);
            assert_eq!(out.node_deleted, None, "{req}");
            assert!(out.verified_absent && !out.key_expired, "{req}: {out:?}");
        }
        let (s, body) = h.revoke(&unused).await;
        let out = outcome(s, &body);
        assert_eq!(
            (out.key_expired, out.node_deleted, out.verified_absent),
            (
                first_unused.key_expired,
                first_unused.node_deleted,
                first_unused.verified_absent
            ),
            "repeat differs from first outcome"
        );
    }
    assert!(
        h.hs.mutations().is_empty(),
        "repeats mutated Headscale: {:?}",
        h.hs.mutations()
    );
}

// ---- mismatched node/key pairs ------------------------------------------------

#[tokio::test]
async fn mismatched_node_and_key_pairs_are_refused_before_any_mutation() {
    let h = Harness::start().await;
    let (alice, alice_node) = joined(&h, "alice-pc", "100.64.0.5").await;
    let (bob, bob_node) = joined(&h, "bob-pc", "100.64.0.6").await;
    let carol = grant(&h, "carol-pc").await;
    h.hs.clear_requests();

    let pairs = [
        (
            "alice node + bob key",
            Some(bob.auth_key_id),
            Some(alice_node),
        ),
        (
            "alice node + carol unused key",
            Some(carol.auth_key_id),
            Some(alice_node),
        ),
        ("unknown node + bob key", Some(bob.auth_key_id), Some(9999)),
        ("alice node + unknown key", Some(9999), Some(alice_node)),
        (
            "bob node + alice key",
            Some(alice.auth_key_id),
            Some(bob_node),
        ),
    ];
    for (what, key, node) in pairs {
        let (s, body) = h.revoke(&revocation("dev-x", key, node)).await;
        assert_eq!(s, StatusCode::CONFLICT, "revoke {what}: {body}");
        assert_eq!(api_error(&body).code, "conflict");
        let (s, body) = h
            .membership(&membership_query("dev-x", "renamed-x", key, node))
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "query {what}: {body}");
    }
    assert!(h.hs.mutations().is_empty(), "{:?}", h.hs.mutations());
    assert!(h.hs.node(alice_node).is_some() && h.hs.node(bob_node).is_some());
    assert_eq!(
        h.hs.node(alice_node).unwrap().given_name,
        "alice-pc",
        "renamed"
    );
    assert!(key_usable(&h, carol.auth_key_id), "carol's key was expired");
    assert!(!audit_ops(&h).contains(&"revoke".to_owned()));
}

#[tokio::test]
async fn revocation_request_without_target_or_requester_is_refused() {
    let h = Harness::start().await;
    let (s, body) = h.revoke(&revocation("dev-x", None, None)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    let mut req = revocation("dev-x", Some(1), None);
    req["requested_by"] = " ".into();
    let (s, body) = h.revoke(&req).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(h.hs.requests().is_empty());
}
