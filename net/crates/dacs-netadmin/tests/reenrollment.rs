//! Item 5: a revoked device cannot come back on its old credentials, and
//! re-enrollment needs a complete, matching approval and consent like a first
//! enrollment.
//!
//! netadmin is stateless (Headscale is its only store, approvals live in the
//! coordinator), so it cannot tell a *replayed* pre-revocation approval from a
//! new one; `replayed_pre_revocation_approval_is_refused` records that gap and
//! is ignored until approval freshness has an enforcement point.

mod common;

use axum::http::StatusCode;
use common::*;
use dacs_net_contract::{EnrollmentGrant, MembershipState, NetworkMembership, RevocationOutcome};
use serde_json::Value;

async fn enroll_ok(h: &Harness, req: &Value) -> EnrollmentGrant {
    let (s, body) = h.enroll(req).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn revoke_ok(h: &Harness, req: &Value) -> RevocationOutcome {
    let (s, body) = h.revoke(req).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let out: RevocationOutcome = serde_json::from_str(&body).unwrap();
    assert!(out.verified_absent, "{out:?}");
    out
}

async fn state(h: &Harness, key: Option<u64>, node: Option<u64>) -> MembershipState {
    let (s, body) = h
        .membership(&membership_query("dev-alice-pc", "alice-pc", key, node))
        .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    serde_json::from_str::<NetworkMembership>(&body).unwrap().state
}

/// A fresh approval for the same device and host (new authorization record).
fn new_approval_request() -> Value {
    enrollment_request("alice-pc", &["worker"])
}

#[tokio::test]
async fn revoked_device_cannot_rejoin_with_its_old_grant() {
    let h = Harness::start().await;
    let g = enroll_ok(&h, &enrollment_request("alice-pc", &["worker"])).await;
    let node = h.hs.register(g.auth_key.expose(), "alice-os", "100.64.0.5").unwrap();
    revoke_ok(&h, &revocation("dev-alice-pc", Some(g.auth_key_id), Some(node))).await;

    assert_eq!(
        h.hs.register(g.auth_key.expose(), "alice-os", "100.64.0.5").unwrap_err(),
        "authkey already used"
    );
    assert!(h.hs.lock().nodes.is_empty());
    assert_eq!(state(&h, Some(g.auth_key_id), Some(node)).await, MembershipState::Revoked { node_id: Some(node) });
    assert_eq!(state(&h, Some(g.auth_key_id), None).await, MembershipState::Revoked { node_id: None });
}

#[tokio::test]
async fn grant_revoked_before_use_cannot_be_used() {
    let h = Harness::start().await;
    let g = enroll_ok(&h, &enrollment_request("alice-pc", &["worker"])).await;
    let out = revoke_ok(&h, &revocation("dev-alice-pc", Some(g.auth_key_id), None)).await;
    assert!(out.key_expired);
    assert_eq!(
        h.hs.register(g.auth_key.expose(), "alice-os", "100.64.0.5").unwrap_err(),
        "authkey expired"
    );
    assert!(matches!(state(&h, Some(g.auth_key_id), None).await, MembershipState::KeyExpired { .. }));
}

#[tokio::test]
async fn reenrollment_without_a_valid_approval_is_refused() {
    let h = Harness::start().await;
    let g = enroll_ok(&h, &enrollment_request("alice-pc", &["worker"])).await;
    let node = h.hs.register(g.auth_key.expose(), "alice-os", "100.64.0.5").unwrap();
    revoke_ok(&h, &revocation("dev-alice-pc", Some(g.auth_key_id), Some(node))).await;
    h.hs.clear_requests();
    let keys_before = h.hs.lock().keys.len();

    type Edit = fn(&mut Value);
    let cases: [(&str, Edit, StatusCode); 7] = [
        ("approval removed", |v| { v.as_object_mut().unwrap().remove("approval"); }, StatusCode::UNPROCESSABLE_ENTITY),
        ("approval null", |v| v["approval"] = Value::Null, StatusCode::UNPROCESSABLE_ENTITY),
        ("no approver", |v| v["approval"]["approved_by"] = "".into(), StatusCode::BAD_REQUEST),
        ("no authorization record", |v| v["approval"]["authorization_ref"] = "".into(), StatusCode::BAD_REQUEST),
        ("approval for another device", |v| v["approval"]["device_id"] = "dev-bob-pc".into(), StatusCode::BAD_REQUEST),
        ("consent for another host", |v| v["consent"]["accepted_host"] = "bob-pc".into(), StatusCode::BAD_REQUEST),
        ("stale disclosure", |v| v["consent"]["disclosure_revision"] = "host-admin-access/0".into(), StatusCode::BAD_REQUEST),
    ];
    for (what, edit, expected) in cases {
        let mut req = new_approval_request();
        edit(&mut req);
        let (s, body) = h.enroll(&req).await;
        assert_eq!(s, expected, "{what}: {body}");
    }
    assert!(h.hs.mutations().is_empty(), "{:?}", h.hs.mutations());
    assert_eq!(h.hs.lock().keys.len(), keys_before);
}

#[tokio::test]
async fn reenrollment_while_revocation_is_incomplete_is_refused() {
    let h = Harness::start().await;
    let g = enroll_ok(&h, &enrollment_request("alice-pc", &["worker"])).await;
    let node = h.hs.register(g.auth_key.expose(), "alice-os", "100.64.0.5").unwrap();
    // Membership renames the node to the approved host name.
    assert!(matches!(state(&h, Some(g.auth_key_id), None).await, MembershipState::Joined { .. }));
    h.hs.fault(Op::DeleteNode, Fault::Status(500, "database is locked".into()));
    let (s, _) = h.revoke(&revocation("dev-alice-pc", Some(g.auth_key_id), Some(node))).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);

    let keys_before = h.hs.lock().keys.len();
    let (s, body) = h.enroll(&new_approval_request()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(h.hs.lock().keys.len(), keys_before, "a second key was issued");
}

#[tokio::test]
async fn reenrollment_with_a_new_approval_issues_new_unique_material() {
    let h = Harness::start().await;
    let first = enroll_ok(&h, &enrollment_request("alice-pc", &["worker"])).await;
    let old_node = h.hs.register(first.auth_key.expose(), "alice-os", "100.64.0.5").unwrap();
    revoke_ok(&h, &revocation("dev-alice-pc", Some(first.auth_key_id), Some(old_node))).await;

    let second = enroll_ok(&h, &new_approval_request()).await;
    assert_ne!(second.auth_key_id, first.auth_key_id);
    assert_ne!(second.auth_key.expose(), first.auth_key.expose());
    assert!(matches!(state(&h, Some(second.auth_key_id), None).await, MembershipState::KeyIssued { .. }));
    assert_eq!(state(&h, Some(first.auth_key_id), None).await, MembershipState::Revoked { node_id: None });

    let new_node = h.hs.register(second.auth_key.expose(), "alice-os", "100.64.0.7").unwrap();
    assert_ne!(new_node, old_node);
    assert!(h.hs.register(first.auth_key.expose(), "alice-os", "100.64.0.5").is_err());
    let audit = h.audit_records();
    let refs: Vec<&Value> = audit
        .iter()
        .filter(|r| r["op"] == "enroll")
        .map(|r| &r["detail"]["authorization_ref"])
        .collect();
    assert_eq!(refs.len(), 2);
    assert_ne!(refs[0], refs[1], "each enrollment is audited against its own approval");
}

#[tokio::test]
#[ignore = "known gap: stateless netadmin cannot detect a replayed approval; the coordinator must not resubmit one"]
async fn replayed_pre_revocation_approval_is_refused() {
    let h = Harness::start().await;
    let original = enrollment_request("alice-pc", &["worker"]);
    let g = enroll_ok(&h, &original).await;
    let node = h.hs.register(g.auth_key.expose(), "alice-os", "100.64.0.5").unwrap();
    revoke_ok(&h, &revocation("dev-alice-pc", Some(g.auth_key_id), Some(node))).await;

    let (s, body) = h.enroll(&original).await;
    assert!(s.is_client_error(), "replayed approval accepted: {s} {body}");
}
