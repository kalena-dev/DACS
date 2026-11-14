//! Measured availability and transfer on compiler paths (Person 4 evidence).
//!
//! - [`sample`] repeats the compiler reachability probe
//!   ([`compiler::check_side`] or [`compiler::check_endpoints`]) over a window
//!   and reports, per endpoint, the success ratio, the path class of every
//!   sample (so direct/relayed flips show) and the connect-latency spread.
//! - [`Sink`] and [`send`] move N bytes over one plain TCP connection between
//!   two overlay nodes and report bytes, duration, throughput and the path
//!   class probed before and after. The sink acknowledges the byte count, so
//!   the sender's duration covers delivery, not local socket buffering.
//! - [`collect`] wraps a window and transfers into one [`MeasureEvidence`]
//!   record to attach to a build.
//!
//! Non-overlay addresses are refused before anything is probed, sent or bound.
//! A target on this host's own overlay IP is labelled `local` and is never
//! overlay proof. Loopback is accepted only by this module's unit tests, and is
//! always `local`. Each figure describes one window and one TCP stream; nothing
//! here predicts relay throughput or build speed.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use dacs_net_contract::{
    CONTRACT_REVISION, CompilerEndpoint, CompilerRole, PathProbe, PeerPath, TailnetClientState,
    is_overlay_ip,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crate::compiler::{self, CompilerSide, EndpointCheck, PathClass, Prober, path_class};

/// Schema tag of [`MeasureEvidence`].
pub const EVIDENCE_SCHEMA: &str = "dacs-net/measure-evidence/1";

/// Distribution of observed values in milliseconds. Percentiles use the
/// nearest-rank method, so every figure is a value that was observed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    pub count: usize,
    pub min: f64,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl LatencyStats {
    /// `None` when there is no finite value.
    pub fn of(values: &[f64]) -> Option<Self> {
        let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let rank = |pct: usize| v[(pct * n).div_ceil(100).clamp(1, n) - 1];
        (n > 0).then(|| Self {
            count: n,
            min: v[0],
            median: rank(50),
            p95: rank(95),
            max: v[n - 1],
        })
    }
}

/// Samples per path class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCounts {
    pub direct: u32,
    pub relayed: u32,
    pub local: u32,
    pub unreachable: u32,
}

impl PathCounts {
    fn add(&mut self, class: PathClass) {
        match class {
            PathClass::Direct => self.direct += 1,
            PathClass::Relayed => self.relayed += 1,
            PathClass::Local => self.local += 1,
            PathClass::Unreachable => self.unreachable += 1,
        }
    }
}

/// One endpoint in one round.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointSample {
    /// Start of the round.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub ok: bool,
    pub tcp_reachable: bool,
    pub connect_ms: Option<f64>,
    pub path_class: PathClass,
    /// Mean `tailscale ping` RTT; none for local endpoints or without a pong.
    pub rtt_ms: Option<f64>,
    pub error: Option<String>,
}

/// A change of path class between consecutive samples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathChange {
    /// 0-based index of the first sample on the new class.
    pub sample: u32,
    pub from: PathClass,
    pub to: PathClass,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointAvailability {
    pub endpoint: CompilerEndpoint,
    pub samples: u32,
    pub successes: u32,
    /// `successes / samples`.
    pub success_ratio: f64,
    /// Successful samples over a direct or relayed path (local ones excluded).
    pub overlay_successes: u32,
    pub paths: PathCounts,
    pub path_changes: Vec<PathChange>,
    /// Switches between `direct` and `relayed`, either way, ignoring
    /// unreachable samples in between.
    pub direct_relay_flips: u32,
    /// TCP connect time of reachable samples.
    pub connect_ms: Option<LatencyStats>,
    /// Path-probe RTT of samples that got a pong.
    pub rtt_ms: Option<LatencyStats>,
    pub series: Vec<EndpointSample>,
}

