//! Device-side network client (Person 3), used by Person 1's setup flow and
//! Person 2's worker.
//!
//! - [`assess`] detects an existing Tailscale/Headscale login and whether
//!   joining DACS would displace it.
//! - [`join`] enrolls the host with a device-specific [`EnrollmentGrant`] only
//!   after the owner's [`HostConsent`] (and tailnet-switch consent when needed).
//!   It uses `tailscale login`, which adds a profile instead of deleting the
//!   existing one, so the owner can `tailscale switch` back.
//! - [`leave`] logs out only the DACS profile; it never touches other networks.
//! - [`peers`], [`ping`], [`probe_tcp`] report direct/relayed paths, transfer
//!   counters and private reachability for build evidence.

pub mod tailscale;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use dacs_net_contract::{
    ADMIN_ACCESS_DISCLOSURE_REVISION, EnrollmentGrant, HostConsent, LocalJoinResult, PathProbe,
    PeerPath, PeerSnapshot, TAILNET_SWITCH_DISCLOSURE, TAILSCALE_DEFAULT_CONTROL_URL,
    TailnetAssessment, TailnetClientState, TcpProbe, is_overlay_ip,
};
use time::OffsetDateTime;

use crate::tailscale::{
    CliError, Prefs, Status, Tailscale, parse_pongs, parse_prefs, parse_status,
};

const SHORT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error("network client prerequisite missing: {0}")]
    Prerequisite(String),
    #[error("consent check failed: {0}")]
    Consent(String),
    #[error(
        "this host is logged in to {previous}; the owner must approve the tailnet switch disclosure before joining DACS"
    )]
    SwitchConsentRequired { previous: String },
    #[error("enrollment grant is not usable: {0}")]
    Grant(String),
    #[error("already joined to the DACS network")]
    AlreadyJoined,
    #[error("refusing to log out: active control server is {active}, not DACS ({dacs})")]
    NotDacs { active: String, dacs: String },
    #[error("join did not reach Running on {login_server} within {secs}s (last state: {state})")]
    JoinTimeout {
        login_server: String,
        secs: u64,
        state: String,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Normalize a control URL for comparison (scheme/host case, trailing slash).
pub fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
            let mut out = format!(
                "{}://{}",
                scheme.to_ascii_lowercase(),
                host.to_ascii_lowercase()
            );
            if !path.is_empty() {
                out.push('/');
                out.push_str(path);
            }
            out
        }
        None => url.to_ascii_lowercase(),
    }
}

/// Classify the local client from `status --json` and `debug prefs`.
pub fn classify(status: &Status, prefs: Option<&Prefs>) -> TailnetClientState {
    let self_ips = status.self_.as_ref().map(|s| s.ips()).unwrap_or_default();
    let no_profile = matches!(status.backend_state.as_str(), "NeedsLogin" | "NoState" | "")
        && self_ips.is_empty()
        && status.current_tailnet.is_none();
    if no_profile || prefs.is_some_and(|p| p.logged_out) {
        return TailnetClientState::LoggedOut;
    }
    let control_url = prefs
        .map(|p| p.control_url.as_str())
        .filter(|u| !u.is_empty())
        .unwrap_or(TAILSCALE_DEFAULT_CONTROL_URL)
        .to_owned();
    TailnetClientState::LoggedIn {
        control_url,
        backend_state: status.backend_state.clone(),
        tailnet: status
            .current_tailnet
            .as_ref()
            .map(|t| t.name.clone())
            .filter(|n| !n.is_empty()),
        self_name: status
            .self_
            .as_ref()
            .map(|s| s.host_name.clone())
            .filter(|n| !n.is_empty()),
        overlay_ips: self_ips,
    }
}

