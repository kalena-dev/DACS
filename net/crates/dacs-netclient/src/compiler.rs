//! Compiler (sccache-dist) reachability evidence for Person 4.
//!
//! Each endpoint gets a TCP connect probe and, unless it is on this host's own
//! overlay IP, an active path probe. The path is classified as `direct`,
//! `relayed` (DERP or peer relay) or `unreachable`; nothing here predicts
//! throughput. With a [`CompilerSide`], only the roles that side must reach are
//! probed (the same pairs `deploy/headscale/policy.hujson` grants), and every
//! required role must be reached over the overlay by at least one endpoint.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;

use dacs_net_contract::{
    CompilerEndpoint, CompilerRole, PathProbe, PeerPath, TcpProbe, is_overlay_ip,
};
use serde::{Deserialize, Serialize};

/// The sccache-dist role of the host running the check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerSide {
    /// Build initiator (worker/coordinator running the sccache client).
    Client,
    Scheduler,
    Server,
}

impl CompilerSide {
    /// Roles this side must reach: clients reach the scheduler (10600) and
    /// servers (10501); the scheduler assigns jobs on servers; servers
    /// register and heartbeat with the scheduler.
    pub fn required_roles(self) -> &'static [CompilerRole] {
        match self {
            CompilerSide::Client => &[CompilerRole::Scheduler, CompilerRole::Server],
            CompilerSide::Scheduler => &[CompilerRole::Server],
            CompilerSide::Server => &[CompilerRole::Scheduler],
        }
    }
}

impl FromStr for CompilerSide {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "client" => Ok(Self::Client),
            "scheduler" => Ok(Self::Scheduler),
            "server" => Ok(Self::Server),
            _ => Err("expected client, scheduler or server".into()),
        }
    }
}

/// How traffic to an endpoint travelled when probed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathClass {
    Direct,
    /// DERP relay or peer relay.
    Relayed,
    /// The endpoint is on this host's own overlay IP; no network path is used.
    Local,
    Unreachable,
}

