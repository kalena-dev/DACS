#![cfg(all(windows, target_env = "msvc"))]

use dacs_protocol::{
    AttemptRecord, EffectClass, Resources, TaskExecution, TaskRecord, TaskSpec, TaskState,
};
use dacs_worker::{
    Supervisor,
    build_execution::BuildProfile,
    build_runner::{self, BehaviorCheck, WorkflowOperation, WorkflowRequest},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn output(program: &Path, args: &[&str], cwd: &Path) -> String {
    let result = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}: {}",
        program.display(),
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn admitted_windows_workflow_links_and_executes_from_attempt_state() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    fs::write(
        source.join("Cargo.toml"),
        "[package]\nname=\"dacs-proof\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(source.join("src/main.rs"), "fn main() { let values = [3, 5, 8]; assert_eq!(values.iter().sum::<i32>(), 16); println!(\"native workflow proof: 16\"); }\n").unwrap();
    let cargo = PathBuf::from(env!("CARGO"));
    let rustc = PathBuf::from(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    let git = PathBuf::from("git");
    output(&cargo, &["generate-lockfile", "--offline"], &source);
    output(&git, &["init", "-b", "main"], &source);
    output(&git, &["config", "core.autocrlf", "false"], &source);
    output(&git, &["add", "."], &source);
    output(
        &git,
        &[
            "-c",
            "user.name=Workflow regression",
            "-c",
            "user.email=workflow@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--no-gpg-sign",
            "-m",
            "Pinned workflow input",
        ],
        &source,
    );
    let revision = output(&git, &["rev-parse", "HEAD"], &source);
    let rustc_verbose = output(&rustc, &["-vV"], &source);
    let target = rustc_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap()
        .to_owned();
    let linker = find_msvc_tools::find_tool(&target, "link.exe")
        .expect("MSVC linker used by this native test target");
    let mut environment: BTreeMap<String, String> = linker
        .env()
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    environment.insert(
        format!(
            "CARGO_TARGET_{}_LINKER",
            target.replace('-', "_").to_uppercase()
        ),
        linker.path().to_string_lossy().into_owned(),
    );
    environment.insert("CARGO_NET_OFFLINE".into(), "true".into());
    let sysroot = PathBuf::from(output(&rustc, &["--print", "sysroot"], &source));
    let request = WorkflowRequest {
        profile: BuildProfile {
            profile_id: "path-proof".into(),
            pool_id: "path-proof".into(),
            source_commit: revision.clone(),
            cargo_lock_sha256: hex::encode(Sha256::digest(
                fs::read(source.join("Cargo.lock")).unwrap(),
            )),
            rustc_version: output(&rustc, &["--version"], &source),
            cargo_version: output(&cargo, &["--version"], &source),
            rustc_verbose,
            sysroot_sha256: build_runner::sysroot_digest(&sysroot).unwrap(),
            target,
            package: "dacs-proof".into(),
            features: vec![],
            default_features: true,
            rustflags: vec![],
            environment,
            cargo_profile: "dev".into(),
            sccache_server_port: 49271,
        },
        cargo,
        rustc,
        git,
        operation: WorkflowOperation::Build,
        distributed: None,
        artifacts: vec![],
        exercise: vec![BehaviorCheck {
            executable_name: "dacs-proof.exe".into(),
            args: vec![],
        }],
    };
    let resources = Resources {
        agent_slots: 0,
        cpu_millis: 1000,
        memory_mb: 512,
        disk_mb: 512,
    };
    let attempt = AttemptRecord {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: "workflow-path".into(),
        worker_id: "local-worker".into(),
        generation: 1,
        state: "admitted".into(),
        lease_expires_at: i64::MAX,
        started_at: 0,
        settled: false,
        pending_children: vec![],
        last_sequence: 0,
        stop_reason: None,
        resources: resources.clone(),
    };
    let task = TaskRecord {
        id: attempt.task_id.clone(),
        spec: TaskSpec {
            project_id: "local-path-regression".into(),
            title: "Build and exercise a native executable".into(),
            instructions: "Run the pinned workflow".into(),
            source_commit: revision,
            execution: TaskExecution::Workflow {
                request: serde_json::to_value(request).unwrap(),
            },
            dependencies: vec![],
            required_capabilities: vec![],
            resources,
            effect_class: EffectClass::External,
            lease_seconds: 120,
            max_runtime_seconds: 120,
            contract_revision: 1,
            policy_revision: 1,
            max_retries: 0,
        },
        state: TaskState::Executing,
        generation: 1,
        parent_task_id: None,
        attempt_id: Some(attempt.id.clone()),
        integrated_sha: None,
        workflow_result: None,
        blocked_reason: None,
        cancel_requested: false,
    };
    // The runtime supplies an already attempt-specific directory. Repeating its
    // UUID underneath this owner path pushes MSVC object inputs past MAX_PATH.
    let state = temporary
        .path()
        .join("worker state with spaces")
        .join("attempts")
        .join(&attempt.id);
    fs::create_dir_all(&state).unwrap();
    let result = build_runner::execute_admitted(
        &Supervisor::new(),
        &task,
        &attempt,
        &fs::canonicalize(source).unwrap(),
        &fs::canonicalize(state).unwrap(),
    )
    .await
    .unwrap();
    if !result.success {
        let logs: Vec<_> = result
            .artifacts
            .iter()
            .filter(|a| a.kind == "stderr" || a.kind == "workflow_error")
            .map(|a| fs::read_to_string(&a.path).unwrap())
            .collect();
        panic!("workflow failed: {result:?}\n{}", logs.join("\n"));
    }
    assert!(result.processes_reconciled);
    let expected = b"native workflow proof: 16\n";
    let digest = hex::encode(Sha256::digest(expected));
    let stdout = result
        .artifacts
        .iter()
        .find(|a| a.kind == "stdout" && a.sha256 == digest)
        .expect("stdout from the compiled and exercised executable");
    assert_eq!(fs::read(&stdout.path).unwrap(), expected);
}
