//! Read-only host inventory. Compile directly with rustc; no shared workspace changes.
use std::process::Command;

fn probe(program: &str, arguments: &[&str]) {
    println!("\n--- {program} {} ---", arguments.join(" "));
    match Command::new(program).args(arguments).output() {
        Ok(output) => {
            println!("exit={}", output.status);
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        Err(error) => println!("unavailable={error}"),
    }
}

fn main() {
    println!("DACS build host preflight");
    println!(
        "os={} architecture={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("Physical host identity and private reachability require fleet verification.");
    println!("Tool presence alone does not certify a distributed build or a signed release.");
    for (program, arguments) in [
        ("rustc", &["-Vv"][..]),
        ("cargo", &["--version"][..]),
        ("rustup", &["show", "active-toolchain"][..]),
        ("rustup", &["target", "list", "--installed"][..]),
        ("git", &["--version"][..]),
        ("sccache", &["--version"][..]),
        ("sccache-dist", &["--version"][..]),
        ("tailscale", &["version"][..]),
        ("omp", &["--version"][..]),
    ] {
        probe(program, arguments);
    }
    if cfg!(target_os = "linux") {
        probe("uname", &["-r"]);
        probe("bwrap", &["--version"]);
        probe("cc", &["--version"]);
        probe("ld", &["--version"]);
    }
    if cfg!(target_os = "macos") {
        probe("xcode-select", &["-p"]);
        probe("xcrun", &["--show-sdk-version"]);
        probe("clang", &["--version"]);
    }
    println!(
        "\nRemote execution, pool membership, signing and package lifecycle remain unverified."
    );
}
