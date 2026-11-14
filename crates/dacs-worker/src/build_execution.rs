//! Build profiles, isolated sccache configurations and compile placement evidence.
//!
//! This module prepares work for [`crate::ManagedChild`]; Person 2's supervisor
//! remains the process owner. It never edits global Cargo or sccache configuration.

use crate::ManagedChild;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuildPlanError {
    #[error("build profile is incomplete: {0}")]
    Invalid(String),
    #[error("build input I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("evidence encoding failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilerServer {
    pub device_id: String,
    pub physical_host_fingerprint: String,
    /// Stable private overlay address with port, e.g. `100.64.0.5:10501`.
    pub public_addr: String,
    pub capacity_units: u32,
    /// Human-auditable handle only; credentials never appear in evidence.
    pub credential_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilerPool {
    pub pool_id: String,
    pub scheduler_url: String,
    pub servers: Vec<CompilerServer>,
}

impl CompilerPool {
    /// A physical host can be assigned to one pool only. A VM must be registered
    /// using its parent physical fingerprint so it cannot inflate capacity.
    pub fn validate(&self, other_pools: &[CompilerPool]) -> Result<(), BuildPlanError> {
        if !safe_label(&self.pool_id) || !self.scheduler_url.starts_with("https://") {
            return Err(BuildPlanError::Invalid(
                "pool id and HTTPS scheduler URL are required".into(),
            ));
        }
        let mut hosts = BTreeSet::new();
        let mut addresses = BTreeSet::new();
        for server in &self.servers {
            if server.physical_host_fingerprint.is_empty()
                || server.capacity_units == 0
                || server.credential_ref.is_empty()
                || server.public_addr.parse::<SocketAddr>().is_err()
            {
                return Err(BuildPlanError::Invalid(format!(
                    "server {} is missing host, capacity, or credential reference",
                    server.device_id
                )));
            }
            if !hosts.insert(server.physical_host_fingerprint.as_str()) {
                return Err(BuildPlanError::Invalid("a physical host appears more than once in this pool (VM capacity is folded onto its host)".into()));
            }
            if !addresses.insert(server.public_addr.as_str()) {
                return Err(BuildPlanError::Invalid(
                    "compiler server addresses must be unique".into(),
                ));
            }
        }
        for pool in other_pools {
            for server in &pool.servers {
                if self
                    .servers
                    .iter()
                    .any(|ours| ours.physical_host_fingerprint == server.physical_host_fingerprint)
                {
                    return Err(BuildPlanError::Invalid(format!(
                        "physical host {} is allocated to pools {} and {}",
                        server.physical_host_fingerprint, pool.pool_id, self.pool_id
                    )));
                }
            }
        }
        Ok(())
    }

    /// Reject local profile reuse of a daemon port; a profile can run only one
    /// distributed build at a time, while distinct profiles use distinct daemons.
    pub fn validate_profile_ports(&self, profiles: &[BuildProfile]) -> Result<(), BuildPlanError> {
        let mut ports = BTreeSet::new();
        for profile in profiles {
            if profile.pool_id != self.pool_id || !ports.insert(profile.sccache_server_port) {
                return Err(BuildPlanError::Invalid(
                    "profiles must belong to this pool and use distinct sccache daemon ports"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Write isolated scheduler credentials/config. Keep this only on the
    /// scheduler host; never distribute the server-auth signing secret to workers.
    pub fn write_scheduler_config(
        &self,
        root: &Path,
        listen_addr: &str,
        client_token: &str,
        server_jwt_secret: &str,
    ) -> Result<PathBuf, BuildPlanError> {
        if !safe_label(&self.pool_id)
            || listen_addr.parse::<SocketAddr>().is_err()
            || client_token.is_empty()
            || server_jwt_secret.is_empty()
        {
            return Err(BuildPlanError::Invalid(
                "scheduler requires a socket address and nonempty generated auth values".into(),
            ));
        }
        fs::create_dir_all(root)?;
        let path = root.join(format!("sccache-scheduler-{}.toml", self.pool_id));
        let contents = format!(
            "public_addr = {}\n\n[client_auth]\ntype = \"token\"\ntoken = {}\n\n[server_auth]\ntype = \"jwt_hs256\"\nsecret_key = {}\n",
            toml_string(listen_addr),
            toml_string(client_token),
            toml_string(server_jwt_secret)
        );
        write_private_config(&path, contents.as_bytes())?;
        Ok(path)
    }

    /// Write one address-pinned server config. `server_token` must be generated
    /// by `sccache-dist auth generate-jwt-hs256-server-token` for `public_addr`.
    pub fn write_server_config(
        &self,
        server: &CompilerServer,
        root: &Path,
        cache_dir: &Path,
        build_dir: &Path,
        bubblewrap: &Path,
        server_token: &str,
    ) -> Result<PathBuf, BuildPlanError> {
        if !safe_label(&server.device_id)
            || !self.servers.iter().any(|registered| registered == server)
            || server_token.is_empty()
        {
            return Err(BuildPlanError::Invalid(
                "server must be registered in this pool and have its generated address token"
                    .into(),
            ));
        }
        fs::create_dir_all(root)?;
        let path = root.join(format!("sccache-server-{}.toml", server.device_id));
        let contents = format!(
            "cache_dir = {}\npublic_addr = {}\nscheduler_url = {}\n\n[builder]\ntype = \"overlay\"\nbuild_dir = {}\nbwrap_path = {}\n\n[scheduler_auth]\ntype = \"jwt_token\"\ntoken = {}\n",
            toml_string(&cache_dir.to_string_lossy()),
            toml_string(&server.public_addr),
            toml_string(&self.scheduler_url),
            toml_string(&build_dir.to_string_lossy()),
            toml_string(&bubblewrap.to_string_lossy()),
            toml_string(server_token)
        );
        write_private_config(&path, contents.as_bytes())?;
        Ok(path)
    }

    pub fn scheduler_descriptor(
        &self,
        sccache_dist: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        reservation_id: &str,
    ) -> ManagedChild {
        ManagedChild {
            name: format!("sccache-scheduler:{}", self.pool_id),
            program: sccache_dist.into(),
            args: vec![
                "scheduler".into(),
                "--config".into(),
                config.into().to_string_lossy().into_owned(),
            ],
            cwd: cwd.into(),
            env: BTreeMap::from([("SCCACHE_NO_DAEMON".into(), "1".into())]),
            effect_class: "host_level".into(),
            reservation_id: Some(reservation_id.to_owned()),
        }
    }

    pub fn server_descriptor(
        &self,
        server: &CompilerServer,
        sccache_dist: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        reservation_id: &str,
    ) -> Result<ManagedChild, BuildPlanError> {
        if !safe_label(&server.device_id)
            || !self.servers.iter().any(|registered| registered == server)
        {
            return Err(BuildPlanError::Invalid(
                "cannot launch a server outside this pool registration".into(),
            ));
        }
        Ok(ManagedChild {
            name: format!("sccache-server:{}", server.device_id),
            program: sccache_dist.into(),
            args: vec![
                "server".into(),
                "--config".into(),
                config.into().to_string_lossy().into_owned(),
            ],
            cwd: cwd.into(),
            env: BTreeMap::from([("SCCACHE_NO_DAEMON".into(), "1".into())]),
            effect_class: "host_level".into(),
            reservation_id: Some(reservation_id.to_owned()),
        })
    }
}

/// Immutable inputs for one explicitly admitted distributed build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildProfile {
    pub profile_id: String,
    pub pool_id: String,
    pub source_commit: String,
    pub cargo_lock_sha256: String,
    pub rustc_version: String,
    pub cargo_version: String,
    pub target: String,
    pub package: String,
    pub features: Vec<String>,
    pub default_features: bool,
    /// Exact rustc flags; passed through `CARGO_ENCODED_RUSTFLAGS`.
    pub rustflags: Vec<String>,
    /// Explicit non-secret build environment pins.
    pub environment: BTreeMap<String, String>,
    /// `release` or a named Cargo profile. Incremental compilation is always off.
    pub cargo_profile: String,
    /// A separate daemon port/config/cache namespace for this active profile.
    pub sccache_server_port: u16,
}

impl BuildProfile {
    pub fn validate(&self, pool: &CompilerPool) -> Result<(), BuildPlanError> {
        if !safe_label(&self.profile_id) || self.pool_id != pool.pool_id {
            return Err(BuildPlanError::Invalid(
                "profile must reference its admitted compiler pool".into(),
            ));
        }
        let oid = self.source_commit.as_str();
        if !((oid.len() == 40 || oid.len() == 64)
            && oid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            return Err(BuildPlanError::Invalid(
                "source_commit must be a full lowercase Git object id".into(),
            ));
        }
        if self.cargo_lock_sha256.len() != 64
            || !self
                .cargo_lock_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(BuildPlanError::Invalid(
                "cargo_lock_sha256 must be 64 hex characters".into(),
            ));
        }
        if self.target.trim().is_empty()
            || self.package.trim().is_empty()
            || self.rustc_version.trim().is_empty()
            || self.cargo_version.trim().is_empty()
        {
            return Err(BuildPlanError::Invalid(
                "target, package and exact Cargo/rustc versions are required".into(),
            ));
        }
        if self.sccache_server_port == 0 || self.cargo_profile.trim().is_empty() {
            return Err(BuildPlanError::Invalid(
                "private sccache port and Cargo profile are required".into(),
            ));
        }
        for name in self.environment.keys() {
            if name.starts_with("CARGO_") && name == "CARGO_INCREMENTAL" {
                return Err(BuildPlanError::Invalid(
                    "CARGO_INCREMENTAL is controlled by DACS and must remain disabled".into(),
                ));
            }
            if name.contains('=')
                || name.contains('\0')
                || name.starts_with("SCCACHE_")
                || name == "RUSTC_WRAPPER"
                || name == "CARGO_ENCODED_RUSTFLAGS"
                || name == "RUSTFLAGS"
            {
                return Err(BuildPlanError::Invalid(format!(
                    "environment key {name} is reserved by the build runner"
                )));
            }
        }
        Ok(())
    }

    /// Verify checked-out lockfile bytes against the admitted profile.
    pub fn verify_lockfile(&self, lockfile: &Path) -> Result<(), BuildPlanError> {
        let bytes = fs::read(lockfile)?;
        let actual = hex::encode(Sha256::digest(bytes));
        if !actual.eq_ignore_ascii_case(&self.cargo_lock_sha256) {
            return Err(BuildPlanError::Invalid(format!(
                "Cargo.lock digest mismatch: expected {}, observed {actual}",
                self.cargo_lock_sha256
            )));
        }
        Ok(())
    }

    /// Refuse to prepare a build if the selected toolchain binaries do not match
    /// the exact versions pinned in the admitted profile.
    pub fn verify_toolchain(&self, rustc: &Path, cargo: &Path) -> Result<(), BuildPlanError> {
        let observed_rustc = Command::new(rustc).arg("--version").output()?;
        let observed_cargo = Command::new(cargo).arg("--version").output()?;
        if !observed_rustc.status.success() || !observed_cargo.status.success() {
            return Err(BuildPlanError::Invalid(
                "rustc/cargo version query failed".into(),
            ));
        }
        let rustc_text = String::from_utf8_lossy(&observed_rustc.stdout)
            .trim()
            .to_owned();
        let cargo_text = String::from_utf8_lossy(&observed_cargo.stdout)
            .trim()
            .to_owned();
        if rustc_text != self.rustc_version || cargo_text != self.cargo_version {
            return Err(BuildPlanError::Invalid(format!(
                "toolchain mismatch: rustc `{rustc_text}`, cargo `{cargo_text}`"
            )));
        }
        Ok(())
    }

    /// Build a fresh Cargo command descriptor. `run_root` must be unique per
    /// attempt; target outputs are rejected if already present, proving a cold
    /// output directory. The caller passes this to Person 2's Supervisor.
    pub fn cargo_build_descriptor(
        &self,
        cargo: impl Into<PathBuf>,
        checkout: impl Into<PathBuf>,
        run_root: impl Into<PathBuf>,
        sccache: impl Into<PathBuf>,
        sccache_config: impl Into<PathBuf>,
        job_id: &str,
        reservation_id: &str,
    ) -> Result<ManagedChild, BuildPlanError> {
        let checkout = checkout.into();
        let root = run_root.into().join(&self.profile_id);
        let target_dir = root.join("target");
        if target_dir.exists() {
            return Err(BuildPlanError::Invalid(format!(
                "cold target directory already exists: {}",
                target_dir.display()
            )));
        }
        fs::create_dir_all(&root)?;
        let mut args = vec![
            "build".to_owned(),
            "--locked".to_owned(),
            "--package".to_owned(),
            self.package.clone(),
            "--target".to_owned(),
            self.target.clone(),
            "--profile".to_owned(),
            self.cargo_profile.clone(),
        ];
        if !self.features.is_empty() {
            args.push("--features".into());
            args.push(self.features.join(","));
        }
        if !self.default_features {
            args.push("--no-default-features".into());
        }
        let mut env = self.environment.clone();
        env.insert("CARGO_INCREMENTAL".into(), "0".into());
        env.insert(
            "CARGO_TARGET_DIR".into(),
            target_dir.to_string_lossy().into_owned(),
        );
        env.insert(
            "RUSTC_WRAPPER".into(),
            sccache.into().to_string_lossy().into_owned(),
        );
        env.insert(
            "SCCACHE_CONF".into(),
            sccache_config.into().to_string_lossy().into_owned(),
        );
        env.insert(
            "SCCACHE_SERVER_PORT".into(),
            self.sccache_server_port.to_string(),
        );
        if !self.rustflags.is_empty() {
            env.insert(
                "CARGO_ENCODED_RUSTFLAGS".into(),
                self.rustflags.join("\u{1f}"),
            );
        }
        Ok(ManagedChild {
            name: format!("cargo-build:{}", self.profile_id),
            program: cargo.into(),
            args,
            cwd: checkout,
            env,
            // Build scripts and proc macros may affect the host; without an
            // enforced sandbox this process cannot be replayed safely.
            effect_class: "host_level".into(),
            reservation_id: Some(reservation_id.to_owned()),
        })
    }

    /// Generate a stock sccache client configuration. The configuration lives in
    /// the profile directory, so profile switches cannot restart or reconfigure an
    /// unrelated owner daemon.
    pub fn write_sccache_client_config(
        &self,
        root: &Path,
        scheduler_url: &str,
        client_token: &str,
    ) -> Result<PathBuf, BuildPlanError> {
        if !safe_label(&self.profile_id)
            || !scheduler_url.starts_with("https://")
            || client_token.is_empty()
        {
            return Err(BuildPlanError::Invalid(
                "sccache client requires HTTPS scheduler and authenticated token".into(),
            ));
        }
        fs::create_dir_all(root)?;
        let path = root.join(format!("sccache-{}.toml", self.profile_id));
        let contents = format!(
            "[dist]\nscheduler_url = {}\n\n[dist.auth]\ntype = \"token\"\ntoken = {}\n",
            toml_string(scheduler_url),
            toml_string(client_token)
        );
        write_private_config(&path, contents.as_bytes())?;
        Ok(path)
    }

    /// Daemon launch descriptor. Caller must stop only this profile's daemon after
    /// all builds using it have drained.
    pub fn sccache_daemon_descriptor(
        &self,
        sccache: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        run_root: impl Into<PathBuf>,
        reservation_id: &str,
    ) -> ManagedChild {
        let root = run_root.into().join(&self.profile_id);
        let env = BTreeMap::from([
            (
                "SCCACHE_CONF".into(),
                config.into().to_string_lossy().into_owned(),
            ),
            (
                "SCCACHE_SERVER_PORT".into(),
                self.sccache_server_port.to_string(),
            ),
            (
                "SCCACHE_DIR".into(),
                root.join("cache").to_string_lossy().into_owned(),
            ),
            ("SCCACHE_START_SERVER".into(), "1".into()),
            ("SCCACHE_NO_DAEMON".into(), "1".into()),
        ]);
        ManagedChild {
            name: format!("sccache-daemon:{}", self.profile_id),
            program: sccache.into(),
            args: Vec::new(),
            cwd: root,
            env,
            effect_class: "host_level".into(),
            reservation_id: Some(reservation_id.to_owned()),
        }
    }
}

/// Per-compilation evidence, derived from scheduler/server logs and action logs.
/// `consumed_by_build` must mean the output participated in the final linked/tested result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileActionEvidence {
    pub invocation_id: String,
    pub crate_name: String,
    pub target: String,
    pub toolchain: String,
    pub device_id: String,
    pub physical_host_fingerprint: String,
    pub remote: bool,
    pub success: bool,
    pub consumed_by_build: bool,
    pub cache_hit: bool,
    pub local_fallback: bool,
    pub attempt_count: u32,
    pub elapsed_ms: u64,
    pub transferred_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildEvidence {
    pub evidence_version: u32,
    pub build_id: String,
    pub profile_id: String,
    pub pool_id: String,
    pub source_commit: String,
    pub cargo_lock_sha256: String,
    pub rustc_version: String,
    pub target: String,
    pub clean_output: bool,
    pub cold_result_cache: bool,
    pub started_at_ms: i64,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    pub actions: Vec<CompileActionEvidence>,
    pub worker_removal_observed: bool,
    pub fallback_behavior: Option<String>,
}

impl BuildEvidence {
    /// Validate the mandatory two-remote-worker gate using distinct eligible
    /// invocations whose results the final Cargo build consumed.
    pub fn require_two_remote_hosts(
        &self,
        expected_hosts: [&str; 2],
    ) -> Result<(), BuildPlanError> {
        if !self.clean_output || !self.cold_result_cache || self.exit_code != Some(0) {
            return Err(BuildPlanError::Invalid(
                "required proof needs a successful build, clean outputs, and cold result cache"
                    .into(),
            ));
        }
        for host in expected_hosts {
            let count = self
                .actions
                .iter()
                .filter(|a| {
                    a.remote
                        && a.success
                        && a.consumed_by_build
                        && !a.cache_hit
                        && a.physical_host_fingerprint == host
                })
                .count();
            if count == 0 {
                return Err(BuildPlanError::Invalid(format!(
                    "no successful, consumed, uncached remote invocation recorded for host {host}"
                )));
            }
        }
        if expected_hosts[0] == expected_hosts[1] {
            return Err(BuildPlanError::Invalid(
                "the two remote hosts must be physically distinct".into(),
            ));
        }
        let ids: BTreeSet<_> = self
            .actions
            .iter()
            .filter(|a| a.remote && a.success && a.consumed_by_build)
            .map(|a| a.invocation_id.as_str())
            .collect();
        let recorded = self
            .actions
            .iter()
            .filter(|a| a.remote && a.success && a.consumed_by_build)
            .count();
        if ids.len() != recorded {
            return Err(BuildPlanError::Invalid(
                "duplicate remote invocation id recorded".into(),
            ));
        }
        Ok(())
    }

    pub fn append_jsonl(&self, path: &Path) -> Result<(), BuildPlanError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(self)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
}

fn toml_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn write_private_config(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
