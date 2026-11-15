//! File operations with undo, clipboard interop (Nautilus-compatible) and
//! launching files / terminals / editors.

use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub enum Op {
    Trashed(Vec<PathBuf>),
    Renamed(PathBuf, PathBuf),
    Created(PathBuf),
    Copied(Vec<PathBuf>),
    Moved(Vec<(PathBuf, PathBuf)>),
}

thread_local! {
    static UNDO: RefCell<Vec<Op>> = const { RefCell::new(Vec::new()) };
    static CUT: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

pub fn push_undo(op: Op) {
    UNDO.with(|u| {
        let mut u = u.borrow_mut();
        u.push(op);
        if u.len() > 100 {
            u.remove(0);
        }
    });
}

pub fn cut_set() -> HashSet<PathBuf> {
    CUT.with(|c| c.borrow().clone())
}

/// A free name in `dir` for `name`: "file.txt" -> "file (2).txt".
pub fn free_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() && std::fs::symlink_metadata(&candidate).is_err() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 2.. {
        let p = dir.join(format!("{stem} ({i}){ext}"));
        if !p.exists() && std::fs::symlink_metadata(&p).is_err() {
            return p;
        }
    }
    unreachable!()
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)?;
    } else if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
        std::fs::set_permissions(dst, meta.permissions())?;
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn move_one(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            // EXDEV: different filesystem.
            copy_recursive(src, dst)?;
            if std::fs::symlink_metadata(src)?.is_dir() {
                std::fs::remove_dir_all(src)
            } else {
                std::fs::remove_file(src)
            }
        }
        Err(e) => Err(e),
    }
}

/// Copy or move `srcs` into `dest_dir` on a worker thread.
pub async fn transfer(srcs: Vec<PathBuf>, dest_dir: PathBuf, mv: bool) -> Result<Vec<PathBuf>, String> {
    let res = gio::spawn_blocking(move || -> Result<(Vec<PathBuf>, Vec<(PathBuf, PathBuf)>), String> {
        let mut out = Vec::new();
        let mut moved = Vec::new();
        for src in srcs {
            let name = src.file_name().ok_or("bad path")?.to_string_lossy().to_string();
            if mv && src.parent() == Some(dest_dir.as_path()) {
                continue;
            }
            if dest_dir.starts_with(&src) {
                return Err(format!("Can't put \"{name}\" inside itself"));
            }
            let dst = free_name(&dest_dir, &name);
            if mv {
                move_one(&src, &dst).map_err(|e| format!("{name}: {e}"))?;
                moved.push((src, dst.clone()));
            } else {
                copy_recursive(&src, &dst).map_err(|e| format!("{name}: {e}"))?;
            }
            out.push(dst);
        }
        Ok((out, moved))
    })
    .await
    .map_err(|_| "worker failed".to_string())??;
    if mv {
        push_undo(Op::Moved(res.1));
    } else {
        push_undo(Op::Copied(res.0.clone()));
    }
    Ok(res.0)
}

pub fn trash(paths: &[PathBuf]) -> Result<(), String> {
    let mut done = Vec::new();
    for p in paths {
        let f = gio::File::for_path(p);
        if let Err(e) = f.trash(gio::Cancellable::NONE) {
            if !done.is_empty() {
                push_undo(Op::Trashed(done));
            }
            return Err(format!("{}: {e}", p.display()));
        }
        done.push(p.clone());
    }
    push_undo(Op::Trashed(done));
    Ok(())
}

