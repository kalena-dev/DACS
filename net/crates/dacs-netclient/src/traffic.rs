//! Passive per-peer counters around a build: `peers` snapshots and `traffic`
//! deltas.
//!
//! Passive status is the local client's last-known view. While the client is
//! not `Running` (observed on macOS 1.102.4 with the app stopped), `tailscale
//! status` still lists peers as online over their last path, and counters can
//! reset when the daemon restarts. So a snapshot records the backend state,
//! and `traffic` flags counter resets and vanished peers instead of reporting
//! zero or dropping them. It shows `path_after` only when the second snapshot
//! came from a running client, and even then it is a hint, not path evidence:
//! path evidence comes from active probes (`ping`, `compiler-check`,
//! `measure`).

use dacs_net_contract::{PeerPath, PeerSnapshot};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::NetError;
use crate::tailscale::{Status, Tailscale, parse_status};

/// `BackendState` of a client whose peer view is current.
pub const RUNNING: &str = "Running";

/// Output of `dacs-net peers`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeersReport {
    #[serde(with = "time::serde::rfc3339")]
    pub captured_at: OffsetDateTime,
    /// `BackendState` of the local client when captured.
    pub backend_state: String,
    /// The client was `Running`, so online flags, paths and counters are current.
    pub running: bool,
    pub warning: Option<String>,
    pub peers: Vec<PeerSnapshot>,
}

/// Peers from one `tailscale status --json`, sorted by host name.
pub fn snapshots(status: &Status) -> Vec<PeerSnapshot> {
    let mut out: Vec<PeerSnapshot> = status
        .peer
        .iter()
        .flat_map(|m| m.values())
        .map(|p| PeerSnapshot {
            host_name: p.host_name.clone(),
            overlay_ips: p.ips(),
            online: p.online,
            path: p.path(),
            rx_bytes: p.rx_bytes,
            tx_bytes: p.tx_bytes,
        })
        .collect();
    out.sort_by(|a, b| a.host_name.cmp(&b.host_name));
    out
}

pub fn report(status: &Status, captured_at: OffsetDateTime) -> PeersReport {
    let running = status.backend_state == RUNNING;
    PeersReport {
        captured_at,
        warning: (!running).then(|| not_running(&status.backend_state)),
        backend_state: status.backend_state.clone(),
        running,
        peers: snapshots(status),
    }
}

fn not_running(state: &str) -> String {
    let state = if state.is_empty() { "unknown" } else { state };
    format!(
        "local client is {state}, not {RUNNING}: peer online flags, paths and counters are \
         last-known values, not current evidence"
    )
}

/// Snapshot the local client's peers with its backend state.
pub async fn capture() -> Result<PeersReport, NetError> {
    let ts = Tailscale::locate()?;
    let out = ts
        .run(&["status", "--json"], std::time::Duration::from_secs(15))
        .await?;
    Ok(report(
        &parse_status(&out.stdout)?,
        OffsetDateTime::now_utc(),
    ))
}

/// A `peers` file: the current report, or the bare list older `dacs-net`
/// versions printed (backend state unknown).
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum SnapshotFile {
    Report(PeersReport),
    Legacy(Vec<PeerSnapshot>),
}

impl SnapshotFile {
    fn backend_state(&self) -> Option<&str> {
        match self {
            Self::Report(r) => Some(&r.backend_state),
            Self::Legacy(_) => None,
        }
    }