/// Rounds in which some endpoint of a required role was reached over the
/// overlay (a co-located endpoint does not count).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleAvailability {
    pub role: CompilerRole,
    pub rounds_verified: u32,
    pub ratio: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AvailabilityReport {
    pub from: Option<CompilerSide>,
    pub required_roles: Vec<CompilerRole>,
    pub rounds: u32,
    /// Rounds that passed the single-shot check (`compiler-check`).
    pub rounds_ok: u32,
    /// Start-to-start spacing; a slow round delays the next one.
    pub interval_ms: u64,
    pub timeout_ms: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
    pub endpoints: Vec<EndpointAvailability>,
    pub roles: Vec<RoleAvailability>,
    /// Every round passed.
    pub ok: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SampleOptions {
    pub rounds: u32,
    pub interval: Duration,
    pub timeout: Duration,
}

/// Repeat the compiler reachability check `opts.rounds` times. With `from`,
/// only the roles that side must reach are probed (as `compiler-check
/// --from`). Refuses non-overlay endpoints before the first probe.
pub async fn sample<P: Prober>(
    endpoints: &[CompilerEndpoint],
    from: Option<CompilerSide>,
    local_ips: &[IpAddr],
    prober: &P,
    opts: &SampleOptions,
) -> Result<AvailabilityReport, String> {
    if opts.rounds == 0 {
        return Err("at least one sample round is required".into());
    }
    let required: Vec<CompilerRole> = from.map_or_else(Vec::new, |s| s.required_roles().to_vec());
    let mut series: Vec<(CompilerEndpoint, Vec<EndpointSample>)> = Vec::new();
    let mut role_rounds = vec![0u32; required.len()];
    let mut rounds_ok = 0;
    let started_at = OffsetDateTime::now_utc();
    let mut next = Instant::now();
    for round in 0..opts.rounds {
        tokio::time::sleep_until(next).await;
        next = Instant::now() + opts.interval;
        let at = OffsetDateTime::now_utc();
        let (checks, unverified, ok) = match from {
            Some(side) => {
                let r =
                    compiler::check_side(side, endpoints, local_ips, prober, opts.timeout).await?;
                (r.checks, r.unverified_roles, r.ok)
            }
            None => {
                let c =
                    compiler::check_endpoints(endpoints, local_ips, prober, opts.timeout).await?;
                let ok = !c.is_empty() && c.iter().all(|c| c.ok);
                (c, Vec::new(), ok)
            }
        };
        rounds_ok += u32::from(ok);
        for (role, n) in required.iter().zip(&mut role_rounds) {
            *n += u32::from(!unverified.contains(role));
        }
        if round == 0 {
            series = checks
                .iter()
                .map(|c| (c.endpoint.clone(), Vec::new()))
                .collect();
        }
        for ((_, s), check) in series.iter_mut().zip(&checks) {
            s.push(sample_of(at, check));
        }
    }
    Ok(AvailabilityReport {
        from,
        rounds: opts.rounds,
        rounds_ok,
        interval_ms: millis(opts.interval),
        timeout_ms: millis(opts.timeout),
        started_at,
        finished_at: OffsetDateTime::now_utc(),
        endpoints: series.into_iter().map(|(e, s)| summarize(e, s)).collect(),
        roles: required
            .iter()
            .zip(role_rounds)
            .map(|(&role, n)| RoleAvailability {
                role,
                rounds_verified: n,
                ratio: ratio(n, opts.rounds),
            })
            .collect(),
        required_roles: required,
        ok: rounds_ok == opts.rounds,
    })
}

fn sample_of(at: OffsetDateTime, check: &EndpointCheck) -> EndpointSample {
    let path_error = match &check.path {
        Some(PathProbe {
            path: PeerPath::Unreachable { detail },
            ..
        }) => Some(detail.clone()),
        None if check.path_class == PathClass::Unreachable => Some("path probe did not run".into()),
        _ => None,
    };
    EndpointSample {
        at,
        ok: check.ok,
        tcp_reachable: check.tcp.reachable,
        connect_ms: check.tcp.connect_ms,
        path_class: check.path_class,
        rtt_ms: check.path.as_ref().and_then(|p| p.rtt_ms),
        error: check.tcp.error.clone().or(path_error),
    }
}

fn summarize(endpoint: CompilerEndpoint, series: Vec<EndpointSample>) -> EndpointAvailability {
    let samples = series.len() as u32;
    let successes = series.iter().filter(|s| s.ok).count() as u32;
    let mut paths = PathCounts::default();
    series.iter().for_each(|s| paths.add(s.path_class));
    let path_changes = series
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0].path_class != w[1].path_class)
        .map(|(i, w)| PathChange {
            sample: i as u32 + 1,
            from: w[0].path_class,
            to: w[1].path_class,
        })
        .collect();
    let mut last = None;
    let mut direct_relay_flips = 0;
    for class in series.iter().map(|s| s.path_class) {
        if matches!(class, PathClass::Direct | PathClass::Relayed) {
            direct_relay_flips += u32::from(last.is_some_and(|l| l != class));
            last = Some(class);
        }
    }
    let connect: Vec<f64> = series.iter().filter_map(|s| s.connect_ms).collect();
    let rtt: Vec<f64> = series.iter().filter_map(|s| s.rtt_ms).collect();
    EndpointAvailability {
        endpoint,
        samples,
        successes,
        success_ratio: ratio(successes, samples),
        overlay_successes: series
            .iter()
            .filter(|s| s.ok && s.path_class != PathClass::Local)
            .count() as u32,
        paths,
        path_changes,
        direct_relay_flips,
        connect_ms: LatencyStats::of(&connect),
        rtt_ms: LatencyStats::of(&rtt),
        series,
    }
}

fn ratio(n: u32, of: u32) -> f64 {
    if of == 0 {
        0.0
    } else {
        f64::from(n) / f64::from(of)
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn mib_per_s(bytes: u64, d: Duration) -> Option<f64> {
    let s = d.as_secs_f64();
    (s > 0.0).then(|| bytes as f64 / s / (1024.0 * 1024.0))
}

fn mbit_per_s(bytes: u64, d: Duration) -> Option<f64> {
    let s = d.as_secs_f64();
    (s > 0.0).then(|| bytes as f64 * 8.0 / s / 1e6)
}

// Transfer protocol, one exchange per connection:
//   sender -> HELLO, u64 byte count
//   sink   -> READY, u64 limit           (or NAK, u64 limit, then close)
//   sender -> payload
//   sink   -> ACK, u64 bytes received, u64 sink microseconds
// Integers are big-endian; every reply is 24 bytes.
const HELLO: [u8; 8] = *b"DACSMSR1";
const READY: [u8; 8] = *b"DACSRDY1";
const NAK: [u8; 8] = *b"DACSNAK1";
const ACK: [u8; 8] = *b"DACSACK1";
const CHUNK: usize = 64 * 1024;

fn admitted(ip: IpAddr, allow_loopback: bool) -> bool {
    is_overlay_ip(ip) || (allow_loopback && ip.is_loopback())
}

fn is_local(ip: IpAddr, local_ips: &[IpAddr]) -> bool {
    ip.is_loopback() || local_ips.contains(&ip)
}

fn reply(tag: [u8; 8], a: u64, b: u64) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[..8].copy_from_slice(&tag);
    out[8..16].copy_from_slice(&a.to_be_bytes());
    out[16..].copy_from_slice(&b.to_be_bytes());
    out
}

fn parse_reply(r: &[u8; 24]) -> ([u8; 8], u64, u64) {
    let word = |i: usize| u64::from_be_bytes(r[i..i + 8].try_into().expect("8 bytes"));
    (r[..8].try_into().expect("8 bytes"), word(8), word(16))
}

async fn within<T>(
    limit: Duration,
    what: &str,
    fut: impl Future<Output = std::io::Result<T>>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{what}: {e}")),
        Err(_) => Err(format!("{what}: no progress for {}ms", limit.as_millis())),
    }
}

/// A transfer is overlay proof only when it completed and the probes before
/// and after it both saw a direct or relayed overlay path.
pub fn is_overlay_proof(complete: bool, before: PathClass, after: PathClass) -> bool {
    let overlay = |c| matches!(c, PathClass::Direct | PathClass::Relayed);
    complete && overlay(before) && overlay(after)
}

