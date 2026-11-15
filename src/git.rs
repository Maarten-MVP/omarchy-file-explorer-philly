//! Git integration. Reads use libgit2 (fast, in-process); writes shell out to the
//! `git` CLI so the user's hooks, signing and config are respected.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflict,
    TypeChange,
}

impl Kind {
    pub fn letter(self) -> &'static str {
        match self {
            Kind::Modified => "M",
            Kind::Added => "A",
            Kind::Deleted => "D",
            Kind::Renamed => "R",
            Kind::Untracked => "U",
            Kind::Conflict => "!",
            Kind::TypeChange => "T",
        }
    }
    /// CSS class suffix used for coloring.
    pub fn class(self) -> &'static str {
        match self {
            Kind::Modified | Kind::TypeChange => "git-M",
            Kind::Added => "git-A",
            Kind::Untracked => "git-U",
            Kind::Deleted => "git-D",
            Kind::Renamed => "git-R",
            Kind::Conflict => "git-C",
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Modified => "Modified",
            Kind::Added => "Added",
            Kind::Deleted => "Deleted",
            Kind::Renamed => "Renamed",
            Kind::Untracked => "Untracked",
            Kind::Conflict => "Conflict",
            Kind::TypeChange => "Type changed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// Path relative to the repo root.
    pub rel: String,
    pub kind: Kind,
    pub staged: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub root: PathBuf,
    pub branch: String,
    pub ahead: usize,
    pub behind: usize,
    /// Upstream branch, e.g. "origin/main" (None: not published).
    pub upstream: Option<String>,
    /// Commit ids of HEAD and the upstream, to notice commits and fetches.
    pub head_id: String,
    pub upstream_id: String,
    /// When the last fetch happened (FETCH_HEAD's mtime, unix seconds).
    pub fetched: Option<i64>,
    pub staged: Vec<Change>,
    pub unstaged: Vec<Change>,
    /// Absolute path -> kind to show in the tree.
    pub files: HashMap<PathBuf, Kind>,
    /// Directories that contain changes.
    pub dirs: HashSet<PathBuf>,
}

impl Status {
    pub fn count(&self) -> usize {
        let mut s: HashSet<&str> = HashSet::new();
        for c in self.staged.iter().chain(self.unstaged.iter()) {
            s.insert(&c.rel);
        }
        s.len()
    }
}

/// Repo root (work tree) containing `path`, if any.
pub fn discover(path: &Path) -> Option<PathBuf> {
    let repo = git2::Repository::discover(path).ok()?;
    let wd = repo.workdir()?.to_path_buf();
    Some(wd.canonicalize().unwrap_or(wd))
}

pub fn open(root: &Path) -> Option<git2::Repository> {
    git2::Repository::open(root).ok()
}

