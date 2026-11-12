//! Git integration for the details view (per-file state column / colours) and the status bar
//! (branch, ahead/behind). Shells out to the user's `git` with `status --porcelain=v2 -z`
//! instead of linking libgit2: no extra binary size, and identical results to the command line
//! (ignore rules, submodules, sparse checkout, ...). Blocking: call off the UI thread.

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileState {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
    Conflicted,
}

impl FileState {
    /// Priority when folding several descendants into one folder state.
    fn rank(self) -> u8 {
        match self {
            FileState::Conflicted => 4,
            FileState::Modified | FileState::Added | FileState::Deleted | FileState::Renamed => 3,
            FileState::Untracked => 2,
            FileState::Ignored => 1,
        }
    }

    /// Short Turkish label for tooltips / a state column.
    pub fn label(self) -> &'static str {
        match self {
            FileState::Modified => "Değiştirildi",
            FileState::Added => "Eklendi",
            FileState::Deleted => "Silindi",
            FileState::Renamed => "Yeniden adlandırıldı",
            FileState::Untracked => "İzlenmiyor",
            FileState::Ignored => "Yoksayıldı",
            FileState::Conflicted => "Çakışma",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    /// Repository root the paths below are relative to.
    pub root: PathBuf,
    /// Current branch, or `HEAD (abc1234)` when detached.
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    /// Repo-relative path -> state. Untracked / ignored folders appear as one entry for the
    /// whole folder (git collapses them), not per file.
    pub files: HashMap<PathBuf, FileState>,
    /// Derived: repo-relative folder -> aggregated state of tracked-or-untracked descendants
    /// (ignored descendants do not propagate). Built by [`GitStatus::reindex`].
    pub dirs: HashMap<PathBuf, FileState>,
}

impl GitStatus {
    /// Rebuilds [`dirs`](Self::dirs) from [`files`](Self::files). Call after editing `files`.
    pub fn reindex(&mut self) {
        let mut dirs: HashMap<PathBuf, FileState> = HashMap::new();
        for (p, &st) in &self.files {
            if st == FileState::Ignored {
                continue;
            }
            // Any change below a folder shows the folder as Modified; Untracked / Conflicted stay.
            let folded = match st {
                FileState::Conflicted => FileState::Conflicted,
                FileState::Untracked => FileState::Untracked,
                _ => FileState::Modified,
            };
            for a in p.ancestors().skip(1) {
                if a.as_os_str().is_empty() {
                    break;
                }
                dirs.entry(a.to_path_buf())
                    .and_modify(|cur| {
                        if folded.rank() > cur.rank() || (folded.rank() == cur.rank() && folded == FileState::Modified) {
                            *cur = folded;
                        }
                    })
                    .or_insert(folded);
            }
        }
        self.dirs = dirs;
    }

    /// True when nothing differs from HEAD (ignored files do not count).
    pub fn is_clean(&self) -> bool {
        self.files.values().all(|&s| s == FileState::Ignored)
    }
}

/// Walks up from `dir` looking for a `.git` entry (directory, or file for worktrees and
/// submodules). Pure filesystem check, cheap enough for every navigation.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|a| a.join(".git").exists()).map(Path::to_path_buf)
}

/// Settings in a repository's own `.git/config` that make `git status` run a program: a
/// file monitor, and clean / process filters (status runs them to compare files). A folder
/// someone else made (a downloaded project) could start anything that way just by being
/// browsed; for the status we read they are switched off.
fn neutralize_repo_programs(c: &mut Command, root: &Path) {
    c.args(["-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false"]);
    let config = std::fs::read_to_string(root.join(".git").join("config")).unwrap_or_default();
    for line in config.lines() {
        let l = line.trim();
        // [filter "lfs"]
        let Some(rest) = l.strip_prefix("[filter") else { continue };
        let name = rest.trim().trim_end_matches(']').trim().trim_matches('"');
        if name.is_empty() || name.contains(['\n', '\0']) {
            continue;
        }
        for key in ["clean", "smudge", "process"] {
            c.args(["-c", &format!("filter.{name}.{key}=")]);
        }
        c.args(["-c", &format!("filter.{name}.required=false")]);
    }
}

fn git_cmd(root: &Path) -> Command {
    let mut c = Command::new("git");
    neutralize_repo_programs(&mut c, root);
    c.arg("--no-optional-locks")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    c
}