pub fn delete_permanently(uris: &[String]) -> Result<(), String> {
    for u in uris {
        let f = gio::File::for_uri(u);
        delete_recursive(&f).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn delete_recursive(f: &gio::File) -> Result<(), glib::Error> {
    if f.query_file_type(gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, gio::Cancellable::NONE) == gio::FileType::Directory {
        let en = f.enumerate_children("standard::name", gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, gio::Cancellable::NONE)?;
        while let Some(info) = en.next_file(gio::Cancellable::NONE)? {
            delete_recursive(&f.child(info.name()))?;
        }
    }
    f.delete(gio::Cancellable::NONE)
}

/// Restore a trashed item to its original location.
pub fn restore(uri: &str, orig: &Path) -> Result<(), String> {
    let dst = if orig.exists() {
        free_name(orig.parent().unwrap_or(Path::new("/")), &orig.file_name().unwrap_or_default().to_string_lossy())
    } else {
        orig.to_path_buf()
    };
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    gio::File::for_uri(uri)
        .move_(&gio::File::for_path(&dst), gio::FileCopyFlags::NOFOLLOW_SYMLINKS, gio::Cancellable::NONE, None)
        .map_err(|e| e.to_string())
}

fn restore_by_orig(orig: &Path) -> Result<(), String> {
    let trash = gio::File::for_uri(crate::tree::TRASH_ROOT);
    let en = trash
        .enumerate_children("standard::name,trash::orig-path,trash::deletion-date", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
        .map_err(|e| e.to_string())?;
    let mut best: Option<(String, String)> = None;
    while let Ok(Some(info)) = en.next_file(gio::Cancellable::NONE) {
        let o = info.attribute_byte_string("trash::orig-path").map(|s| s.to_string()).unwrap_or_default();
        if Path::new(&o) == orig {
            let date = info.attribute_string("trash::deletion-date").map(|s| s.to_string()).unwrap_or_default();
            if best.as_ref().map(|(_, d)| date > *d).unwrap_or(true) {
                best = Some((trash.child(info.name()).uri().to_string(), date));
            }
        }
    }
    let (uri, _) = best.ok_or_else(|| format!("{} is no longer in the trash", orig.display()))?;
    restore(&uri, orig)
}

pub fn rename(from: &Path, new_name: &str) -> Result<PathBuf, String> {
    if new_name.is_empty() || new_name.contains('/') {
        return Err("Invalid name".into());
    }
    let to = from.parent().unwrap_or(Path::new("/")).join(new_name);
    if to == from {
        return Ok(to);
    }
    // Allow case-only renames on case-insensitive filesystems.
    let case_only = to.to_string_lossy().to_lowercase() == from.to_string_lossy().to_lowercase();
    if to.exists() && !case_only {
        return Err(format!("\"{new_name}\" already exists"));
    }
    std::fs::rename(from, &to).map_err(|e| e.to_string())?;
    push_undo(Op::Renamed(from.to_path_buf(), to.clone()));
    Ok(to)
}

pub fn create(dir: &Path, name: &str, folder: bool) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("Name can't be empty".into());
    }
    // VS Code style: "a/b/c.txt" creates intermediate folders.
    let p = dir.join(name.trim_start_matches('/'));
    if p.exists() {
        return Err(format!("\"{name}\" already exists"));
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if folder {
        std::fs::create_dir(&p).map_err(|e| e.to_string())?;
    } else {
        std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map_err(|e| e.to_string())?;
    }
    push_undo(Op::Created(p.clone()));
    Ok(p)
}

pub fn duplicate(p: &Path) -> Result<PathBuf, String> {
    let dir = p.parent().ok_or("bad path")?;
    let dst = free_name(dir, &p.file_name().unwrap_or_default().to_string_lossy());
    copy_recursive(p, &dst).map_err(|e| e.to_string())?;
    push_undo(Op::Copied(vec![dst.clone()]));
    Ok(dst)
}

/// Undo the last operation. Returns a description of what was undone.
pub fn undo() -> Result<String, String> {
    let op = UNDO.with(|u| u.borrow_mut().pop()).ok_or("Nothing to undo")?;
    match op {
        Op::Trashed(paths) => {
            for p in &paths {
                restore_by_orig(p)?;
            }
            Ok(format!("Restored {}", plural(paths.len(), "item")))
        }
        Op::Renamed(from, to) => {
            std::fs::rename(&to, &from).map_err(|e| e.to_string())?;
            Ok(format!("Renamed back to {}", from.file_name().unwrap_or_default().to_string_lossy()))
        }
        Op::Created(p) => {
            gio::File::for_path(&p).trash(gio::Cancellable::NONE).map_err(|e| e.to_string())?;
            Ok(format!("Removed {}", p.file_name().unwrap_or_default().to_string_lossy()))
        }
        Op::Copied(paths) => {
            for p in &paths {
                gio::File::for_path(p).trash(gio::Cancellable::NONE).map_err(|e| e.to_string())?;
            }
            Ok(format!("Removed {} copied", plural(paths.len(), "item")))
        }
        Op::Moved(pairs) => {
            for (from, to) in pairs.iter().rev() {
                move_one(to, from).map_err(|e| e.to_string())?;
            }
            Ok(format!("Moved {} back", plural(pairs.len(), "item")))
        }
    }
}

pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

// ---------- clipboard ----------

pub fn set_clipboard(display: &gdk::Display, paths: &[PathBuf], cut: bool) {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let uri_list: String = uris.iter().map(|u| format!("{u}\r\n")).collect();
    let text = paths.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n");
    let provider = gdk::ContentProvider::new_union(&[
        gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()),
        gdk::ContentProvider::for_bytes("x-special/gnome-copied-files", &glib::Bytes::from_owned(gnome.into_bytes())),
        gdk::ContentProvider::for_bytes("text/uri-list", &glib::Bytes::from_owned(uri_list.into_bytes())),
        gdk::ContentProvider::for_bytes("text/plain;charset=utf-8", &glib::Bytes::from_owned(text.into_bytes())),
    ]);
    let _ = display.clipboard().set_content(Some(&provider));
    CUT.with(|c| {
        let mut c = c.borrow_mut();
        c.clear();
        if cut {
            c.extend(paths.iter().cloned());
        }
    });
}

pub fn clear_cut() {
    CUT.with(|c| c.borrow_mut().clear());
}

/// Read files from the clipboard. Returns (paths, is_cut).
pub async fn read_clipboard(display: &gdk::Display) -> Option<(Vec<PathBuf>, bool)> {
    let cb = display.clipboard();
    let formats = cb.formats();
    if formats.contain_mime_type("x-special/gnome-copied-files") {
        if let Ok((stream, _)) = cb.read_future(&["x-special/gnome-copied-files"], glib::Priority::DEFAULT).await {
            let mut data = Vec::new();
            loop {
                match stream.read_bytes_future(65536, glib::Priority::DEFAULT).await {
                    Ok(b) if !b.is_empty() => data.extend_from_slice(&b),
                    _ => break,
                }
            }
            let text = String::from_utf8_lossy(&data).to_string();
            let mut lines = text.lines();
            let cut = lines.next() == Some("cut");
            let paths: Vec<PathBuf> = lines.filter_map(|u| gio::File::for_uri(u.trim()).path()).collect();
            if !paths.is_empty() {
                return Some((paths, cut));
            }
        }
    }
    if let Ok(v) = cb.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await {
        if let Ok(list) = v.get::<gdk::FileList>() {
            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            if !paths.is_empty() {
                let cut = CUT.with(|c| {
                    let c = c.borrow();
                    !c.is_empty() && paths.iter().all(|p| c.contains(p))
                });
                return Some((paths, cut));
            }
        }
    }
    // Plain text paths (e.g. copied from a terminal).
    if let Ok(Some(text)) = cb.read_text_future().await {
        let paths: Vec<PathBuf> = text.lines().map(|l| PathBuf::from(l.trim())).filter(|p| p.is_absolute() && p.exists()).collect();
        if !paths.is_empty() {
            return Some((paths, false));
        }
    }
    None
}

pub fn copy_text(display: &gdk::Display, text: &str) {
    display.clipboard().set_text(text);
}

// ---------- launching ----------

pub fn open_default(widget: &impl IsA<gtk::Widget>, path: &Path) -> Result<(), String> {
    let uri = gio::File::for_path(path).uri();
    let ctx = widget.display().app_launch_context();
    gio::AppInfo::launch_default_for_uri(&uri, Some(&ctx)).map_err(|e| e.to_string())
}

pub fn open_with(widget: &impl IsA<gtk::Widget>, app: &gio::AppInfo, paths: &[PathBuf]) -> Result<(), String> {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    let ctx = widget.display().app_launch_context();
    app.launch(&files, Some(&ctx)).map_err(|e| e.to_string())
}

pub fn apps_for(path: &Path) -> Vec<gio::AppInfo> {
    let ctype = if path.is_dir() {
        "inode/directory".to_string()
    } else {
        gio::content_type_guess(Some(path), None).0.to_string()
    };
    let mut apps = gio::AppInfo::recommended_for_type(&ctype);
    for a in gio::AppInfo::all_for_type(&ctype) {
        if !apps.iter().any(|b| b.id() == a.id()) {
            apps.push(a);
        }
    }
    apps.retain(|a| a.id().map(|id| !id.starts_with("philly")).unwrap_or(true));
    apps
}

fn spawn_detached(mut cmd: std::process::Command) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

extern "C" {
    fn setsid() -> i32;
}
fn libc_setsid() {
    unsafe {
        setsid();
    }
}

fn which(cmd: &str) -> bool {
    glib::find_program_in_path(cmd).is_some()
}

pub fn open_terminal(dir: &Path) -> Result<(), String> {
    let mut cmd = if which("uwsm-app") {
        let mut c = std::process::Command::new("uwsm-app");
        c.args(["--", "xdg-terminal-exec"]);
        c
    } else {
        std::process::Command::new("xdg-terminal-exec")
    };
    cmd.current_dir(dir);
    spawn_detached(cmd)
}

/// Open in VS Code (or $VISUAL / $EDITOR in a terminal as a fallback).
pub fn open_in_editor(path: &Path, line: Option<u32>, workspace: Option<&Path>) -> Result<(), String> {
    if which("code") {
        let mut cmd = std::process::Command::new("code");
        if let Some(ws) = workspace {
            if ws != path {
                cmd.arg(ws);
            }
        }
        match line {
            Some(l) => {
                cmd.arg("-g").arg(format!("{}:{l}", path.display()));
            }
            None => {
                cmd.arg(path);
            }
        }
        return spawn_detached(cmd);
    }
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "nvim".into());
    let mut cmd = std::process::Command::new("xdg-terminal-exec");
    cmd.arg(editor);
    if let Some(l) = line {
        cmd.arg(format!("+{l}"));
    }
    cmd.arg(path);
    if let Some(d) = path.parent() {
        cmd.current_dir(d);
    }
    spawn_detached(cmd)
}
