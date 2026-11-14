//! Runs the real `dacs-net` binary against a fake `tailscale` CLI selected
//! through `DACS_TAILSCALE_BIN` (set on the child process only, so tests stay
//! parallel-safe and never reach the host's real Tailscale client).
//!
//! The fake records every argument vector. That lets these tests assert which
//! commands DACS actually runs: read-only commands for `status`/`assess`, no
//! network-changing command before consent, and the auth key passed only
//! through a private file that is gone afterwards.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use dacs_net_contract::{
    ADMIN_ACCESS_DISCLOSURE_REVISION, CONTRACT_REVISION, CoordinatorEndpoints, DeviceId,
    EnrollmentGrant, HostConsent, HostName, LocalJoinResult, Secret, TailnetClientState,
    TailnetSwitchConsent,
};
use time::OffsetDateTime;

const DACS: &str = "https://hs.dacs.test";
const SAAS: &str = "https://controlplane.tailscale.com";
const KEY: &str = "hskey-auth-FAKE-test-secret-0123456789";

/// Commands that change membership, profiles or settings. None may run
/// before the owner's consent has been checked.
const MUTATING: [&str; 6] = ["login", "up", "logout", "switch", "set", "down"];

const FAKE_TAILSCALE: &str = r#"#!/bin/sh
d="$FAKE_TS_DIR"
printf '%s\n' "$*" >> "$d/calls.log"
case "$1" in
  status)
    if [ -f "$d/daemon_down" ]; then
      echo "failed to connect to local Tailscale service; is Tailscale running?" >&2
      exit 1
    fi
    cat "$d/status.json"; exit 0 ;;
  debug)
    if [ "$2" = prefs ]; then cat "$d/prefs.json"; fi
    exit 0 ;;
  login)
    for a in "$@"; do
      case "$a" in
        --auth-key=file:*)
          f="${a#--auth-key=file:}"
          if [ -f "$f" ] && [ "$(cat "$f")" = "$FAKE_EXPECT_KEY" ]; then : > "$d/keyfile_seen"; fi ;;
      esac
    done
    cp "$d/status.joined.json" "$d/status.json"
    cp "$d/prefs.joined.json" "$d/prefs.json"
    exit 0 ;;
  *) echo "fake tailscale: unexpected command $1" >&2; exit 3 ;;
esac
"#;

/// Shape observed from Tailscale 1.102.4 (standalone macOS app) while the
/// owner's existing login was stopped; identifiers replaced with fakes.
const STATUS_MACOS_STOPPED: &str = r#"{
  "Version": "1.102.4-t3caf7d9e7-g084ee3b64", "TUN": true, "BackendState": "Stopped",
  "HaveNodeKey": true, "AuthURL": "",
  "Self": {"HostName": "owner-mac", "DNSName": "owner-mac.example.ts.net.", "OS": "macOS",
           "TailscaleIPs": ["100.100.1.2", "fd7a:115c:a1e0::1:2"], "Online": false,
           "InNetworkMap": true, "CurAddr": "", "Relay": ""},
  "Health": ["Tailscale is stopped."],
  "CurrentTailnet": {"Name": "owner@example.com", "MagicDNSSuffix": "example.ts.net", "MagicDNSEnabled": true},
  "Peer": {"nodekey:aa": {"HostName": "phone", "TailscaleIPs": ["100.100.1.3"], "Online": true,
                          "CurAddr": "", "Relay": "region"}}
}"#;
const PREFS_MACOS_STOPPED: &str = r#"{"ControlURL": "https://controlplane.tailscale.com", "LoggedOut": false, "WantRunning": false}"#;