/// Runs `git status` for the repository at `repo_root` (use [`repo_root`] to find it).
/// Errors when git is not installed or the command fails.
pub fn status(repo_root: &Path) -> Result<GitStatus> {
    let out = git_cmd(repo_root)
        .args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=normal", "--ignored=traditional"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("Git bulunamadı (kurulu değil ya da PATH'te yok)")
            } else {
                anyhow::Error::new(e).context("git çalıştırılamadı")
            }
        })?;
    if !out.status.success() {
        bail!("git status başarısız: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(parse_porcelain_v2(repo_root, &out.stdout))
}

fn map_xy(kind: char, x: char, y: char) -> FileState {
    if kind == '2' {
        FileState::Renamed
    } else if x == 'D' || y == 'D' {
        FileState::Deleted
    } else if x == 'A' || y == 'A' {
        FileState::Added
    } else {
        FileState::Modified
    }
}

/// Parses `git status --porcelain=v2 --branch -z` output. Tolerant: unknown or malformed
/// records are skipped.
pub fn parse_porcelain_v2(root: &Path, raw: &[u8]) -> GitStatus {
    let text = String::from_utf8_lossy(raw);
    let mut st = GitStatus { root: root.to_path_buf(), ..Default::default() };
    let mut oid = String::new();
    let mut recs = text.split('\0');
    while let Some(rec) = recs.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(h) = rec.strip_prefix("# ") {
            if let Some(v) = h.strip_prefix("branch.oid ") {
                oid = v.to_string();
            } else if let Some(v) = h.strip_prefix("branch.head ") {
                st.branch = v.to_string();
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                for tok in v.split_whitespace() {
                    if let Some(n) = tok.strip_prefix('+') {
                        st.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = tok.strip_prefix('-') {
                        st.behind = n.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let mut chars = rec.chars();
        let kind = chars.next().unwrap_or(' ');
        let (state, path) = match kind {
            '1' => {
                // 1 XY sub mH mI mW hH hI path  (path may contain spaces => splitn(9))
                let f: Vec<&str> = rec.splitn(9, ' ').collect();
                if f.len() < 9 {
                    continue;
                }
                let mut xy = f[1].chars();
                (map_xy('1', xy.next().unwrap_or('.'), xy.next().unwrap_or('.')), f[8])
            }
            '2' => {
                // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
                let f: Vec<&str> = rec.splitn(10, ' ').collect();
                let _orig = recs.next(); // consumed: the old path is not a separate record
                if f.len() < 10 {
                    continue;
                }
                (FileState::Renamed, f[9])
            }
            'u' => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let f: Vec<&str> = rec.splitn(11, ' ').collect();
                if f.len() < 11 {
                    continue;
                }
                (FileState::Conflicted, f[10])
            }
            '?' => match rec.get(2..) {
                Some(p) => (FileState::Untracked, p),
                None => continue,
            },
            '!' => match rec.get(2..) {
                Some(p) => (FileState::Ignored, p),
                None => continue,
            },
            _ => continue,
        };
        let p = path.trim_end_matches('/');
        if !p.is_empty() {
            st.files.insert(PathBuf::from(p), state);
        }
    }
    if st.branch == "(detached)" {
        let short: String = oid.chars().take(7).collect();
        st.branch = if short.is_empty() { "HEAD".into() } else { format!("HEAD ({short})") };
    }
    st.reindex();
    st
}

/// `path` relative to `root`, comparing components case-insensitively (Windows).
fn rel_to(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut rc = root.components();
    let mut pc = path.components();
    loop {
        match (rc.next(), pc.next()) {
            (None, rest) => {
                let mut out = PathBuf::new();
                if let Some(c) = rest {
                    out.push(c.as_os_str());
                }
                out.extend(pc.map(|c| c.as_os_str()));
                return Some(out);
            }
            (Some(a), Some(b)) if a.as_os_str().eq_ignore_ascii_case(b.as_os_str()) => {}
            _ => return None,
        }
    }
}

/// State of the entry `name` inside the listed folder `dir` (absolute path inside the repo).
/// Files report their own state. Folders report the aggregate of their descendants (any
/// changed descendant -> `Modified`, conflict wins, then `Untracked`); a folder inside an
/// untracked or ignored folder inherits that state. `None` = clean / not in the repo.
pub fn state_for_child(status: &GitStatus, dir: &Path, name: &str) -> Option<FileState> {
    let child = rel_to(&status.root, dir)?.join(name);
    if let Some(&s) = status.files.get(&child) {
        return Some(s);
    }
    // Inside a collapsed untracked / ignored folder?
    for a in child.ancestors().skip(1) {
        if a.as_os_str().is_empty() {
            break;
        }
        if let Some(&s @ (FileState::Untracked | FileState::Ignored)) = status.files.get(a) {
            return Some(s);
        }
    }
    status.dirs.get(&child).copied()
}

// ---------------------------------------------------------------------------------------------
// Branches, checkout, network operations, last commit. All blocking: call off the UI thread.
// ---------------------------------------------------------------------------------------------

fn spawn_err(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!("Git bulunamadı (kurulu değil ya da PATH'te yok)")
    } else {
        anyhow::Error::new(e).context("git çalıştırılamadı")
    }
}

/// Runs git and returns the raw output (even on a non-zero exit).
fn run_git(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    git_cmd(root).args(args).output().map_err(spawn_err)
}

/// Runs git and returns stdout; on failure bails with `what: <stderr>`.
fn git_text(root: &Path, args: &[&str], what: &str) -> Result<String> {
    let out = run_git(root, args)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { err };
        bail!("{what}: {}", err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A local or remote-tracking branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// `main` for local branches, `origin/main` for remote-tracking ones.
    pub name: String,
    pub is_remote: bool,
    /// The currently checked-out branch.
    pub is_head: bool,
    /// Short name of the upstream (`origin/main`), local branches only.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// Lists local branches (HEAD first, then by name) followed by remote-tracking branches
/// (by name). Symbolic `origin/HEAD` refs are skipped.
pub fn branches(root: &Path) -> Result<Vec<Branch>> {
    let text = git_text(
        root,
        &[
            "for-each-ref",
            "--format=%(HEAD)%00%(refname)%00%(upstream:short)%00%(upstream:track,nobracket)",
            "refs/heads",
            "refs/remotes",
        ],
        "Dallar listelenemedi",
    )?;
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\0').collect();
        if f.len() < 4 {
            continue;
        }
        let (is_head, refname, upstream, track) = (f[0].trim() == "*", f[1], f[2], f[3]);
        if let Some(n) = refname.strip_prefix("refs/heads/") {
            let (mut ahead, mut behind) = (0, 0);
            for part in track.split(',') {
                let part = part.trim();
                if let Some(v) = part.strip_prefix("ahead ") {
                    ahead = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = part.strip_prefix("behind ") {
                    behind = v.trim().parse().unwrap_or(0);
                }
            }
            local.push(Branch {
                name: n.to_string(),
                is_remote: false,
                is_head,
                upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
                ahead,
                behind,
            });
        } else if let Some(n) = refname.strip_prefix("refs/remotes/") {
            if n.ends_with("/HEAD") || !n.contains('/') {
                continue;
            }
            remote.push(Branch { name: n.to_string(), is_remote: true, is_head: false, upstream: None, ahead: 0, behind: 0 });
        }
    }
    local.sort_by(|a, b| b.is_head.cmp(&a.is_head).then_with(|| a.name.cmp(&b.name)));
    remote.sort_by(|a, b| a.name.cmp(&b.name));
    local.extend(remote);
    Ok(local)
}

/// What to do with uncommitted changes when switching branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutMode {
    /// Plain switch; fails when local changes would be overwritten.
    Normal,
    /// Stash (including untracked files), switch, then pop the stash.
    Stash,
    /// `--force`: throw away conflicting local changes.
    Discard,
}

enum Target {
    Local(String),
    /// Remote-tracking ref (`origin/foo`) without a local branch yet: create and track.
    Track(String),
}

fn ref_exists(root: &Path, full_ref: &str) -> bool {
    run_git(root, &["show-ref", "--verify", "--quiet", full_ref]).map(|o| o.status.success()).unwrap_or(false)
}

fn resolve_target(root: &Path, branch: &str) -> Result<Target> {
    if branch.starts_with('-') || branch.is_empty() {
        bail!("Geçersiz dal adı: {branch}");
    }
    if ref_exists(root, &format!("refs/heads/{branch}")) {
        return Ok(Target::Local(branch.to_string()));
    }
    if ref_exists(root, &format!("refs/remotes/{branch}")) {
        let local = branch.split_once('/').map(|(_, n)| n).unwrap_or(branch);
        if ref_exists(root, &format!("refs/heads/{local}")) {
            return Ok(Target::Local(local.to_string()));
        }
        return Ok(Target::Track(branch.to_string()));
    }
    bail!("Dal bulunamadı: {branch}")
}

fn switch_to(root: &Path, target: &Target, force: bool) -> Result<()> {
    let mut args: Vec<&str> = vec!["switch"];
    if force {
        args.push("--force");
    }
    match target {
        Target::Local(n) => args.push(n),
        Target::Track(r) => {
            args.push("--track");
            args.push(r);
        }
    }
    let out = run_git(root, &args)?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    if err.contains("overwritten") || err.contains("would be lost") {
        bail!("Dal değiştirilemedi: yerel değişiklikler çakışıyor. Değişiklikleri saklayın ya da atın.\n{err}");
    }
    bail!("Dal değiştirilemedi: {err}")
}

/// Switches to `branch`. A remote-tracking name (`origin/foo`) creates and tracks local `foo`
/// (or just switches to it when it already exists).
pub fn checkout(root: &Path, branch: &str, mode: CheckoutMode) -> Result<()> {
    let target = resolve_target(root, branch)?;
    match mode {
        CheckoutMode::Normal => switch_to(root, &target, false),
        CheckoutMode::Discard => switch_to(root, &target, true),
        CheckoutMode::Stash => {
            let dirty = !git_text(root, &["status", "--porcelain"], "git status başarısız")?.trim().is_empty();
            if dirty {
                git_text(
                    root,
                    &["stash", "push", "--include-untracked", "-m", "smoothyfiles checkout"],
                    "Değişiklikler stash'e alınamadı",
                )?;
            }
            if let Err(e) = switch_to(root, &target, false) {
                if dirty {
                    let _ = run_git(root, &["stash", "pop"]);
                }
                return Err(e);
            }
            if dirty {
                git_text(
                    root,
                    &["stash", "pop"],
                    "Dal değiştirildi ama saklanan değişiklikler geri uygulanamadı (git stash listesinde duruyor)",
                )?;
            }
            Ok(())
        }
    }
}

/// Local check mirroring `git check-ref-format --branch` (without spawning git).
pub fn is_valid_branch_name(name: &str) -> bool {
    if name.is_empty() || name == "@" || name.starts_with('-') || name.starts_with('/') {
        return false;
    }
    if name.ends_with('/') || name.ends_with(".lock") || name.ends_with('.') {
        return false;
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return false;
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)) {
        return false;
    }
    name.split('/').all(|c| !c.starts_with('.') && !c.ends_with(".lock"))
}

/// Creates branch `name` (from `from`, default HEAD); optionally switches to it.
pub fn create_branch(root: &Path, name: &str, from: Option<&str>, checkout: bool) -> Result<()> {
    if !is_valid_branch_name(name) {
        bail!("Geçersiz dal adı: {name}");
    }
    if let Some(f) = from
        && (f.is_empty() || f.starts_with('-'))
    {
        bail!("Geçersiz başlangıç noktası: {f}");
    }
    let mut args: Vec<&str> = if checkout { vec!["switch", "-c", name] } else { vec!["branch", name] };
    if let Some(f) = from {
        args.push(f);
    }
    git_text(root, &args, "Dal oluşturulamadı").map(|_| ())
}

/// Deletes a local branch (`force` = `-D`, allows unmerged branches).
pub fn delete_branch(root: &Path, name: &str, force: bool) -> Result<()> {
    if name.is_empty() || name.starts_with('-') {
        bail!("Geçersiz dal adı: {name}");
    }
    let flag = if force { "-D" } else { "-d" };
    git_text(root, &["branch", flag, name], "Dal silinemedi").map(|_| ())
}

fn flush_line(cur: &mut Vec<u8>, kept: &mut Vec<String>, cb: &mut dyn FnMut(&str)) {
    if cur.is_empty() {
        return;
    }
    let line = String::from_utf8_lossy(cur).trim().to_string();
    cur.clear();
    if line.is_empty() {
        return;
    }
    cb(&line);
    if !is_progress_line(&line) {
        kept.push(line);
    }
}

fn is_progress_line(line: &str) -> bool {
    let l = line.strip_prefix("remote: ").unwrap_or(line);
    l.contains('%')
        && ["Receiving objects", "Resolving deltas", "Counting objects", "Compressing objects", "Writing objects", "Enumerating objects", "Updating files"]
            .iter()
            .any(|p| l.starts_with(p))
}

/// Runs `cmd`, feeding every stderr line (split on `\r` and `\n`, so progress updates arrive
/// live) to `on_progress`. On failure the error carries the last meaningful output lines.
fn run_streaming(mut cmd: Command, what: &str, on_progress: &mut dyn FnMut(&str)) -> Result<()> {
    use std::io::Read;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(spawn_err)?;
    let mut stdout = child.stdout.take().expect("stdout piped");
    let out_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let mut stderr = child.stderr.take().expect("stderr piped");
    let mut kept: Vec<String> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\r' || b == b'\n' {
                        flush_line(&mut cur, &mut kept, on_progress);
                    } else {
                        cur.push(b);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    flush_line(&mut cur, &mut kept, on_progress);
    let status = child.wait().map_err(spawn_err)?;
    let out_bytes = out_thread.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let tail = |lines: Vec<String>| -> String {
        let skip = lines.len().saturating_sub(6);
        lines[skip..].join("\n")
    };
    let mut msg = tail(kept);
    if msg.is_empty() {
        let stdout_lines: Vec<String> =
            String::from_utf8_lossy(&out_bytes).lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
        msg = tail(stdout_lines);
    }
    if msg.is_empty() {
        msg = format!("çıkış kodu {}", status.code().unwrap_or(-1));
    }
    bail!("{what} başarısız: {msg}")
}

fn remotes(root: &Path) -> Result<Vec<String>> {
    let t = git_text(root, &["remote"], "Uzak depolar okunamadı")?;
    Ok(t.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
}

fn require_remote(root: &Path) -> Result<Vec<String>> {
    let r = remotes(root)?;
    if r.is_empty() {
        bail!("Uzak depo tanımlı değil");
    }
    Ok(r)
}

fn current_branch(root: &Path) -> Option<String> {
    let out = run_git(root, &["symbolic-ref", "--short", "-q", "HEAD"]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn upstream_of(root: &Path, branch: &str) -> Option<String> {
    let out = run_git(root, &["for-each-ref", "--format=%(upstream:short)", &format!("refs/heads/{branch}")]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `git fetch --all --prune` with live progress lines.
pub fn fetch(root: &Path, on_progress: &mut dyn FnMut(&str)) -> Result<()> {
    require_remote(root)?;
    let mut c = git_cmd(root);
    c.args(["fetch", "--all", "--prune", "--progress"]);
    run_streaming(c, "git fetch", on_progress)
}

/// `git pull --no-edit` (merge strategy unless the user configured `pull.rebase`).
pub fn pull(root: &Path, on_progress: &mut dyn FnMut(&str)) -> Result<()> {
    require_remote(root)?;
    let Some(branch) = current_branch(root) else {
        bail!("Ayrık HEAD durumunda çekme yapılamaz");
    };
    if upstream_of(root, &branch).is_none() {
        bail!("'{branch}' dalının izlediği bir uzak dal yok");
    }
    let mut c = git_cmd(root);
    let has_rebase_cfg = run_git(root, &["config", "--get", "pull.rebase"]).map(|o| o.status.success()).unwrap_or(false);
    if !has_rebase_cfg {
        c.args(["-c", "pull.rebase=false"]);
    }
    c.args(["pull", "--no-edit", "--progress"]);
    run_streaming(c, "git pull", on_progress)
}

/// `git push`; a branch without upstream is pushed with `-u <remote> <branch>`
/// (`origin` when present, otherwise the first remote).
pub fn push(root: &Path, on_progress: &mut dyn FnMut(&str)) -> Result<()> {
    let remotes = require_remote(root)?;
    let Some(branch) = current_branch(root) else {
        bail!("Ayrık HEAD durumunda gönderilemez");
    };
    let mut c = git_cmd(root);
    c.args(["push", "--progress"]);
    if upstream_of(root, &branch).is_none() {
        let remote = remotes.iter().find(|r| *r == "origin").unwrap_or(&remotes[0]);
        c.args(["-u", remote, &branch]);
    }
    run_streaming(c, "git push", on_progress)
}

/// Pull (when the branch has an upstream) then push.
pub fn sync(root: &Path, on_progress: &mut dyn FnMut(&str)) -> Result<()> {
    require_remote(root)?;
    if let Some(b) = current_branch(root)
        && upstream_of(root, &b).is_some()
    {
        pull(root, on_progress)?;
    }
    push(root, on_progress)
}

/// URL of `origin` (or the first remote); `None` when there is no remote.
pub fn remote_url(root: &Path) -> Option<String> {
    let r = remotes(root).ok()?;
    let name = r.iter().find(|n| *n == "origin").or_else(|| r.first())?;
    let out = run_git(root, &["remote", "get-url", name]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Points `origin` at `url` (adds it, or changes its address).
pub fn connect_remote(root: &Path, url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        bail!("Depo adresi boş");
    }
    let verb = if remotes(root)?.iter().any(|r| r == "origin") { "set-url" } else { "add" };
    git_text(root, &["remote", verb, "origin", url], "Uzak depo bağlanamadı").map(|_| ())
}

/// Whether there is anything to commit (changed, added, deleted or untracked files).
pub fn has_changes(root: &Path) -> bool {
    git_text(root, &["status", "--porcelain"], "").is_ok_and(|t| !t.trim().is_empty())
}

/// Stages everything and commits it with `message`.
pub fn commit_all(root: &Path, message: &str) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        bail!("Kayıt açıklaması boş");
    }
    if !has_changes(root) {
        bail!("Kaydedilecek değişiklik yok");
    }
    git_text(root, &["add", "-A"], "Değişiklikler eklenemedi")?;
    git_text(root, &["commit", "-q", "-m", message], "Kaydedilemedi").map(|_| ())
}

/// `git init` in `dir` (created when missing).
pub fn init(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| anyhow::Error::new(e).context("Klasör oluşturulamadı"))?;
    git_text(dir, &["init", "-q"], "Depo başlatılamadı").map(|_| ())
}

/// Clones `url` into a new folder under `dest_parent` (named after the repository) and returns
/// that folder.
pub fn clone(url: &str, dest_parent: &Path, on_progress: &mut dyn FnMut(&str)) -> Result<PathBuf> {
    let url = url.trim();
    if url.is_empty() {
        bail!("Depo adresi boş");
    }
    let last = url.trim_end_matches(['/', '\\']).rsplit(['/', '\\', ':']).next().unwrap_or("");
    let name = last.strip_suffix(".git").unwrap_or(last);
    let name = if name.is_empty() { "repo" } else { name };
    std::fs::create_dir_all(dest_parent).map_err(|e| anyhow::Error::new(e).context("Hedef klasör oluşturulamadı"))?;
    let mut c = git_cmd(dest_parent);
    c.args(["clone", "--progress", "--", url, name]);
    run_streaming(c, "git clone", on_progress)?;
    Ok(dest_parent.join(name))
}

/// Latest commit touching a path (or the whole repo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Abbreviated hash.
    pub sha: String,
    /// Subject line.
    pub message: String,
    pub author: String,
    /// Commit time, milliseconds since the Unix epoch.
    pub time_ms: i64,
}

/// `git log -1` for `path` (or HEAD when `None`). `None` when there is no such commit.
pub fn last_commit(root: &Path, path: Option<&Path>) -> Option<Commit> {
    let mut c = git_cmd(root);
    c.args(["log", "-1", "--format=%h%x00%s%x00%an%x00%ct"]);
    if let Some(p) = path {
        c.arg("--").arg(p);
    }
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let f: Vec<&str> = text.trim_end_matches(['\r', '\n']).split('\0').collect();
    if f.len() < 4 || f[0].is_empty() {
        return None;
    }
    Some(Commit { sha: f[0].into(), message: f[1].into(), author: f[2].into(), time_ms: f[3].trim().parse::<i64>().ok()?.saturating_mul(1000) })
}

/// The latest commit touching each of `names` (children of `dir`, a folder inside the
/// repository at `root`), like Files' commit columns. One `git log` streamed newest first and
/// stopped as soon as every name is found (or after `max_commits`): untracked names just stay
/// missing. Keys are the names as given.
pub fn last_commits_of_children(root: &Path, dir: &Path, names: &[String], max_commits: usize) -> HashMap<String, Commit> {
    use std::io::BufRead;
    let mut out = HashMap::new();
    let Some(rel) = rel_inside(root, dir) else { return out };
    let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
    // git reports paths in its own case; Windows names match case-insensitively
    let wanted: HashMap<String, &String> = names.iter().map(|n| (n.to_lowercase(), n)).collect();
    if wanted.is_empty() {
        return out;
    }
    let mut c = git_cmd(root);
    c.args(["-c", "core.quotepath=false", "log", "--no-renames", "--name-only", "--format=%x01%h%x1f%s%x1f%an%x1f%ct", "--"])
        .arg(if rel.is_empty() { ".".to_string() } else { rel.clone() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // a long history is background work: below-normal priority, no console window
        .creation_flags(CREATE_NO_WINDOW | 0x0000_4000);
    let Ok(mut child) = c.spawn() else { return out };
    let Some(stdout) = child.stdout.take() else { return out };
    let mut current: Option<Commit> = None;
    let mut commits = 0usize;
    for line in std::io::BufReader::new(stdout).split(b'\n') {
        let Ok(line) = line else { break };
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end_matches('\r');
        if let Some(h) = line.strip_prefix('\u{1}') {
            commits += 1;
            if commits > max_commits {
                break;
            }
            let f: Vec<&str> = h.split('\u{1f}').collect();
            current = (f.len() >= 4).then(|| Commit {
                sha: f[0].into(),
                message: f[1].into(),
                author: f[2].into(),
                time_ms: f[3].trim().parse::<i64>().unwrap_or(0).saturating_mul(1000),
            });
            continue;
        }
        let (Some(cm), Some(rest)) = (&current, line.strip_prefix(prefix.as_str())) else { continue };
        let first = rest.split('/').next().unwrap_or("");
        if first.is_empty() {
            continue;
        }
        if let Some(name) = wanted.get(&first.to_lowercase()) {
            out.entry((*name).clone()).or_insert_with(|| cm.clone());
            if out.len() == wanted.len() {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    out
}

/// `dir` relative to `root` with `/` separators ("" for the root itself); `None` outside it.
/// The root prefix matches case-insensitively (drive letters, Explorer-typed paths).
fn rel_inside(root: &Path, dir: &Path) -> Option<String> {
    let r = root.to_string_lossy().replace('\\', "/");
    let d = dir.to_string_lossy().replace('\\', "/");
    let (r, d) = (r.trim_end_matches('/'), d.trim_end_matches('/'));
    if d.len() < r.len() || !d.is_char_boundary(r.len()) || !d[..r.len()].eq_ignore_ascii_case(r) {
        return None;
    }
    let rest = &d[r.len()..];
    if rest.is_empty() {
        return Some(String::new());
    }
    rest.strip_prefix('/').map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository whose own config runs programs (a clean filter, a file monitor): reading its
    /// status must not start them. Skipped without git.
    #[test]
    fn status_does_not_run_the_repos_programs() {
        let git = |dir: &Path, args: &[&str]| Command::new("git").args(args).current_dir(dir).creation_flags(CREATE_NO_WINDOW).output();
        let dir = std::env::temp_dir().join(format!("rf-git-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if git(&dir, &["init", "-q", "."]).is_err() {
            return;
        }
        let _ = git(&dir, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "x"]);
        std::fs::write(dir.join("f.txt"), "a").unwrap();
        let _ = git(&dir, &["add", "f.txt"]);
        let _ = git(&dir, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "y"]);
        let (m1, m2) = (dir.join("marker1"), dir.join("marker2"));
        let _ = git(&dir, &["config", "filter.evil.clean", &format!("cmd /c echo x> \"{}\" & more", m1.display())]);
        let _ = git(&dir, &["config", "core.fsmonitor", &format!("cmd /c echo x> \"{}\"", m2.display())]);
        std::fs::write(dir.join(".gitattributes"), "* filter=evil").unwrap();
        std::fs::write(dir.join("f.txt"), "b").unwrap();
        let st = status(&dir);
        assert!(st.is_ok(), "{st:?}");
        assert!(!m1.exists() && !m2.exists(), "a program from the repository's config ran");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_synthetic_output() {
        let raw = b"# branch.oid 0123456789abcdef\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaaa bbbb src/a file.rs\0\
1 A. N... 000000 100644 100644 0000 cccc new.txt\0\
1 .D N... 100644 100644 000000 aaaa aaaa gone.txt\0\
2 R. N... 100644 100644 100644 aaaa bbbb R100 dir/new name.txt\0dir/old name.txt\0\
u UU N... 100644 100644 100644 100644 a b c conflict.txt\0\
? untracked dir/\0\
! target/\0";
        let st = parse_porcelain_v2(Path::new(r"C:\repo"), raw);
        assert_eq!((st.branch.as_str(), st.ahead, st.behind), ("main", 2, 1));
        assert_eq!(st.files[Path::new("src/a file.rs")], FileState::Modified);
        assert_eq!(st.files[Path::new("new.txt")], FileState::Added);
        assert_eq!(st.files[Path::new("gone.txt")], FileState::Deleted);
        assert_eq!(st.files[Path::new("dir/new name.txt")], FileState::Renamed);
        assert!(!st.files.contains_key(Path::new("dir/old name.txt")));
        assert_eq!(st.files[Path::new("conflict.txt")], FileState::Conflicted);
        assert_eq!(st.files[Path::new("untracked dir")], FileState::Untracked);
        assert_eq!(st.files[Path::new("target")], FileState::Ignored);
        assert!(!st.is_clean());

        let root = Path::new(r"C:\repo");
        assert_eq!(state_for_child(&st, root, "src"), Some(FileState::Modified));
        // Only the repo-root prefix is matched case-insensitively; names inside use git's case.
        assert_eq!(state_for_child(&st, &Path::new(r"c:\REPO").join("src"), "a file.rs"), Some(FileState::Modified));
        assert_eq!(state_for_child(&st, &root.join("untracked dir"), "deep.txt"), Some(FileState::Untracked));
        assert_eq!(state_for_child(&st, root, "target"), Some(FileState::Ignored));
        assert_eq!(state_for_child(&st, root, "README.md"), None);
        assert_eq!(state_for_child(&st, Path::new(r"D:\other"), "src"), None);
    }

    #[test]
    fn detached_and_empty() {
        let st = parse_porcelain_v2(Path::new("."), b"# branch.oid abcdef0123\0# branch.head (detached)\0");
        assert_eq!(st.branch, "HEAD (abcdef0)");
        assert!(st.is_clean());
    }

    #[test]
    fn finds_root() {
        let base = std::env::temp_dir().join(format!("rf-git-root-{}", std::process::id()));
        std::fs::create_dir_all(base.join("r/.git")).unwrap();
        std::fs::create_dir_all(base.join("r/x/y")).unwrap();
        assert_eq!(repo_root(&base.join("r/x/y")), Some(base.join("r")));
        assert_eq!(repo_root(&base.join("r")), Some(base.join("r")));
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn real_repo() {
        let tmp = std::env::temp_dir().join(format!("rf-git-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let remote = tmp.join("remote.git");
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if !git(&tmp, &["--version"]) {
            eprintln!("git yok: test atlandı");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        assert!(git(&tmp, &["init", "-q", "--bare", "remote.git"]));
        assert!(git(&repo, &["init", "-q"]));
        assert!(git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]));
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::create_dir_all(repo.join("deep/er")).unwrap();
        for (f, c) in [
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("sub/c.txt", "c\n"),
            ("deep/er/d.txt", "d\n"),
            (".gitignore", "*.log\n"),
        ] {
            std::fs::write(repo.join(f), c).unwrap();
        }
        assert!(git(&repo, &["add", "-A"]));
        assert!(git(&repo, &["commit", "-q", "-m", "ilk"]));
        assert!(git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]));
        assert!(git(&repo, &["push", "-q", "-u", "origin", "main"]));
        assert!(status(&repo).unwrap().is_clean());

        // ahead by one
        std::fs::write(repo.join("a.txt"), "a2\n").unwrap();
        assert!(git(&repo, &["commit", "-q", "-am", "ikinci"]));

        // working tree states
        std::fs::write(repo.join("a.txt"), "a3\n").unwrap(); // Modified
        std::fs::write(repo.join("added.txt"), "x\n").unwrap();
        assert!(git(&repo, &["add", "added.txt"])); // Added
        std::fs::remove_file(repo.join("b.txt")).unwrap(); // Deleted
        assert!(git(&repo, &["mv", "sub/c.txt", "sub/c2.txt"])); // Renamed
        std::fs::write(repo.join("untracked.txt"), "u\n").unwrap(); // Untracked
        std::fs::write(repo.join("noise.log"), "l\n").unwrap(); // Ignored
        std::fs::write(repo.join("deep/er/d.txt"), "d2\n").unwrap(); // nested modification

        let st = status(&repo).unwrap();
        assert_eq!(st.branch, "main");
        assert_eq!((st.ahead, st.behind), (1, 0));
        let f = |p: &str| st.files.get(Path::new(p)).copied();
        assert_eq!(f("a.txt"), Some(FileState::Modified));
        assert_eq!(f("added.txt"), Some(FileState::Added));
        assert_eq!(f("b.txt"), Some(FileState::Deleted));
        assert_eq!(f("sub/c2.txt"), Some(FileState::Renamed));
        assert_eq!(f("untracked.txt"), Some(FileState::Untracked));
        assert_eq!(f("noise.log"), Some(FileState::Ignored));
        assert_eq!(state_for_child(&st, &repo, "sub"), Some(FileState::Modified));
        assert_eq!(state_for_child(&st, &repo, "deep"), Some(FileState::Modified));
        assert_eq!(state_for_child(&st, &repo.join("deep"), "er"), Some(FileState::Modified));
        assert_eq!(state_for_child(&st, &repo, ".gitignore"), None);

        // merge conflict
        assert!(git(&repo, &["add", "-A"]));
        assert!(git(&repo, &["commit", "-q", "-m", "ucuncu"]));
        assert!(git(&repo, &["checkout", "-q", "-b", "yan", "HEAD~1"]));
        std::fs::write(repo.join("a.txt"), "yan\n").unwrap();
        assert!(git(&repo, &["commit", "-q", "-am", "yan"]));
        assert!(!git(&repo, &["merge", "main"])); // conflicts => non-zero
        let st = status(&repo).unwrap();
        assert_eq!(st.files.get(Path::new("a.txt")).copied(), Some(FileState::Conflicted));
        assert_eq!(st.branch, "yan");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- branches / checkout / network ----

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// Local repo identity so stash / merge commits work without a global git config.
    fn configure(dir: &Path) {
        assert!(git(dir, &["config", "user.name", "t"]));
        assert!(git(dir, &["config", "user.email", "t@t"]));
        assert!(git(dir, &["config", "commit.gpgsign", "false"]));
        assert!(git(dir, &["config", "core.autocrlf", "false"]));
    }

    struct Fixture {
        tmp: PathBuf,
        remote: PathBuf,
        repo: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    /// bare `remote.git` (origin) + `repo` with two files committed on `main` and pushed.
    fn fixture(name: &str) -> Option<Fixture> {
        let tmp = std::env::temp_dir().join(format!("rf-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        if !git(&tmp, &["--version"]) {
            eprintln!("git yok: test atlandı");
            let _ = std::fs::remove_dir_all(&tmp);
            return None;
        }
        let remote = tmp.join("remote.git");
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git(&tmp, &["init", "-q", "--bare", "remote.git"]));
        assert!(git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]));
        assert!(git(&repo, &["init", "-q"]));
        assert!(git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]));
        configure(&repo);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        assert!(git(&repo, &["add", "-A"]));
        assert!(git(&repo, &["commit", "-q", "-m", "ilk"]));
        assert!(git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]));
        assert!(git(&repo, &["push", "-q", "-u", "origin", "main"]));
        Some(Fixture { tmp, remote, repo })
    }

    /// A second clone of the remote, to simulate another user.
    fn second_clone(fx: &Fixture) -> PathBuf {
        let other = fx.tmp.join("other");
        assert!(git(&fx.tmp, &["clone", "-q", fx.remote.to_str().unwrap(), "other"]));
        configure(&other);
        other
    }

    #[test]
    fn branch_name_validation() {
        let ok = ["main", "feature/x", "fix-1", "a.b", "v1.0.0", "ünicode", "a@b", "x/y/z"];
        let bad = [
            "", " ", "a b", "a..b", "a~b", "a^b", "a:b", "a?b", "a*b", "a[b", "a\\b", "-x", "/x", "x/", "x.lock", "x.", "a@{b", "@",
            "a//b", ".hidden", "a/.b", "a.lock/b", "tab\tx",
        ];
        for n in ok {
            assert!(is_valid_branch_name(n), "{n:?} geçerli olmalı");
        }
        for n in bad {
            assert!(!is_valid_branch_name(n), "{n:?} geçersiz olmalı");
        }
    }

    #[test]
    fn create_delete_and_list_branches() {
        let Some(fx) = fixture("branches") else { return };
        let repo = &fx.repo;
        assert!(create_branch(repo, "bad name", None, false).is_err());
        create_branch(repo, "zeta", None, false).unwrap();
        create_branch(repo, "alpha", Some("main"), true).unwrap();
        assert!(create_branch(repo, "alpha", None, false).is_err()); // exists

        let list = branches(repo).unwrap();
        let names: Vec<(&str, bool, bool)> = list.iter().map(|b| (b.name.as_str(), b.is_head, b.is_remote)).collect();
        // HEAD (alpha) first, then the rest by name, then remotes.
        assert_eq!(names, [("alpha", true, false), ("main", false, false), ("zeta", false, false), ("origin/main", false, true)]);
        let main = list.iter().find(|b| b.name == "main").unwrap();
        assert_eq!(main.upstream.as_deref(), Some("origin/main"));
        assert!(list.iter().all(|b| !b.name.ends_with("HEAD")));

        // ahead of upstream
        checkout(repo, "main", CheckoutMode::Normal).unwrap();
        std::fs::write(repo.join("a.txt"), "a2\n").unwrap();
        assert!(git(repo, &["commit", "-q", "-am", "iki"]));
        let main = branches(repo).unwrap().into_iter().find(|b| b.name == "main").unwrap();
        assert_eq!((main.ahead, main.behind), (1, 0));

        // delete: unmerged needs force
        checkout(repo, "alpha", CheckoutMode::Normal).unwrap();
        assert!(delete_branch(repo, "alpha", false).is_err()); // current branch
        checkout(repo, "main", CheckoutMode::Normal).unwrap();
        delete_branch(repo, "alpha", false).unwrap(); // merged (same commit as main's parent)
        assert!(git(repo, &["switch", "-q", "-c", "unmerged"]));
        std::fs::write(repo.join("u.txt"), "u\n").unwrap();
        assert!(git(repo, &["add", "u.txt"]));
        assert!(git(repo, &["commit", "-q", "-m", "u"]));
        checkout(repo, "main", CheckoutMode::Normal).unwrap();
        assert!(delete_branch(repo, "unmerged", false).is_err());
        delete_branch(repo, "unmerged", true).unwrap();
        assert!(!branches(repo).unwrap().iter().any(|b| b.name == "unmerged"));
    }

    #[test]
    fn checkout_remote_branch_tracks_it() {
        let Some(fx) = fixture("remote-co") else { return };
        let other = second_clone(&fx);
        assert!(git(&other, &["switch", "-q", "-c", "feat"]));
        std::fs::write(other.join("f.txt"), "f\n").unwrap();
        assert!(git(&other, &["add", "f.txt"]));
        assert!(git(&other, &["commit", "-q", "-m", "feat"]));
        assert!(git(&other, &["push", "-q", "origin", "feat"]));

        let mut lines = Vec::new();
        fetch(&fx.repo, &mut |l| lines.push(l.to_string())).unwrap();
        assert!(branches(&fx.repo).unwrap().iter().any(|b| b.is_remote && b.name == "origin/feat"));

        checkout(&fx.repo, "origin/feat", CheckoutMode::Normal).unwrap();
        assert_eq!(status(&fx.repo).unwrap().branch, "feat");
        assert!(fx.repo.join("f.txt").exists());
        let feat = branches(&fx.repo).unwrap().into_iter().find(|b| b.name == "feat").unwrap();
        assert!(feat.is_head);
        assert_eq!(feat.upstream.as_deref(), Some("origin/feat"));

        // Local `feat` exists now: selecting the remote ref again just switches.
        checkout(&fx.repo, "main", CheckoutMode::Normal).unwrap();
        checkout(&fx.repo, "origin/feat", CheckoutMode::Normal).unwrap();
        assert_eq!(status(&fx.repo).unwrap().branch, "feat");

        assert!(checkout(&fx.repo, "nope", CheckoutMode::Normal).is_err());
        assert!(checkout(&fx.repo, "-x", CheckoutMode::Normal).is_err());
    }

    /// `other` branch changes a.txt; main has a dirty a.txt => conflicting switch.
    fn conflicting_setup(repo: &Path) {
        assert!(git(repo, &["switch", "-q", "-c", "other"]));
        std::fs::write(repo.join("a.txt"), "other\n").unwrap();
        assert!(git(repo, &["commit", "-q", "-am", "other"]));
        assert!(git(repo, &["switch", "-q", "main"]));
        std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
    }

    #[test]
    fn checkout_normal_conflict_has_turkish_error() {
        let Some(fx) = fixture("co-conflict") else { return };
        conflicting_setup(&fx.repo);
        let err = checkout(&fx.repo, "other", CheckoutMode::Normal).unwrap_err().to_string();
        assert!(err.contains("çakışıyor"), "{err}");
        assert!(err.contains("a.txt"), "git stderr eklenmeli: {err}");
        assert_eq!(status(&fx.repo).unwrap().branch, "main");
    }

    #[test]
    fn checkout_discard_overwrites() {
        let Some(fx) = fixture("co-discard") else { return };
        conflicting_setup(&fx.repo);
        checkout(&fx.repo, "other", CheckoutMode::Discard).unwrap();
        assert_eq!(std::fs::read_to_string(fx.repo.join("a.txt")).unwrap().trim(), "other");
        assert!(status(&fx.repo).unwrap().is_clean());
    }

    #[test]
    fn checkout_stash_carries_changes() {
        let Some(fx) = fixture("co-stash") else { return };
        let repo = &fx.repo;
        assert!(git(repo, &["branch", "other"]));
        std::fs::write(repo.join("b.txt"), "dirty b\n").unwrap();
        std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();
        checkout(repo, "other", CheckoutMode::Stash).unwrap();
        assert_eq!(status(repo).unwrap().branch, "other");
        assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap().trim(), "dirty b");
        assert_eq!(std::fs::read_to_string(repo.join("new.txt")).unwrap().trim(), "untracked");
        assert_eq!(git_out(repo, &["stash", "list"]), ""); // popped

        // Clean tree: no stash needed.
        assert!(git(repo, &["checkout", "-q", "--", "b.txt"]));
        std::fs::remove_file(repo.join("new.txt")).unwrap();
        checkout(repo, "main", CheckoutMode::Stash).unwrap();
        assert_eq!(status(repo).unwrap().branch, "main");
    }

    #[test]
    fn push_pull_sync_with_bare_origin() {
        let Some(fx) = fixture("net") else { return };
        let repo = &fx.repo;

        // push: new branch without upstream gets -u
        create_branch(repo, "topic", None, true).unwrap();
        std::fs::write(repo.join("t.txt"), "t\n").unwrap();
        assert!(git(repo, &["add", "t.txt"]));
        assert!(git(repo, &["commit", "-q", "-m", "topic"]));
        let mut lines = Vec::new();
        push(repo, &mut |l| lines.push(l.to_string())).unwrap();
        assert!(git(&fx.remote, &["rev-parse", "--verify", "-q", "refs/heads/topic"]));
        let topic = branches(repo).unwrap().into_iter().find(|b| b.name == "topic").unwrap();
        assert_eq!(topic.upstream.as_deref(), Some("origin/topic"));
        assert!(!lines.is_empty(), "ilerleme satırları gelmeli");
        assert!(lines.iter().all(|l| !l.is_empty()));
        checkout(repo, "main", CheckoutMode::Normal).unwrap();

        // pull: fast-forward from another clone
        let other = second_clone(&fx);
        std::fs::write(other.join("o1.txt"), "o1\n").unwrap();
        assert!(git(&other, &["add", "o1.txt"]));
        assert!(git(&other, &["commit", "-q", "-m", "o1"]));
        assert!(git(&other, &["push", "-q", "origin", "main"]));
        pull(repo, &mut |_| {}).unwrap();
        assert!(repo.join("o1.txt").exists());

        // sync: diverged histories => merge, then push
        std::fs::write(other.join("o2.txt"), "o2\n").unwrap();
        assert!(git(&other, &["add", "o2.txt"]));
        assert!(git(&other, &["commit", "-q", "-m", "o2"]));
        assert!(git(&other, &["push", "-q", "origin", "main"]));
        std::fs::write(repo.join("l.txt"), "l\n").unwrap();
        assert!(git(repo, &["add", "l.txt"]));
        assert!(git(repo, &["commit", "-q", "-m", "local"]));
        sync(repo, &mut |_| {}).unwrap();
        assert!(repo.join("o2.txt").exists() && repo.join("l.txt").exists());
        assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), git_out(&fx.remote, &["rev-parse", "refs/heads/main"]));
        let main = branches(repo).unwrap().into_iter().find(|b| b.name == "main").unwrap();
        assert_eq!((main.ahead, main.behind), (0, 0));

        assert_eq!(remote_url(repo).as_deref(), fx.remote.to_str());
    }

    #[test]
    fn network_errors_are_clear() {
        let Some(fx) = fixture("net-err") else { return };
        // No remote at all.
        let lonely = fx.tmp.join("lonely");
        init(&lonely).unwrap();
        configure(&lonely);
        assert!(lonely.join(".git").exists());
        assert_eq!(remote_url(&lonely), None);
        for r in [fetch(&lonely, &mut |_| {}), pull(&lonely, &mut |_| {}), push(&lonely, &mut |_| {}), sync(&lonely, &mut |_| {})] {
            assert!(r.unwrap_err().to_string().contains("Uzak depo tanımlı değil"));
        }
        // Unreachable remote: error carries git's message.
        assert!(git(&lonely, &["remote", "add", "origin", fx.tmp.join("yok.git").to_str().unwrap()]));
        let err = fetch(&lonely, &mut |_| {}).unwrap_err().to_string();
        assert!(err.starts_with("git fetch başarısız: "), "{err}");
        assert!(err.len() > "git fetch başarısız: ".len());
    }

    #[test]
    fn init_commit_connect_push() {
        let Some(fx) = fixture("connect") else { return };
        // a new folder with files, never under git: init, commit, connect, push
        let dir = fx.tmp.join("yeni proje");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("not.txt"), "merhaba\n").unwrap();
        init(&dir).unwrap();
        configure(&dir);
        assert!(has_changes(&dir));
        assert!(commit_all(&dir, "   ").is_err(), "empty message refused");
        commit_all(&dir, "İlk kayıt").unwrap();
        assert!(!has_changes(&dir));
        assert!(commit_all(&dir, "boş").is_err(), "nothing to commit");
        assert!(remote_url(&dir).is_none());
        let bare = fx.tmp.join("yeni.git");
        assert!(git(&fx.tmp, &["init", "-q", "--bare", "yeni.git"]));
        connect_remote(&dir, bare.to_str().unwrap()).unwrap();
        assert_eq!(remote_url(&dir).as_deref(), bare.to_str());
        push(&dir, &mut |_| {}).unwrap();
        let branch = git_out(&dir, &["symbolic-ref", "--short", "HEAD"]);
        assert_eq!(git_out(&bare, &["rev-parse", &branch]), git_out(&dir, &["rev-parse", "HEAD"]));
        // connecting again changes the address instead of failing
        connect_remote(&dir, fx.remote.to_str().unwrap()).unwrap();
        assert_eq!(remote_url(&dir).as_deref(), fx.remote.to_str());
        assert!(connect_remote(&dir, "  ").is_err());
    }

    #[test]
    fn clone_returns_new_folder() {
        let Some(fx) = fixture("clone") else { return };
        let parent = fx.tmp.join("dest");
        let got = clone(fx.remote.to_str().unwrap(), &parent, &mut |_| {}).unwrap();
        assert_eq!(got, parent.join("remote"));
        assert_eq!(repo_root(&got), Some(got.clone()));
        assert!(got.join("a.txt").exists());
        assert_eq!(status(&got).unwrap().branch, "main");
        // Existing non-empty target => clear failure.
        assert!(clone(fx.remote.to_str().unwrap(), &parent, &mut |_| {}).is_err());
    }

    #[test]
    fn last_commit_for_repo_and_path() {
        let Some(fx) = fixture("last") else { return };
        let repo = &fx.repo;
        std::fs::write(repo.join("b.txt"), "b2\n").unwrap();
        assert!(git(repo, &["commit", "-q", "-am", "b değişti"]));
        let head = last_commit(repo, None).unwrap();
        assert_eq!((head.message.as_str(), head.author.as_str()), ("b değişti", "t"));
        assert!(!head.sha.is_empty() && head.sha.len() <= 12);
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        assert!((now_ms - head.time_ms).abs() < 5 * 60 * 1000);
        let a = last_commit(repo, Some(Path::new("a.txt"))).unwrap();
        assert_eq!(a.message, "ilk");
        assert_ne!(a.sha, head.sha);
        assert_eq!(last_commit(repo, Some(Path::new("yok.txt"))), None);
        assert_eq!(last_commit(&fx.tmp, None), None); // not a repo
    }

    #[test]
    fn last_commits_of_children_per_item() {
        let Some(fx) = fixture("children") else { return };
        let repo = &fx.repo;
        std::fs::create_dir_all(repo.join("klasör/alt")).unwrap();
        std::fs::write(repo.join("klasör/alt/derin.txt"), "d\n").unwrap();
        std::fs::write(repo.join("klasör/üst.txt"), "u\n").unwrap();
        assert!(git(repo, &["add", "-A"]));
        assert!(git(repo, &["commit", "-q", "-m", "klasör eklendi"]));
        std::fs::write(repo.join("b.txt"), "b2\n").unwrap();
        assert!(git(repo, &["commit", "-q", "-am", "b değişti"]));
        std::fs::write(repo.join("izlenmeyen.txt"), "x").unwrap();
        let names: Vec<String> = ["a.txt", "B.TXT", "klasör", "izlenmeyen.txt"].map(String::from).to_vec();
        let got = last_commits_of_children(repo, repo, &names, 1000);
        assert_eq!(got["a.txt"].message, "ilk");
        assert_eq!(got["B.TXT"].message, "b değişti"); // case-insensitive like Windows
        assert_eq!(got["klasör"].message, "klasör eklendi"); // a folder: its newest file
        assert!(!got.contains_key("izlenmeyen.txt"));
        // inside a subfolder, the path typed in another case
        let sub = PathBuf::from(repo.to_string_lossy().to_uppercase()).join("klasör");
        let got = last_commits_of_children(repo, &sub, &["alt".into(), "üst.txt".into()], 1000);
        assert_eq!(got.len(), 2);
        assert_eq!(rel_inside(Path::new(r"C:\r"), Path::new(r"c:\R\a\b")).as_deref(), Some("a/b"));
        assert_eq!(rel_inside(Path::new(r"C:\r"), Path::new(r"C:\rx")), None);
    }
}