/// Decide whether joining `login_server` needs tailnet-switch consent.
pub fn assessment(client: TailnetClientState, login_server: &str) -> TailnetAssessment {
    let dacs = normalize_url(login_server);
    let (already_dacs, previous) = match &client {
        TailnetClientState::LoggedIn { control_url, .. } => {
            let same = normalize_url(control_url) == dacs;
            (same, (!same).then(|| control_url.clone()))
        }
        _ => (false, None),
    };
    TailnetAssessment {
        login_server: login_server.to_owned(),
        client,
        already_dacs,
        requires_switch_consent: previous.is_some(),
        disclosure: previous
            .is_some()
            .then(|| TAILNET_SWITCH_DISCLOSURE.to_owned()),
        previous_control_url: previous,
    }
}

/// Check grant, consent and assessment before any network change. Returns the
/// control server that will be displaced, if any.
pub fn check_join(
    grant: &EnrollmentGrant,
    consent: &HostConsent,
    assessment: &TailnetAssessment,
    now: OffsetDateTime,
) -> Result<Option<String>, NetError> {
    if grant.device_id != consent.device_id {
        return Err(NetError::Consent(
            "consent belongs to another device".into(),
        ));
    }
    if grant.host_name != consent.accepted_host {
        return Err(NetError::Consent(format!(
            "owner accepted host {:?}, grant is for {:?}",
            consent.accepted_host.as_str(),
            grant.host_name.as_str()
        )));
    }
    if consent.disclosure_revision != ADMIN_ACCESS_DISCLOSURE_REVISION {
        return Err(NetError::Consent(
            "consent was given for an outdated disclosure".into(),
        ));
    }
    if grant.expires_at <= now {
        return Err(NetError::Grant(
            "enrollment key expired; request a new grant".into(),
        ));
    }
    if !grant.login_server.starts_with("https://") {
        return Err(NetError::Grant("login server must be https://".into()));
    }
    match &assessment.client {
        TailnetClientState::NotInstalled => {
            return Err(NetError::Prerequisite(
                "Tailscale client is not installed".into(),
            ));
        }
        TailnetClientState::DaemonUnavailable { detail } => {
            return Err(NetError::Prerequisite(format!(
                "Tailscale service is not running: {detail}"
            )));
        }
        _ => {}
    }
    if assessment.already_dacs {
        return Err(NetError::AlreadyJoined);
    }
    if let Some(previous) = &assessment.previous_control_url {
        let approved = consent
            .tailnet_switch
            .as_ref()
            .is_some_and(|s| normalize_url(&s.previous_control_url) == normalize_url(previous));
        if !approved {
            return Err(NetError::SwitchConsentRequired {
                previous: previous.clone(),
            });
        }
    }
    Ok(assessment.previous_control_url.clone())
}

async fn read_state(ts: &Tailscale) -> TailnetClientState {
    let status = match ts
        .run(&["status", "--json"], SHORT)
        .await
        .and_then(|o| parse_status(&o.stdout))
    {
        Ok(s) => s,
        Err(e) => {
            return TailnetClientState::DaemonUnavailable {
                detail: e.to_string(),
            };
        }
    };
    let prefs = ts
        .run(&["debug", "prefs"], SHORT)
        .await
        .and_then(|o| parse_prefs(&o.stdout))
        .ok();
    classify(&status, prefs.as_ref())
}

/// Current local client state.
pub async fn detect() -> TailnetClientState {
    match Tailscale::locate() {
        Ok(ts) => read_state(&ts).await,
        Err(_) => TailnetClientState::NotInstalled,
    }
}

/// Assess joining `login_server` from this host. Makes no changes.
pub async fn assess(login_server: &str) -> TailnetAssessment {
    assessment(detect().await, login_server)
}

#[derive(Clone, Debug, Default)]
pub struct JoinOptions {
    /// Windows only: keep the tunnel up when the GUI user logs out.
    pub unattended: bool,
    /// Accept MagicDNS configuration (stable `host.<base_domain>` names).
    pub accept_dns: bool,
    pub timeout: Option<Duration>,
}

