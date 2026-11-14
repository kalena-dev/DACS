//! Negative `check_join` cases not covered by the unit tests: every refusal
//! happens before any network change.

use dacs_net_contract::{
    ADMIN_ACCESS_DISCLOSURE_REVISION, CONTRACT_REVISION, CoordinatorEndpoints, DeviceId,
    EnrollmentGrant, HostConsent, HostName, Secret, TailnetClientState,
};
use dacs_netclient::{NetError, assessment, check_join};
use time::OffsetDateTime;
use time::macros::datetime;

const NOW: OffsetDateTime = datetime!(2026-11-14 10:05 UTC);
const DACS: &str = "https://hs.dacs.test";

fn grant() -> EnrollmentGrant {
    EnrollmentGrant {
        contract_revision: CONTRACT_REVISION.into(),
        device_id: DeviceId("dev-1".into()),
        host_name: HostName::parse("alice-pc").unwrap(),
        login_server: DACS.into(),
        auth_key: Secret::new("hskey-auth-x"),
        auth_key_id: 1,
        tags: vec!["tag:dacs-worker".into()],
        expires_at: datetime!(2026-11-14 10:15 UTC),
        coordinator: CoordinatorEndpoints {
            bootstrap_url: "https://bootstrap.dacs.test".into(),
            overlay_url: "https://coordinator.dacs.internal:7443".into(),
        },
    }
}

fn consent() -> HostConsent {
    HostConsent {
        device_id: DeviceId("dev-1".into()),
        accepted_host: HostName::parse("alice-pc").unwrap(),
        accepted_by: "alice".into(),
        accepted_at: datetime!(2026-11-14 10:00 UTC),
        disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
        tailnet_switch: None,
    }
}

fn fresh() -> dacs_net_contract::TailnetAssessment {
    assessment(TailnetClientState::LoggedOut, DACS)
}

#[test]
fn consent_for_another_device_is_refused() {
    let mut c = consent();
    c.device_id = DeviceId("dev-2".into());
    assert!(matches!(
        check_join(&grant(), &c, &fresh(), NOW),
        Err(NetError::Consent(_))
    ));
}

#[test]
fn consent_to_an_outdated_disclosure_is_refused() {
    let mut c = consent();
    c.disclosure_revision = "host-admin-access/0".into();
    assert!(matches!(
        check_join(&grant(), &c, &fresh(), NOW),
        Err(NetError::Consent(_))
    ));
}

#[test]
fn plaintext_login_server_is_refused() {
    let mut g = grant();
    g.login_server = "http://hs.dacs.test".into();
    let a = assessment(TailnetClientState::LoggedOut, &g.login_server);
    assert!(matches!(
        check_join(&g, &consent(), &a, NOW),
        Err(NetError::Grant(_))
    ));
}

#[test]
fn unreachable_daemon_is_a_prerequisite_blocker() {
    let a = assessment(
        TailnetClientState::DaemonUnavailable {
            detail: "failed to connect to local Tailscale service".into(),
        },
        DACS,
    );
    assert!(!a.requires_switch_consent);
    assert!(matches!(
        check_join(&grant(), &consent(), &a, NOW),
        Err(NetError::Prerequisite(_))
    ));
}

#[test]
fn stopped_login_on_another_server_still_needs_switch_consent() {
    // Observed on macOS: the app was stopped but its SaaS login was retained.
    let a = assessment(
        TailnetClientState::LoggedIn {
            control_url: "https://controlplane.tailscale.com".into(),
            backend_state: "Stopped".into(),
            tailnet: Some("owner@example.com".into()),
            self_name: Some("owner-mac".into()),
            overlay_ips: vec!["100.100.1.2".parse().unwrap()],
        },
        DACS,
    );
    assert!(a.requires_switch_consent);
    assert!(matches!(
        check_join(&grant(), &consent(), &a, NOW),
        Err(NetError::SwitchConsentRequired { .. })
    ));
}
