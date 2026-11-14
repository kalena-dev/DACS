mod instrumentation;
mod packaging;

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::File,
    io::{self, Read},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Serialize)]
struct Artifact {
    path: PathBuf,
    bytes: u64,
    sha256: String,
    signature_verification: &'static str,
    installation_verification: &'static str,
}

fn inspect(path: PathBuf) -> io::Result<Artifact> {
    let mut file = File::open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("artifact must be a regular file"));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut bytes = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        bytes += count as u64;
    }
    let after = file.metadata()?;
    if bytes != metadata.len()
        || after.len() != metadata.len()
        || after.modified()? != metadata.modified()?
    {
        return Err(io::Error::other("artifact changed during hashing"));
    }
    Ok(Artifact {
        path,
        bytes,
        sha256: format!("{:x}", digest.finalize()),
        signature_verification: "not_run",
        installation_verification: "not_run",
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|arg| arg == "instrument-sccache")
    {
        if arguments.len() != 2 {
            return Err(
                "usage: dacs-release-audit instrument-sccache <clean-v0.10.0-checkout>".into(),
            );
        }
        return instrumentation::instrument(std::path::Path::new(&arguments[1]));
    }
    if arguments.first().is_some_and(|arg| arg == "package") {
        if arguments.len() != 2 {
            return Err("usage: dacs-release-audit package <request.json>".into());
        }
        let request: packaging::PackageRequest =
            serde_json::from_reader(File::open(&arguments[1])?)?;
        let report = packaging::produce(request)?;
        // Reuse the same race-checked artifact inspector as standalone audits.
        inspect(report.artifact.clone())?;
        serde_json::to_writer_pretty(io::stdout().lock(), &report)?;
        println!();
        return Ok(());
    }
    if arguments.first().is_some_and(|arg| arg == "verify-signed") {
        if arguments.len() != 3 {
            return Err(
                "usage: dacs-release-audit verify-signed <package> <new-log-directory>".into(),
            );
        }
        packaging::verify_signed(
            std::path::Path::new(&arguments[1]),
            std::path::Path::new(&arguments[2]),
        )?;
        println!(
            "Platform signature verification passed. Installation and authenticated application-update acceptance remain not run."
        );
        return Ok(());
    }
    let paths: Vec<PathBuf> = env::args_os().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        return Err("usage: dacs-release-audit <artifact> [artifact ...]".into());
    }
    let artifacts: Vec<Artifact> = paths.into_iter().map(inspect).collect::<Result<_, _>>()?;
    serde_json::to_writer_pretty(io::stdout().lock(), &artifacts)?;
    println!();
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("release audit failed: {error}");
            ExitCode::FAILURE
        }
    }
}
