use reqwest::{StatusCode, blocking::Client, header};
use serde_json::Value;
use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// A reserved port is released before the child binds it, so another test in this
/// process could be handed the same port in between. Hold this from reservation
/// until the child is ready or has exited.
static STARTUP: Mutex<()> = Mutex::new(());

fn startup_lock() -> MutexGuard<'static, ()> {
    STARTUP.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Server {
    process: Child,
    _data: tempfile::TempDir,
    url: String,
    token: String,
    client: Client,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Server {
    fn start() -> Self {
        let _startup = startup_lock();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let data = tempfile::tempdir().unwrap();
        let url = format!("http://127.0.0.1:{port}");
        let process = Command::new(env!("CARGO_BIN_EXE_dacs"))
            .args([
                "serve",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--public-url",
                &url,
                "--data-dir",
            ])
            .arg(data.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let mut server = Self {
            process,
            _data: data,
            url,
            token: String::new(),
            client,
        };
        server.wait_ready();
        server
    }

    fn start_tls() -> (Self, String) {
        let _startup = startup_lock();
        let control = TcpListener::bind("127.0.0.1:0").unwrap();
        let bootstrap = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = control.local_addr().unwrap();
        let bootstrap_listen = bootstrap.local_addr().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (mut command, client) = tls_command(data.path(), listen, bootstrap_listen);
        drop((control, bootstrap));
        let process = command.spawn().unwrap();
        let mut server = Self {
            process,
            _data: data,
            url: format!("https://{listen}"),
            token: String::new(),
            client,
        };
        server.wait_ready();
        (server, format!("https://{bootstrap_listen}"))
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                self.process.try_wait().unwrap().is_none(),
                "Coordinator exited before serving HTTP"
            );
            if self
                .client
                .get(format!("{}/health", self.url))
                .send()
                .is_ok_and(|r| r.status() == StatusCode::OK)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Coordinator did not become ready"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        self.token = fs::read_to_string(self._data.path().join("admin.token")).unwrap();
    }

    fn projects(&self) -> Vec<Value> {
        self.client
            .get(format!("{}/api/state", self.url))
            .bearer_auth(&self.token)
            .send()
            .unwrap()
            .json::<Value>()
            .unwrap()["projects"]
            .as_array()
            .unwrap()
            .clone()
    }
}

fn tls_command(data: &Path, listen: SocketAddr, bootstrap: SocketAddr) -> (Command, Client) {
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let pem = certified.cert.pem();
    fs::write(data.join("server.pem"), &pem).unwrap();
    fs::write(data.join("server.key"), certified.key_pair.serialize_pem()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dacs"));
    command
        .args([
            "serve",
            "--listen",
            &listen.to_string(),
            "--bootstrap-listen",
            &bootstrap.to_string(),
            "--data-dir",
        ])
        .arg(data)
        .arg("--tls-cert")
        .arg(data.join("server.pem"))
        .arg("--tls-key")
        .arg(data.join("server.key"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let client = Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    (command, client)
}

fn hidden_value<'a>(form: &'a str, name: &str) -> &'a str {
    let rest = &form[form
        .find(&format!("name=\"{name}\""))
        .expect("Required browser form input")..];
    let value = &rest[rest.find("value=\"").unwrap() + 7..];
    &value[..value.find('"').unwrap()]
}

#[test]
fn browser_commands_require_origin_and_csrf_and_deduplicate_delivery() {
    let server = Server::start();
    let login = server
        .client
        .get(format!("{}/login", server.url))
        .send()
        .unwrap();
    assert_eq!(
        login.headers().get(header::REFERRER_POLICY).unwrap(),
        "strict-origin"
    );
    let login_cookie = login
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let login_html = login.text().unwrap();
    assert!(login_html.contains("name=\"referrer\" content=\"strict-origin\""));
    assert!(!login_html.contains("no-referrer"));
    let nonce = login_cookie.split_once('=').unwrap().1;
    for rejected_origin in ["null", "https://untrusted.invalid"] {
        let rejected = server
            .client
            .post(format!("{}/session", server.url))
            .header(header::ORIGIN, rejected_origin)
            .header(header::COOKIE, &login_cookie)
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-dest", "document")
            .form(&[("csrf", nonce), ("token", server.token.as_str())])
            .send()
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    }
    let signed_in = server
        .client
        .post(format!("{}/session", server.url))
        .header(header::ORIGIN, &server.url)
        .header(header::COOKIE, &login_cookie)
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-dest", "document")
        .form(&[("csrf", nonce), ("token", server.token.as_str())])
        .send()
        .unwrap();
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    let session_cookie = signed_in
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("dacs_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let dashboard = server
        .client
        .get(&server.url)
        .header(header::COOKIE, session_cookie)
        .send()
        .unwrap()
        .text()
        .unwrap();
    let form = &dashboard[dashboard.find("action=\"/ui/create-project\"").unwrap()..];
    let form = &form[..form.find("</form>").unwrap()];
    let csrf = hidden_value(form, "csrf");
    let id = hidden_value(form, "command_id");
    let mut fields = vec![
        ("csrf", csrf),
        ("command_id", id),
        ("name", "Browser delivery"),
        ("target_branch", "main"),
        ("max_tasks", "12"),
        ("max_depth", "3"),
    ];
    let post = |origin: &str, fields: &[(&str, &str)]| {
        server
            .client
            .post(format!("{}/ui/create-project", server.url))
            .header(header::ORIGIN, origin)
            .header(header::COOKIE, session_cookie)
            .form(fields)
            .send()
            .unwrap()
    };
    assert_eq!(
        post("https://untrusted.invalid", &fields).status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(post("null", &fields).status(), StatusCode::FORBIDDEN);
    fields[0].1 = "invalid";
    assert_eq!(post(&server.url, &fields).status(), StatusCode::FORBIDDEN);
    assert!(server.projects().is_empty());
    fields[0].1 = csrf;
    assert_eq!(post(&server.url, &fields).status(), StatusCode::SEE_OTHER);
    assert_eq!(post(&server.url, &fields).status(), StatusCode::SEE_OTHER);
    fields[2].1 = "Conflicting redelivery";
    assert_eq!(post(&server.url, &fields).status(), StatusCode::SEE_OTHER);
    let projects = server.projects();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["name"], "Browser delivery");
    // Notices from separate actions queue until rendered, then show once, in order.
    let rejected = |action: &str| {
        server
            .client
            .post(format!("{}/ui/{action}", server.url))
            .header(header::ORIGIN, &server.url)
            .header(header::COOKIE, session_cookie)
            .form(&[("csrf", csrf)])
            .send()
            .unwrap()
            .status()
    };
    assert_eq!(rejected("create-project"), StatusCode::SEE_OTHER);
    assert_eq!(rejected("invitation"), StatusCode::SEE_OTHER);
    let render = || {
        server
            .client
            .get(&server.url)
            .header(header::COOKIE, session_cookie)
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    let rendered = render();
    let first = rendered
        .find("Operation rejected: Missing name")
        .expect("First pending notice");
    let second = rendered
        .find("Operation rejected: Missing project_id")
        .expect("Second pending notice");
    assert!(first < second);
    assert!(!render().contains("Operation rejected"));
    assert_eq!(
        server
            .client
            .get(format!("{}/api/setup/catalog", server.url))
            .bearer_auth(&server.token)
            .send()
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let logout = || {
        server
            .client
            .delete(format!("{}/session", server.url))
            .header(header::ORIGIN, &server.url)
            .header(header::COOKIE, session_cookie)
    };
    assert_eq!(logout().send().unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        logout()
            .header("x-dacs-csrf", csrf)
            .send()
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server
            .client
            .get(format!("{}/api/state", server.url))
            .header(header::COOKIE, session_cookie)
            .send()
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn tls_bootstrap_never_exposes_control_or_local_owner_routes() {
    let (server, bootstrap) = Server::start_tls();
    assert_eq!(
        server
            .client
            .get(format!("{bootstrap}/health"))
            .send()
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .client
            .get(format!("{}/api/state", server.url))
            .bearer_auth(&server.token)
            .send()
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .client
            .get(format!("{}/network", server.url))
            .bearer_auth(&server.token)
            .send()
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for path in [
        "/",
        "/login",
        "/api/state",
        "/api/commands",
        "/network",
        "/onboarding",
        "/setup",
        "/api/setup/catalog",
        "/api/projects/test/source",
        "/api/projects/test/artifacts/test",
        "/api/candidates/test/diff",
    ] {
        assert_eq!(
            server
                .client
                .get(format!("{bootstrap}{path}"))
                .bearer_auth(&server.token)
                .send()
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for path in [
        "/session",
        "/api/command",
        "/api/setup/install",
        "/api/onboarding/inspect",
        "/api/network/devices/test/revoke",
        "/api/candidates/test/publish",
    ] {
        assert_eq!(
            server
                .client
                .post(format!("{bootstrap}{path}"))
                .bearer_auth(&server.token)
                .json(&serde_json::json!({}))
                .send()
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        server
            .client
            .get(format!("{bootstrap}/api/network/state"))
            .send()
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .client
            .get(format!("{bootstrap}/api/network/state"))
            .bearer_auth(&server.token)
            .send()
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .client
            .post(format!("{bootstrap}/api/network/devices/test/refresh"))
            .bearer_auth(&server.token)
            .send()
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    // No TLS verification bypass: the generated certificate is trusted only by
    // this test's explicitly configured client.
    let untrusted = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    assert!(untrusted.get(format!("{bootstrap}/health")).send().is_err());
}

#[test]
fn network_routes_reject_browser_cookies_even_with_valid_control_session() {
    let (server, bootstrap) = Server::start_tls();
    let login = server
        .client
        .get(format!("{}/login", server.url))
        .send()
        .unwrap();
    let login_cookie = login
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let nonce = login_cookie.split_once('=').unwrap().1;
    let signed_in = server
        .client
        .post(format!("{}/session", server.url))
        .header(header::ORIGIN, &server.url)
        .header(header::COOKIE, &login_cookie)
        .form(&[("csrf", nonce), ("token", server.token.as_str())])
        .send()
        .unwrap();
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    let session_cookie = signed_in
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("dacs_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert_eq!(
        server
            .client
            .get(format!("{}/network", server.url))
            .header(header::COOKIE, session_cookie)
            .send()
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for origin in [&server.url, &bootstrap] {
        assert_eq!(
            server
                .client
                .get(format!("{origin}/api/network/state"))
                .header(header::COOKIE, session_cookie)
                .send()
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            server
                .client
                .get(format!("{origin}/api/network/grant"))
                .header(header::COOKIE, session_cookie)
                .send()
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        server
            .client
            .get(format!("{}/onboarding", server.url))
            .header(header::COOKIE, session_cookie)
            .send()
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[test]
fn occupied_bootstrap_socket_aborts_before_control_readiness() {
    let _startup = startup_lock();
    let reserved_control = TcpListener::bind("127.0.0.1:0").unwrap();
    let control = reserved_control.local_addr().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let data = tempfile::tempdir().unwrap();
    let (mut command, client) = tls_command(data.path(), control, occupied.local_addr().unwrap());
    drop(reserved_control);
    let mut server = Server {
        process: command.spawn().unwrap(),
        _data: data,
        url: format!("https://{control}"),
        token: String::new(),
        client,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = server.process.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            server
                .client
                .get(format!("{}/health", server.url))
                .send()
                .is_err(),
            "Control served while bootstrap bind failed"
        );
        assert!(
            Instant::now() < deadline,
            "Failed startup left coordinator alive"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // The exit observed above is the release of the already-bound control port: the
    // operating system closes a terminated process's sockets. Rebinding the port here
    // would race with unrelated processes taking it.
}

#[test]
fn second_instance_on_the_same_data_directory_fails_before_touching_state() {
    let server = Server::start();
    let job = server
        ._data
        .path()
        .join("jobs")
        .join(format!("{}.json", uuid::Uuid::new_v4()));
    fs::write(
        &job,
        r#"{"id":"fixture","operation":"fixture","state":"running","created_at":0,"updated_at":0,"result":null,"error":null}"#,
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_dacs"))
        .args([
            "serve",
            "--listen",
            server.url.strip_prefix("http://").unwrap(),
            "--public-url",
            &server.url,
            "--data-dir",
        ])
        .arg(server._data.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
    // Opening the job journal marks running jobs interrupted; the losing instance
    // must fail to bind before it gets that far.
    let journal: Value = serde_json::from_str(&fs::read_to_string(&job).unwrap()).unwrap();
    assert_eq!(journal["state"], "running");
}

#[test]
fn bootstrap_listener_requires_tls_and_a_distinct_address() {
    let data = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_dacs"))
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--bootstrap-listen",
            "127.0.0.1:1",
            "--data-dir",
        ])
        .arg(data.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
    let address = "127.0.0.1:0".parse().unwrap();
    let (mut command, _) = tls_command(data.path(), address, address);
    assert!(!command.status().unwrap().success());
}

#[test]
fn bootstrap_enrollment_shares_identity_without_approving_the_device() {
    let (server, bootstrap) = Server::start_tls();
    let command = |command: Value| {
        let response = server.client.post(format!("{}/api/command", server.url))
            .bearer_auth(&server.token)
            .json(&serde_json::json!({
                "protocol_version": 1, "command_id": uuid::Uuid::new_v4().to_string(), "command": command
            })).send().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Value>().unwrap()["result"].clone()
    };
    let project = command(serde_json::json!({
        "type":"create_project", "name":"Bootstrap test", "target_branch":"main", "max_tasks":10, "max_depth":2
    }));
    let invite = command(serde_json::json!({
        "type":"create_invitation", "project_id":project["id"], "host_name":"Named coordinator", "expires_in_seconds":300
    }));
    let token = invite["token"].as_str().unwrap();
    assert_eq!(
        server
            .client
            .get(format!("{bootstrap}/api/invitations/{token}"))
            .send()
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let enrollment = server
        .client
        .post(format!("{bootstrap}/api/enroll"))
        .json(&serde_json::json!({
            "invitation_token": token,
            "registration": {
                "name":"Bootstrap device", "physical_host_id":"bootstrap-test-host", "os":"linux",
                "architecture":"x86_64", "capabilities":["rust"], "capacity":{"agent_slots":1},
                "omp_version":"pinned-1", "policy_revision":1, "owner_consented":true,
                "consent_host":"Named coordinator"
            }
        }))
        .send()
        .unwrap();
    assert_eq!(enrollment.status(), StatusCode::OK);
    let enrollment = enrollment.json::<Value>().unwrap();
    assert_eq!(enrollment["worker"]["approved"], false);
    let device_token = enrollment["device_token"].as_str().unwrap();
    for origin in [&server.url, &bootstrap] {
        assert_eq!(
            server
                .client
                .get(format!("{origin}/api/network/state"))
                .bearer_auth(device_token)
                .send()
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        server
            .client
            .post(format!(
                "{bootstrap}/api/network/devices/another-device/refresh"
            ))
            .bearer_auth(device_token)
            .send()
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .client
            .get(format!("{bootstrap}/api/state"))
            .bearer_auth(device_token)
            .send()
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let snapshot = server
        .client
        .get(format!("{}/api/state", server.url))
        .bearer_auth(&server.token)
        .send()
        .unwrap()
        .json::<Value>()
        .unwrap();
    assert!(snapshot["workers"].as_array().unwrap().iter().any(|worker| worker["id"] == enrollment["worker"]["id"] && worker["approved"] == false));
}

#[test]
fn workflow_completion_is_bound_to_the_uploaded_result_and_owned_attempt() {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    let server = Server::start();
    let post_command = |token: &str, command: Value, expected: StatusCode| {
        let response = server.client.post(format!("{}/api/command", server.url))
            .bearer_auth(token)
            .json(&json!({"protocol_version":1,"command_id":uuid::Uuid::new_v4().to_string(),"command":command}))
            .send().unwrap();
        assert_eq!(response.status(), expected);
        response.json::<Value>().unwrap()
    };
    let project = post_command(&server.token, json!({"type":"create_project","name":"Workflow contract fixture","target_branch":"main","max_tasks":4,"max_depth":2}), StatusCode::OK)["result"].clone();
    let invitation = post_command(&server.token, json!({"type":"create_invitation","project_id":project["id"],"host_name":"workflow-fixture","expires_in_seconds":600}), StatusCode::OK)["result"].clone();
    let enrollment: Value = server.client.post(format!("{}/api/enroll", server.url)).json(&json!({
        "invitation_token":invitation["token"],
        "registration":{"name":"Workflow fixture","physical_host_id":"fixture-host","os":"windows","architecture":"x86_64","capabilities":["rust"],"capacity":{"cpu_millis":1000,"memory_mb":1024,"disk_mb":1024},"omp_version":"fixture","policy_revision":1,"owner_consented":true,"consent_host":"workflow-fixture"}
    })).send().unwrap().error_for_status().unwrap().json().unwrap();
    let token = enrollment["device_token"].as_str().unwrap();
    let worker = &enrollment["worker"]["id"];
    post_command(
        &server.token,
        json!({"type":"approve_worker","worker_id":worker}),
        StatusCode::OK,
    );
    let source = "1".repeat(40);
    let task = post_command(&server.token, json!({"type":"create_task","spec":{
        "project_id":project["id"],"title":"Result transport fixture","instructions":"No compiler or provider is run by this protocol test.",
        "source_commit":source,"execution":{"kind":"workflow","request":{"operation":{"kind":"cargo_build"}}},
        "dependencies":[],"required_capabilities":["rust"],"resources":{"cpu_millis":100,"memory_mb":64,"disk_mb":64},
        "effect_class":"external","lease_seconds":600,"max_runtime_seconds":600,"contract_revision":1,"policy_revision":1,"max_retries":0
    }}), StatusCode::OK)["result"].clone();
    let attempt = post_command(
        &server.token,
        json!({"type":"admit_task","task_id":task["id"],"worker_id":worker}),
        StatusCode::OK,
    )["result"]
        .clone();
    let event = |sequence, event| json!({"type":"report_event","attempt_id":attempt["id"],"generation":attempt["generation"],"sequence":sequence,"event":event});
    post_command(token, event(1, json!({"type":"settled"})), StatusCode::OK);
    let checks = json!([{"name":"Fixture evidence","command":"protocol-fixture","source_sha":source,"passed":true,"artifact_digest":null}]);
    let completion = |digest| {
        event(
            2,
            json!({"type":"workflow_completed","success":true,"artifact_digest":digest,"evidence":checks}),
        )
    };
    post_command(token, completion("0".repeat(64)), StatusCode::NOT_FOUND);
    let mut result = json!({"task_id":task["id"],"attempt_id":attempt["id"],"generation":attempt["generation"].as_u64().unwrap()+1,
        "source_commit":source,"success":true,"exit_code":0,"elapsed_ms":1,"checks":checks,"artifacts":[],
        "distribution_failure":null,"processes_reconciled":true});
    let upload = |result: &Value| {
        let bytes = serde_json::to_vec(result).unwrap();
        let digest = hex::encode(Sha256::digest(&bytes));
        server
            .client
            .post(format!(
                "{}/api/projects/{}/artifacts/{digest}",
                server.url,
                project["id"].as_str().unwrap()
            ))
            .bearer_auth(token)
            .body(bytes)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap();
        digest
    };
    post_command(token, completion(upload(&result)), StatusCode::CONFLICT);
    result["generation"] = attempt["generation"].clone();
    post_command(token, completion(upload(&result)), StatusCode::OK);
    post_command(token, event(3, json!({"type":"stopped"})), StatusCode::OK);
    let state: Value = server
        .client
        .get(format!("{}/api/state", server.url))
        .bearer_auth(&server.token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(state["tasks"][0]["state"], "accepted");
    assert_eq!(state["workers"][0]["reserved"]["cpu_millis"], 0);
}
