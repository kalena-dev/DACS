use crate::util::{self, CommandSpec};
use dacs_protocol::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub(crate) const CATEGORIES: &[(&str, &str)] = &[
    ("rust_toolchains", "Rust toolchains"),
    ("rust_components", "rustfmt / Clippy"),
    ("rust_web", "Rust web tooling"),
    ("git", "Git"),
    ("debuggers", "Debuggers"),
    ("editors", "Editors"),
    ("browsers", "Browsers"),
    ("databases", "Databases"),
];
#[derive(Clone, Copy)]
pub(crate) struct Package {
    pub id: &'static str,
    pub category: &'static str,
    pub label: &'static str,
    pub executable: &'static str,
    pub winget: &'static str,
    pub brew: &'static str,
    pub apt: &'static str,
    pub prerequisites: &'static [&'static str],
    pub cask: bool,
}
pub(crate) const PACKAGES: &[Package] = &[
    Package {
        id: "rustup",
        category: "rust_toolchains",
        label: "Rustup toolchain manager",
        executable: "rustup",
        winget: "Rustlang.Rustup",
        brew: "rustup",
        apt: "rustup",
        prerequisites: &[],
        cask: false,
    },
    Package {
        id: "rust-stable",
        category: "rust_toolchains",
        label: "Rust toolchain (stable or exact release)",
        executable: "rustc",
        winget: "",
        brew: "",
        apt: "",
        prerequisites: &["rustup"],
        cask: false,
    },
    Package {
        id: "rustfmt",
        category: "rust_components",
        label: "Rustfmt",
        executable: "rustfmt",
        winget: "",
        brew: "",
        apt: "",
        prerequisites: &["rust-stable"],
        cask: false,
    },
    Package {
        id: "clippy",
        category: "rust_components",
        label: "Clippy",
        executable: "clippy-driver",
        winget: "",
        brew: "",
        apt: "",
        prerequisites: &["rust-stable"],
        cask: false,
    },
    Package {
        id: "trunk",
        category: "rust_web",
        label: "Trunk and wasm32-unknown-unknown target",
        executable: "trunk",
        winget: "",
        brew: "",
        apt: "",
        prerequisites: &["rust-stable"],
        cask: false,
    },
    Package {
        id: "git",
        category: "git",
        label: "Git",
        executable: "git",
        winget: "Git.Git",
        brew: "git",
        apt: "git",
        prerequisites: &[],
        cask: false,
    },
    Package {
        id: "lldb",
        category: "debuggers",
        label: "LLDB (LLVM)",
        executable: "lldb",
        winget: "LLVM.LLVM",
        brew: "llvm",
        apt: "lldb",
        prerequisites: &[],
        cask: false,
    },
    Package {
        id: "geany",
        category: "editors",
        label: "Geany",
        executable: "geany",
        winget: "Geany.Geany",
        brew: "geany",
        apt: "geany",
        prerequisites: &[],
        cask: true,
    },
    Package {
        id: "firefox",
        category: "browsers",
        label: "Firefox (ESR on Debian)",
        executable: "firefox",
        winget: "Mozilla.Firefox",
        brew: "firefox",
        apt: "firefox-esr",
        prerequisites: &[],
        cask: true,
    },
    Package {
        id: "sqlite",
        category: "databases",
        label: "SQLite command-line tools",
        executable: "sqlite3",
        winget: "SQLite.SQLite",
        brew: "sqlite",
        apt: "sqlite3",
        prerequisites: &[],
        cask: false,
    },
];
pub(crate) fn package(id: &str) -> Option<&'static Package> {
    PACKAGES.iter().find(|p| p.id == id)
}
pub(crate) fn platform() -> &'static str {
    std::env::consts::OS
}
pub(crate) fn arch() -> &'static str {
    std::env::consts::ARCH
}
pub(crate) fn manager() -> &'static str {
    match platform() {
        "windows" => "winget",
        "macos" => "brew",
        "linux" => "apt-get",
        _ => "unsupported",
    }
}
pub(crate) fn profile() -> Value {
    let supported = match platform() {
        "windows" | "macos" => true,
        "linux" => std::fs::read_to_string("/etc/os-release")
            .map(|s| {
                s.lines().any(|l| l == "ID=debian" || l == "ID=\"debian\"")
                    && s.lines()
                        .any(|l| l == "VERSION_ID=\"13\"" || l == "VERSION_ID=13")
            })
            .unwrap_or(false),
        _ => false,
    };
    json!({"supported":supported,"name":match platform(){"windows"=>"Windows 10/11 with App Installer winget", "macos"=>"macOS with owner-installed Homebrew", "linux"=>"Debian 13 (trixie) apt/rustup", _=>"unsupported"},"manager":manager(),"elevation":"apt-get requires local root; winget may prompt UAC; Homebrew must run as its local owner", "sdk_prerequisite":"Rust compilation additionally requires the platform linker/SDK: Windows MSVC Build Tools, macOS Command Line Tools, or Debian build-essential. SDK presence is reported separately, not installed by arbitrary scripts."})
}

