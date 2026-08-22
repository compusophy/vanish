use std::process::Command;

fn main() {
    // stamp the binary with the commit it was built from so the running ui
    // can detect that a newer build shipped.
    //
    // order matters: a ci build has no .git directory to interrogate, so the
    // platform-provided sha is checked first there. without this fallback the
    // build id would be "dev" in production and ota updates would never fire.
    let sha = env_sha()
        .or_else(git_sha)
        .unwrap_or_else(|| "dev".to_string());
    let subject = env_subject()
        .or_else(git_subject)
        .unwrap_or_else(|| "no commit message".to_string());

    println!("cargo:rustc-env=VANISH_BUILD={sha}");

    // the same identity, written where the *browser* can read it.
    //
    // the ui used to detect updates by asking github for the branch head.
    // that is the wrong question: the branch moving does not mean a new build
    // shipped. when a build failed, head advanced, production stayed put, and
    // the mismatch was permanent — so the ui reloaded itself every poll,
    // forever. asking the server what it is serving cannot produce that loop,
    // and needs no github token.
    write_manifest(&sha, &subject);

    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=VERCEL_GIT_COMMIT_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}

fn write_manifest(sha: &str, subject: &str) {
    let manifest = format!(
        "{{\"build\":{},\"message\":{}}}\n",
        json_string(sha),
        json_string(subject)
    );
    // best effort: a read-only checkout should not fail the build. a missing
    // manifest degrades to "no update checks", never to a broken page.
    if let Err(e) = std::fs::write("web/build.json", manifest) {
        println!("cargo:warning=could not write web/build.json: {e}");
    }
}

/// minimal json string escaping — a commit subject can contain quotes and
/// backslashes, and pulling in serde for one string is not worth it.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push(' '),
            '\t' => out.push_str("    "),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
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

fn env_subject() -> Option<String> {
    std::env::var("VERCEL_GIT_COMMIT_MESSAGE")
        .ok()
        .map(|m| m.lines().next().unwrap_or_default().to_string())
        .filter(|m| !m.is_empty())
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

fn git_subject() -> Option<String> {
    let out = Command::new("git")
        .args(["log", "-1", "--pretty=%s"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
