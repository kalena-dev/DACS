//! Process-level checks of the real `dacs-netadmin` binary against the
//! in-process mock Headscale: TLS serving, plaintext refusal, secret-file
//! checks, the `issue` subcommand, and a scan of everything it logs.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::*;
use dacs_net_contract::EnrollmentGrant;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_dacs-netadmin");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Proc {
    child: tokio::process::Child,
    out: tokio::task::JoinHandle<Vec<u8>>,
    err: tokio::task::JoinHandle<Vec<u8>>,
}

fn spawn(args: &[&str]) -> Proc {
    let mut child = Command::new(BIN)
        .args(args)
        .env("RUST_LOG", "trace")
        .env("NO_COLOR", "1")
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("ALL_PROXY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn dacs-netadmin");
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let out = tokio::spawn(async move {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b).await;
        b
    });
    let err = tokio::spawn(async move {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b).await;
        b
    });
    Proc { child, out, err }
}

impl Proc {
    /// Wait for exit (bounded) and return (success, stdout, stderr).
    async fn finish(mut self, kill: bool) -> (bool, String, String) {
        if kill {
            let _ = self.child.start_kill();
        }
        let status = tokio::time::timeout(Duration::from_secs(20), self.child.wait())
            .await
            .expect("dacs-netadmin did not exit")
            .unwrap();
        let out = String::from_utf8_lossy(&self.out.await.unwrap()).into_owned();
        let err = String::from_utf8_lossy(&self.err.await.unwrap()).into_owned();
        (status.success(), out, err)
    }
}

