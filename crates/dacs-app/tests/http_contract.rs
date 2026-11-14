use reqwest::{StatusCode, blocking::Client, header};
use serde_json::Value;
use std::{fs, net::TcpListener, process::{Child, Command, Stdio}, time::{Duration, Instant}};

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
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let data = tempfile::tempdir().unwrap();
        let url = format!("http://127.0.0.1:{port}");
        let process = Command::new(env!("CARGO_BIN_EXE_dacs"))
            .args(["serve", "--listen", &format!("127.0.0.1:{port}"), "--public-url", &url, "--data-dir"])
            .arg(data.path()).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap();
        let client = Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(3)).build().unwrap();
        let mut server = Self { process, _data: data, url, token: String::new(), client };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(server.process.try_wait().unwrap().is_none(), "Coordinator exited before serving HTTP");
            if server.client.get(format!("{}/health", server.url)).send().is_ok_and(|r| r.status() == StatusCode::OK) { break; }
            assert!(Instant::now() < deadline, "Coordinator did not become ready");
            std::thread::sleep(Duration::from_millis(50));
        }
        server.token = fs::read_to_string(server._data.path().join("admin.token")).unwrap();
        server
    }

    fn projects(&self) -> Vec<Value> {
        self.client.get(format!("{}/api/state", self.url)).bearer_auth(&self.token).send().unwrap().json::<Value>().unwrap()["projects"].as_array().unwrap().clone()
    }
}

fn hidden_value<'a>(form: &'a str, name: &str) -> &'a str {
    let rest = &form[form.find(&format!("name=\"{name}\"")).expect("Required browser form input")..];
    let value = &rest[rest.find("value=\"").unwrap() + 7..];
    &value[..value.find('"').unwrap()]
}

#[test]
fn browser_commands_require_origin_and_csrf_and_deduplicate_delivery() {
    let server = Server::start();
    let login = server.client.get(format!("{}/login", server.url)).send().unwrap();
    let login_cookie = login.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
    let nonce = login_cookie.split_once('=').unwrap().1;
    let signed_in = server.client.post(format!("{}/session", server.url))
        .header(header::ORIGIN, &server.url).header(header::COOKIE, &login_cookie)
        .form(&[("csrf", nonce), ("token", server.token.as_str())]).send().unwrap();
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    let session_cookie = signed_in.headers().get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("dacs_session=")).unwrap().split(';').next().unwrap();
    let dashboard = server.client.get(&server.url).header(header::COOKIE, session_cookie).send().unwrap().text().unwrap();
    let form = &dashboard[dashboard.find("action=\"/ui/create-project\"").unwrap()..];
    let form = &form[..form.find("</form>").unwrap()];
    let csrf = hidden_value(form, "csrf");
    let id = hidden_value(form, "command_id");
    let mut fields = vec![("csrf", csrf), ("command_id", id), ("name", "Browser delivery"), ("target_branch", "main"), ("max_tasks", "12"), ("max_depth", "3")];
    let post = |origin: &str, fields: &[(&str, &str)]| server.client.post(format!("{}/ui/create-project", server.url))
        .header(header::ORIGIN, origin).header(header::COOKIE, session_cookie).form(fields).send().unwrap();
    assert_eq!(post("https://untrusted.invalid", &fields).status(), StatusCode::FORBIDDEN);
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
    assert_eq!(server.client.get(format!("{}/api/setup/catalog", server.url)).bearer_auth(&server.token).send().unwrap().status(), StatusCode::FORBIDDEN);
    let logout = || server.client.delete(format!("{}/session", server.url)).header(header::ORIGIN, &server.url).header(header::COOKIE, session_cookie);
    assert_eq!(logout().send().unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(logout().header("x-dacs-csrf", csrf).send().unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(server.client.get(format!("{}/api/state", server.url)).header(header::COOKIE, session_cookie).send().unwrap().status(), StatusCode::UNAUTHORIZED);
}
