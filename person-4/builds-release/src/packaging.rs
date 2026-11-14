//! Native package production. No signing keys or implicit signing operations.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRequest {
    pub workspace: PathBuf,
    pub output: PathBuf,
    pub target: String,
    pub version: String,
    pub source_commit: String,
    pub cargo_lock_sha256: String,
    pub rustc_version: String,
    pub cargo_version: String,
}
#[derive(Debug, Serialize)]
pub struct PackageReport {
    pub source_commit: String,
    pub cargo_lock_sha256: String,
    pub target: String,
    pub artifact: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub classification: &'static str,
    pub signature_verification: &'static str,
    pub installation_verification: &'static str,
    pub executable_smoke_verification: &'static str,
    pub update_verification: &'static str,
    pub other_platforms: &'static str,
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
fn digest(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    let mut sha = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        sha.update(&buffer[..count]);
        bytes += count as u64;
    }
    let after = file.metadata()?;
    if bytes != before.len()
        || after.len() != before.len()
        || before.modified()? != after.modified()?
    {
        return Err(invalid("artifact changed while hashing").into());
    }
    Ok((format!("{:x}", sha.finalize()), bytes))
}
fn query(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new(program).args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(invalid(&format!(
            "{program} query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn execute(program: &str, args: &[String], cwd: &Path, log: &Path) -> Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(log)?;
    let mut command = Command::new(program);
    if program == "cargo" {
        // Release packaging is local work. Explicit empty values override both
        // inherited wrappers and Cargo configuration pointing at an owner's daemon.
        command
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "");
    }
    let status = command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file.try_clone()?)
        .status()?;
    file.sync_all()?;
    if !status.success() {
        return Err(invalid(&format!(
            "{program} failed ({status}); inspect {}",
            log.display()
        ))
        .into());
    }
    Ok(())
}
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn native_target(target: &str) -> Result<(&'static str, &'static str)> {
    let (os, arch) = match target {
        "x86_64-pc-windows-msvc" => ("windows", "x64"),
        "aarch64-pc-windows-msvc" => ("windows", "arm64"),
        "x86_64-unknown-linux-gnu" => ("linux", "amd64"),
        "aarch64-unknown-linux-gnu" => ("linux", "arm64"),
        "x86_64-apple-darwin" => ("macos", "x86_64"),
        "aarch64-apple-darwin" => ("macos", "arm64"),
        _ => {
            return Err(invalid(
                "unsupported native packaging target; no simulated cross-platform package",
            )
            .into());
        }
    };
    if os != std::env::consts::OS {
        return Err(invalid(
            "target platform tooling unavailable on this host; run on its native release host",
        )
        .into());
    }
    Ok((os, arch))
}
fn validate_version(version: &str) -> Result<()> {
    let components: Vec<_> = version.split('.').collect();
    if components.len() != 3
        || components
            .iter()
            .any(|v| v.is_empty() || !v.bytes().all(|c| c.is_ascii_digit()))
    {
        return Err(invalid("package version must be numeric major.minor.patch").into());
    }
    let numbers: Vec<u32> = components
        .iter()
        .map(|v| v.parse())
        .collect::<std::result::Result<_, _>>()?;
    if numbers[0] > 255 || numbers[1] > 255 || numbers[2] > 65535 {
        return Err(invalid("version exceeds MSI product version range").into());
    }
    Ok(())
}

fn verify_source(workspace: &Path, request: &PackageRequest) -> Result<()> {
    if query("git", &["rev-parse", "HEAD"], workspace)? != request.source_commit
        || !query(
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
            workspace,
        )?
        .is_empty()
        || digest(&workspace.join("Cargo.lock"))?.0 != request.cargo_lock_sha256
    {
        return Err(
            invalid("release source must remain clean at the pinned commit and lockfile").into(),
        );
    }
    Ok(())
}

fn create_output(workspace: &Path, requested: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(requested)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| invalid("output has no parent"))?
        .canonicalize()?;
    let name = absolute
        .file_name()
        .ok_or_else(|| invalid("output has no directory name"))?;
    let output = parent.join(name);
    if output.starts_with(workspace) {
        return Err(invalid("release output must be outside the source checkout").into());
    }
    // Check containment before creating anything in a potentially pinned checkout.
    fs::create_dir(&output)?;
    Ok(output.canonicalize()?)
}