pub fn path_class(path: &PeerPath) -> PathClass {
    match path {
        PeerPath::Direct { .. } => PathClass::Direct,
        PeerPath::Derp { .. } | PeerPath::PeerRelay { .. } => PathClass::Relayed,
        PeerPath::Unreachable { .. } => PathClass::Unreachable,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EndpointCheck {
    pub endpoint: CompilerEndpoint,
    pub tcp: TcpProbe,
    pub path: Option<PathProbe>,
    pub path_class: PathClass,
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompilerReport {
    pub from: CompilerSide,
    pub required_roles: Vec<CompilerRole>,
    /// Required roles with no endpoint reached over the overlay (a co-located
    /// endpoint on this host does not count).
    pub unverified_roles: Vec<CompilerRole>,
    pub checks: Vec<EndpointCheck>,
    pub ok: bool,
}

/// Probe operations, injectable for tests.
pub trait Prober {
    fn tcp(&self, target: SocketAddr, timeout: Duration) -> impl Future<Output = TcpProbe>;
    /// Active path probe; `None` when the probe itself could not run.
    fn path(&self, peer: IpAddr) -> impl Future<Output = Option<PathProbe>>;
}

/// TCP connect plus `tailscale ping` through the local client.
pub struct Live;

impl Prober for Live {
    async fn tcp(&self, target: SocketAddr, timeout: Duration) -> TcpProbe {
        crate::probe_tcp(target, timeout).await
    }
    async fn path(&self, peer: IpAddr) -> Option<PathProbe> {
        crate::ping(&peer.to_string(), 3).await.ok()
    }
}

/// Probe every endpoint. Refuses the whole list, before sending anything, if
/// any address is outside the overlay prefixes.
pub async fn check_endpoints<P: Prober>(
    endpoints: &[CompilerEndpoint],
    local_ips: &[IpAddr],
    prober: &P,
    timeout: Duration,
) -> Result<Vec<EndpointCheck>, String> {
    if let Some(ep) = endpoints.iter().find(|e| !is_overlay_ip(e.overlay_ip)) {
        return Err(format!(
            "compiler endpoint {} is not a private overlay address; no probe sent",
            ep.socket_addr()
        ));
    }
    let mut out = Vec::with_capacity(endpoints.len());
    for ep in endpoints {
        let tcp = prober.tcp(ep.socket_addr(), timeout).await;
        let local = local_ips.contains(&ep.overlay_ip);
        let path = if local {
            None
        } else {
            prober.path(ep.overlay_ip).await
        };
        let class = match &path {
            _ if local => PathClass::Local,
            Some(p) => path_class(&p.path),
            None => PathClass::Unreachable,
        };
        out.push(EndpointCheck {
            ok: tcp.overlay_private && tcp.reachable && class != PathClass::Unreachable,
            endpoint: ep.clone(),
            tcp,
            path,
            path_class: class,
        });
    }
    Ok(out)
}

/// Check the endpoints `from` must reach; endpoints of other roles are not
/// probed (the ACL denies them).
pub async fn check_side<P: Prober>(
    from: CompilerSide,
    endpoints: &[CompilerEndpoint],
    local_ips: &[IpAddr],
    prober: &P,
    timeout: Duration,
) -> Result<CompilerReport, String> {
    let required = from.required_roles();
    let relevant: Vec<CompilerEndpoint> = endpoints
        .iter()
        .filter(|e| required.contains(&e.role))
        .cloned()
        .collect();
    let checks = check_endpoints(&relevant, local_ips, prober, timeout).await?;
    let unverified_roles: Vec<CompilerRole> = required
        .iter()
        .copied()
        .filter(|role| {
            !checks
                .iter()
                .any(|c| c.endpoint.role == *role && c.ok && c.path_class != PathClass::Local)
        })
        .collect();
    Ok(CompilerReport {
        from,
        required_roles: required.to_vec(),
        ok: !checks.is_empty() && checks.iter().all(|c| c.ok) && unverified_roles.is_empty(),
        unverified_roles,
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    use dacs_net_contract::HostName;
    use time::macros::datetime;

    /// Per-IP canned results; records every probe sent.
    #[derive(Default)]
    struct Fake {
        tcp_ok: HashMap<IpAddr, bool>,
        paths: HashMap<IpAddr, PeerPath>,
        sent: RefCell<Vec<String>>,
    }

    impl Fake {
        fn node(mut self, ip: &str, tcp_ok: bool, path: Option<PeerPath>) -> Self {
            let ip: IpAddr = ip.parse().unwrap();
            self.tcp_ok.insert(ip, tcp_ok);
            if let Some(p) = path {
                self.paths.insert(ip, p);
            }
            self
        }
    }

    impl Prober for Fake {
        async fn tcp(&self, target: SocketAddr, _: Duration) -> TcpProbe {
            self.sent.borrow_mut().push(format!("tcp {target}"));
            let reachable = self.tcp_ok.get(&target.ip()).copied().unwrap_or(false);
            TcpProbe {
                target,
                overlay_private: is_overlay_ip(target.ip()),
                reachable,
                connect_ms: reachable.then_some(1.5),
                error: (!reachable).then(|| "connection refused".into()),
            }
        }
        async fn path(&self, peer: IpAddr) -> Option<PathProbe> {
            self.sent.borrow_mut().push(format!("ping {peer}"));
            let path = self.paths.get(&peer)?.clone();
            Some(PathProbe {
                peer: peer.to_string(),
                rtt_ms: (!matches!(path, PeerPath::Unreachable { .. })).then_some(2.0),
                path,
                measured_at: datetime!(2026-11-14 10:00 UTC),
            })
        }
    }

    fn ep(host: &str, role: CompilerRole, ip: &str) -> CompilerEndpoint {
        CompilerEndpoint {
            host_name: HostName::parse(host).unwrap(),
            role,
            overlay_ip: ip.parse().unwrap(),
            port: match role {
                CompilerRole::Scheduler => 10600,
                CompilerRole::Server => 10501,
            },
        }
    }

    fn direct() -> Option<PeerPath> {
        Some(PeerPath::Direct {
            endpoint: "192.0.2.10:41641".into(),
        })
    }

    fn derp() -> Option<PeerPath> {
        Some(PeerPath::Derp {
            region: "dacs".into(),
        })
    }

    const T: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn non_overlay_endpoint_refuses_the_list_before_any_probe() {
        let fake = Fake::default().node("100.64.0.5", true, direct());
        let list = [
            ep("guest", CompilerRole::Scheduler, "100.64.0.5"),
            ep("nat-host", CompilerRole::Server, "192.168.1.20"),
        ];
        let err = check_endpoints(&list, &[], &fake, T).await.unwrap_err();
        assert!(err.contains("192.168.1.20:10501"), "{err}");
        assert!(fake.sent.borrow().is_empty(), "probe sent before refusal");
    }

    #[tokio::test]
    async fn paths_are_classified_and_unreachable_fails() {
        let fake = Fake::default()
            .node("100.64.0.5", true, direct())
            .node("100.64.0.6", true, derp())
            .node(
                "100.64.0.7",
                true,
                Some(PeerPath::Unreachable {
                    detail: "no pong".into(),
                }),
            )
            .node("100.64.0.8", false, direct());
        let list = [
            ep("a", CompilerRole::Server, "100.64.0.5"),
            ep("b", CompilerRole::Server, "100.64.0.6"),
            ep("c", CompilerRole::Server, "100.64.0.7"),
            ep("d", CompilerRole::Server, "100.64.0.8"),
            ep("e", CompilerRole::Server, "100.64.0.9"),
        ];
        let checks = check_endpoints(&list, &[], &fake, T).await.unwrap();
        let got: Vec<(PathClass, bool)> = checks.iter().map(|c| (c.path_class, c.ok)).collect();
        assert_eq!(
            got,
            [
                (PathClass::Direct, true),
                (PathClass::Relayed, true),
                (PathClass::Unreachable, false),
                (PathClass::Direct, false), // path up, listener refused
                (PathClass::Unreachable, false), // no ping result at all
            ]
        );
        assert_eq!(checks[0].tcp.connect_ms, Some(1.5));
        assert_eq!(checks[1].path.as_ref().unwrap().rtt_ms, Some(2.0));
    }

    #[tokio::test]
    async fn client_needs_both_scheduler_and_server() {
        let fake =
            Fake::default()
                .node("100.64.0.5", true, direct())
                .node("100.64.0.6", true, derp());
        let only_scheduler = [ep("sched", CompilerRole::Scheduler, "100.64.0.5")];
        let r = check_side(CompilerSide::Client, &only_scheduler, &[], &fake, T)
            .await
            .unwrap();
        assert!(!r.ok, "client->server must not pass vacuously");
        assert_eq!(r.unverified_roles, [CompilerRole::Server]);

        let both = [
            ep("sched", CompilerRole::Scheduler, "100.64.0.5"),
            ep("srv", CompilerRole::Server, "100.64.0.6"),
        ];
        let r = check_side(CompilerSide::Client, &both, &[], &fake, T)
            .await
            .unwrap();
        assert!(r.ok && r.unverified_roles.is_empty());
        assert_eq!(r.checks[1].path_class, PathClass::Relayed);

        let r = check_side(CompilerSide::Client, &[], &[], &fake, T)
            .await
            .unwrap();
        assert!(!r.ok, "an empty list proves nothing");
    }

    #[tokio::test]
    async fn server_probes_only_the_scheduler() {
        let fake =
            Fake::default()
                .node("100.64.0.5", true, direct())
                .node("100.64.0.6", true, direct());
        let list = [
            ep("sched", CompilerRole::Scheduler, "100.64.0.5"),
            ep("other-srv", CompilerRole::Server, "100.64.0.6"),
        ];
        let r = check_side(CompilerSide::Server, &list, &[], &fake, T)
            .await
            .unwrap();
        assert!(r.ok);
        assert_eq!(r.checks.len(), 1);
        assert!(fake.sent.borrow().iter().all(|s| s.contains("100.64.0.5")));
    }

    #[tokio::test]
    async fn co_located_endpoint_is_local_and_does_not_verify_the_overlay() {
        // A guest that is both scheduler and server checks scheduler->server.
        let fake = Fake::default().node("100.64.0.5", true, None);
        let list = [
            ep("guest", CompilerRole::Scheduler, "100.64.0.5"),
            ep("guest", CompilerRole::Server, "100.64.0.5"),
        ];
        let me: IpAddr = "100.64.0.5".parse().unwrap();
        let r = check_side(CompilerSide::Scheduler, &list, &[me], &fake, T)
            .await
            .unwrap();
        assert_eq!(r.checks.len(), 1);
        assert_eq!(r.checks[0].path_class, PathClass::Local);
        assert!(r.checks[0].ok, "the local listener answered");
        assert!(r.checks[0].path.is_none());
        assert!(!fake.sent.borrow().iter().any(|s| s.starts_with("ping")));
        assert_eq!(r.unverified_roles, [CompilerRole::Server]);
        assert!(!r.ok, "a co-located server is not overlay evidence");
    }

    #[test]
    fn side_parses() {
        assert_eq!("server".parse::<CompilerSide>(), Ok(CompilerSide::Server));
        assert!("worker".parse::<CompilerSide>().is_err());
    }
}
