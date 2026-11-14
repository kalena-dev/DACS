use crate::{
    catalogue::{self, Operation, Plan, State},
    util,
};
use dacs_protocol::{ApiError, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Consent {
    plan_sha256: String,
    owner_approved: bool,
    #[serde(default)]
    administrator_access_approved: bool,
    #[serde(default)]
    resume_interrupted: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StepStatus {
    Pending,
    Running,
    Completed,
    Failed,
}
#[derive(Debug, Serialize, Deserialize)]
struct JournalStep {
    package_id: String,
    command_index: usize,
    status: StepStatus,
    started_at: Option<u64>,
    completed_at: Option<u64>,
    exit_code: Option<i32>,
    detail: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    plan_sha256: String,
    status: String,
    started_at: u64,
    updated_at: u64,
    steps: Vec<JournalStep>,
    enrollment: Value,
}

fn validate_plan(plan: &Plan, consent: &Consent) -> Result<()> {
    if !consent.owner_approved
        || consent.plan_sha256 != plan.sha256
        || plan.sha256 != catalogue::plan_digest(plan)?
    {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "Explicit local owner approval must match the complete reviewed plan SHA-256",
        ));
    }
    if plan.schema_version != 1
        || plan.platform != catalogue::platform()
        || plan.arch != catalogue::arch()
    {
        return Err(util::invalid("Plan schema/platform/architecture mismatch"));
    }
    if catalogue::profile()["supported"] != true {
        return Err(util::invalid("Current platform profile is unsupported"));
    }
    if !plan.unresolved.is_empty() {
        return Err(util::conflict(
            "Resolve all plan prerequisites and unknown requirements before approval",
        ));
    }
    if (!plan.steps.iter().all(|s| s.commands.is_empty()) || plan.request.invitation.is_some())
        && !consent.administrator_access_approved
    {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "Local owner must acknowledge installer privileges and full-trust host access; this is not a sandbox",
        ));
    }
    let (selected, unresolved, disabled) = catalogue::selections(&plan.request)?;
    if !unresolved.is_empty()
        || disabled != plan.disabled_capabilities
        || selected.len() != plan.steps.len()
    {
        return Err(util::invalid("Plan selections were changed after review"));
    }
    let toolchain = selected
        .iter()
        .find(|s| s.id == "rust-stable")
        .and_then(|s| s.version.as_deref())
        .unwrap_or("stable");
    for (selected, step) in selected.iter().zip(&plan.steps) {
        if selected.id != step.package_id {
            return Err(util::invalid(
                "Package steps must match canonical prerequisite order",
            ));
        }
        if selected.version.is_some() && selected.version != step.required_version {
            return Err(util::invalid("Requested exact version changed in plan"));
        }
        let p = catalogue::package(&step.package_id)
            .ok_or_else(|| util::invalid("Unknown package ID"))?;
        if step.operation != catalogue::effective_operation(&plan.request, p.id)
            || step.toolchain != toolchain
        {
            return Err(util::invalid(
                "Dependency operation/toolchain changed after review",
            ));
        }
        if step
            .prerequisites
            .iter()
            .map(String::as_str)
            .ne(p.prerequisites.iter().copied())
        {
            return Err(util::invalid("Package prerequisites were modified"));
        }
        let expected = catalogue::selected_recipes(
            p,
            step.required_version.as_deref(),
            step.operation,
            &step.toolchain,
            step.observed_version.as_deref(),
        )?;
        if !step.commands.is_empty() && step.commands != expected {
            return Err(util::invalid(
                "Command arrays do not match reviewed local catalogue recipes",
            ));
        }
        if step.commands.is_empty()
            && (step.state != State::Installed || step.operation != Operation::Install)
        {
            return Err(util::invalid("Required commands are missing"));
        }
        if step.operation == Operation::Repair
            && step.required_version.is_none()
            && !matches!(p.id, "rustfmt" | "clippy")
        {
            return Err(util::invalid(
                "Repair requires an exact installed version to avoid implicit upgrades",
            ));
        }
    }
    Ok(())
}

fn fresh_journal(plan: &Plan) -> Journal {
    Journal {
        schema_version: 1,
        plan_sha256: plan.sha256.clone(),
        status: "approved".into(),
        started_at: util::now(),
        updated_at: util::now(),
        steps: plan
            .steps
            .iter()
            .flat_map(|s| {
                (0..s.commands.len()).map(move |index| JournalStep {
                    package_id: s.package_id.clone(),
                    command_index: index,
                    status: StepStatus::Pending,
                    started_at: None,
                    completed_at: None,
                    exit_code: None,
                    detail: None,
                })
            })
            .collect(),
        enrollment: json!({"status":"not_requested","worker_started":false,"network_joined":false}),
    }
}