pub fn status(root: &Path) -> Option<Status> {
    let repo = open(root)?;
    let mut st = Status { root: root.to_path_buf(), ..Default::default() };

    st.branch = match repo.head() {
        Ok(h) if h.is_branch() => h.shorthand().unwrap_or("HEAD").to_string(),
        Ok(h) => h
            .target()
            .map(|o| format!("{} (detached)", &o.to_string()[..7]))
            .unwrap_or_else(|| "HEAD".into()),
        Err(_) => repo
            .find_reference("HEAD")
            .ok()
            .and_then(|r| r.symbolic_target().ok().flatten().map(|s| s.trim_start_matches("refs/heads/").to_string()))
            .unwrap_or_else(|| "main".into()),
    };
    st.fetched = std::fs::metadata(repo.path().join("FETCH_HEAD"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    if let Ok(head) = repo.head() {
        st.head_id = head.target().map(|o| o.to_string()).unwrap_or_default();
        if let (Some(local), Ok(name)) = (head.target(), head.name().map(str::to_string)) {
            if let Ok(up) = repo.branch_upstream_name(&name) {
                if let Ok(up) = up.as_str() {
                    st.upstream = Some(up.trim_start_matches("refs/remotes/").to_string());
                    if let Ok(r) = repo.find_reference(up) {
                        if let Some(remote) = r.target() {
                            st.upstream_id = remote.to_string();
                            if let Ok((a, b)) = repo.graph_ahead_behind(local, remote) {
                                st.ahead = a;
                                st.behind = b;
                            }
                        }
                    }
                }
            }
        }
    }

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    let statuses = repo.statuses(Some(&mut opts)).ok()?;
    for e in statuses.iter() {
        let s = e.status();
        let rel = match e.path() {
            Ok(p) => p.to_string(),
            Err(_) => continue,
        };
        let mut tree_kind = None;
        if s.is_conflicted() {
            st.unstaged.push(Change { rel: rel.clone(), kind: Kind::Conflict, staged: false });
            tree_kind = Some(Kind::Conflict);
        } else {
            let idx = if s.is_index_new() {
                Some(Kind::Added)
            } else if s.is_index_modified() {
                Some(Kind::Modified)
            } else if s.is_index_deleted() {
                Some(Kind::Deleted)
            } else if s.is_index_renamed() {
                Some(Kind::Renamed)
            } else if s.is_index_typechange() {
                Some(Kind::TypeChange)
            } else {
                None
            };
            let wt = if s.is_wt_new() {
                Some(Kind::Untracked)
            } else if s.is_wt_modified() {
                Some(Kind::Modified)
            } else if s.is_wt_deleted() {
                Some(Kind::Deleted)
            } else if s.is_wt_renamed() {
                Some(Kind::Renamed)
            } else if s.is_wt_typechange() {
                Some(Kind::TypeChange)
            } else {
                None
            };
            if let Some(k) = idx {
                st.staged.push(Change { rel: rel.clone(), kind: k, staged: true });
            }
            if let Some(k) = wt {
                st.unstaged.push(Change { rel: rel.clone(), kind: k, staged: false });
            }
            tree_kind = wt.or(idx);
        }
        if let Some(k) = tree_kind {
            let abs = root.join(&rel);
            let mut dir = abs.parent();
            while let Some(d) = dir {
                if d.starts_with(root) && d != root {
                    if !st.dirs.insert(d.to_path_buf()) {
                        break;
                    }
                    dir = d.parent();
                } else {
                    break;
                }
            }
            st.files.insert(abs, k);
        }
    }
    st.staged.sort_by(|a, b| a.rel.cmp(&b.rel));
    st.unstaged.sort_by(|a, b| a.rel.cmp(&b.rel));
    Some(st)
}

/// Content of a path at HEAD.
pub fn head_content(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let repo = open(root)?;
    let tree = repo.head().ok()?.peel_to_tree().ok()?;
    let entry = tree.get_path(Path::new(rel)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    Some(blob.content().to_vec())
}

/// Content of a path in the index (staged version).
pub fn index_content(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let repo = open(root)?;
    let index = repo.index().ok()?;
    let entry = index.get_path(Path::new(rel), 0)?;
    let blob = repo.find_blob(entry.id).ok()?;
    Some(blob.content().to_vec())
}

pub fn workdir_content(root: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(rel)).ok()
}

/// (old, new) contents for showing a change.
pub fn change_contents(root: &Path, ch: &Change) -> (Vec<u8>, Vec<u8>) {
    if ch.staged {
        (
            head_content(root, &ch.rel).unwrap_or_default(),
            index_content(root, &ch.rel).unwrap_or_default(),
        )
    } else {
        let old = index_content(root, &ch.rel)
            .or_else(|| head_content(root, &ch.rel))
            .unwrap_or_default();
        (old, workdir_content(root, &ch.rel).unwrap_or_default())
    }
}

/// Checks for ignored paths with one open repository.
pub struct IgnoreChecker {
    repo: Option<git2::Repository>,
    root: PathBuf,
}

impl IgnoreChecker {
    pub fn new(root: Option<&Path>) -> Self {
        IgnoreChecker {
            repo: root.and_then(open),
            root: root.map(Path::to_path_buf).unwrap_or_default(),
        }
    }
    pub fn is_ignored(&self, abs: &Path) -> bool {
        let Some(repo) = &self.repo else { return false };
        let Ok(rel) = abs.strip_prefix(&self.root) else { return false };
        if rel.as_os_str().is_empty() {
            return false;
        }
        repo.is_path_ignored(rel).unwrap_or(false)
    }
}

fn git(root: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    if let Some(input) = stdin {
        use std::io::Write;
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(input.as_bytes());
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { "git command failed".into() } else { err })
    }
}

pub fn stage(root: &Path, rels: &[String]) -> Result<(), String> {
    let mut args = vec!["add", "-A", "--"];
    args.extend(rels.iter().map(String::as_str));
    git(root, &args, None).map(|_| ())
}

pub fn unstage(root: &Path, rels: &[String]) -> Result<(), String> {
    let has_head = open(root).map(|r| r.head().is_ok()).unwrap_or(false);
    let mut args: Vec<&str> =
        if has_head { vec!["restore", "--staged", "--"] } else { vec!["rm", "--cached", "-r", "-q", "--"] };
    args.extend(rels.iter().map(String::as_str));
    git(root, &args, None).map(|_| ())
}

/// Discard working-tree changes. Untracked files are moved to the trash, not deleted.
pub fn discard(root: &Path, ch: &Change) -> Result<(), String> {
    if ch.kind == Kind::Untracked {
        let f = gtk::gio::File::for_path(root.join(&ch.rel));
        return f.trash(gtk::gio::Cancellable::NONE).map_err(|e| e.to_string());
    }
    git(root, &["restore", "--", &ch.rel], None).map(|_| ())
}

pub fn commit(root: &Path, message: &str, all: bool) -> Result<String, String> {
    let mut args = vec!["commit", "-F", "-"];
    if all {
        args.push("-a");
    }
    git(root, &args, Some(message))
}

use gtk::gio::prelude::FileExt;


// ------------------------------------------------------------------ history

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// In both HEAD and the upstream (or there is no upstream).
    Shared,
    /// Only in HEAD: ready to push.
    Outgoing,
    /// Only in the upstream: ready to pull.
    Incoming,
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub id: String,
    pub summary: String,
    pub message: String,
    pub author: String,
    pub time: i64,
    pub merge: bool,
    pub refs: Vec<String>,
    pub head: bool,
    pub flow: Flow,
}

