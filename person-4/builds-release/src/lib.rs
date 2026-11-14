//! Evidence evaluation for observed build results. These are local verification
//! inputs, independent of coordinator wire contracts and admission authority.
use std::collections::BTreeSet;

pub const REQUIRED_TOOL_CATEGORIES: [&str; 8] = [
    "rust-toolchain",
    "rustfmt-clippy",
    "rust-web",
    "git",
    "debugger",
    "editor",
    "browser",
    "database",
];

pub struct ToolVerification {
    pub platform: String,
    pub architecture: String,
    pub category: String,
    pub package: String,
    pub version: String,
    pub install_passed: bool,
    pub detection_passed: bool,
    pub repair_passed: bool,
    pub update_passed: bool,
    pub evidence_location: String,
}

pub fn missing_catalogue_coverage(
    platform: &str,
    architecture: &str,
    observations: &[ToolVerification],
) -> Vec<&'static str> {
    REQUIRED_TOOL_CATEGORIES
        .into_iter()
        .filter(|category| {
            !observations.iter().any(|item| {
                item.platform == platform
                    && item.architecture == architecture
                    && item.category == *category
                    && !item.package.is_empty()
                    && !item.version.is_empty()
                    && !item.evidence_location.is_empty()
                    && item.install_passed
                    && item.detection_passed
                    && item.repair_passed
                    && item.update_passed
            })
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteAction {
    pub invocation_id: String,
    pub physical_host: String,
    pub enrolled_worker: String,
    pub source_revision: String,
    pub toolchain: String,
    pub target: String,
    pub consumed_output_digest: String,
    pub successful_compilation: bool,
    pub cache_hit: bool,
}

pub struct BuildEvidence {
    pub source_revision: String,
    pub toolchain: String,
    pub target: String,
    pub initiator_physical_host: String,
    pub required_remote_hosts: BTreeSet<String>,
    pub native_linux_profile_verified: bool,
    pub clean_outputs: bool,
    pub cold_cache: bool,
    pub incremental_disabled: bool,
    pub linked: bool,
    pub behavioral_checks_passed: bool,
    pub actions: Vec<RemoteAction>,
}

/// Evaluate only observations supplied by the action-log reconciliation adapter.
/// A successful result does not establish authenticity of untrusted input.
pub fn verify_distributed_build(evidence: &BuildEvidence) -> Result<(), Vec<String>> {
    let mut failures = Vec::new();
    for (verified, reason) in [
        (
            evidence.native_linux_profile_verified,
            "native Linux profile was not verified",
        ),
        (
            evidence.clean_outputs,
            "Cargo outputs were not verified clean",
        ),
        (
            evidence.cold_cache,
            "compiler result cache was not verified cold",
        ),
        (
            evidence.incremental_disabled,
            "incremental compilation was not disabled",
        ),
        (evidence.linked, "result was not successfully linked"),
        (
            evidence.behavioral_checks_passed,
            "behavioral checks did not pass",
        ),
        (
            !evidence.source_revision.is_empty(),
            "source revision is missing",
        ),
        (
            !evidence.toolchain.is_empty(),
            "toolchain identity is missing",
        ),
        (!evidence.target.is_empty(), "target is missing"),
        (
            !evidence.initiator_physical_host.is_empty(),
            "initiator identity is missing",
        ),
    ] {
        if !verified {
            failures.push(reason.to_owned());
        }
    }
    if evidence.required_remote_hosts.len() != 2
        || evidence.required_remote_hosts.contains("")
        || evidence
            .required_remote_hosts
            .contains(&evidence.initiator_physical_host)
    {
        failures.push("proof requires two distinct physical hosts other than the initiator".into());
    }
    let mut invocations = BTreeSet::new();
    let mut contributors = BTreeSet::new();
    for action in &evidence.actions {
        if action.invocation_id.is_empty() || !invocations.insert(&action.invocation_id) {
            failures.push("empty or repeated invocation identity requires reconciliation".into());
            continue;
        }
        if action.successful_compilation
            && !action.cache_hit
            && !action.enrolled_worker.is_empty()
            && !action.consumed_output_digest.is_empty()
            && action.source_revision == evidence.source_revision
            && action.toolchain == evidence.toolchain
            && action.target == evidence.target
            && evidence
                .required_remote_hosts
                .contains(&action.physical_host)
        {
            contributors.insert(action.physical_host.clone());
        }
    }
    for missing in evidence.required_remote_hosts.difference(&contributors) {
        failures.push(format!(
            "no verified consumed compilation from physical host {missing}"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_platform_evidence_leaves_all_categories_unverified() {
        assert_eq!(
            missing_catalogue_coverage("windows", "x86_64", &[]).len(),
            8
        );
    }

    #[test]
    fn detection_alone_and_wrong_platform_cannot_certify_package_lifecycle() {
        let mut observation = ToolVerification {
            platform: "windows".into(),
            architecture: "x86_64".into(),
            category: "git".into(),
            package: "Git".into(),
            version: "observed-version".into(),
            install_passed: false,
            detection_passed: true,
            repair_passed: false,
            update_passed: false,
            evidence_location: "test-log".into(),
        };
        assert_eq!(
            missing_catalogue_coverage("windows", "x86_64", std::slice::from_ref(&observation))
                .len(),
            8
        );
        observation.install_passed = true;
        observation.repair_passed = true;
        observation.update_passed = true;
        assert_eq!(
            missing_catalogue_coverage("windows", "x86_64", std::slice::from_ref(&observation))
                .len(),
            7
        );
        assert_eq!(
            missing_catalogue_coverage("linux", "x86_64", &[observation]).len(),
            8
        );
    }

    fn evidence() -> BuildEvidence {
        BuildEvidence {
            source_revision: "source-a".into(),
            toolchain: "rust-a".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            initiator_physical_host: "initiator".into(),
            required_remote_hosts: BTreeSet::from(["host-b".into(), "host-c".into()]),
            native_linux_profile_verified: true,
            clean_outputs: true,
            cold_cache: true,
            incremental_disabled: true,
            linked: true,
            behavioral_checks_passed: true,
            actions: ["host-b", "host-c"]
                .into_iter()
                .map(|host| RemoteAction {
                    invocation_id: format!("invocation-{host}"),
                    physical_host: host.into(),
                    enrolled_worker: format!("worker-{host}"),
                    source_revision: "source-a".into(),
                    toolchain: "rust-a".into(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    consumed_output_digest: format!("digest-{host}"),
                    successful_compilation: true,
                    cache_hit: false,
                })
                .collect(),
        }
    }

    #[test]
    fn accepts_two_distinct_consumed_remote_invocations() {
        assert!(verify_distributed_build(&evidence()).is_ok());
    }

    #[test]
    fn cached_output_cannot_replace_remote_compilation() {
        let mut evidence = evidence();
        evidence.actions[1].cache_hit = true;
        assert!(verify_distributed_build(&evidence).is_err());
    }

    #[test]
    fn two_guests_on_one_physical_host_do_not_satisfy_proof() {
        let mut evidence = evidence();
        evidence.actions[1].physical_host = "host-b".into();
        assert!(verify_distributed_build(&evidence).is_err());
    }

    #[test]
    fn stale_revision_and_unconsumed_outputs_are_rejected() {
        let mut evidence = evidence();
        evidence.actions[0].source_revision = "old-source".into();
        evidence.actions[1].consumed_output_digest.clear();
        assert!(verify_distributed_build(&evidence).is_err());
    }

    #[test]
    fn replayed_invocation_cannot_count_twice() {
        let mut evidence = evidence();
        evidence.actions[1].invocation_id = evidence.actions[0].invocation_id.clone();
        assert!(verify_distributed_build(&evidence).is_err());
    }
}