/// Join the DACS network with a device-specific grant after consent.
/// The auth key is passed to `tailscale` through a private temporary file,
/// never on the command line.
pub async fn join(
    grant: &EnrollmentGrant,
    consent: &HostConsent,
    opts: &JoinOptions,
) -> Result<LocalJoinResult, NetError> {
    let ts = Tailscale::locate()
        .map_err(|_| NetError::Prerequisite("Tailscale client is not installed".into()))?;
    let assessed = assessment(read_state(&ts).await, &grant.login_server);
    let displaced = check_join(grant, consent, &assessed, OffsetDateTime::now_utc())?;

    let timeout = opts.timeout.unwrap_or(Duration::from_secs(90));
    let mut key_file = tempfile::Builder::new().prefix("dacs-enroll-").tempfile()?;
    {
        use std::io::Write;
        key_file.write_all(grant.auth_key.expose().as_bytes())?;
        key_file.flush()?;
    }
    let key_arg = format!("--auth-key=file:{}", key_file.path().display());
    let server_arg = format!("--login-server={}", grant.login_server);
    let host_arg = format!("--hostname={}", grant.host_name);
    let dns_arg = format!("--accept-dns={}", opts.accept_dns);
    let timeout_arg = format!("--timeout={}s", timeout.as_secs());
    let mut args = vec![
        "login",
        server_arg.as_str(),
        key_arg.as_str(),
        host_arg.as_str(),
        "--accept-routes=false",
        dns_arg.as_str(),
        timeout_arg.as_str(),
    ];
    if opts.unattended && cfg!(windows) {
        args.push("--unattended");
    }
    let result = ts.run(&args, timeout + Duration::from_secs(10)).await;
    // Remove the key file before inspecting the result.
    drop(key_file);
    result?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let dacs = normalize_url(&grant.login_server);
    loop {
        let state = read_state(&ts).await;
        if let TailnetClientState::LoggedIn {
            control_url,
            backend_state,
            overlay_ips,
            ..
        } = &state
            && normalize_url(control_url) == dacs
            && backend_state == "Running"
            && !overlay_ips.is_empty()
        {
            return Ok(LocalJoinResult {
                device_id: grant.device_id.clone(),
                host_name: grant.host_name.clone(),
                overlay_ips: overlay_ips.clone(),
                coordinator: grant.coordinator.clone(),
                displaced_control_url: displaced,
            });
        }
        if Instant::now() >= deadline {
            return Err(NetError::JoinTimeout {
                login_server: grant.login_server.clone(),
                secs: 30,
                state: serde_json::to_string(&state).unwrap_or_default(),
            });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Log out of the DACS profile. Refuses when another network is active.
pub async fn leave(login_server: &str) -> Result<(), NetError> {
    let ts = Tailscale::locate()
        .map_err(|_| NetError::Prerequisite("Tailscale client is not installed".into()))?;
    match read_state(&ts).await {
        TailnetClientState::LoggedIn { control_url, .. }
            if normalize_url(&control_url) == normalize_url(login_server) =>
        {
            ts.run(&["logout"], Duration::from_secs(30)).await?;
            Ok(())
        }
        TailnetClientState::LoggedIn { control_url, .. } => Err(NetError::NotDacs {
            active: control_url,
            dacs: login_server.to_owned(),
        }),
        TailnetClientState::LoggedOut => Ok(()),
        TailnetClientState::NotInstalled => Err(NetError::Prerequisite(
            "Tailscale client is not installed".into(),
        )),
        TailnetClientState::DaemonUnavailable { detail } => Err(NetError::Prerequisite(detail)),
    }
}

/// Passive per-peer path and transfer counters.
pub async fn peers() -> Result<Vec<PeerSnapshot>, NetError> {
    let ts = Tailscale::locate()?;
    let status = parse_status(&ts.run(&["status", "--json"], SHORT).await?.stdout)?;
    let mut out: Vec<PeerSnapshot> = status
        .peer
        .unwrap_or_default()
        .into_values()
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
    Ok(out)
}

/// Active path probe: sends `count` disco pings without stopping at the first
/// direct pong, and reports the last observed path and mean RTT.
pub async fn ping(peer: &str, count: u32) -> Result<PathProbe, NetError> {
    let ts = Tailscale::locate()?;
    let count_arg = format!("--c={}", count.max(1));
    let args = [
        "ping",
        count_arg.as_str(),
        "--until-direct=false",
        "--timeout=5s",
        peer,
    ];
    let budget = Duration::from_secs(6 * u64::from(count.max(1)) + 5);
    let (stdout, detail) = match ts.run(&args, budget).await {
        Ok(o) => (o.stdout, String::new()),
        // `tailscale ping` exits non-zero when no pong arrives; keep its output.
        Err(CliError::Failed { stderr, .. }) => {
            (stderr.replace(" | ", "\n"), "no pong received".to_owned())
        }
        Err(e) => return Err(e.into()),
    };
    let pongs = parse_pongs(&stdout);
    let measured_at = OffsetDateTime::now_utc();
    Ok(match pongs.last() {
        Some(last) => PathProbe {
            peer: peer.to_owned(),
            path: last.path.clone(),
            rtt_ms: Some(pongs.iter().map(|p| p.rtt_ms).sum::<f64>() / pongs.len() as f64),
            measured_at,
        },
        None => PathProbe {
            peer: peer.to_owned(),
            path: PeerPath::Unreachable {
                detail: if detail.is_empty() {
                    stdout.lines().last().unwrap_or("").to_owned()
                } else {
                    detail
                },
            },
            rtt_ms: None,
            measured_at,
        },
    })
}

/// TCP connect probe, flagging whether the target is overlay-private.
pub async fn probe_tcp(target: SocketAddr, timeout: Duration) -> TcpProbe {
    let start = Instant::now();
    let res = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target)).await;
    let overlay_private = is_overlay_ip(target.ip());
    match res {
        Ok(Ok(_)) => TcpProbe {
            target,
            overlay_private,
            reachable: true,
            connect_ms: Some(start.elapsed().as_secs_f64() * 1e3),
            error: None,
        },
        Ok(Err(e)) => TcpProbe {
            target,
            overlay_private,
            reachable: false,
            connect_ms: None,
            error: Some(e.to_string()),
        },
        Err(_) => TcpProbe {
            target,
            overlay_private,
            reachable: false,
            connect_ms: None,
            error: Some(format!("timed out after {}ms", timeout.as_millis())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dacs_net_contract::{
        CoordinatorEndpoints, DeviceId, HostName, Secret, TailnetSwitchConsent,
    };
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-11-14 10:05 UTC);
    const DACS: &str = "https://hs.example.org";

    fn grant() -> EnrollmentGrant {
        EnrollmentGrant {
            contract_revision: dacs_net_contract::CONTRACT_REVISION.into(),
            device_id: DeviceId("dev-1".into()),
            host_name: HostName::parse("alice-pc").unwrap(),
            login_server: DACS.into(),
            auth_key: Secret::new("hskey-auth-x"),
            auth_key_id: 1,
            tags: vec!["tag:dacs-worker".into()],
            expires_at: datetime!(2026-11-14 10:15 UTC),
            coordinator: CoordinatorEndpoints {
                bootstrap_url: "https://bootstrap.example.org".into(),
                overlay_url: "https://coord.dacs.internal:7443".into(),
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

    fn logged_in(url: &str) -> TailnetClientState {
        TailnetClientState::LoggedIn {
            control_url: url.into(),
            backend_state: "Running".into(),
            tailnet: Some("corp".into()),
            self_name: Some("alice-pc".into()),
            overlay_ips: vec!["100.100.1.1".parse().unwrap()],
        }
    }

    #[test]
    fn fresh_host_needs_no_switch_consent() {
        let a = assessment(TailnetClientState::LoggedOut, DACS);
        assert!(!a.requires_switch_consent);
        assert_eq!(check_join(&grant(), &consent(), &a, NOW).unwrap(), None);
    }

    #[test]
    fn existing_tailnet_requires_matching_switch_consent() {
        let a = assessment(logged_in("https://controlplane.tailscale.com"), DACS);
        assert!(a.requires_switch_consent);
        assert_eq!(a.disclosure.as_deref(), Some(TAILNET_SWITCH_DISCLOSURE));
        assert!(matches!(
            check_join(&grant(), &consent(), &a, NOW),
            Err(NetError::SwitchConsentRequired { .. })
        ));

        let mut c = consent();
        c.tailnet_switch = Some(TailnetSwitchConsent {
            previous_control_url: "https://other-headscale.example.net".into(),
            previous_tailnet: None,
            accepted_at: NOW,
        });
        assert!(
            matches!(
                check_join(&grant(), &c, &a, NOW),
                Err(NetError::SwitchConsentRequired { .. })
            ),
            "consent for a different network must not count"
        );

        c.tailnet_switch.as_mut().unwrap().previous_control_url =
            "https://ControlPlane.tailscale.com/".into();
        assert_eq!(
            check_join(&grant(), &c, &a, NOW).unwrap().as_deref(),
            Some("https://controlplane.tailscale.com")
        );
    }

    #[test]
    fn already_on_dacs_is_not_rejoined() {
        let a = assessment(logged_in("https://hs.example.org/"), DACS);
        assert!(a.already_dacs && !a.requires_switch_consent);
        assert!(matches!(
            check_join(&grant(), &consent(), &a, NOW),
            Err(NetError::AlreadyJoined)
        ));
    }

    #[test]
    fn consent_and_grant_must_match() {
        let a = assessment(TailnetClientState::LoggedOut, DACS);
        let mut c = consent();
        c.accepted_host = HostName::parse("bob-pc").unwrap();
        assert!(matches!(
            check_join(&grant(), &c, &a, NOW),
            Err(NetError::Consent(_))
        ));
        let mut g = grant();
        g.expires_at = NOW;
        assert!(matches!(
            check_join(&g, &consent(), &a, NOW),
            Err(NetError::Grant(_))
        ));
        let a = assessment(TailnetClientState::NotInstalled, DACS);
        assert!(matches!(
            check_join(&grant(), &consent(), &a, NOW),
            Err(NetError::Prerequisite(_))
        ));
    }

    #[test]
    fn classify_states() {
        let fresh = parse_status(
            r#"{"BackendState":"NeedsLogin","Self":{"TailscaleIPs":null},"CurrentTailnet":null}"#,
        )
        .unwrap();
        assert_eq!(classify(&fresh, None), TailnetClientState::LoggedOut);

        let stopped = parse_status(r#"{"BackendState":"Stopped","Self":{"HostName":"pc","TailscaleIPs":["100.101.1.2"]},"CurrentTailnet":{"Name":"corp.example"}}"#).unwrap();
        let prefs = parse_prefs(r#"{"ControlURL":"","LoggedOut":false}"#).unwrap();
        match classify(&stopped, Some(&prefs)) {
            TailnetClientState::LoggedIn {
                control_url,
                backend_state,
                tailnet,
                ..
            } => {
                assert_eq!(control_url, TAILSCALE_DEFAULT_CONTROL_URL);
                assert_eq!(backend_state, "Stopped");
                assert_eq!(tailnet.as_deref(), Some("corp.example"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn tcp_probe_reports_reachability() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let p = probe_tcp(addr, Duration::from_secs(2)).await;
        assert!(p.reachable && !p.overlay_private);
        drop(listener);
        let p = probe_tcp(addr, Duration::from_secs(2)).await;
        assert!(!p.reachable);
    }
}
