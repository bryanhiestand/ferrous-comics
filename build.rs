fn main() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_HASH={hash}");

    // Rerun when HEAD changes (branch switch, detached HEAD commit, etc.)
    println!("cargo:rerun-if-changed=.git/HEAD");

    // Rerun when the branch ref file changes (i.e. a new commit is made).
    // HEAD contains "ref: refs/heads/<branch>" for a normal branch, or a bare
    // hash for a detached HEAD. In the latter case .git/HEAD itself changes on
    // each commit so no additional path is needed.
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(refname) = head.strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{}", refname.trim());
    }

    // Rerun when refs are repacked (git gc, fetch with pack, etc.)
    println!("cargo:rerun-if-changed=.git/packed-refs");
}
