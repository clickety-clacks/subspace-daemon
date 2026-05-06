use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SUBSPACE_BUILD_COMMIT_OVERRIDE");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let commit = std::env::var("SUBSPACE_BUILD_COMMIT_OVERRIDE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_commit)
        .unwrap_or_else(|| "unknown".to_string());
    let timestamp = command_output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .unwrap_or_else(|| "unknown".to_string());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());

    println!("cargo:rustc-env=SUBSPACE_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=SUBSPACE_BUILD_TIMESTAMP={timestamp}");
    println!("cargo:rustc-env=SUBSPACE_BUILD_TARGET={target}");
    println!("cargo:rustc-env=SUBSPACE_BUILD_PROFILE={profile}");
}

fn git_commit() -> Option<String> {
    command_output("git", &["rev-parse", "--short=12", "HEAD"])
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