    fn peers(&self) -> &[PeerSnapshot] {
        match self {
            Self::Report(r) => &r.peers,
            Self::Legacy(p) => p,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficStatus {
    Measured,
    /// A counter went down between snapshots (daemon restart, re-enrollment,
    /// peer re-added). Both deltas are unknown.
    CounterReset,
    /// Absent from the first snapshot; the deltas are its full counters.
    NewPeer,
    /// Present in the first snapshot, absent from the second: no deltas.
    MissingAfter,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerTraffic {
    pub host_name: String,
    pub status: TrafficStatus,
    /// Null on a counter reset or when the peer is missing.
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    /// Passive path from the second snapshot, only when that snapshot is known
    /// to come from a running client. A hint, not path evidence.
    pub path_after: Option<PeerPath>,
}

/// Output of `dacs-net traffic`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrafficReport {
    /// Null for a snapshot without backend state (older `peers` output).
    pub before_backend_state: Option<String>,
    pub after_backend_state: Option<String>,
    /// Both snapshots are known to come from a running client.
    pub counters_current: bool,
    pub warnings: Vec<String>,
    pub peers: Vec<PeerTraffic>,
    /// No warnings: every delta is measured from two current snapshots.
    pub ok: bool,
}

/// Per-peer deltas between two snapshots, matched by host name.
pub fn diff(before: &SnapshotFile, after: &SnapshotFile) -> TrafficReport {
    let mut warnings = Vec::new();
    for (which, file) in [("before", before), ("after", after)] {
        match file.backend_state() {
            None => warnings.push(format!(
                "{which} snapshot has no backend state (older `dacs-net peers` output); \
                 counters and paths are unverified"
            )),
            Some(state) if state != RUNNING => {
                warnings.push(format!("{which} snapshot: {}", not_running(state)));
            }
            Some(_) => {}
        }
    }
    let after_running = after.backend_state() == Some(RUNNING);
    let mut peers = Vec::new();
    for a in after.peers() {
        let b = before.peers().iter().find(|b| b.host_name == a.host_name);
        let (status, rx, tx) = match b {
            None => (TrafficStatus::NewPeer, Some(a.rx_bytes), Some(a.tx_bytes)),
            Some(b) if a.rx_bytes < b.rx_bytes || a.tx_bytes < b.tx_bytes => {
                warnings.push(format!(
                    "{}: counters went down between snapshots (reset); delta unknown",
                    a.host_name
                ));
                (TrafficStatus::CounterReset, None, None)
            }
            Some(b) => (
                TrafficStatus::Measured,
                Some(a.rx_bytes - b.rx_bytes),
                Some(a.tx_bytes - b.tx_bytes),
            ),
        };
        peers.push(PeerTraffic {
            host_name: a.host_name.clone(),
            status,
            rx_bytes: rx,
            tx_bytes: tx,
            path_after: after_running.then(|| a.path.clone()),
        });
    }
    for b in before.peers() {
        if !after.peers().iter().any(|a| a.host_name == b.host_name) {
            warnings.push(format!(
                "{}: present before, missing after; no delta",
                b.host_name
            ));
            peers.push(PeerTraffic {
                host_name: b.host_name.clone(),
                status: TrafficStatus::MissingAfter,
                rx_bytes: None,
                tx_bytes: None,
                path_after: None,
            });
        }
    }
    TrafficReport {
        before_backend_state: before.backend_state().map(str::to_owned),
        after_backend_state: after.backend_state().map(str::to_owned),
        counters_current: before.backend_state() == Some(RUNNING) && after_running,
        ok: warnings.is_empty(),
        warnings,
        peers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const AT: OffsetDateTime = datetime!(2026-11-14 10:00 UTC);

    fn peer(host: &str, rx: u64, tx: u64, path: PeerPath) -> PeerSnapshot {
        PeerSnapshot {
            host_name: host.into(),
            overlay_ips: vec![],
            online: true,
            path,
            rx_bytes: rx,
            tx_bytes: tx,
        }
    }

    fn direct() -> PeerPath {
        PeerPath::Direct {
            endpoint: "192.0.2.10:41641".into(),
        }
    }

    fn snap(state: &str, peers: Vec<PeerSnapshot>) -> SnapshotFile {
        SnapshotFile::Report(PeersReport {
            captured_at: AT,
            backend_state: state.into(),
            running: state == RUNNING,
            warning: None,
            peers,
        })
    }

    #[test]
    fn running_snapshots_give_measured_deltas_and_passive_path() {
        let before = snap(RUNNING, vec![peer("srv", 100, 1_000, direct())]);
        let after = snap(RUNNING, vec![peer("srv", 150, 65_000, direct())]);
        let r = diff(&before, &after);
        assert!(r.ok && r.counters_current && r.warnings.is_empty(), "{r:?}");
        assert_eq!(
            r.peers,
            [PeerTraffic {
                host_name: "srv".into(),
                status: TrafficStatus::Measured,
                rx_bytes: Some(50),
                tx_bytes: Some(64_000),
                path_after: Some(direct()),
            }]
        );
    }

    #[test]
    fn counter_reset_is_flagged_not_reported_as_zero() {
        // Daemon restarted mid-build: rx fell, tx grew past its old value.
        let before = snap(RUNNING, vec![peer("srv", 5_000, 5_000, direct())]);
        let after = snap(RUNNING, vec![peer("srv", 10, 9_000, direct())]);
        let r = diff(&before, &after);
        assert!(!r.ok);
        assert_eq!(r.peers[0].status, TrafficStatus::CounterReset);
        assert_eq!((r.peers[0].rx_bytes, r.peers[0].tx_bytes), (None, None));
        assert!(r.warnings[0].contains("srv") && r.warnings[0].contains("reset"));
    }

    #[test]
    fn peer_missing_from_second_snapshot_is_reported() {
        let before = snap(
            RUNNING,
            vec![peer("gone", 1, 1, direct()), peer("srv", 1, 1, direct())],
        );
        let after = snap(
            RUNNING,
            vec![peer("new", 7, 8, direct()), peer("srv", 2, 2, direct())],
        );
        let r = diff(&before, &after);
        assert!(!r.ok);
        let by = |h: &str| r.peers.iter().find(|p| p.host_name == h).unwrap();
        assert_eq!(by("gone").status, TrafficStatus::MissingAfter);
        assert_eq!(
            (by("gone").rx_bytes, by("gone").path_after.as_ref()),
            (None, None)
        );
        assert_eq!(by("new").status, TrafficStatus::NewPeer);
        assert_eq!(by("new").tx_bytes, Some(8));
        assert_eq!(by("srv").status, TrafficStatus::Measured);
        assert_eq!(r.warnings.len(), 1, "a new peer is not a warning: {r:?}");
        assert!(r.warnings[0].contains("gone"));
    }

    /// Shape seen from Tailscale 1.102.4 on macOS with the app stopped: the
    /// peer still reads online over a relay.
    const STATUS_STOPPED: &str = r#"{
      "BackendState": "Stopped",
      "Self": {"HostName": "owner-mac", "TailscaleIPs": ["100.100.1.2"], "Online": false},
      "Peer": {"nodekey:aa": {"HostName": "phone", "TailscaleIPs": ["100.100.1.3"], "Online": true,
                              "CurAddr": "", "Relay": "region", "RxBytes": 10, "TxBytes": 20}}
    }"#;

    #[test]
    fn stopped_client_snapshot_warns_and_withholds_paths() {
        let status = parse_status(STATUS_STOPPED).unwrap();
        let rep = report(&status, AT);
        assert!(!rep.running);
        assert!(rep.warning.as_deref().unwrap().contains("Stopped"));
        assert!(rep.peers[0].online, "raw last-known data is kept");

        let after = SnapshotFile::Report(rep);
        let before = snap(RUNNING, vec![peer("phone", 1, 2, direct())]);
        let r = diff(&before, &after);
        assert!(!r.ok && !r.counters_current);
        assert_eq!(r.after_backend_state.as_deref(), Some("Stopped"));
        assert!(r.peers[0].path_after.is_none(), "stale path labelled");
        assert!(r.warnings.iter().any(|w| w.starts_with("after snapshot")));

        let running = report(
            &parse_status(&STATUS_STOPPED.replace("Stopped", RUNNING)).unwrap(),
            AT,
        );
        assert!(running.running && running.warning.is_none());
    }

    #[test]
    fn legacy_array_snapshots_parse_but_are_unverified() {
        let legacy = serde_json::to_string(&[peer("srv", 1, 1, direct())]).unwrap();
        let before: SnapshotFile = serde_json::from_str(&legacy).unwrap();
        let current = serde_json::to_string(&PeersReport {
            captured_at: AT,
            backend_state: RUNNING.into(),
            running: true,
            warning: None,
            peers: vec![peer("srv", 3, 4, direct())],
        })
        .unwrap();
        let after: SnapshotFile = serde_json::from_str(&current).unwrap();
        assert!(matches!(before, SnapshotFile::Legacy(_)));
        assert!(matches!(after, SnapshotFile::Report(_)));
        let r = diff(&before, &after);
        assert!(!r.ok && !r.counters_current);
        assert_eq!(r.before_backend_state, None);
        assert_eq!(r.peers[0].rx_bytes, Some(2));
        assert!(
            r.peers[0].path_after.is_some(),
            "the second snapshot was running"
        );
        let r = diff(&after, &before);
        assert!(
            r.peers[0].path_after.is_none(),
            "unknown state: path withheld"
        );
    }
}