#[derive(Clone, Copy, Debug)]
pub struct TransferOptions {
    pub bytes: u64,
    pub connect_timeout: Duration,
    /// Limit for the whole exchange after connecting.
    pub deadline: Duration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferResult {
    pub to: SocketAddr,
    pub requested_bytes: u64,
    /// Payload bytes written to the socket.
    pub sent_bytes: u64,
    /// Payload bytes the sink reported receiving.
    pub acknowledged_bytes: Option<u64>,
    pub connect_ms: Option<f64>,
    /// First payload byte written until the sink's acknowledgement arrived
    /// (includes one one-way trip for the acknowledgement).
    pub duration_ms: Option<f64>,
    /// `acknowledged_bytes / duration`, for a complete transfer only.
    pub mib_per_s: Option<f64>,
    pub mbit_per_s: Option<f64>,
    /// The sink's own first-to-last-byte time.
    pub sink_duration_ms: Option<f64>,
    pub path_before: Option<PathProbe>,
    pub path_after: Option<PathProbe>,
    pub path_class_before: PathClass,
    pub path_class_after: PathClass,
    pub complete: bool,
    /// See [`is_overlay_proof`]; never true for a local target.
    pub overlay_proof: bool,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
}

/// Send `opts.bytes` to a [`Sink`] at `to`, which must be an overlay address.
/// A target on one of `local_ips` is measured but labelled `local`.
pub async fn send<P: Prober>(
    to: SocketAddr,
    local_ips: &[IpAddr],
    prober: &P,
    opts: &TransferOptions,
) -> Result<TransferResult, String> {
    send_inner(to, local_ips, prober, opts, false).await
}

async fn send_inner<P: Prober>(
    to: SocketAddr,
    local_ips: &[IpAddr],
    prober: &P,
    opts: &TransferOptions,
    allow_loopback: bool,
) -> Result<TransferResult, String> {
    if !admitted(to.ip(), allow_loopback) {
        return Err(format!(
            "transfer target {to} is not a private overlay address; nothing sent"
        ));
    }
    if opts.bytes == 0 {
        return Err("transfer size must be at least 1 byte".into());
    }
    let local = is_local(to.ip(), local_ips);
    let started_at = OffsetDateTime::now_utc();
    let probe = || async {
        if local {
            None
        } else {
            prober.path(to.ip()).await
        }
    };
    let class = |p: &Option<PathProbe>| match p {
        _ if local => PathClass::Local,
        Some(p) => path_class(&p.path),
        None => PathClass::Unreachable,
    };
    let path_before = probe().await;
    let pushed = push(to, opts).await;
    let path_after = probe().await;
    let complete = pushed.ack.is_some_and(|(n, _)| n == opts.bytes);
    let (before, after) = (class(&path_before), class(&path_after));
    let acked = pushed.ack.map(|(n, _)| n);
    let timed = |f: fn(u64, Duration) -> Option<f64>| match (complete, acked, pushed.duration) {
        (true, Some(n), Some(d)) => f(n, d),
        _ => None,
    };
    Ok(TransferResult {
        to,
        requested_bytes: opts.bytes,
        sent_bytes: pushed.sent,
        acknowledged_bytes: acked,
        connect_ms: pushed.connect_ms,
        duration_ms: pushed.duration.map(ms),
        mib_per_s: timed(mib_per_s),
        mbit_per_s: timed(mbit_per_s),
        sink_duration_ms: pushed.ack.map(|(_, us)| us as f64 / 1e3),
        path_before,
        path_after,
        path_class_before: before,
        path_class_after: after,
        complete,
        overlay_proof: is_overlay_proof(complete, before, after),
        error: pushed.error.or_else(|| {
            (!complete).then(|| format!("sink acknowledged {acked:?} of {} bytes", opts.bytes))
        }),
        started_at,
        finished_at: OffsetDateTime::now_utc(),
    })
}

#[derive(Default)]
struct Pushed {
    connect_ms: Option<f64>,
    sent: u64,
    duration: Option<Duration>,
    ack: Option<(u64, u64)>,
    error: Option<String>,
}

async fn push(to: SocketAddr, opts: &TransferOptions) -> Pushed {
    let mut out = Pushed::default();
    let t = Instant::now();
    let mut stream = match within(opts.connect_timeout, "connect", TcpStream::connect(to)).await {
        Ok(s) => s,
        Err(e) => {
            out.error = Some(e);
            return out;
        }
    };
    out.connect_ms = Some(ms(t.elapsed()));
    let _ = stream.set_nodelay(true);
    let mut started = None;
    let exchange = exchange(&mut stream, opts.bytes, &mut out.sent, &mut started);
    match tokio::time::timeout(opts.deadline, exchange).await {
        Ok(Ok(ack)) => {
            out.duration = started.map(|s: Instant| s.elapsed());
            out.ack = Some(ack);
        }
        Ok(Err(e)) => out.error = Some(e),
        Err(_) => {
            out.error = Some(format!(
                "transfer did not finish within {}s",
                opts.deadline.as_secs()
            ));
        }
    }
    out
}

async fn exchange(
    stream: &mut TcpStream,
    bytes: u64,
    sent: &mut u64,
    started: &mut Option<Instant>,
) -> Result<(u64, u64), String> {
    let mut hello = [0u8; 16];
    hello[..8].copy_from_slice(&HELLO);
    hello[8..].copy_from_slice(&bytes.to_be_bytes());
    stream
        .write_all(&hello)
        .await
        .map_err(|e| format!("send header: {e}"))?;
    let mut r = [0u8; 24];
    stream
        .read_exact(&mut r)
        .await
        .map_err(|e| format!("no reply from a measure sink: {e}"))?;
    match parse_reply(&r) {
        (READY, _, _) => {}
        (NAK, limit, _) => {
            return Err(format!(
                "sink refused {bytes} bytes; its limit is {limit} bytes"
            ));
        }
        _ => return Err("unexpected reply; is `dacs-net measure sink` listening there?".into()),
    }
    let chunk: Vec<u8> = (0..CHUNK).map(|i| (i % 251) as u8).collect();
    *started = Some(Instant::now());
    while *sent < bytes {
        let n = (bytes - *sent).min(CHUNK as u64) as usize;
        stream
            .write_all(&chunk[..n])
            .await
            .map_err(|e| format!("send failed after {} bytes: {e}", *sent))?;
        *sent += n as u64;
    }
    stream
        .read_exact(&mut r)
        .await
        .map_err(|e| format!("no acknowledgement from the sink: {e}"))?;
    match parse_reply(&r) {
        (ACK, received, micros) => Ok((received, micros)),
        _ => Err("unexpected acknowledgement from the sink".into()),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SinkOptions {
    /// Connections to serve before returning.
    pub count: u32,
    /// Larger transfers are refused before any payload is sent.
    pub max_bytes: u64,
    /// Wait for each connection.
    pub wait: Duration,
    /// Abort a connection that makes no progress for this long.
    pub idle: Duration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SinkResult {
    pub peer: SocketAddr,
    /// The sender is this host (its own overlay IP or loopback): not overlay proof.
    pub peer_local: bool,
    pub expected_bytes: Option<u64>,
    pub received_bytes: u64,
    pub duration_ms: Option<f64>,
    pub mib_per_s: Option<f64>,
    pub complete: bool,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SinkReport {
    pub listen: SocketAddr,
    pub results: Vec<SinkResult>,
    /// Why serving stopped before `count` connections, if it did.
    pub stopped: Option<String>,
    pub ok: bool,
}

/// Receiving end of a measured transfer, bound to an overlay address.
pub struct Sink {
    listener: TcpListener,
    allow_loopback: bool,
}

impl Sink {
    /// Bind `listen`, which must be an overlay address of this host
    /// (`0.0.0.0` and LAN addresses are refused).
    pub async fn bind(listen: SocketAddr) -> Result<Self, String> {
        Self::bind_inner(listen, false).await
    }

    async fn bind_inner(listen: SocketAddr, allow_loopback: bool) -> Result<Self, String> {
        if !admitted(listen.ip(), allow_loopback) {
            return Err(format!(
                "sink address {listen} is not a private overlay address; refusing to listen"
            ));
        }
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|e| format!("bind {listen}: {e}"))?;
        Ok(Self {
            listener,
            allow_loopback,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, String> {
        self.listener.local_addr().map_err(|e| e.to_string())
    }

    /// Serve up to `opts.count` connections, one at a time.
    pub async fn serve(&self, opts: &SinkOptions, local_ips: &[IpAddr]) -> SinkReport {
        let mut results = Vec::new();
        let mut stopped = None;
        for _ in 0..opts.count {
            match tokio::time::timeout(opts.wait, self.listener.accept()).await {
                Ok(Ok((stream, peer))) => {
                    results.push(self.receive(stream, peer, opts, local_ips).await);
                }
                Ok(Err(e)) => {
                    stopped = Some(format!("accept: {e}"));
                    break;
                }
                Err(_) => {
                    stopped = Some(format!(
                        "no sender connected within {}s",
                        opts.wait.as_secs()
                    ));
                    break;
                }
            }
        }
        SinkReport {
            listen: self
                .listener
                .local_addr()
                .unwrap_or(SocketAddr::from(([0; 4], 0))),
            ok: stopped.is_none() && !results.is_empty() && results.iter().all(|r| r.complete),
            results,
            stopped,
        }
    }

    async fn receive(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        opts: &SinkOptions,
        local_ips: &[IpAddr],
    ) -> SinkResult {
        let mut r = SinkResult {
            peer,
            peer_local: is_local(peer.ip(), local_ips),
            expected_bytes: None,
            received_bytes: 0,
            duration_ms: None,
            mib_per_s: None,
            complete: false,
            error: None,
            started_at: OffsetDateTime::now_utc(),
            finished_at: OffsetDateTime::now_utc(),
        };
        if admitted(peer.ip(), self.allow_loopback) {
            match read_transfer(&mut stream, opts, &mut r).await {
                Ok(()) => r.complete = true,
                Err(e) => r.error = Some(e),
            }
        } else {
            r.error = Some("refused a sender outside the overlay".into());
        }
        r.finished_at = OffsetDateTime::now_utc();
        r
    }
}

async fn read_transfer(
    stream: &mut TcpStream,
    opts: &SinkOptions,
    r: &mut SinkResult,
) -> Result<(), String> {
    let mut hello = [0u8; 16];
    within(opts.idle, "read header", stream.read_exact(&mut hello)).await?;
    if hello[..8] != HELLO {
        return Err("not a `dacs-net measure send` client".into());
    }
    let expected = u64::from_be_bytes(hello[8..].try_into().expect("8 bytes"));
    r.expected_bytes = Some(expected);
    if expected > opts.max_bytes {
        let _ = within(
            opts.idle,
            "refuse",
            stream.write_all(&reply(NAK, opts.max_bytes, 0)),
        )
        .await;
        return Err(format!(
            "refused {expected} bytes; limit is {}",
            opts.max_bytes
        ));
    }
    within(
        opts.idle,
        "send ready",
        stream.write_all(&reply(READY, opts.max_bytes, 0)),
    )
    .await?;
    let start = Instant::now();
    let mut buf = vec![0u8; CHUNK];
    while r.received_bytes < expected {
        let want = (expected - r.received_bytes).min(CHUNK as u64) as usize;
        let n = within(opts.idle, "read payload", stream.read(&mut buf[..want])).await?;
        if n == 0 {
            return Err(format!(
                "sender closed after {} of {expected} bytes",
                r.received_bytes
            ));
        }
        r.received_bytes += n as u64;
    }
    let elapsed = start.elapsed();
    r.duration_ms = Some(ms(elapsed));
    r.mib_per_s = mib_per_s(r.received_bytes, elapsed);
    let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    within(
        opts.idle,
        "send acknowledgement",
        stream.write_all(&reply(ACK, r.received_bytes, micros)),
    )
    .await
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolVersions {
    pub dacs_net: String,
    pub contract_revision: String,
    /// First line of `tailscale version`; none when the CLI was not found.
    pub tailscale: Option<String>,
}

impl ToolVersions {
    pub async fn detect() -> Self {
        let tailscale = match crate::tailscale::Tailscale::locate() {
            Ok(ts) => ts
                .run(&["version"], Duration::from_secs(10))
                .await
                .ok()
                .and_then(|o| o.stdout.lines().next().map(|l| l.trim().to_owned()))
                .filter(|v| !v.is_empty()),
            Err(_) => None,
        };
        Self {
            dacs_net: env!("CARGO_PKG_VERSION").into(),
            contract_revision: CONTRACT_REVISION.into(),
            tailscale,
        }
    }
}

/// Evidence record for one build: an availability window plus transfers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasureEvidence {
    pub schema: String,
    /// Build identifier supplied by the caller (Person 4).
    pub build: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub tools: ToolVersions,
    /// This host's overlay IPs, used to label local paths.
    pub local_overlay_ips: Vec<IpAddr>,
    pub from: Option<CompilerSide>,
    pub availability: AvailabilityReport,
    pub transfers: Vec<TransferResult>,
    /// Path classes seen in any sample or transfer probe.
    pub paths_observed: PathCounts,
    /// Some sample or transfer used a direct or relayed overlay path.
    pub overlay_proof: bool,
    /// Every round passed and every transfer is overlay proof.
    pub ok: bool,
    pub notes: Vec<String>,
}

/// Wrap a window and transfers into one record.
pub fn assemble(
    build: Option<String>,
    tools: ToolVersions,
    local_overlay_ips: Vec<IpAddr>,
    availability: AvailabilityReport,
    transfers: Vec<TransferResult>,
) -> MeasureEvidence {
    let mut paths = PathCounts::default();
    for e in &availability.endpoints {
        e.series.iter().for_each(|s| paths.add(s.path_class));
    }
    for t in &transfers {
        paths.add(t.path_class_before);
        paths.add(t.path_class_after);
    }
    let overlay_proof = availability
        .endpoints
        .iter()
        .any(|e| e.overlay_successes > 0)
        || transfers.iter().any(|t| t.overlay_proof);
    let mut notes = vec![
        "Figures describe this window and these paths only, one TCP stream per transfer; \
         they do not predict relay throughput or build speed."
            .to_owned(),
    ];
    if paths.local > 0 {
        notes.push(
            "Endpoints or targets on this host were labelled local and are not overlay proof."
                .into(),
        );
    }
    if paths.relayed > 0 {
        notes.push(
            "Relayed (DERP or peer relay) paths were observed; relay throughput is not promised."
                .into(),
        );
    }
    if availability
        .endpoints
        .iter()
        .any(|e| e.direct_relay_flips > 0)
        || transfers
            .iter()
            .any(|t| t.path_class_before != t.path_class_after)
    {
        notes.push(
            "The path changed during the window; see path_changes and transfer probes.".into(),
        );
    }
    if !overlay_proof {
        notes.push(
            "No sample or transfer used an overlay path: this record is not overlay evidence."
                .into(),
        );
    }
    MeasureEvidence {
        schema: EVIDENCE_SCHEMA.into(),
        build,
        created_at: OffsetDateTime::now_utc(),
        tools,
        local_overlay_ips,
        from: availability.from,
        ok: availability.ok && overlay_proof && transfers.iter().all(|t| t.overlay_proof),
        availability,
        transfers,
        paths_observed: paths,
        overlay_proof,
        notes,
    }
}

/// Endpoints `from` would probe (all of them without a side).
fn probed(endpoints: &[CompilerEndpoint], from: Option<CompilerSide>) -> Vec<&CompilerEndpoint> {
    endpoints
        .iter()
        .filter(|e| from.is_none_or(|s| s.required_roles().contains(&e.role)))
        .collect()
}

fn refuse_non_overlay(
    endpoints: &[CompilerEndpoint],
    from: Option<CompilerSide>,
    targets: &[SocketAddr],
    allow_loopback: bool,
) -> Result<(), String> {
    if let Some(e) = probed(endpoints, from)
        .into_iter()
        .find(|e| !is_overlay_ip(e.overlay_ip))
    {
        return Err(format!(
            "compiler endpoint {} is not a private overlay address; no probe sent",
            e.socket_addr()
        ));
    }
    if let Some(t) = targets.iter().find(|t| !admitted(t.ip(), allow_loopback)) {
        return Err(format!(
            "transfer target {t} is not a private overlay address; nothing sent"
        ));
    }
    Ok(())
}

pub struct EvidenceRequest<'a> {
    pub endpoints: &'a [CompilerEndpoint],
    pub from: Option<CompilerSide>,
    pub window: SampleOptions,
    /// Sinks to send `transfer.bytes` to, one after another, after the window.
    pub transfers: &'a [SocketAddr],
    pub transfer: TransferOptions,
    pub build: Option<String>,
}

/// Run the window and transfers and assemble the record. Every address is
/// checked before the first probe.
pub async fn collect<P: Prober>(
    req: &EvidenceRequest<'_>,
    local_ips: &[IpAddr],
    prober: &P,
    tools: ToolVersions,
) -> Result<MeasureEvidence, String> {
    collect_inner(req, local_ips, prober, tools, false).await
}

async fn collect_inner<P: Prober>(
    req: &EvidenceRequest<'_>,
    local_ips: &[IpAddr],
    prober: &P,
    tools: ToolVersions,
    allow_loopback: bool,
) -> Result<MeasureEvidence, String> {
    refuse_non_overlay(req.endpoints, req.from, req.transfers, allow_loopback)?;
    if !req.transfers.is_empty() && req.transfer.bytes == 0 {
        return Err("transfer size must be at least 1 byte".into());
    }
    let availability = sample(req.endpoints, req.from, local_ips, prober, &req.window).await?;
    let mut transfers = Vec::with_capacity(req.transfers.len());
    for &to in req.transfers {
        transfers.push(send_inner(to, local_ips, prober, &req.transfer, allow_loopback).await?);
    }
    Ok(assemble(
        req.build.clone(),
        tools,
        local_ips.to_vec(),
        availability,
        transfers,
    ))
}

/// This host's overlay IPs, or none when the client is not logged in.
pub async fn local_overlay_ips() -> Vec<IpAddr> {
    match crate::detect().await {
        TailnetClientState::LoggedIn { overlay_ips, .. } => overlay_ips,
        _ => Vec::new(),
    }
}

/// `dacs-net measure` subcommands.
#[derive(clap::Subcommand)]
pub enum MeasureCommand {
    /// Repeat the compiler reachability probe over a window: per-endpoint
    /// success ratio, path class per sample and connect-latency spread.
    Availability {
        #[command(flatten)]
        window: WindowArgs,
    },
    /// Receive measured transfers on this host's overlay address. Use a port
    /// the ACL grants between the two nodes (e.g. 10501 on a compile server
    /// before sccache-dist binds it).
    Sink {
        /// This host's overlay IP and port; other addresses are refused.
        #[arg(long)]
        listen: SocketAddr,
        /// Transfers to receive before exiting.
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Refuse larger transfers.
        #[arg(long, default_value_t = 1 << 30)]
        max_bytes: u64,
        /// Give up waiting for a sender after this long.
        #[arg(long, default_value_t = 600)]
        wait_secs: u64,
        /// Abort a transfer that makes no progress for this long.
        #[arg(long, default_value_t = 30)]
        idle_secs: u64,
    },
    /// Send N bytes to a `measure sink` over the overlay; reports duration,
    /// throughput and the path class before and after. Exit 2 unless the
    /// transfer completed over a direct or relayed overlay path.
    Send {
        /// Overlay address of the sink; other addresses are refused.
        #[arg(long)]
        to: SocketAddr,
        #[arg(long)]
        bytes: u64,
        #[command(flatten)]
        transfer: TransferArgs,
    },
    /// Availability window plus optional transfers as one evidence record
    /// (`dacs-net/measure-evidence/1`) to attach to a build.
    Evidence {
        #[command(flatten)]
        window: WindowArgs,
        /// Sink to send `--bytes` to after the window (repeatable).
        #[arg(long = "transfer-to", requires = "bytes")]
        transfer_to: Vec<SocketAddr>,
        /// Bytes per transfer.
        #[arg(long, requires = "transfer_to")]
        bytes: Option<u64>,
        #[command(flatten)]
        transfer: TransferArgs,
        /// Build identifier to record.
        #[arg(long)]
        build: Option<String>,
    },
}

#[derive(clap::Args)]
pub struct WindowArgs {
    /// JSON list from netadmin /v1/compiler-endpoints.
    #[arg(long)]
    endpoints: PathBuf,
    /// This host's sccache role: probe only the roles it must reach.
    #[arg(long)]
    from: Option<CompilerSide>,
    /// Probe rounds.
    #[arg(long, default_value_t = 10)]
    samples: u32,
    /// Start-to-start spacing of rounds.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
    /// TCP connect timeout per probe.
    #[arg(long, default_value_t = 3000)]
    timeout_ms: u64,
}

impl WindowArgs {
    fn options(&self) -> SampleOptions {
        SampleOptions {
            rounds: self.samples,
            interval: Duration::from_millis(self.interval_ms),
            timeout: Duration::from_millis(self.timeout_ms),
        }
    }

    fn endpoints(&self) -> Result<Vec<CompilerEndpoint>, String> {
        let p = &self.endpoints;
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
    }
}

#[derive(clap::Args)]
pub struct TransferArgs {
    #[arg(long, default_value_t = 3000)]
    connect_timeout_ms: u64,
    /// Limit for one whole transfer.
    #[arg(long, default_value_t = 300)]
    deadline_secs: u64,
}

impl TransferArgs {
    fn options(&self, bytes: u64) -> TransferOptions {
        TransferOptions {
            bytes,
            connect_timeout: Duration::from_millis(self.connect_timeout_ms),
            deadline: Duration::from_secs(self.deadline_secs),
        }
    }
}

fn print<T: Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serializable")
    );
}

/// Run a `measure` subcommand. `Ok(false)` means it ran but a check failed.
pub async fn run_cli(cmd: MeasureCommand) -> Result<bool, String> {
    let live = &compiler::Live;
    match cmd {
        MeasureCommand::Availability { window } => {
            let endpoints = window.endpoints()?;
            refuse_non_overlay(&endpoints, window.from, &[], false)?;
            let local = local_overlay_ips().await;
            let r = sample(&endpoints, window.from, &local, live, &window.options()).await?;
            print(&r);
            Ok(r.ok)
        }
        MeasureCommand::Sink {
            listen,
            count,
            max_bytes,
            wait_secs,
            idle_secs,
        } => {
            let sink = Sink::bind(listen).await?;
            let local = local_overlay_ips().await;
            eprintln!("measure sink listening on {}", sink.local_addr()?);
            let opts = SinkOptions {
                count,
                max_bytes,
                wait: Duration::from_secs(wait_secs),
                idle: Duration::from_secs(idle_secs),
            };
            let r = sink.serve(&opts, &local).await;
            print(&r);
            Ok(r.ok)
        }
        MeasureCommand::Send {
            to,
            bytes,
            transfer,
        } => {
            refuse_non_overlay(&[], None, &[to], false)?;
            let local = local_overlay_ips().await;
            let r = send(to, &local, live, &transfer.options(bytes)).await?;
            print(&r);
            Ok(r.overlay_proof)
        }
        MeasureCommand::Evidence {
            window,
            transfer_to,
            bytes,
            transfer,
            build,
        } => {
            let endpoints = window.endpoints()?;
            refuse_non_overlay(&endpoints, window.from, &transfer_to, false)?;
            let local = local_overlay_ips().await;
            let tools = ToolVersions::detect().await;
            let req = EvidenceRequest {
                endpoints: &endpoints,
                from: window.from,
                window: window.options(),
                transfers: &transfer_to,
                transfer: transfer.options(bytes.unwrap_or(0)),
                build,
            };
            let r = collect(&req, &local, live, tools).await?;
            print(&r);
            Ok(r.ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    use dacs_net_contract::{HostName, TcpProbe};

    /// One scripted probe outcome: connect time when the listener answers,
    /// and the path a ping would see.
    #[derive(Clone)]
    struct Step {
        connect_ms: Option<f64>,
        path: Option<PeerPath>,
    }

    /// Per-IP scripted outcomes, consumed one per round; records every probe.
    #[derive(Default)]
    struct Scripted {
        steps: RefCell<HashMap<IpAddr, VecDeque<Step>>>,
        current: RefCell<HashMap<IpAddr, Step>>,
        sent: RefCell<Vec<String>>,
    }

    impl Scripted {
        fn script(self, ip: &str, steps: Vec<Step>) -> Self {
            self.steps
                .borrow_mut()
                .insert(ip.parse().unwrap(), steps.into());
            self
        }
    }

    impl Prober for Scripted {
        async fn tcp(&self, target: SocketAddr, _: Duration) -> TcpProbe {
            self.sent.borrow_mut().push(format!("tcp {target}"));
            let step = self
                .steps
                .borrow_mut()
                .get_mut(&target.ip())
                .and_then(VecDeque::pop_front)
                .unwrap_or(Step {
                    connect_ms: None,
                    path: None,
                });
            self.current.borrow_mut().insert(target.ip(), step.clone());
            TcpProbe {
                target,
                overlay_private: is_overlay_ip(target.ip()),
                reachable: step.connect_ms.is_some(),
                connect_ms: step.connect_ms,
                error: step
                    .connect_ms
                    .is_none()
                    .then(|| "connection refused".into()),
            }
        }

        async fn path(&self, peer: IpAddr) -> Option<PathProbe> {
            self.sent.borrow_mut().push(format!("ping {peer}"));
            let path = self.current.borrow().get(&peer)?.path.clone()?;
            Some(PathProbe {
                peer: peer.to_string(),
                rtt_ms: (!matches!(path, PeerPath::Unreachable { .. })).then_some(2.0),
                path,
                measured_at: OffsetDateTime::now_utc(),
            })
        }
    }

    fn up(ms: f64, path: PeerPath) -> Step {
        Step {
            connect_ms: Some(ms),
            path: Some(path),
        }
    }

    fn direct() -> PeerPath {
        PeerPath::Direct {
            endpoint: "192.0.2.10:41641".into(),
        }
    }

    fn derp() -> PeerPath {
        PeerPath::Derp {
            region: "dacs".into(),
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

    fn window(rounds: u32) -> SampleOptions {
        SampleOptions {
            rounds,
            interval: Duration::ZERO,
            timeout: Duration::from_millis(100),
        }
    }

    fn tools() -> ToolVersions {
        ToolVersions {
            dacs_net: "test".into(),
            contract_revision: CONTRACT_REVISION.into(),
            tailscale: Some("1.104.7-test".into()),
        }
    }

    #[test]
    fn latency_stats_use_nearest_rank() {
        let mut v: Vec<f64> = (1..=20).map(f64::from).rev().collect();
        v.push(f64::NAN);
        let s = LatencyStats::of(&v).unwrap();
        assert_eq!(
            (s.count, s.min, s.median, s.p95, s.max),
            (20, 1.0, 10.0, 19.0, 20.0)
        );
        let one = LatencyStats::of(&[4.5]).unwrap();
        assert_eq!(
            (one.min, one.median, one.p95, one.max),
            (4.5, 4.5, 4.5, 4.5)
        );
        assert_eq!(LatencyStats::of(&[]), None);
    }

    #[tokio::test]
    async fn window_reports_ratio_flips_and_latency_per_endpoint() {
        let fake = Scripted::default()
            .script(
                "100.64.0.5",
                vec![
                    up(1.0, direct()),
                    up(3.0, derp()),
                    up(
                        9.0,
                        PeerPath::Unreachable {
                            detail: "no pong".into(),
                        },
                    ),
                    up(2.0, direct()),
                    Step {
                        connect_ms: None,
                        path: Some(direct()),
                    },
                ],
            )
            .script("100.64.0.6", vec![up(1.0, direct()); 5]);
        let list = [
            ep("sched", CompilerRole::Scheduler, "100.64.0.6"),
            ep("srv", CompilerRole::Server, "100.64.0.5"),
        ];
        let r = sample(&list, Some(CompilerSide::Client), &[], &fake, &window(5))
            .await
            .unwrap();
        assert_eq!((r.rounds, r.rounds_ok, r.ok), (5, 3, false));
        let srv = &r.endpoints[1];
        assert_eq!(srv.endpoint.role, CompilerRole::Server);
        assert_eq!((srv.samples, srv.successes, srv.success_ratio), (5, 3, 0.6));
        assert_eq!(
            srv.paths,
            PathCounts {
                direct: 3,
                relayed: 1,
                local: 0,
                unreachable: 1
            }
        );
        let changes: Vec<(u32, PathClass, PathClass)> = srv
            .path_changes
            .iter()
            .map(|c| (c.sample, c.from, c.to))
            .collect();
        assert_eq!(
            changes,
            [
                (1, PathClass::Direct, PathClass::Relayed),
                (2, PathClass::Relayed, PathClass::Unreachable),
                (3, PathClass::Unreachable, PathClass::Direct),
            ]
        );
        assert_eq!(
            srv.direct_relay_flips, 2,
            "relayed -> (gap) -> direct counts"
        );
        let c = srv.connect_ms.as_ref().unwrap();
        assert_eq!((c.count, c.min, c.median, c.max), (4, 1.0, 2.0, 9.0));
        assert_eq!(srv.series[4].error.as_deref(), Some("connection refused"));
        assert_eq!(srv.series[2].error.as_deref(), Some("no pong"));
        // The path is probed even when the listener refuses (sample 4).
        assert_eq!(srv.rtt_ms.as_ref().unwrap().count, 4);
        let sched = &r.endpoints[0];
        assert_eq!((sched.success_ratio, sched.direct_relay_flips), (1.0, 0));
        let roles: Vec<(CompilerRole, u32)> = r
            .roles
            .iter()
            .map(|x| (x.role, x.rounds_verified))
            .collect();
        assert_eq!(
            roles,
            [(CompilerRole::Scheduler, 5), (CompilerRole::Server, 3)]
        );
    }

    #[tokio::test]
    async fn window_refuses_non_overlay_endpoints_before_any_probe() {
        let fake = Scripted::default();
        let list = [
            ep("ok", CompilerRole::Server, "100.64.0.5"),
            ep("nat", CompilerRole::Server, "192.168.1.20"),
        ];
        let err = sample(&list, None, &[], &fake, &window(3))
            .await
            .unwrap_err();
        assert!(err.contains("192.168.1.20:10501"), "{err}");
        assert!(fake.sent.borrow().is_empty());
        assert!(
            sample(&list[..1], None, &[], &fake, &window(0))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn co_located_endpoint_samples_are_local_and_not_overlay_successes() {
        let fake = Scripted::default().script("100.64.0.5", vec![up(0.1, direct()); 3]);
        let list = [ep("guest", CompilerRole::Server, "100.64.0.5")];
        let me: IpAddr = "100.64.0.5".parse().unwrap();
        let r = sample(
            &list,
            Some(CompilerSide::Scheduler),
            &[me],
            &fake,
            &window(3),
        )
        .await
        .unwrap();
        let e = &r.endpoints[0];
        assert_eq!((e.successes, e.overlay_successes, e.paths.local), (3, 0, 3));
        assert_eq!(r.roles[0].rounds_verified, 0);
        assert!(!r.ok);
        assert!(!fake.sent.borrow().iter().any(|s| s.starts_with("ping")));
    }

    #[tokio::test]
    async fn rounds_are_spaced_by_the_interval() {
        let fake = Scripted::default().script("100.64.0.5", vec![up(1.0, direct()); 3]);
        let list = [ep("srv", CompilerRole::Server, "100.64.0.5")];
        let opts = SampleOptions {
            interval: Duration::from_millis(40),
            ..window(3)
        };
        let r = sample(&list, None, &[], &fake, &opts).await.unwrap();
        assert!(r.ok);
        let span = r.endpoints[0].series[2].at - r.endpoints[0].series[0].at;
        assert!(span >= time::Duration::milliseconds(75), "{span}");
    }

    fn sink_opts(max_bytes: u64) -> SinkOptions {
        SinkOptions {
            count: 1,
            max_bytes,
            wait: Duration::from_secs(10),
            idle: Duration::from_secs(5),
        }
    }

    fn xfer(bytes: u64) -> TransferOptions {
        TransferOptions {
            bytes,
            connect_timeout: Duration::from_secs(2),
            deadline: Duration::from_secs(20),
        }
    }

    async fn loopback_sink() -> (Sink, SocketAddr) {
        let sink = Sink::bind_inner("127.0.0.1:0".parse().unwrap(), true)
            .await
            .unwrap();
        let addr = sink.local_addr().unwrap();
        (sink, addr)
    }

    /// Serve one connection while `client` runs on the same task.
    async fn with_sink<F: Future>(
        sink: &Sink,
        opts: SinkOptions,
        client: F,
    ) -> (SinkReport, F::Output) {
        tokio::join!(sink.serve(&opts, &[]), client)
    }

    #[tokio::test]
    async fn loopback_transfer_is_measured_but_labelled_local() {
        let (sink, to) = loopback_sink().await;
        let fake = Scripted::default();
        let bytes = 3 * CHUNK as u64 + 17;
        let (report, sent) = with_sink(
            &sink,
            sink_opts(1 << 20),
            send_inner(to, &[], &fake, &xfer(bytes), true),
        )
        .await;
        let t = sent.unwrap();
        assert!(t.complete, "{t:?}");
        assert_eq!((t.sent_bytes, t.acknowledged_bytes), (bytes, Some(bytes)));
        assert!(t.duration_ms.unwrap() > 0.0 && t.mib_per_s.unwrap() > 0.0);
        assert!(t.connect_ms.is_some() && t.sink_duration_ms.is_some());
        assert_eq!(
            (t.path_class_before, t.path_class_after),
            (PathClass::Local, PathClass::Local)
        );
        assert!(!t.overlay_proof, "loopback must never be overlay proof");
        assert!(t.path_before.is_none() && t.error.is_none());
        assert!(
            fake.sent.borrow().is_empty(),
            "no path probe for a local target"
        );
        assert!(report.ok, "{report:?}");
        let got = &report.results[0];
        assert!(got.complete && got.peer_local);
        assert_eq!(
            (got.expected_bytes, got.received_bytes),
            (Some(bytes), bytes)
        );
    }

    #[tokio::test]
    async fn production_entry_points_refuse_loopback_and_non_overlay() {
        let fake = Scripted::default();
        for to in ["127.0.0.1:10501", "192.168.1.20:10501", "0.0.0.0:10501"] {
            let err = send(to.parse().unwrap(), &[], &fake, &xfer(10))
                .await
                .unwrap_err();
            assert!(err.contains("not a private overlay address"), "{err}");
        }
        assert!(fake.sent.borrow().is_empty());
        for listen in ["127.0.0.1:0", "0.0.0.0:0", "[::]:0"] {
            let err = Sink::bind(listen.parse().unwrap()).await.err().unwrap();
            assert!(err.contains("refusing to listen"), "{err}");
        }
        let endpoints = [ep("srv", CompilerRole::Server, "100.64.0.5")];
        let transfers = ["127.0.0.1:10501".parse().unwrap()];
        let req = EvidenceRequest {
            endpoints: &endpoints,
            from: None,
            window: window(1),
            transfers: &transfers,
            transfer: xfer(10),
            build: None,
        };
        let err = collect(&req, &[], &fake, tools()).await.unwrap_err();
        assert!(err.contains("127.0.0.1:10501"), "{err}");
        assert!(fake.sent.borrow().is_empty(), "refused before sampling");
    }

    #[tokio::test]
    async fn oversized_transfer_is_refused_before_payload() {
        let (sink, to) = loopback_sink().await;
        let (report, sent) = with_sink(
            &sink,
            sink_opts(1000),
            send_inner(to, &[], &Scripted::default(), &xfer(2000), true),
        )
        .await;
        let t = sent.unwrap();
        assert!(!t.complete && !t.overlay_proof);
        assert_eq!(t.sent_bytes, 0);
        assert!(
            t.error.as_deref().unwrap().contains("limit is 1000"),
            "{t:?}"
        );
        assert!(t.mib_per_s.is_none());
        assert!(!report.ok);
        let err = report.results[0].error.as_deref().unwrap();
        assert!(err.contains("refused 2000"), "{err}");
    }

    #[tokio::test]
    async fn unreachable_sink_and_foreign_client_fail_cleanly() {
        let gone = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = gone.local_addr().unwrap();
        drop(gone);
        let t = send_inner(to, &[], &Scripted::default(), &xfer(10), true)
            .await
            .unwrap();
        assert!(!t.complete && t.connect_ms.is_none());
        assert!(t.error.as_deref().unwrap().starts_with("connect"), "{t:?}");

        let (sink, addr) = loopback_sink().await;
        let foreign = async {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            s
        };
        let (report, _keep_open) = with_sink(&sink, sink_opts(1 << 20), foreign).await;
        assert!(!report.ok);
        let err = report.results[0].error.as_deref().unwrap();
        assert!(
            err.contains("not a `dacs-net measure send` client"),
            "{err}"
        );
    }

    #[test]
    fn overlay_proof_needs_completion_and_an_overlay_path_both_sides() {
        use PathClass::*;
        assert!(is_overlay_proof(true, Direct, Direct));
        assert!(is_overlay_proof(true, Direct, Relayed));
        assert!(!is_overlay_proof(true, Local, Local));
        assert!(!is_overlay_proof(false, Direct, Direct));
        assert!(!is_overlay_proof(true, Direct, Unreachable));
    }

    #[tokio::test]
    async fn evidence_record_combines_window_and_transfers_without_local_proof() {
        let fake = Scripted::default().script(
            "100.64.0.5",
            vec![up(1.0, direct()), up(1.5, derp()), up(1.2, direct())],
        );
        let (sink, to) = loopback_sink().await;
        let endpoints = [ep("srv", CompilerRole::Server, "100.64.0.5")];
        let transfers = [to];
        let req = EvidenceRequest {
            endpoints: &endpoints,
            from: None,
            window: window(3),
            transfers: &transfers,
            transfer: xfer(4096),
            build: Some("build-42".into()),
        };
        let (_, ev) = with_sink(
            &sink,
            sink_opts(1 << 20),
            collect_inner(&req, &[], &fake, tools(), true),
        )
        .await;
        let ev = ev.unwrap();
        assert_eq!(ev.schema, EVIDENCE_SCHEMA);
        assert_eq!(ev.build.as_deref(), Some("build-42"));
        assert!(ev.availability.ok);
        assert!(ev.overlay_proof, "the window reached srv over the overlay");
        assert!(ev.transfers[0].complete && !ev.transfers[0].overlay_proof);
        assert!(!ev.ok, "a local transfer keeps the record from passing");
        assert_eq!(
            ev.paths_observed,
            PathCounts {
                direct: 2,
                relayed: 1,
                local: 2,
                unreachable: 0
            }
        );
        let notes = ev.notes.join("\n");
        assert!(notes.contains("labelled local") && notes.contains("not promised"));
        assert!(notes.contains("path changed"), "{notes}");

        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["tools"]["tailscale"], "1.104.7-test");
        assert_eq!(
            v["availability"]["endpoints"][0]["series"][1]["path_class"],
            "relayed"
        );
        assert_eq!(v["transfers"][0]["path_class_after"], "local");
        let back: MeasureEvidence = serde_json::from_value(v).unwrap();
        assert_eq!(back, ev);

        let window_only = assemble(None, tools(), vec![], ev.availability.clone(), vec![]);
        assert!(window_only.ok, "a window alone passes over direct/relayed");
        let mut no_overlay = ev.availability.clone();
        no_overlay.endpoints[0].overlay_successes = 0;
        let r = assemble(None, tools(), vec![], no_overlay, ev.transfers.clone());
        assert!(!r.overlay_proof && !r.ok);
        assert!(r.notes.iter().any(|n| n.contains("not overlay evidence")));
    }
}
