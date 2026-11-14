//! Process-level checks for `dacs-net peers`, `traffic` and `measure` against
//! a fake `tailscale` CLI (`DACS_TAILSCALE_BIN`, set on the child only). No
//! test here opens a socket to anything but loopback; refusals are asserted
//! to happen before any probe is sent.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

const FAKE_TAILSCALE: &str = r#"#!/bin/sh
d="$FAKE_TS_DIR"
printf '%s\n' "$*" >> "$d/calls.log"
case "$1" in
  status) cat "$d/status.json"; exit 0 ;;
  debug) echo '{"ControlURL":"https://hs.dacs.test","LoggedOut":false}'; exit 0 ;;
  version) echo "1.104.7-fake"; exit 0 ;;
  *) echo "fake tailscale: unexpected command $1" >&2; exit 3 ;;
esac
"#;

const STATUS_RUNNING: &str = r#"{
  "BackendState": "Running",
  "Self": {"HostName": "alice-pc", "TailscaleIPs": ["100.64.0.9"], "Online": true},
  "CurrentTailnet": {"Name": "dacs"},
  "Peer": {"nodekey:aa": {"HostName": "srv", "TailscaleIPs": ["100.64.0.5"], "Online": true,
                          "CurAddr": "192.0.2.10:41641", "Relay": "dacs", "RxBytes": 100, "TxBytes": 200}}
}"#;

/// Tailscale 1.102.4 on macOS with the app stopped still lists the peer as
/// online over a relay.
const STATUS_STOPPED: &str = r#"{
  "BackendState": "Stopped",
  "Self": {"HostName": "alice-pc", "TailscaleIPs": ["100.64.0.9"], "Online": false},
  "CurrentTailnet": {"Name": "dacs"},
  "Peer": {"nodekey:aa": {"HostName": "srv", "TailscaleIPs": ["100.64.0.5"], "Online": true,
                          "CurAddr": "", "Relay": "dacs", "RxBytes": 150, "TxBytes": 900}}
}"#;

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new(status: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("status.json"), status).unwrap();
        let bin = dir.path().join("tailscale");
        std::fs::write(&bin, FAKE_TAILSCALE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn set_status(&self, status: &str) {
        std::fs::write(self.path("status.json"), status).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dacs-net"))
            .args(args)
            .env("DACS_TAILSCALE_BIN", self.path("tailscale"))
            .env("FAKE_TS_DIR", self.dir.path())
            .output()
            .unwrap()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn save(&self, name: &str, out: &Output) -> String {
        let p = self.path(name);
        std::fs::write(&p, &out.stdout).unwrap();
        p.to_str().unwrap().to_owned()
    }
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn peers_while_stopped_warns_and_traffic_withholds_the_stale_path() {
    let fake = Fake::new(STATUS_RUNNING);
    let before = fake.run(&["peers"]);
    assert_eq!(before.status.code(), Some(0), "{before:?}");
    let v = json(&before);
    assert_eq!(v["backend_state"], "Running");
    assert_eq!(v["running"], true);
    assert_eq!(v["peers"][0]["path"]["path"], "direct");
    let before_file = fake.save("before.json", &before);

    fake.set_status(STATUS_STOPPED);
    let after = fake.run(&["peers"]);
    assert_eq!(
        after.status.code(),
        Some(2),
        "not Running must fail the check"
    );
    let v = json(&after);
    assert_eq!(v["running"], false);
    assert!(v["warning"].as_str().unwrap().contains("Stopped"));
    let after_file = fake.save("after.json", &after);

    let t = fake.run(&["traffic", "--before", &before_file, "--after", &after_file]);
    assert_eq!(t.status.code(), Some(2));
    let v = json(&t);
    assert_eq!(v["counters_current"], false);
    assert_eq!(v["peers"][0]["status"], "measured");
    assert_eq!(v["peers"][0]["tx_bytes"], 700);
    assert!(v["peers"][0]["path_after"].is_null(), "{v}");
    assert!(
        fake.calls().iter().all(|c| c == "status --json"),
        "{:?}",
        fake.calls()
    );
}

#[test]
fn traffic_flags_reset_and_missing_peer_from_older_array_snapshots() {
    let fake = Fake::new(STATUS_RUNNING);
    let before = fake.path("before.json");
    let after = fake.path("after.json");
    let peer = |host: &str, rx: u64| {
        serde_json::json!({"host_name": host, "overlay_ips": [], "online": true,
            "path": {"path": "derp", "region": "dacs"}, "rx_bytes": rx, "tx_bytes": rx})
    };
    std::fs::write(
        &before,
        serde_json::to_vec(&[peer("srv", 5000), peer("gone", 1)]).unwrap(),
    )
    .unwrap();
    std::fs::write(&after, serde_json::to_vec(&[peer("srv", 10)]).unwrap()).unwrap();
    let t = fake.run(&[
        "traffic",
        "--before",
        before.to_str().unwrap(),
        "--after",
        after.to_str().unwrap(),
    ]);
    assert_eq!(t.status.code(), Some(2));
    let v = json(&t);
    let peers = v["peers"].as_array().unwrap();
    assert_eq!(peers[0]["status"], "counter_reset");
    assert!(peers[0]["rx_bytes"].is_null());
    assert_eq!(peers[1]["host_name"], "gone");
    assert_eq!(peers[1]["status"], "missing_after");
    assert_eq!(v["warnings"].as_array().unwrap().len(), 4, "{v}");
    assert!(fake.calls().is_empty(), "traffic is offline");
}