pub fn catalog() -> Value {
    json!({"schema_version":1,"platform":platform(),"arch":arch(),"profile":profile(),
        "categories":CATEGORIES.iter().map(|(id,label)|json!({"id":id,"label":label})).collect::<Vec<_>>(),
        "packages":PACKAGES.iter().map(|p|json!({"id":p.id,"category":p.category,"label":p.label,"prerequisites":p.prerequisites,
            "platforms":{"windows":{"manager":if p.winget.is_empty(){"rustup/cargo"}else{"winget"},"package":p.winget},"macos":{"manager":if p.brew.is_empty(){"rustup/cargo"}else{"brew"},"package":p.brew,"cask":p.cask},"linux":{"manager":if p.apt.is_empty(){"rustup/cargo"}else{"apt-get"},"package":p.apt,"profile":"Debian 13"}},
            "operations":["detect","install","repair","update"]})).collect::<Vec<_>>(),
        "presets":{"rust-cli":["rust-stable","rustfmt","clippy","git"],"rust-web":["rust-stable","rustfmt","clippy","git","trunk"]},
        "limits":{"custom_commands":"unresolved; not executable through this engine","dependency_upgrades":"explicit reviewed update or approved version change only","versions":"optional exact package-manager version, not a semver range; rustfmt/clippy versions name the coherent Rust toolchain release","repair":"Rust toolchains require an exact release; manager repair/reinstall support and cached package availability are checked by the real manager, with failures journaled","brew_pinning":"only current formula/cask versions; repair refuses an implicit version upgrade","runtime_and_network":"external owner adapters required; OMP login remains manual","validation":"Recipe identities are reviewed against upstream package catalogues; successful cross-platform installation/signing is not claimed without release-owner evidence"},
        "recipe_sources":["https://rust-lang.github.io/rustup/concepts/components.html","https://learn.microsoft.com/en-us/windows/package-manager/winget/install","https://github.com/microsoft/winget-pkgs","https://formulae.brew.sh/","https://packages.debian.org/trixie/","https://trunkrs.dev/guide/getting-started/"]})
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Installed,
    Missing,
    Incompatible,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Detection {
    pub id: String,
    pub state: State,
    pub version: Option<String>,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Requirement {
    pub id: String,
    #[serde(default)]
    pub version: Option<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Operation {
    Install,
    Repair,
    Update,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvitationRequest {
    pub coordinator_name: String,
    pub invitation_token: String,
    pub requirements_version: String,
    pub required_packages: Vec<Requirement>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanRequest {
    pub operation: Operation,
    #[serde(default)]
    pub packages: Vec<Requirement>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub declined: Vec<String>,
    #[serde(default)]
    pub invitation: Option<InvitationRequest>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Step {
    pub package_id: String,
    pub operation: Operation,
    pub toolchain: String,
    pub required_version: Option<String>,
    pub state: State,
    pub observed_version: Option<String>,
    pub commands: Vec<CommandSpec>,
    pub prerequisites: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub schema_version: u32,
    pub request: PlanRequest,
    pub platform: String,
    pub arch: String,
    pub steps: Vec<Step>,
    pub unresolved: Vec<String>,
    pub disabled_capabilities: Vec<String>,
    pub sha256: String,
}

pub(crate) fn validate_version(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 80
        || !value.as_bytes()[0].is_ascii_digit()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+~:-_".contains(&b))
    {
        return Err(util::invalid(
            "Version must be a bounded exact version beginning with a digit, not a command, path, range, URL or channel",
        ));
    }
    Ok(())
}
fn numeric_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|s| s.trim_matches(|c: char| c == '(' || c == ')' || c == ','))
        .find(|s| s.as_bytes().first().is_some_and(u8::is_ascii_digit) && s.contains('.'))
        .map(str::to_owned)
}
fn manager_version(id: &str, operating_system: &str, version: String) -> String {
    if id == "git" && operating_system == "windows" {
        if let Some((release, revision)) = version.split_once(".windows.") {
            if semver::Version::parse(release).is_ok()
                && !revision.is_empty()
                && revision.bytes().all(|byte| byte.is_ascii_digit())
            {
                // Git.Git manifests omit .windows.1, but retain subsequent installer revisions:
                // https://github.com/microsoft/winget-pkgs/blob/master/manifests/g/Git/Git/2.54.0/Git.Git.installer.yaml
                // https://github.com/microsoft/winget-pkgs/blob/master/manifests/g/Git/Git/2.51.0.2/Git.Git.installer.yaml
                return if revision == "1" { release.into() } else { format!("{release}.{revision}") };
            }
        }
    }
    version
}

pub(crate) fn probe(p: &Package, requested: Option<&str>) -> Detection {
    probe_toolchain(p, requested, "stable")
}
pub(crate) fn probe_toolchain(p: &Package, requested: Option<&str>, toolchain: &str) -> Detection {
    let mut spec = CommandSpec::new(
        p.executable,
        &[if p.id == "sqlite" {
            "-version"
        } else {
            "--version"
        }],
    );
    if matches!(p.id, "rust-stable" | "rustfmt" | "clippy") {
        spec = CommandSpec::new(
            "rustup",
            &[
                "run",
                requested.unwrap_or(toolchain),
                p.executable,
                "--version",
            ],
        );
    }
    let mut detection = match util::run(&spec, Duration::from_secs(20)) {
        Ok(out) if out.success => Detection {
            id: p.id.into(),
            state: State::Installed,
            version: numeric_version(&out.stdout).map(|version| manager_version(p.id, platform(), version)),
            detail: out.stdout.trim().chars().take(500).collect(),
        },
        Ok(out) => Detection {
            id: p.id.into(),
            state: State::Unknown,
            version: None,
            detail: format!(
                "Version probe failed (exit {:?}): {}",
                out.code,
                out.stderr.chars().take(300).collect::<String>()
            ),
        },
        Err(e) => Detection {
            id: p.id.into(),
            state: if util::executable(&spec.program).is_none() {
                State::Missing
            } else {
                State::Unknown
            },
            version: None,
            detail: e.message,
        },
    };
    // Package database probes cover graphical programs and tools not added to this process's PATH.
    if !p.apt.is_empty() && platform() == "linux" {
        let query = CommandSpec::new("dpkg-query", &["-W", "-f=${Status}\n${Version}", p.apt]);
        if let Ok(out) = util::run(&query, Duration::from_secs(10)) {
            if out.success && out.stdout.starts_with("install ok installed\n") {
                detection.state = State::Installed;
                detection.version = out.stdout.lines().nth(1).map(str::to_owned);
                detection.detail="Installed dpkg package; runtime probe may require PATH/desktop/session prerequisites".into();
            } else if !out.success && detection.state != State::Installed {
                detection.state = State::Missing;
            }
        }
    }
    if !p.brew.is_empty() && platform() == "macos" {
        let mut query = CommandSpec::new("brew", &["list", "--versions"]);
        query
            .args
            .push(if p.cask { "--cask" } else { "--formula" }.into());
        query.args.push(p.brew.into());
        if let Ok(out) = util::run(&query, Duration::from_secs(15)) {
            if out.success && !out.stdout.trim().is_empty() {
                detection.state = State::Installed;
                detection.version = out.stdout.split_whitespace().nth(1).map(str::to_owned);
                detection.detail = "Installed Homebrew package".into();
            } else if !out.success && detection.state != State::Installed {
                detection.state = State::Missing;
            }
        }
    }
    if matches!(p.id, "rust-stable" | "rustfmt" | "clippy") && detection.state == State::Unknown {
        if let Ok(out) = util::run(
            &CommandSpec::new("rustup", &["toolchain", "list"]),
            Duration::from_secs(10),
        ) {
            let channel = requested.unwrap_or(toolchain);
            if out.success
                && !out
                    .stdout
                    .lines()
                    .any(|l| l.starts_with(&format!("{channel}-")))
            {
                detection.state = State::Missing;
                detection.detail = format!("Toolchain {channel} is not installed");
            } else if out.success && p.id != "rust-stable" {
                if let Ok(components) = util::run(
                    &CommandSpec::new(
                        "rustup",
                        &["component", "list", "--installed", "--toolchain", channel],
                    ),
                    Duration::from_secs(10),
                ) {
                    if components.success && !components.stdout.lines().any(|l| l.starts_with(p.id))
                    {
                        detection.state = State::Missing;
                        detection.detail =
                            format!("Component {} is not installed for {channel}", p.id);
                    }
                }
            }
        }
    }
    if p.id == "trunk" && detection.state == State::Installed {
        match util::run(
            &CommandSpec::new(
                "rustup",
                &["target", "list", "--installed", "--toolchain", toolchain],
            ),
            Duration::from_secs(10),
        ) {
            Ok(output)
                if output.success
                    && output
                        .stdout
                        .lines()
                        .any(|line| line == "wasm32-unknown-unknown") =>
            {
                ()
            }
            Ok(output) if output.success => {
                detection.state = State::Missing;
                detection.detail =
                    "Trunk is present but its wasm32-unknown-unknown target is missing".into();
            }
            _ => {
                detection.state = State::Unknown;
                detection.detail =
                    "Trunk is present but its Rust/WASM target could not be verified".into();
            }
        }
    }
    if detection.state == State::Installed && !matches!(p.id, "rustfmt" | "clippy") {
        if let Some(required) = requested {
            if detection.version.as_deref() != Some(required) {
                detection.state = State::Incompatible;
            }
        }
    }
    detection
}

pub fn detect() -> Result<Value> {
    Ok(
        json!({"platform":platform(),"arch":arch(),"profile":profile(),
        "managers":(["winget","brew","apt-get","rustup","cargo"].iter().map(|id|json!({"id":id,"available":util::executable(id).is_some()})).collect::<Vec<_>>()),
        "packages":PACKAGES.iter().map(|p|probe(p,None)).collect::<Vec<_>>(),
        "prerequisites":{"linker":{"available":util::executable(if cfg!(windows){"link"}else{"cc"}).is_some(),"note":"Presence is not SDK compatibility validation; use the platform's developer environment"},"omp":{"available":util::executable("omp").is_some(),"pin":"external owner-supplied pinned OMP version required before enrollment"}},
        "enrollment":{"runtime":"external_unavailable","network":"external_unavailable","provider_login":"manual_required","worker_started":false,"network_joined":false}}),
    )
}

pub(crate) fn recipes(
    p: &Package,
    version: Option<&str>,
    operation: Operation,
    toolchain: &str,
) -> Result<Vec<CommandSpec>> {
    if let Some(v) = version {
        validate_version(v)?;
    }
    if toolchain != "stable" {
        validate_version(toolchain)?;
    }
    let channel = version.unwrap_or(toolchain);
    match p.id {
        "rust-stable" => {
            let mut commands = Vec::new();
            if operation == Operation::Repair {
                if version.is_none() {
                    return Err(util::invalid(
                        "Toolchain repair requires an explicit exact release; an active moving channel is not removed automatically",
                    ));
                }
                commands.push(CommandSpec::new(
                    "rustup",
                    &["toolchain", "uninstall", channel],
                ));
            }
            commands.push(CommandSpec::new(
                "rustup",
                &["toolchain", "install", channel, "--profile", "minimal"],
            ));
            Ok(commands)
        }
        "rustfmt" | "clippy" => {
            let mut commands = Vec::new();
            if operation == Operation::Repair {
                commands.push(CommandSpec::new(
                    "rustup",
                    &["component", "remove", p.id, "--toolchain", channel],
                ));
            }
            commands.push(CommandSpec::new(
                "rustup",
                &["component", "add", p.id, "--toolchain", channel],
            ));
            Ok(commands)
        }
        "trunk" => {
            let mut command = CommandSpec::new(
                "rustup",
                &[
                    "run",
                    toolchain,
                    "cargo",
                    "install",
                    "trunk",
                    "--locked",
                    "--registry",
                    "crates-io",
                ],
            );
            if operation != Operation::Install {
                command.args.push("--force".into());
            }
            if let Some(v) = version {
                command.args.extend(["--version".into(), v.into()]);
            }
            Ok(vec![
                CommandSpec::new(
                    "rustup",
                    &[
                        "target",
                        "add",
                        "wasm32-unknown-unknown",
                        "--toolchain",
                        toolchain,
                    ],
                ),
                command,
            ])
        }
        _ => {
            let command = match platform() {
                "windows" => {
                    let mut command = CommandSpec::new(
                        "winget",
                        &[
                            match operation {
                                Operation::Install => "install",
                                Operation::Repair => "repair",
                                Operation::Update => "upgrade",
                            },
                            "--id",
                            p.winget,
                            "--exact",
                            "--source",
                            "winget",
                            "--disable-interactivity",
                            "--accept-source-agreements",
                        ],
                    );
                    if operation != Operation::Repair {
                        command.args.push("--accept-package-agreements".into());
                    }
                    if operation == Operation::Install && version.is_none() {
                        command.args.push("--no-upgrade".into());
                    }
                    if let Some(v) = version {
                        command.args.extend(["--version".into(), v.into()]);
                    }
                    command
                }
                "macos" => {
                    let mut command = CommandSpec::new(
                        "brew",
                        &[
                            match operation {
                                Operation::Install => "install",
                                Operation::Repair => "reinstall",
                                Operation::Update => "upgrade",
                            },
                            if p.cask { "--cask" } else { "--formula" },
                        ],
                    );
                    command.args.push(p.brew.into());
                    command
                }
                "linux" => {
                    let mut command =
                        CommandSpec::new("apt-get", &["install", "--yes", "--no-remove"]);
                    if operation == Operation::Repair {
                        command.args.push("--reinstall".into());
                    }
                    if operation == Operation::Update {
                        command.args.push("--only-upgrade".into());
                    }
                    command.args.push(
                        version.map_or_else(|| p.apt.to_owned(), |v| format!("{}={v}", p.apt)),
                    );
                    command
                }
                _ => return Err(util::invalid("Unsupported operating system")),
            };
            Ok(vec![command])
        }
    }
}

pub(crate) fn selected_recipes(p: &Package, version: Option<&str>, operation: Operation, toolchain: &str, observed_version: Option<&str>) -> Result<Vec<CommandSpec>> {
    let mut commands = recipes(p, version, operation, toolchain)?;
    if p.id == "trunk" && operation == Operation::Install && observed_version.is_some()
        && (version.is_none() || version == observed_version)
    {
        // The tool and its WASM target have separate lifecycles. Preserve an installed tool.
        commands.truncate(1);
    }
    Ok(commands)
}

fn declined_dependency<'a>(id: &str, declined: &'a [String]) -> Option<&'a str> {
    if let Some(id) = declined.iter().find(|candidate| candidate.as_str() == id) {
        return Some(id);
    }
    package(id).and_then(|package| package.prerequisites.iter().find_map(|id| declined_dependency(id, declined)))
}

pub(crate) fn effective_operation(request: &PlanRequest, id: &str) -> Operation {
    let direct = request.packages.iter().any(|p| p.id == id)
        || request
            .invitation
            .as_ref()
            .is_some_and(|i| i.required_packages.iter().any(|p| p.id == id))
        || request.preset.as_deref().is_some_and(|preset| {
            matches!(id, "rust-stable" | "rustfmt" | "clippy" | "git")
                || (preset == "rust-web" && id == "trunk")
        });
    if direct {
        request.operation
    } else {
        Operation::Install
    }
}

pub(crate) fn selections(
    request: &PlanRequest,
) -> Result<(Vec<Requirement>, Vec<String>, Vec<String>)> {
    if request.packages.len() > 128 || request.declined.len() > 128 {
        return Err(util::invalid("Too many package selections"));
    }
    if request.declined.iter().any(|id| id.len() > 100) {
        return Err(util::invalid("Declined package ID exceeds length limit"));
    }
    let mut selected = request.packages.clone();
    if let Some(preset) = &request.preset {
        let ids: &[&str] = match preset.as_str() {
            "rust-cli" => &["rust-stable", "rustfmt", "clippy", "git"],
            "rust-web" => &["rust-stable", "rustfmt", "clippy", "git", "trunk"],
            _ => return Err(util::invalid("Unknown project preset")),
        };
        for id in ids {
            if !selected.iter().any(|p| p.id == *id) {
                selected.push(Requirement {
                    id: (*id).into(),
                    version: None,
                });
            }
        }
    }
    if let Some(invite) = &request.invitation {
        if invite.coordinator_name.is_empty()
            || invite.coordinator_name.len() > 200
            || invite.invitation_token.is_empty()
            || invite.invitation_token.len() > 1024
            || invite.requirements_version.is_empty()
            || invite.requirements_version.len() > 128
            || invite.required_packages.len() > 128
        {
            return Err(util::invalid("Invalid invitation review metadata"));
        }
        selected.extend(invite.required_packages.clone());
    }
    let mut map = BTreeMap::<String, Requirement>::new();
    let mut unresolved = Vec::new();
    let mut disabled = Vec::new();
    for requirement in selected {
        if requirement.id.len() > 100 {
            return Err(util::invalid("Package ID too long"));
        }
        if request.declined.contains(&requirement.id) {
            disabled.push(format!(
                "Capabilities requiring {} are disabled by local owner",
                requirement.id
            ));
            continue;
        }
        if let Some(version) = &requirement.version {
            validate_version(version)?;
        }
        if package(&requirement.id).is_none() {
            unresolved.push(format!(
                "Unknown catalogue requirement: {} (custom commands are not accepted)",
                requirement.id
            ));
            continue;
        }
        if let Some(old) = map.get(&requirement.id) {
            if old.version.is_some()
                && requirement.version.is_some()
                && old.version != requirement.version
            {
                return Err(util::conflict(format!(
                    "Conflicting version requirements for {}",
                    requirement.id
                )));
            }
            if requirement.version.is_none() {
                continue;
            }
        }
        map.insert(requirement.id.clone(), requirement);
    }
    map.retain(|id, _| {
        if let Some(prerequisite) = declined_dependency(id, &request.declined) {
            disabled.push(format!("Capabilities requiring {id} are disabled because prerequisite {prerequisite} was declined"));
            false
        } else {
            true
        }
    });
    let versions: BTreeSet<String> = ["rust-stable", "rustfmt", "clippy"]
        .iter()
        .filter_map(|id| map.get(*id).and_then(|r| r.version.clone()))
        .collect();
    if versions.len() > 1 {
        return Err(util::conflict(
            "Rust toolchain and component requirements must use one coherent exact toolchain release",
        ));
    }
    if let Some(version) = versions.iter().next() {
        map.insert(
            "rust-stable".into(),
            Requirement { id: "rust-stable".into(), version: Some(version.clone()) },
        );
        for id in ["rustfmt", "clippy"] {
            if let Some(requirement) = map.get_mut(id) {
                requirement.version = Some(version.clone());
            }
        }
    }
    let mut ordered = Vec::new();
    let mut seen = BTreeSet::new();
    fn visit(
        id: &str,
        map: &BTreeMap<String, Requirement>,
        seen: &mut BTreeSet<String>,
        ordered: &mut Vec<Requirement>,
    ) {
        if !seen.insert(id.into()) {
            return;
        }
        let Some(p) = package(id) else {
            return;
        };
        for prerequisite in p.prerequisites {
            visit(prerequisite, map, seen, ordered);
        }
        ordered.push(map.get(id).cloned().unwrap_or(Requirement {
            id: id.into(),
            version: None,
        }));
    }
    for id in map.keys() {
        visit(id, &map, &mut seen, &mut ordered);
    }
    Ok((ordered, unresolved, disabled))
}

pub(crate) fn plan_digest(plan: &Plan) -> Result<String> {
    let mut content = plan.clone();
    content.sha256.clear();
    Ok(util::digest(
        &serde_json::to_vec(&content).map_err(util::io)?,
    ))
}
pub(crate) fn brew_available(p: &Package) -> Result<String> {
    let out = util::run(
        &CommandSpec::new(
            "brew",
            &[
                "info",
                "--json=v2",
                if p.cask { "--cask" } else { "--formula" },
                p.brew,
            ],
        ),
        Duration::from_secs(30),
    )?;
    if !out.success {
        return Err(util::conflict(format!(
            "Homebrew metadata unavailable for {}",
            p.id
        )));
    }
    let value: Value = serde_json::from_str(&out.stdout).map_err(util::io)?;
    let version = if p.cask {
        value.pointer("/casks/0/version")
    } else {
        value.pointer("/formulae/0/versions/stable")
    };
    let mut version = version
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| util::conflict("Homebrew did not report a current version"))?;
    if !p.cask {
        if let Some(revision) = value
            .pointer("/formulae/0/revision")
            .and_then(Value::as_u64)
        {
            if revision > 0 {
                version.push_str(&format!("_{revision}"));
            }
        }
    }
    Ok(version)
}

pub fn plan(value: Value) -> Result<Value> {
    let request: PlanRequest = util::decode(value)?;
    let (selected, mut unresolved, disabled_capabilities) = selections(&request)?;
    if profile()["supported"] != true {
        unresolved.push(
            "Unsupported OS profile: only Windows, Homebrew macOS and Debian 13 apt are declared"
                .into(),
        );
    }
    let mut steps = Vec::new();
    let toolchain = selected
        .iter()
        .find(|s| s.id == "rust-stable")
        .and_then(|s| s.version.clone())
        .unwrap_or_else(|| "stable".into());
    for selection in selected {
        let p = package(&selection.id).ok_or_else(|| util::invalid("Unknown package"))?;
        let operation = effective_operation(&request, p.id);
        let detection = probe_toolchain(p, selection.version.as_deref(), &toolchain);
        let mut required_version = selection.version.clone();
        if operation == Operation::Repair
            && required_version.is_none()
            && !matches!(p.id, "rust-stable" | "rustfmt" | "clippy")
        {
            required_version = detection.version.clone();
        }
        if operation == Operation::Repair && p.id == "rust-stable" && selection.version.is_none() {
            unresolved.push("Toolchain repair requires selecting an exact installed Rust release; active moving channels are not removed automatically".into());
        }
        if detection.state == State::Unknown {
            unresolved.push(format!(
                "{} detection is uncertain: {}",
                p.id, detection.detail
            ));
        }
        if detection.state == State::Incompatible
            && operation == Operation::Install
            && selection.version.is_none()
        {
            unresolved.push(format!("{} requires an explicit version change", p.id));
        }
        if operation == Operation::Repair
            && required_version.is_none()
            && !matches!(p.id, "rustfmt" | "clippy")
        {
            unresolved.push(format!(
                "Cannot repair {} without its installed version",
                p.id
            ));
        }
        let should_run = operation != Operation::Install || detection.state != State::Installed;
        if should_run && !p.brew.is_empty() && platform() == "macos" {
            match brew_available(p) {
                Ok(available) => {
                    if let Some(required) = &required_version {
                        if required != &available {
                            unresolved.push(format!("{}: Homebrew current version {available} cannot satisfy reviewed exact version {required}; no silent upgrade",p.id));
                        }
                    }
                    required_version = Some(available);
                }
                Err(e) => unresolved.push(e.message),
            }
        }
        let commands = if should_run {
            match selected_recipes(p, required_version.as_deref(), operation, &toolchain, detection.version.as_deref()) {
                Ok(commands) => commands,
                Err(error) => {
                    unresolved.push(error.message);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        if commands.iter().any(|c| c.program == manager()) && util::executable(manager()).is_none()
        {
            unresolved.push(format!(
                "{} must be installed locally before applying {}",
                manager(),
                p.id
            ));
        }
        if p.id == "trunk"
            && commands.iter().any(|command| command.args.iter().any(|argument| argument == "cargo"))
            && util::executable(if cfg!(windows) { "link" } else { "cc" }).is_none()
        {
            unresolved.push("Trunk's Cargo installation requires a working local linker/SDK developer environment".into());
        }
        steps.push(Step {
            package_id: p.id.into(),
            operation,
            toolchain: toolchain.clone(),
            required_version,
            state: detection.state,
            observed_version: detection.version,
            commands,
            prerequisites: p.prerequisites.iter().map(|s| (*s).into()).collect(),
        });
    }
    let mut result = Plan {
        schema_version: 1,
        request,
        platform: platform().into(),
        arch: arch().into(),
        steps,
        unresolved,
        disabled_capabilities,
        sha256: String::new(),
    };
    result.sha256 = plan_digest(&result)?;
    util::json(&result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_git_executable_and_winget_versions_match() {
        let first_revision = manager_version("git", "windows", "2.54.0.windows.1".into());
        assert_eq!(first_revision, "2.54.0");
        validate_version(&first_revision).unwrap();
        let installed = manager_version("git", "windows", "2.51.0.windows.2".into());
        assert_eq!(installed, "2.51.0.2");
        assert_eq!(manager_version("git", "linux", "2.51.0".into()), "2.51.0");
        validate_version(&installed).unwrap();
    }

    #[test]
    fn missing_wasm_target_preserves_existing_trunk() {
        let commands = selected_recipes(package("trunk").unwrap(), None, Operation::Install, "stable", Some("0.20.0")).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].args, ["target", "add", "wasm32-unknown-unknown", "--toolchain", "stable"]);
        let explicit_update = selected_recipes(package("trunk").unwrap(), None, Operation::Update, "stable", Some("0.20.0")).unwrap();
        assert!(explicit_update.iter().any(|command| command.args.iter().any(|argument| argument == "cargo")));
    }

    #[test]
    fn declined_toolchains_disable_dependents_but_not_git() {
        for declined in ["rust-stable", "rustup"] {
            let request = PlanRequest {operation:Operation::Install,packages:vec![],preset:Some("rust-web".into()),declined:vec![declined.into()],invitation:None};
            let (selected, unresolved, disabled) = selections(&request).unwrap();
            assert!(unresolved.is_empty());
            assert_eq!(selected.iter().map(|requirement| requirement.id.as_str()).collect::<Vec<_>>(), ["git"]);
            assert!(disabled.iter().any(|reason| reason.contains("trunk")));
        }
    }
    #[test]
    fn every_category_has_platform_recipes() {
        assert_eq!(CATEGORIES.len(), 8);
        for (category, _) in CATEGORIES {
            let members: Vec<_> = PACKAGES
                .iter()
                .filter(|p| p.category == *category)
                .collect();
            assert!(!members.is_empty());
            for package in members {
                assert!(
                    !package.winget.is_empty()
                        && !package.brew.is_empty()
                        && !package.apt.is_empty()
                        || matches!(package.id, "rust-stable" | "rustfmt" | "clippy" | "trunk")
                );
            }
        }
    }
    #[test]
    fn toolchain_pins_propagate_without_upgrading_prerequisites() {
        let request = PlanRequest {
            operation: Operation::Repair,
            packages: vec![Requirement {
                id: "clippy".into(),
                version: Some("1.90.0".into()),
            }],
            preset: None,
            declined: vec![],
            invitation: None,
        };
        let (selected, unresolved, _) = selections(&request).unwrap();
        assert!(unresolved.is_empty());
        assert_eq!(
            selected
                .iter()
                .find(|r| r.id == "rust-stable")
                .unwrap()
                .version
                .as_deref(),
            Some("1.90.0")
        );
        assert_eq!(effective_operation(&request, "rustup"), Operation::Install);
        assert_eq!(
            effective_operation(&request, "rust-stable"),
            Operation::Install
        );
        assert_eq!(effective_operation(&request, "clippy"), Operation::Repair);
        let commands = recipes(
            package("trunk").unwrap(),
            Some("0.21.14"),
            Operation::Install,
            "1.90.0",
        )
        .unwrap();
        assert!(
            commands
                .iter()
                .all(|c| !c.args.iter().any(|a| a == "stable"))
        );
    }
    #[test]
    fn unknown_packages_never_become_executable_steps() {
        let request = PlanRequest {
            operation: Operation::Install,
            packages: vec![Requirement {
                id: "git;curl attacker".into(),
                version: None,
            }],
            preset: None,
            declined: vec![],
            invitation: None,
        };
        let (selected, unresolved, _) = selections(&request).unwrap();
        assert!(selected.is_empty());
        assert_eq!(unresolved.len(), 1);
    }
    #[test]
    fn exact_toolchain_repair_is_a_journalable_remove_install_pair() {
        let commands = recipes(
            package("rust-stable").unwrap(),
            Some("1.90.0"),
            Operation::Repair,
            "1.90.0",
        )
        .unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].args, ["toolchain", "uninstall", "1.90.0"]);
        assert!(
            recipes(
                package("rust-stable").unwrap(),
                None,
                Operation::Repair,
                "stable"
            )
            .is_err()
        );
    }
}