// The plan digest does not cover time: an identical later plan maps to the same journal,
// so a stored `completed` result is only reusable while every step still probes as installed.
fn steps_satisfied(
    plan: &Plan,
    probe: impl Fn(&catalogue::Package, Option<&str>, &str) -> State,
) -> bool {
    plan.steps.iter().all(|step| {
        catalogue::package(&step.package_id).is_some_and(|p| {
            probe(p, step.required_version.as_deref(), &step.toolchain) == State::Installed
        })
    })
}

fn mark_replan_required(journal: &mut Journal, journal_path: &Path) -> Result<()> {
    journal.status = "replan_required".into();
    journal.updated_at = util::now();
    util::atomic_json(journal_path, journal)
}

pub fn apply(value: Value, approval: Value, state_dir: &Path) -> Result<Value> {
    let plan: Plan = util::decode(value)?;
    let consent: Consent = util::decode(approval)?;
    validate_plan(&plan, &consent)?;
    let state = util::state_directory(state_dir)?;
    let _lock = util::lock(&state.join("setup.lock"))?;
    let journal_path = state.join(format!("setup-{}.json", plan.sha256));
    let canonical_plan_path = state.join(format!("plan-{}.json", plan.sha256));
    if canonical_plan_path.exists() {
        let old: Plan = util::read_json(&canonical_plan_path)?;
        if catalogue::plan_digest(&old)? != plan.sha256 {
            return Err(util::conflict("Persisted reviewed plan was altered"));
        }
    } else {
        util::atomic_json(&canonical_plan_path, &plan)?;
    }
    let mut journal: Journal = if journal_path.exists() {
        util::read_json(&journal_path)?
    } else {
        fresh_journal(&plan)
    };
    if journal.plan_sha256 != plan.sha256 || journal.schema_version != 1 {
        return Err(util::conflict(
            "Operation journal does not match reviewed plan",
        ));
    }
    let expected: Vec<_> = plan
        .steps
        .iter()
        .flat_map(|s| (0..s.commands.len()).map(move |i| (&s.package_id, i)))
        .collect();
    if expected.len() != journal.steps.len()
        || expected
            .iter()
            .zip(&journal.steps)
            .any(|((id, index), step)| *id != &step.package_id || *index != step.command_index)
    {
        return Err(util::conflict("Operation journal step mapping is invalid"));
    }
    if journal.status == "completed" {
        if steps_satisfied(&plan, |p, version, toolchain| {
            catalogue::probe_toolchain(p, version, toolchain).state
        }) {
            return util::json(&journal);
        }
        // Installed state drifted since the stored run: replace the stale journal and run again.
        journal = fresh_journal(&plan);
        util::atomic_json(&journal_path, &journal)?;
    }
    if journal
        .steps
        .iter()
        .any(|s| matches!(s.status, StepStatus::Running | StepStatus::Failed))
        && !consent.resume_interrupted
    {
        journal.status = "interrupted_requires_local_review".into();
        journal.updated_at = util::now();
        util::atomic_json(&journal_path, &journal)?;
        return Err(util::conflict(
            "An interrupted/failed command may have external effects. Review installed state and journal; explicit resume_interrupted is required, never automatic replay",
        ));
    }
    journal.status = "running".into();
    util::atomic_json(&journal_path, &journal)?;
    for step in &plan.steps {
        let p =
            catalogue::package(&step.package_id).ok_or_else(|| util::invalid("Unknown package"))?;
        if step.commands.is_empty() {
            let detected =
                catalogue::probe_toolchain(p, step.required_version.as_deref(), &step.toolchain);
            if detected.state != State::Installed {
                journal.status = "replan_required".into();
                util::atomic_json(&journal_path, &journal)?;
                return Err(util::conflict(format!(
                    "{} changed since review; create and approve a new plan",
                    p.id
                )));
            }
            continue;
        }
        if catalogue::platform() == "macos" && !p.brew.is_empty() {
            let available = catalogue::brew_available(p)?;
            if step.required_version.as_deref() != Some(available.as_str()) {
                mark_replan_required(&mut journal, &journal_path)?;
                return Err(util::conflict(
                    "Homebrew formula changed since approval; new plan and consent required",
                ));
            }
            if step.operation == Operation::Repair
                && step.observed_version.as_deref() != Some(available.as_str())
            {
                mark_replan_required(&mut journal, &journal_path)?;
                return Err(util::conflict(
                    "Repair would upgrade Homebrew package; use explicit update instead",
                ));
            }
        }
        // Unversioned install must not upgrade a tool that appeared between review and execution.
        if step.operation == Operation::Install && step.required_version.is_none() {
            let detected = catalogue::probe_toolchain(p, None, &step.toolchain);
            if detected.state == State::Installed {
                for entry in journal
                    .steps
                    .iter_mut()
                    .filter(|s| s.package_id == step.package_id)
                {
                    entry.status = StepStatus::Completed;
                    entry.completed_at = Some(util::now());
                    entry.detail =
                        Some("Already installed at execution time; no upgrade performed".into());
                }
                util::atomic_json(&journal_path, &journal)?;
                continue;
            }
        }
        if p.id == "trunk" && step.operation == Operation::Install
            && step.observed_version.is_some()
            && (step.required_version.is_none() || step.required_version == step.observed_version)
        {
            let observed = catalogue::probe_toolchain(p, None, &step.toolchain);
            if observed.version != step.observed_version {
                mark_replan_required(&mut journal, &journal_path)?;
                return Err(util::conflict("The Trunk executable changed since review; replan rather than reinstalling/upgrading it implicitly"));
            }
        }
        for (command_index, command) in step.commands.iter().enumerate() {
            let index = journal
                .steps
                .iter()
                .position(|s| s.package_id == step.package_id && s.command_index == command_index)
                .ok_or_else(|| util::conflict("Missing journal command"))?;
            if matches!(journal.steps[index].status, StepStatus::Completed) {
                continue;
            }
            if p.id == "trunk" && step.operation == Operation::Install
                && step.required_version.is_none()
                && command.args.iter().any(|argument| argument == "cargo")
            {
                let observed = catalogue::probe_toolchain(p, None, &step.toolchain);
                if observed.version.is_some() {
                    journal.steps[index].status = StepStatus::Completed;
                    journal.steps[index].completed_at = Some(util::now());
                    journal.steps[index].detail = Some("Preserved installed Trunk; only missing WASM prerequisites were installed".into());
                    util::atomic_json(&journal_path, &journal)?;
                    continue;
                }
                if observed.state == State::Unknown {
                    mark_replan_required(&mut journal, &journal_path)?;
                    return Err(util::conflict("Cannot determine whether Trunk appeared since review; refusing an implicit upgrade"));
                }
            }
            journal.steps[index].status = StepStatus::Running;
            journal.steps[index].started_at = Some(util::now());
            journal.updated_at = util::now();
            util::atomic_json(&journal_path, &journal)?;
            let output = util::run(command, Duration::from_secs(1800));
            match output {
                Ok(out) if out.success => {
                    journal.steps[index].status = StepStatus::Completed;
                    journal.steps[index].completed_at = Some(util::now());
                    journal.steps[index].exit_code = out.code;
                    journal.steps[index].detail = Some(out.stdout.chars().take(4000).collect());
                }
                Ok(out) => {
                    journal.steps[index].status = StepStatus::Failed;
                    journal.steps[index].exit_code = out.code;
                    journal.steps[index].detail = Some(out.stderr.chars().take(4000).collect());
                    journal.status = "failed_requires_local_review".into();
                    journal.updated_at = util::now();
                    util::atomic_json(&journal_path, &journal)?;
                    return Err(util::conflict(format!(
                        "{} command {} failed (exit {:?}); durable journal retains partial effects",
                        p.id, command_index, out.code
                    )));
                }
                Err(error) => {
                    journal.steps[index].status = StepStatus::Failed;
                    journal.steps[index].detail = Some(error.message.clone());
                    journal.status = "interrupted_requires_local_review".into();
                    journal.updated_at = util::now();
                    util::atomic_json(&journal_path, &journal)?;
                    return Err(error);
                }
            }
            journal.updated_at = util::now();
            util::atomic_json(&journal_path, &journal)?;
        }
        let detected =
            catalogue::probe_toolchain(p, step.required_version.as_deref(), &step.toolchain);
        if detected.state != State::Installed {
            journal.status = "verification_requires_local_review".into();
            journal.updated_at = util::now();
            util::atomic_json(&journal_path, &journal)?;
            return Err(util::conflict(format!(
                "{} installer exited successfully but tool/version verification is {:?}: {}. Refresh PATH/SDK/session as needed; no fabricated readiness",
                p.id, detected.state, detected.detail
            )));
        }
    }
    if let Some(invite) = &plan.request.invitation {
        journal.enrollment = json!({"status":"pending_external_enrollment","coordinator_name":invite.coordinator_name,"requirements_version":invite.requirements_version,"local_owner_approved":true,"host_device_approval":"pending","network_adapter":"external_unavailable","worker_adapter":"external_unavailable","provider_login":"manual_required","disabled_capabilities":plan.disabled_capabilities,"worker_started":false,"network_joined":false,"full_trust_disclosure":"Administrator-level access is not sandboxed; restrictions remain advisory"});
        // Token remains in private local state, not returned to UI/logs.
        util::atomic_json(
            &state.join(format!("enrollment-{}.json", plan.sha256)),
            &json!({"schema_version":1,"plan_sha256":plan.sha256,"invitation":invite,"consent":journal.enrollment,"created_at":util::now()}),
        )?;
    }
    journal.status = "completed".into();
    journal.updated_at = util::now();
    util::atomic_json(&journal_path, &journal)?;
    util::json(&journal)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consent_rejects_tampered_plan_without_executing() {
        let mut plan = Plan {
            schema_version: 1,
            request: catalogue::PlanRequest {
                operation: Operation::Install,
                packages: vec![],
                preset: None,
                declined: vec![],
                invitation: None,
            },
            platform: catalogue::platform().into(),
            arch: catalogue::arch().into(),
            steps: vec![],
            unresolved: vec![],
            disabled_capabilities: vec![],
            sha256: String::new(),
        };
        plan.sha256 = catalogue::plan_digest(&plan).unwrap();
        let consent = Consent {
            plan_sha256: plan.sha256.clone(),
            owner_approved: true,
            administrator_access_approved: true,
            resume_interrupted: false,
        };
        plan.request.preset = Some("rust-web".into());
        assert_eq!(
            validate_plan(&plan, &consent).unwrap_err().code,
            ErrorCode::Forbidden
        );
    }
    #[test]
    fn rejects_command_like_versions() {
        for value in [
            "--force",
            "../tool",
            "1.2;rm",
            "https://example.com/x",
            "1.2\n--flag",
        ] {
            assert!(catalogue::validate_version(value).is_err());
        }
    }
    #[test]
    fn interrupted_journal_round_trip_is_not_completed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let journal = Journal {
            schema_version: 1,
            plan_sha256: "ab".repeat(32),
            status: "running".into(),
            started_at: 1,
            updated_at: 1,
            steps: vec![JournalStep {
                package_id: "git".into(),
                command_index: 0,
                status: StepStatus::Running,
                started_at: Some(1),
                completed_at: None,
                exit_code: None,
                detail: None,
            }],
            enrollment: Value::Null,
        };
        util::atomic_json(&path, &journal).unwrap();
        let loaded: Journal = util::read_json(&path).unwrap();
        assert!(matches!(loaded.steps[0].status, StepStatus::Running));
        assert_ne!(loaded.status, "completed");
    }
    #[test]
    fn completed_journal_is_reused_only_while_steps_still_probe_installed() {
        let mut plan = Plan {
            schema_version: 1,
            request: catalogue::PlanRequest {
                operation: Operation::Install,
                packages: vec![],
                preset: None,
                declined: vec![],
                invitation: None,
            },
            platform: catalogue::platform().into(),
            arch: catalogue::arch().into(),
            steps: vec![catalogue::Step {
                package_id: "git".into(),
                operation: Operation::Install,
                toolchain: "stable".into(),
                required_version: Some("2.50.0".into()),
                state: State::Missing,
                observed_version: None,
                commands: vec![],
                prerequisites: vec![],
            }],
            unresolved: vec![],
            disabled_capabilities: vec![],
            sha256: String::new(),
        };
        plan.sha256 = catalogue::plan_digest(&plan).unwrap();
        assert!(steps_satisfied(&plan, |p, version, toolchain| {
            assert_eq!((p.id, version, toolchain), ("git", Some("2.50.0"), "stable"));
            State::Installed
        }));
        for drifted in [State::Missing, State::Incompatible, State::Unknown] {
            assert!(!steps_satisfied(&plan, |_, _, _| drifted.clone()));
        }
        // A stale completed journal is replaced by one with every command pending again.
        let fresh = fresh_journal(&plan);
        assert_eq!(fresh.status, "approved");
        assert!(fresh.steps.iter().all(|s| matches!(s.status, StepStatus::Pending)));
    }
}