/// Recent commits of HEAD and its upstream, newest first.
pub fn log(root: &Path, limit: usize) -> Vec<Commit> {
    let Some(repo) = open(root) else { return Vec::new() };
    let Some(head) = repo.head().ok().and_then(|h| h.target()) else { return Vec::new() };
    let upstream = repo
        .head()
        .ok()
        .and_then(|h| h.name().map(str::to_string).ok())
        .and_then(|n| repo.branch_upstream_name(&n).ok())
        .and_then(|b| b.as_str().ok().map(str::to_string))
        .and_then(|u| repo.find_reference(&u).ok())
        .and_then(|r| r.target());

    let only = |from: git2::Oid, hide: git2::Oid| -> HashSet<git2::Oid> {
        let mut out = HashSet::new();
        if let Ok(mut w) = repo.revwalk() {
            if w.push(from).is_ok() && w.hide(hide).is_ok() {
                out.extend(w.flatten().take(1000));
            }
        }
        out
    };
    let (outgoing, incoming) = match upstream {
        Some(up) => (only(head, up), only(up, head)),
        None => (HashSet::new(), HashSet::new()),
    };

    // Branch and tag names by commit.
    let mut refs: HashMap<git2::Oid, Vec<String>> = HashMap::new();
    if let Ok(it) = repo.references() {
        for r in it.flatten() {
            let Ok(name) = r.shorthand().map(str::to_string) else { continue };
            if name.ends_with("/HEAD") || r.is_note() {
                continue;
            }
            if let Ok(c) = r.peel_to_commit() {
                refs.entry(c.id()).or_default().push(name);
            }
        }
    }

    let mut out = Vec::new();
    let Ok(mut walk) = repo.revwalk() else { return out };
    let _ = walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME);
    let _ = walk.push(head);
    if let Some(up) = upstream {
        let _ = walk.push(up);
    }
    for oid in walk.flatten().take(limit) {
        let Ok(c) = repo.find_commit(oid) else { continue };
        let flow = if outgoing.contains(&oid) {
            Flow::Outgoing
        } else if incoming.contains(&oid) {
            Flow::Incoming
        } else {
            Flow::Shared
        };
        let mut r = refs.remove(&oid).unwrap_or_default();
        r.sort_by_key(|n| (n.contains('/'), n.clone()));
        out.push(Commit {
            id: oid.to_string(),
            summary: c.summary().ok().flatten().unwrap_or("").to_string(),
            message: c.message().ok().unwrap_or("").trim_end().to_string(),
            author: c.author().name().ok().unwrap_or("").to_string(),
            time: c.time().seconds(),
            merge: c.parent_count() > 1,
            refs: r,
            head: oid == head,
            flow,
        });
    }
    out
}

/// `git show` of one commit: message, file stats and the patch.
pub fn show(root: &Path, id: &str) -> Result<String, String> {
    git(root, &["show", "--stat", "--patch", "--format=fuller", "--no-color", id], None)
}

// ------------------------------------------------------------------ remotes

/// Network git: never prompt (philly has no terminal); ssh keys via the agent.
fn git_net(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let has_ssh_cmd = std::env::var_os("GIT_SSH_COMMAND").is_some()
        || git(root, &["config", "core.sshCommand"], None).map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !has_ssh_cmd {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=15");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        // The last meaningful line is usually the reason.
        let line = err.lines().rev().find(|l| !l.trim().is_empty() && !l.starts_with("hint:")).unwrap_or("git failed");
        Err(line.trim().trim_start_matches("fatal: ").trim_start_matches("error: ").to_string())
    }
}

pub fn fetch(root: &Path) -> Result<(), String> {
    git_net(root, &["fetch", "--prune"]).map(|_| ())
}

/// Fast-forward only: a pull never creates a merge commit or conflicts.
pub fn pull(root: &Path) -> Result<(), String> {
    git_net(root, &["pull", "--ff-only"]).map(|_| ())
}

/// Push; a branch without an upstream is published to its remote (origin).
pub fn push(root: &Path, publish: bool) -> Result<(), String> {
    if publish {
        let remotes = git(root, &["remote"], None)?;
        let remote = remotes.lines().find(|r| *r == "origin").or_else(|| remotes.lines().next()).ok_or("This repository has no remote")?.to_string();
        git_net(root, &["push", "-u", &remote, "HEAD"]).map(|_| ())
    } else {
        git_net(root, &["push"]).map(|_| ())
    }
}
