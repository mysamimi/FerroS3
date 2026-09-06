use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("freebsd") {
        // FreeBSD 11.x still needs pthread_setname_np compatibility.
        cc::Build::new()
            .file("src/freebsd11_shim.c")
            .compile("freebsd11_shim");
    }

    stamp_version();
}

/// Bake the revision and build time into the binary so a running server can say which
/// build it is. Deploys go out as a bare binary over rsync, with no package version to
/// read back, so this stamp is the only way to tell a stale server from a current one —
/// which is exactly the question that cost us an afternoon when a July binary was still
/// running in September.
fn stamp_version() {
    println!("cargo:rerun-if-env-changed=FERROS3_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=FERROS3_GIT_DESCRIBE");
    // Re-stamp when the checked-out revision moves. HEAD covers checkouts and refs/
    // covers new commits on the current branch.
    for path in [".git/HEAD", ".git/refs"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let commit = git_or_env("FERROS3_GIT_COMMIT", &["rev-parse", "--short=12", "HEAD"]);
    let describe = git_or_env(
        "FERROS3_GIT_DESCRIBE",
        &["describe", "--tags", "--always", "--dirty"],
    );

    // Emitted as an epoch and formatted at runtime, so the build script needs no date
    // library of its own — chrono is already linked into the binary.
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!("cargo:rustc-env=FERROS3_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=FERROS3_GIT_DESCRIBE={describe}");
    println!("cargo:rustc-env=FERROS3_BUILD_EPOCH={epoch}");
}

/// Read a version field from the environment, falling back to `git`, falling back to
/// "unknown". Never fails the build: release builds run inside containers that bind-mount
/// the checkout, where `git` may be missing entirely or may reject the repository as
/// "dubious ownership" because it belongs to another uid. An unstamped binary is a much
/// smaller problem than a build that will not start.
fn git_or_env(var: &str, args: &[&str]) -> String {
    if let Ok(value) = std::env::var(var) {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }

    let output = Command::new("git")
        .args(["-c", "safe.directory=*"])
        .args(args)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if text.is_empty() {
                "unknown".to_string()
            } else {
                text
            }
        }
        _ => "unknown".to_string(),
    }
}
