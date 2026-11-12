// The commit this DLL was built from, written to dialog.log (which DLL an app loaded).
// Git runs with --no-optional-locks so `git status` doesn't rewrite .git/index; HEAD and the
// branch ref are watched instead of the index (watching the index rebuilt this on every build).
fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("--no-optional-locks")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let hash = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=SMOOTHYFILES_COMMIT={hash}{}", if dirty { "+" } else { "" });
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }
    // only files that exist: Cargo counts a missing one as changed on every build
    if let Some(common) = git(&["rev-parse", "--git-common-dir"]) {
        let branch = git(&["symbolic-ref", "-q", "HEAD"]);
        for f in std::iter::once("packed-refs".to_string()).chain(branch) {
            let path = format!("{common}/{f}");
            if std::path::Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}
