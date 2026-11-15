//! One still frame of a video, for the preview and the media grid.
//!
//! philly doesn't play video: GStreamer's GL video sink crashed it (libepoxy
//! "Couldn't find current GLX or EGL context"). The frame comes from
//! `ffmpegthumbnailer` in a separate process, cached under
//! ~/.cache/philly/frames, or from the freedesktop thumbnail cache.
//! Blocking: call from a worker thread.

use gtk::{gio, glib, prelude::*};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A cached freedesktop thumbnail (made by Nautilus and friends), largest first.
pub fn thumbnail_cache(path: &Path) -> Option<PathBuf> {
    let uri = gio::File::for_path(path).uri();
    let md5 = glib::compute_checksum_for_string(glib::ChecksumType::Md5, &uri)?;
    let cache = glib::user_cache_dir().join("thumbnails");
    ["x-large", "large", "normal"].iter().map(|s| cache.join(s).join(format!("{md5}.png"))).find(|p| p.exists())
}

/// A PNG with one frame of the video at `path`, made if needed.
fn video_frame(path: &Path) -> Option<PathBuf> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let key = format!("{}\n{mtime}", path.display());
    let md5 = glib::compute_checksum_for_string(glib::ChecksumType::Md5, &key)?;
    let dir = glib::user_cache_dir().join("philly/frames");
    let out = dir.join(format!("{md5}.png"));
    if out.exists() {
        return Some(out);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!("{md5}.{}.tmp.png", std::process::id()));
    let mut child = Command::new("ffmpegthumbnailer")
        .arg("-i")
        .arg(path)
        .arg("-o")
        .arg(&tmp)
        .args(["-s", "1280", "-t", "10%", "-c", "png"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(15) => std::thread::sleep(Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    if ok && std::fs::rename(&tmp, &out).is_ok() {
        Some(out)
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

/// The best still for a video: our own frame, else the shared thumbnail cache.
pub fn video_still(path: &Path) -> Option<PathBuf> {
    video_frame(path).or_else(|| thumbnail_cache(path))
}
