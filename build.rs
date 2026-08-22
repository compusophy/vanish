use std::process::Command;

fn main() {
    // stamp the binary with the commit it was built from so the running ui
    // can detect that a newer build shipped without asking a server.
    //
    // order matters: a ci build has no .git directory to interrogate, so the
    // platform-provided sha is checked first there. without this fallback the
    // build id would be "dev" in production and ota updates would never fire.
    let sha = env_sha()
        .or_else(git_sha)
        .unwrap_or_else(|| "dev".to_string());

    println!("cargo:rustc-env=VANISH_BUILD={sha}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=VERCEL_GIT_COMMIT_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}

fn short(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() < 7 {
        return None;
    }
    Some(t.chars().take(7).collect())
}

fn env_sha() -> Option<String> {
    ["VERCEL_GIT_COMMIT_SHA", "GITHUB_SHA", "VANISH_BUILD"]
        .iter()
        .find_map(|k| std::env::var(k).ok().as_deref().and_then(short))
}

fn git_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    short(&String::from_utf8_lossy(&out.stdout))
}