const STATUS_DACS_RUNNING: &str = r#"{
  "BackendState": "Running",
  "Self": {"HostName": "alice-pc", "TailscaleIPs": ["100.64.0.9", "fd7a:115c:a1e0::9"], "Online": true},
  "CurrentTailnet": {"Name": "dacs", "MagicDNSSuffix": "dacs.internal", "MagicDNSEnabled": true}
}"#;
const PREFS_DACS_RUNNING: &str =
    r#"{"ControlURL": "https://hs.dacs.test", "LoggedOut": false, "WantRunning": true}"#;

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new(status: &str, prefs: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("status.json"), status).unwrap();
        std::fs::write(p.join("prefs.json"), prefs).unwrap();
        std::fs::write(p.join("status.joined.json"), STATUS_DACS_RUNNING).unwrap();
        std::fs::write(p.join("prefs.joined.json"), PREFS_DACS_RUNNING).unwrap();
        let bin = p.join("tailscale");
        std::fs::write(&bin, FAKE_TAILSCALE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn run_with_bin(&self, bin: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dacs-net"))
            .args(args)
            .env("DACS_TAILSCALE_BIN", bin)
            .env("FAKE_TS_DIR", self.dir.path())
            .env("FAKE_EXPECT_KEY", KEY)
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_bin(&self.path("tailscale"), args)
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn assert_no_mutation(&self) {
        for call in self.calls() {
            let cmd = call.split_whitespace().next().unwrap_or("");
            assert!(
                !MUTATING.contains(&cmd),
                "network-changing call ran: {call}"
            );
        }
    }

    fn write_json<T: serde::Serialize>(&self, name: &str, value: &T) -> String {
        let path = self.path(name);
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

fn stdout_json<T: serde::de::DeserializeOwned>(out: &Output) -> T {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not the expected JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn grant() -> EnrollmentGrant {
    EnrollmentGrant {
        contract_revision: CONTRACT_REVISION.into(),
        device_id: DeviceId("dev-mac".into()),
        host_name: HostName::parse("alice-pc").unwrap(),
        login_server: DACS.into(),
        auth_key: Secret::new(KEY),
        auth_key_id: 7,
        tags: vec!["tag:dacs-worker".into()],
        expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(10),
        coordinator: CoordinatorEndpoints {
            bootstrap_url: "https://bootstrap.dacs.test".into(),
            overlay_url: "https://coordinator.dacs.internal:7443".into(),
        },
    }
}

fn consent(switch_from: Option<&str>) -> HostConsent {
    let now = OffsetDateTime::now_utc();
    HostConsent {
        device_id: DeviceId("dev-mac".into()),
        accepted_host: HostName::parse("alice-pc").unwrap(),
        accepted_by: "alice".into(),
        accepted_at: now,
        disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
        tailnet_switch: switch_from.map(|url| TailnetSwitchConsent {
            previous_control_url: url.into(),
            previous_tailnet: None,
            accepted_at: now,
        }),
    }
}

fn read_only_calls_only(fake: &Fake) {
    let calls = fake.calls();
    assert!(!calls.is_empty(), "the fake CLI was never called");
    for call in &calls {
        assert!(
            call == "status --json" || call == "debug prefs",
            "unexpected non-read-only call: {call}"
        );
    }
}

#[test]
fn stopped_existing_login_is_reported_with_read_only_calls() {
    let fake = Fake::new(STATUS_MACOS_STOPPED, PREFS_MACOS_STOPPED);
    let out = fake.run(&["status"]);
    assert!(out.status.success());
    match stdout_json::<TailnetClientState>(&out) {
        TailnetClientState::LoggedIn {
            control_url,
            backend_state,
            overlay_ips,
            ..
        } => {
            assert_eq!(control_url, SAAS);
            assert_eq!(backend_state, "Stopped");
            assert_eq!(overlay_ips.len(), 2);
        }
        other => panic!("a stopped login must not look logged out: {other:?}"),
    }
    read_only_calls_only(&fake);
}

#[test]
fn assess_requires_switch_consent_for_a_stopped_login() {
    let fake = Fake::new(STATUS_MACOS_STOPPED, PREFS_MACOS_STOPPED);
    let out = fake.run(&["assess", DACS]);
    assert!(out.status.success());
    let a: dacs_net_contract::TailnetAssessment = stdout_json(&out);
    assert!(a.requires_switch_consent && !a.already_dacs);
    assert_eq!(a.previous_control_url.as_deref(), Some(SAAS));
    assert_eq!(
        a.disclosure.as_deref(),
        Some(dacs_net_contract::TAILNET_SWITCH_DISCLOSURE)
    );
    read_only_calls_only(&fake);
}

#[test]
fn unreachable_daemon_and_missing_cli_are_prerequisite_states() {
    let fake = Fake::new(STATUS_MACOS_STOPPED, PREFS_MACOS_STOPPED);
    std::fs::write(fake.path("daemon_down"), "").unwrap();
    let out = fake.run(&["status"]);
    assert!(out.status.success());
    assert!(matches!(
        stdout_json(&out),
        TailnetClientState::DaemonUnavailable { .. }
    ));

    // An explicit DACS_TAILSCALE_BIN that does not exist never falls back to PATH.
    let out = fake.run_with_bin(&fake.path("absent-tailscale"), &["status"]);
    assert!(out.status.success());
    assert_eq!(
        stdout_json::<TailnetClientState>(&out),
        TailnetClientState::NotInstalled
    );
}

#[test]
fn join_without_switch_consent_runs_no_network_change() {
    let fake = Fake::new(STATUS_MACOS_STOPPED, PREFS_MACOS_STOPPED);
    let g = fake.write_json("grant.json", &grant());
    for (name, c) in [
        ("no-switch.json", consent(None)),
        (
            "wrong-switch.json",
            consent(Some("https://other-headscale.test")),
        ),
    ] {
        let c = fake.write_json(name, &c);
        let out = fake.run(&["join", "--grant", &g, "--consent", &c]);
        assert_eq!(out.status.code(), Some(1), "{name}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("approve the tailnet switch"), "{name}: {err}");
        assert!(!err.contains(KEY) && !String::from_utf8_lossy(&out.stdout).contains(KEY));
    }
    fake.assert_no_mutation();
    assert!(!fake.path("keyfile_seen").exists());
}

#[test]
fn consented_join_passes_key_by_private_file_and_removes_it() {
    let fake = Fake::new(STATUS_MACOS_STOPPED, PREFS_MACOS_STOPPED);
    let g = fake.write_json("grant.json", &grant());
    let c = fake.write_json(
        "consent.json",
        &consent(Some("https://ControlPlane.tailscale.com/")),
    );
    let out = fake.run(&["join", "--grant", &g, "--consent", &c]);
    assert!(
        out.status.success(),
        "join failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains(KEY), "auth key printed");
    let result: LocalJoinResult = stdout_json(&out);
    assert_eq!(result.displaced_control_url.as_deref(), Some(SAAS));
    assert_eq!(result.overlay_ips.len(), 2);
    assert_eq!(result.coordinator, grant().coordinator);

    let calls = fake.calls();
    assert!(calls.iter().all(|c| !c.contains(KEY)), "auth key on argv");
    let logins: Vec<&String> = calls.iter().filter(|c| c.starts_with("login ")).collect();
    assert_eq!(logins.len(), 1, "exactly one login: {calls:?}");
    let login = logins[0];
    assert!(login.contains(&format!("--login-server={DACS}")));
    assert!(login.contains("--hostname=alice-pc"));
    assert!(login.contains("--accept-routes=false"));
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("logout") || c.starts_with("switch"))
    );

    // The fake read the key from the file while `login` ran; the file is gone now.
    assert!(
        fake.path("keyfile_seen").exists(),
        "key file missing during login"
    );
    let key_file = login
        .split_whitespace()
        .find_map(|a| a.strip_prefix("--auth-key=file:"))
        .expect("auth key passed as file");
    assert!(!Path::new(key_file).exists(), "key file left behind");
}

#[test]
fn leave_refuses_a_non_dacs_profile() {
    let fake = Fake::new(STATUS_MACOS_STOPPED, PREFS_MACOS_STOPPED);
    let out = fake.run(&["leave", DACS]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing to log out"));
    fake.assert_no_mutation();
}

#[test]
fn compiler_check_refuses_non_overlay_endpoints_before_probing() {
    let fake = Fake::new(STATUS_MACOS_STOPPED, PREFS_MACOS_STOPPED);
    // A guest reached through its host's NAT/LAN address is not an overlay node.
    let list = r#"[
      {"host_name": "compile-guest", "role": "scheduler", "overlay_ip": "100.64.0.5", "port": 10600},
      {"host_name": "compile-guest", "role": "server", "overlay_ip": "192.168.1.20", "port": 10501}
    ]"#;
    let path = fake.path("endpoints.json");
    std::fs::write(&path, list).unwrap();
    for extra in [&[][..], &["--from", "client"][..]] {
        let mut args = vec!["compiler-check", "--endpoints", path.to_str().unwrap()];
        args.extend_from_slice(extra);
        let out = fake.run(&args);
        assert_eq!(out.status.code(), Some(1), "{extra:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("not a private overlay address"), "{err}");
    }
    // Only the read-only local state lookup ran; no ping was sent.
    read_only_calls_only(&fake);
}