pub fn produce(request: PackageRequest) -> Result<PackageReport> {
    validate_version(&request.version)?;
    let (os, arch) = native_target(&request.target)?;
    let workspace = request.workspace.canonicalize()?;
    if query("git", &["rev-parse", "HEAD"], &workspace)? != request.source_commit
        || !query(
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
            &workspace,
        )?
        .is_empty()
    {
        return Err(invalid("release source must exactly match a clean pinned commit").into());
    }
    if digest(&workspace.join("Cargo.lock"))?.0 != request.cargo_lock_sha256 {
        return Err(invalid("Cargo.lock digest mismatch").into());
    }
    if query("rustc", &["--version"], &workspace)? != request.rustc_version
        || query("cargo", &["--version"], &workspace)? != request.cargo_version
    {
        return Err(invalid("release toolchain pin mismatch").into());
    }
    let output = create_output(&workspace, &request.output)?;
    let target_dir = output.join("cargo-target");
    execute(
        "cargo",
        &[
            "build".into(),
            "--locked".into(),
            "--release".into(),
            "--package".into(),
            "dacs".into(),
            "--features".into(),
            "native".into(),
            "--target".into(),
            request.target.clone(),
            "--target-dir".into(),
            target_dir.to_string_lossy().into_owned(),
        ],
        &workspace,
        &output.join("cargo-build.log"),
    )?;
    verify_source(&workspace, &request)?;
    let binary = target_dir
        .join(&request.target)
        .join("release")
        .join(if os == "windows" { "dacs.exe" } else { "dacs" });
    if !binary.is_file() {
        return Err(invalid("Cargo did not produce the requested native executable").into());
    }
    // A foreign architecture may package successfully but cannot be exercised
    // on this host. Record that distinction instead of claiming a runtime check.
    let host_arch = std::env::consts::ARCH;
    let executable_smoke_verification = if request.target.starts_with(&format!("{host_arch}-")) {
        execute(
            binary
                .to_str()
                .ok_or_else(|| invalid("binary path is not UTF-8"))?,
            &["--help".into()],
            &output,
            &output.join("executable-smoke.log"),
        )?;
        "passed_help_command"
    } else {
        "not_run_requires_target_architecture"
    };
    let stem = format!(
        "dacs-{}-{}-UNSIGNED-PREVIEW",
        request.version, request.target
    );
    let artifact = match os {
        "windows" => {
            let source = output.join("dacs.wxs");
            fs::write(
                &source,
                format!(
                    r#"<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs"><Package Name="DACS Unsigned Preview" Manufacturer="DACS" Version="{}" UpgradeCode="7EC6F9CC-955B-46CE-B90C-249A64AE5981" Scope="perMachine"><MajorUpgrade DowngradeErrorMessage="A newer DACS version is already installed."/><MediaTemplate EmbedCab="yes"/><StandardDirectory Id="ProgramFiles64Folder"><Directory Id="INSTALLFOLDER" Name="DACS"><Component Id="MainBinary" Guid="*"><File Id="DacsExe" Source="{}" KeyPath="yes"/></Component></Directory></StandardDirectory><Feature Id="Main" Title="DACS" Level="1"><ComponentRef Id="MainBinary"/></Feature></Package></Wix>"#,
                    request.version,
                    xml(&binary.to_string_lossy())
                ),
            )?;
            let package = output.join(format!("{stem}.msi"));
            execute(
                "wix",
                &[
                    "build".into(),
                    source.to_string_lossy().into_owned(),
                    "-arch".into(),
                    arch.into(),
                    "-o".into(),
                    package.to_string_lossy().into_owned(),
                ],
                &output,
                &output.join("wix-build.log"),
            )?;
            package
        }
        "linux" => {
            let root = output.join("deb-root");
            fs::create_dir_all(root.join("usr/bin"))?;
            fs::create_dir_all(root.join("DEBIAN"))?;
            fs::copy(&binary, root.join("usr/bin/dacs"))?;
            fs::write(
                root.join("DEBIAN/control"),
                format!(
                    "Package: dacs-preview\nVersion: {}\nArchitecture: {arch}\nMaintainer: DACS Release Operators\nSection: devel\nPriority: optional\nDepends: libc6, libgtk-3-0, libwebkit2gtk-4.1-0, libssl3\nDescription: DACS unsigned preview; not an authenticated application update\n",
                    request.version
                ),
            )?;
            fs::create_dir_all(root.join("usr/share/applications"))?;
            fs::write(
                root.join("usr/share/applications/dacs.desktop"),
                "[Desktop Entry]\nType=Application\nName=DACS Preview\nExec=dacs desktop\nTerminal=false\nCategories=Development;\n",
            )?;
            let package = output.join(format!("{stem}.deb"));
            execute(
                "dpkg-deb",
                &[
                    "--build".into(),
                    "--root-owner-group".into(),
                    root.to_string_lossy().into_owned(),
                    package.to_string_lossy().into_owned(),
                ],
                &output,
                &output.join("dpkg-build.log"),
            )?;
            execute(
                "dpkg-deb",
                &["--info".into(), package.to_string_lossy().into_owned()],
                &output,
                &output.join("dpkg-inspect.log"),
            )?;
            package
        }
        "macos" => {
            let root = output.join("pkg-root");
            fs::create_dir_all(root.join("usr/local/bin"))?;
            fs::copy(&binary, root.join("usr/local/bin/dacs"))?;
            let package = output.join(format!("{stem}.pkg"));
            execute(
                "pkgbuild",
                &[
                    "--root".into(),
                    root.to_string_lossy().into_owned(),
                    "--identifier".into(),
                    "org.dacs.preview".into(),
                    "--version".into(),
                    request.version.clone(),
                    "--install-location".into(),
                    "/".into(),
                    package.to_string_lossy().into_owned(),
                ],
                &output,
                &output.join("pkgbuild.log"),
            )?;
            execute(
                "pkgutil",
                &[
                    "--payload-files".into(),
                    package.to_string_lossy().into_owned(),
                ],
                &output,
                &output.join("pkg-inspect.log"),
            )?;
            package
        }
        _ => unreachable!(),
    };
    verify_source(&workspace, &request)?;
    let (sha256, bytes) = digest(&artifact)?;
    let report = PackageReport {
        source_commit: request.source_commit,
        cargo_lock_sha256: request.cargo_lock_sha256,
        target: request.target,
        artifact,
        sha256,
        bytes,
        classification: "unsigned_preview_not_trusted_update",
        signature_verification: "failed_required_identity_not_used",
        installation_verification: "not_run",
        executable_smoke_verification,
        update_verification: "not_run",
        other_platforms: "not_run_requires_native_host",
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("release-evidence.json"))?;
    serde_json::to_writer_pretty(&mut file, &report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(report)
}

/// Verification of packages returned by an existing trusted signing environment.
/// This command never signs and never asks for/imports signing keys.
pub fn verify_signed(path: &Path, log_directory: &Path) -> Result<()> {
    let path = path.canonicalize()?;
    let (sha256, bytes) = digest(&path)?;
    fs::create_dir(log_directory)?;
    let logs = log_directory.canonicalize()?;
    match std::env::consts::OS {
        "windows" => execute("signtool", &["verify".into(), "/pa".into(), "/all".into(), "/v".into(), path.to_string_lossy().into_owned()], &logs, &logs.join("signature.log")),
        "macos" => {
            execute("pkgutil", &["--check-signature".into(), path.to_string_lossy().into_owned()], &logs, &logs.join("signature.log"))?;
            execute("spctl", &["--assess".into(), "--type".into(), "install".into(), "--verbose=2".into(), path.to_string_lossy().into_owned()], &logs, &logs.join("gatekeeper.log"))?;
            execute("xcrun", &["stapler".into(), "validate".into(), path.to_string_lossy().into_owned()], &logs, &logs.join("notarization.log"))
        }
        _ => Err(invalid("no platform signing verifier configured; authenticated DACS update manifest verification is a separate required gate").into()),
    }?;
    if digest(&path)? != (sha256.clone(), bytes) {
        return Err(invalid("package changed during signature verification").into());
    }
    let mut report = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(logs.join("signature-evidence.json"))?;
    serde_json::to_writer_pretty(
        &mut report,
        &serde_json::json!({
            "artifact": path, "sha256": sha256, "bytes": bytes,
            "platform": std::env::consts::OS,
            "signature_verification": "passed",
            "installation_verification": "not_run",
            "update_verification": "not_run"
        }),
    )?;
    report.write_all(b"\n")?;
    report.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_validation_does_not_modify_checkout_or_reuse_existing_directory() {
        let root = std::env::temp_dir().join(format!("dacs-package-output-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let workspace = root.join("source");
        fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let forbidden = workspace.join("release");
        assert!(create_output(&workspace, &forbidden).is_err());
        assert!(!forbidden.exists());
        let destination = root.join("release");
        assert!(create_output(&workspace, &destination).is_ok());
        assert!(create_output(&workspace, &destination).is_err());
        fs::remove_dir(destination).unwrap();
        fs::remove_dir(workspace).unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn version_rejects_command_and_metadata_injection() {
        for version in [
            "1.2",
            "1.2.3\nArchitecture: all",
            "256.0.0",
            "1.0.65536",
            "-1.2.3",
        ] {
            assert!(validate_version(version).is_err());
        }
        assert!(validate_version("1.2.3").is_ok());
    }
    #[test]
    fn wix_source_paths_are_xml_escaped() {
        assert_eq!(xml("a&\"<>'"), "a&amp;&quot;&lt;&gt;&apos;");
    }
    #[test]
    fn unsupported_platform_is_not_simulated() {
        assert!(native_target("wasm32-unknown-unknown").is_err());
    }
}