fn tls_files(dir: &std::path::Path) -> (String, Value) {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let cert_pem = cert.pem();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, &cert_pem).unwrap();
    write_private(&key_path, &key_pair.serialize_pem());
    (cert_pem, json!({"cert_path": cert_path, "key_path": key_path}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_only_tls_checks_tokens_and_logs_no_secrets() {
    let hs = MockHeadscale::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (cert_pem, tls) = tls_files(dir.path());
    let port = free_port();
    let cfg = write_config(
        dir.path(),
        &format!("127.0.0.1:{port}"),
        &hs.url(),
        &hs.api_key,
        json!({"tls": tls}),
    );
    let proc = spawn(&["serve", "--config", cfg.to_str().unwrap()]);

    let trusted = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).unwrap())
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("https://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..100 {
        if let Ok(r) = trusted.get(format!("{base}/v1/health")).send().await {
            assert_eq!(r.status(), 200);
            let v: Value = r.json().await.unwrap();
            assert_eq!(v["ok"], true);
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !ready {
        let (_, out, err) = proc.finish(true).await;
        panic!("netadmin never served TLS on {port}\nstdout:\n{out}\nstderr:\n{err}");
    }

    // Plaintext HTTP on the TLS listener gets no HTTP answer.
    let plain = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let r = plain
        .get(format!("http://127.0.0.1:{port}/v1/health"))
        .send()
        .await;
    assert!(r.is_err(), "plaintext request was answered: {:?}", r.map(|r| r.status()));
    // A client that does not trust the certificate refuses the connection.
    let r = plain.get(format!("{base}/v1/health")).send().await;
    assert!(r.is_err(), "untrusted certificate accepted");

    // Operation token over TLS.
    let get = |auth: Option<String>| {
        let mut b = trusted.get(format!("{base}/v1/nodes"));
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        b.send()
    };
    assert_eq!(get(None).await.unwrap().status(), 401);
    assert_eq!(get(Some("Bearer wrong".into())).await.unwrap().status(), 401);
    let ok = get(Some(format!("Bearer {TOKEN}"))).await.unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.text().await.unwrap(), "[]");

    // Enroll, a failing upstream that echoes the API key, then revoke.
    let r = trusted
        .post(format!("{base}/v1/enrollments"))
        .bearer_auth(TOKEN)
        .json(&enrollment_request("alice-pc", &["worker"]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let grant: EnrollmentGrant = r.json().await.unwrap();
    hs.fault(Op::ListNodes, Fault::EchoAuth);
    let r = get(Some(format!("Bearer {TOKEN}"))).await.unwrap();
    assert_eq!(r.status(), 502);
    let body = r.text().await.unwrap();
    assert_no_secrets(&body, &hs.secrets(), "502 body");
    let r = trusted
        .post(format!("{base}/v1/revocations"))
        .bearer_auth(TOKEN)
        .json(&revocation("dev-alice-pc", Some(grant.auth_key_id), None))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let (_, out, err) = proc.finish(true).await;
    let logs = format!("{out}\n{err}");
    assert!(logs.contains("dacs-netadmin serving"), "{logs}");
    assert!(logs.contains("tls=true"), "{logs}");
    assert!(logs.contains("headscale operation failed"), "upstream failure was logged");
    let mut secrets = hs.secrets();
    secrets.push(TOKEN.into());
    assert!(secrets.contains(&grant.auth_key.expose().to_owned()));
    assert_no_secrets(&logs, &secrets, "netadmin stdout/stderr at RUST_LOG=trace");
    let audit = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains("\"op\":\"enroll\"") && audit.contains("\"op\":\"revoke\""));
    assert_no_secrets(&audit, &secrets, "audit file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_to_start_without_tls_on_a_routable_address() {
    let hs = MockHeadscale::start().await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    for (listen, extra) in [
        (format!("0.0.0.0:{port}"), json!({"allow_plaintext_loopback": true})),
        (format!("100.64.0.1:{port}"), json!({"allow_plaintext_loopback": true})),
        (format!("127.0.0.1:{port}"), json!({})),
    ] {
        let cfg = write_config(dir.path(), &listen, &hs.url(), &hs.api_key, extra);
        for cmd in ["serve", "check"] {
            let (ok, out, err) = spawn(&[cmd, "--config", cfg.to_str().unwrap()])
                .finish(false)
                .await;
            assert!(!ok, "{cmd} started with plaintext on {listen}\n{out}{err}");
            assert!(err.contains("tls is required"), "{cmd} {listen}: {err}");
        }
    }
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "something is listening on the refused port"
    );
    assert!(hs.requests().is_empty(), "a refused config reached Headscale");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_to_start_with_readable_secret_files() {
    use std::os::unix::fs::PermissionsExt;
    let hs = MockHeadscale::start().await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let cfg = write_config(
        dir.path(),
        &format!("127.0.0.1:{port}"),
        &hs.url(),
        &hs.api_key,
        json!({"allow_plaintext_loopback": true}),
    );
    for file in ["headscale-api-key", "operation-tokens.sha256"] {
        let p = dir.path().join(file);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (ok, out, err) = spawn(&["serve", "--config", cfg.to_str().unwrap()])
            .finish(false)
            .await;
        assert!(!ok, "served with world-readable {file}");
        assert!(err.contains("chmod 600"), "{file}: {err}");
        assert_no_secrets(&format!("{out}{err}"), &hs.secrets(), "startup error");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(hs.requests().is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_writes_a_private_grant_and_prints_no_secret() {
    use std::os::unix::fs::PermissionsExt;
    let hs = MockHeadscale::start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(
        dir.path(),
        "127.0.0.1:9",
        &hs.url(),
        &hs.api_key,
        json!({"allow_plaintext_loopback": true}),
    );
    let req = dir.path().join("request.json");
    std::fs::write(&req, enrollment_request("coordinator", &["coordinator", "worker"]).to_string()).unwrap();
    let out_path = dir.path().join("grant.json");
    let args = |out: &std::path::Path| {
        vec![
            "issue".to_owned(),
            "--config".into(),
            cfg.to_str().unwrap().into(),
            "--request".into(),
            req.to_str().unwrap().into(),
            "--out".into(),
            out.to_str().unwrap().into(),
        ]
    };
    let a = args(&out_path);
    let (ok, out, err) = spawn(&a.iter().map(String::as_str).collect::<Vec<_>>())
        .finish(false)
        .await;
    assert!(ok, "{out}{err}");
    // Logs share stdout; the summary is the one JSON line.
    let printed: Value = out
        .lines()
        .find_map(|l| serde_json::from_str::<Value>(l).ok().filter(|v| v.get("auth_key_id").is_some()))
        .unwrap_or_else(|| panic!("no summary line in:\n{out}"));
    assert!(printed.get("auth_key").is_none(), "summary prints the key");
    let grant: EnrollmentGrant =
        serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
    assert_eq!(printed["auth_key_id"], grant.auth_key_id);
    assert_eq!(hs.key(grant.auth_key_id).unwrap().secret, grant.auth_key.expose());
    let mode = std::fs::metadata(&out_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "grant file mode {mode:o}");
    let mut secrets = hs.secrets();
    secrets.push(TOKEN.into());
    assert_no_secrets(&format!("{out}\n{err}"), &secrets, "issue output");

    // An existing output file is refused without issuing (and discarding) a key.
    let keys_before = hs.lock().keys.len();
    let (ok, out, err) = spawn(&a.iter().map(String::as_str).collect::<Vec<_>>())
        .finish(false)
        .await;
    assert!(!ok, "overwrote an existing grant file");
    assert_no_secrets(&format!("{out}\n{err}"), &hs.secrets(), "issue failure output");
    assert_eq!(
        hs.lock().keys.len(),
        keys_before,
        "a key was issued although its grant could not be written"
    );

    // An invalid request issues nothing and leaves no grant file behind.
    let mut bad = enrollment_request("guest-1", &["compile_server"]);
    bad["consent"]["accepted_host"] = "someone-else".into();
    std::fs::write(&req, bad.to_string()).unwrap();
    let out2 = dir.path().join("grant-2.json");
    let a2 = args(&out2);
    let (ok, _, err) = spawn(&a2.iter().map(String::as_str).collect::<Vec<_>>())
        .finish(false)
        .await;
    assert!(!ok);
    assert!(err.contains("different host name"), "{err}");
    assert!(!out2.exists(), "left a grant file for a refused request");
    assert_eq!(hs.lock().keys.len(), keys_before);
}
