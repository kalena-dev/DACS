//! Cargo RUSTC_WRAPPER for the DACS pinned sccache proof profile.
//! Records actual input/output digests around the real compiler command.
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::Instant,
};
fn hash(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().enumerate().find_map(|(i, a)| {
        if a == name {
            args.get(i + 1).cloned()
        } else {
            a.strip_prefix(&format!("{name}=")).map(str::to_owned)
        }
    })
}
fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        return Err("Cargo compiler argument missing".into());
    }
    let engine = env::var_os("DACS_SCCACHE").ok_or("DACS_SCCACHE missing")?;
    let log = env::var_os("DACS_CONSUMPTION_LOG").ok_or("DACS_CONSUMPTION_LOG missing")?;
    let compiler_args = &args[1..];
    let mut inputs = Vec::new();
    for (i, arg) in compiler_args.iter().enumerate() {
        let external = if arg == "--extern" {
            compiler_args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--extern=")
        };
        if let Some((_, path)) = external.and_then(|v| v.split_once('=')) {
            let path = PathBuf::from(path);
            if path.is_file() {
                inputs.push(json!({"path":path,"sha256":hash(&path)?}));
            }
        }
    }
    let started = Instant::now();
    let status = Command::new(engine).args(&args).status()?;
    let mut outputs = Vec::new();
    if status.success() {
        if let (Some(out), Some(name)) = (
            flag(compiler_args, "--out-dir"),
            flag(compiler_args, "--crate-name"),
        ) {
            let extra = compiler_args
                .iter()
                .enumerate()
                .find_map(|(i, value)| {
                    if value == "-C" {
                        compiler_args
                            .get(i + 1)
                            .and_then(|v| v.strip_prefix("extra-filename="))
                    } else {
                        value.strip_prefix("-Cextra-filename=")
                    }
                })
                .unwrap_or("");
            for filename in [
                format!("lib{name}{extra}.rlib"),
                format!("{name}{extra}"),
                format!("{name}{extra}.exe"),
            ] {
                let path = Path::new(&out).join(filename);
                if path.is_file() {
                    outputs.push(json!({"path":path,"sha256":hash(&path)?}));
                }
            }
        }
    }
    let record = json!({"schema":1,"kind":"consume","pid":std::process::id(),"inputs":inputs,"outputs":outputs,"exit_code":status.code(),"elapsed_ms":started.elapsed().as_millis()});
    // A single write under an OS file lock prevents concurrent Cargo rustc jobs
    // from interleaving records; the proof fails if telemetry cannot be persisted.
    let mut file = OpenOptions::new().create(true).append(true).open(log)?;
    fs2::FileExt::lock_exclusive(&file)?;
    let mut bytes = serde_json::to_vec(&record)?;
    bytes.push(b'\n');
    if bytes.len() > 65536
        || file.metadata()?.len().saturating_add(bytes.len() as u64) > 16 * 1024 * 1024
    {
        return Err("compiler consumption journal bound exceeded".into());
    }
    file.write_all(&bytes)?;
    file.sync_data()?;
    Ok(status.code().unwrap_or(1))
}
fn main() -> ExitCode {
    match run() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("DACS compiler evidence failure: {error}");
            ExitCode::FAILURE
        }
    }
}
