//! Exact-source instrumentation for upstream sccache v0.10.0, commit below.
//! Patches emit observed output SHA-256 at server completion and client extraction.
//! No placement/consumption claim is accepted from task-supplied JSON.
use std::{fs, path::Path, process::Command};
type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;
pub const ENGINE_COMMIT: &str = "e3d1ed560a28d114bd26299b0f717b372e7fc3d2";
fn replace_once(text: &mut String, needle: &str, replacement: &str) -> Result<()> {
    if text.matches(needle).count() != 1 {
        return Err(format!("pinned engine anchor is absent/ambiguous: {needle}").into());
    }
    *text = text.replacen(needle, replacement, 1);
    Ok(())
}
pub fn instrument(root: &Path) -> Result<()> {
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()?;
    let clean = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()?;
    if !revision.status.success()
        || String::from_utf8(revision.stdout)?.trim() != ENGINE_COMMIT
        || !clean.status.success()
        || !clean.stdout.is_empty()
    {
        return Err("instrumentation requires an untouched exact sccache v0.10.0 checkout".into());
    }
    let dist_path = root.join("src/dist/mod.rs");
    let server_path = root.join("src/bin/sccache-dist/main.rs");
    let client_path = root.join("src/compiler/compiler.rs");
    let mut dist = fs::read_to_string(&dist_path)?;
    let mut server = fs::read_to_string(&server_path)?;
    let mut client = fs::read_to_string(&client_path)?;
    replace_once(
        &mut server,
        "pub const INSECURE_DIST_SERVER_TOKEN:",
        r#"static DACS_SLOTS: (std::sync::Mutex<usize>, std::sync::Condvar) = (std::sync::Mutex::new(0), std::sync::Condvar::new());
struct DacsSlot;
impl DacsSlot {
    fn acquire() -> Result<Self> {
        let limit: usize = std::env::var("DACS_COMPILER_SLOTS")?.parse()?;
        anyhow::ensure!(limit > 0, "DACS compiler slots must be positive");
        let (lock, wake) = &DACS_SLOTS;
        let mut active = lock.lock().map_err(|_| anyhow::anyhow!("compiler slot lock poisoned"))?;
        while *active >= limit { active = wake.wait(active).map_err(|_| anyhow::anyhow!("compiler slot wait poisoned"))?; }
        *active += 1;
        Ok(Self)
    }
}
impl Drop for DacsSlot {
    fn drop(&mut self) {
        let (lock, wake) = &DACS_SLOTS;
        if let Ok(mut active) = lock.lock() { *active -= 1; wake.notify_one(); }
    }
}
pub const INSECURE_DIST_SERVER_TOKEN:"#,
    )?;
    replace_once(
        &mut server,
        "        requester\n            .do_update_job_state(job_id, JobState::Started)",
        "        let _dacs_slot = DacsSlot::acquire()?;\n        requester\n            .do_update_job_state(job_id, JobState::Started)",
    )?;
    replace_once(
        &mut dist,
        "impl OutputData {",
        r#"impl OutputData {
    #[cfg(any(feature = "dist-client", feature = "dist-server"))]
    pub fn dacs_sha256(&self) -> std::io::Result<String> {
        use sha2::{Digest, Sha256};
        let mut reader = flate2::read::ZlibDecoder::new(std::io::Cursor::new(&self.0));
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop { let count = std::io::Read::read(&mut reader, &mut buffer)?; if count == 0 { break; } digest.update(&buffer[..count]); }
        Ok(format!("{:x}", digest.finalize()))
    }
"#,
    )?;
    replace_once(
        &mut dist,
        "impl ProcessOutput {",
        "impl ProcessOutput {\n    pub fn dacs_exit_code(&self) -> i32 { self.code }\n",
    )?;
    replace_once(
        &mut server,
        "        let res = match tc {",
        "        let dacs_started = Instant::now();\n        let res = match tc {",
    )?;
    replace_once(
        &mut server,
        "        requester\n            .do_update_job_state(job_id, JobState::Complete)",
        r#"        if let Ok(RunJobResult::Complete(completed)) = &res {
            let mut outputs = Vec::new();
            for (path, data) in &completed.outputs {
                outputs.push(serde_json::json!({"path":path,"sha256":data.dacs_sha256()?,"bytes":data.lens().actual,"transferred_bytes":data.lens().compressed}));
            }
            eprintln!("DACS_ENGINE_JSON:{}", serde_json::json!({"schema":1,"engine":"sccache-0.10.0-dacs-1","kind":"server_complete","job_id":job_id.to_string(),"exit_code":completed.output.dacs_exit_code(),"outputs":outputs,"elapsed_ms":dacs_started.elapsed().as_millis()}));
        }
        requester
            .do_update_job_state(job_id, JobState::Complete)"#,
    )?;
    replace_once(
        &mut client,
        "    let do_dist_compile = async move {",
        "    let do_dist_compile = async move {\n        let dacs_started = Instant::now();",
    )?;
    replace_once(
        &mut client,
        "        Ok((DistType::Ok(server_id), jc.output.into()))",
        r#"        let mut dacs_outputs = Vec::new();
        for path in &output_paths {
            use sha2::{Digest, Sha256};
            let mut file = std::fs::File::open(path)?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 65536];
            let mut bytes = 0u64;
            loop { let count = std::io::Read::read(&mut file, &mut buffer)?; if count == 0 { break; } digest.update(&buffer[..count]); bytes += count as u64; }
            dacs_outputs.push(serde_json::json!({"path":path,"sha256":format!("{:x}",digest.finalize()),"bytes":bytes}));
        }
        eprintln!("DACS_ENGINE_JSON:{}", serde_json::json!({"schema":1,"engine":"sccache-0.10.0-dacs-1","kind":"received","job_id":job_id.to_string(),"server_addr":server_id.addr().to_string(),"exit_code":jc.output.dacs_exit_code(),"outputs":dacs_outputs,"elapsed_ms":dacs_started.elapsed().as_millis()}));
        Ok((DistType::Ok(server_id), jc.output.into()))"#,
    )?;
    // Validate every replacement before writing any file. Source stays reviewable;
    // this command deliberately does not compile, install, start or sign engines.
    fs::write(dist_path, dist)?;
    fs::write(server_path, server)?;
    fs::write(client_path, client)?;
    println!(
        "Instrumented exact sccache {ENGINE_COMMIT}. Build on Linux with cargo build --locked --release --no-default-features --features dist-client,dist-server. Pin produced executable SHA-256 in each workflow."
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_rejects_missing_or_duplicate_anchors() {
        assert!(replace_once(&mut "abc".into(), "missing", "x").is_err());
        assert!(replace_once(&mut "aa".into(), "a", "x").is_err());
        let mut source = "abc".into();
        replace_once(&mut source, "b", "X").unwrap();
        assert_eq!(source, "aXc");
    }
}
