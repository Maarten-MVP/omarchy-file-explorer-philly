//! Smart previews for folders without a README:
//! - code projects: a project card (stack, name, version, scripts, git log)
//! - photo / video folders: a thumbnail grid
//! - other folders with files: recently changed files + space per file type
//! Folders that only contain folders fall back to the info card.
//!
//! `scan` runs off the main thread and returns plain data; `build` makes the
//! widgets.

use crate::icons;
use gtk::{gdk, gio, glib, prelude::*};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub struct Scan {
    pub dirs: usize,
    pub files: usize,
    pub bytes: u64,
    pub readme: Option<PathBuf>,
    pub view: View,
}

pub enum View {
    Project(Project),
    Media(Vec<(PathBuf, bool)>), // (path, is_video)
    Mixed(Mixed),
    Plain,
}

#[derive(Default)]
pub struct Project {
    pub stack: String,
    pub glyph: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub scripts: Vec<String>,
    pub deps: Option<usize>,
    pub git: Option<GitInfo>,
}

pub struct GitInfo {
    pub branch: String,
    pub changes: usize,
    pub commits: Vec<(String, String, String, i64)>, // short id, summary, author, time
}

pub struct Mixed {
    pub recent: Vec<(PathBuf, u64, i64)>, // path, size, mtime
    pub by_kind: Vec<(Category, u64)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Images,
    Video,
    Audio,
    Documents,
    Archives,
    Code,
    Other,
}

impl Category {
    fn of(name: &str) -> Category {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic" | "heif" | "avif" | "tif" | "tiff" | "ico" | "raw" | "cr2" | "nef" | "dng" => Category::Images,
            "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "wmv" | "flv" => Category::Video,
            "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac" | "wma" => Category::Audio,
            "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ods" | "csv" | "ppt" | "pptx" | "odp" | "pages" | "numbers" | "key" | "epub" => {
                Category::Documents
            }
            "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "rar" | "zst" | "deb" | "rpm" | "dmg" | "iso" | "apk" | "aab" | "ipa" | "appimage" => Category::Archives,
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "dart" | "java" | "kt" | "swift" | "c" | "h" | "cpp" | "hpp" | "cs" | "rb" | "php" | "sh" | "lua" | "json" | "yaml"
            | "yml" | "toml" | "html" | "css" | "scss" | "sql" | "vue" | "svelte" => Category::Code,
            _ => Category::Other,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Category::Images => "Images",
            Category::Video => "Video",
            Category::Audio => "Audio",
            Category::Documents => "Documents",
            Category::Archives => "Archives",
            Category::Code => "Code",
            Category::Other => "Other",
        }
    }
    fn color(self) -> String {
        crate::theme::color(match self {
            Category::Images => "magenta",
            Category::Video => "red",
            Category::Audio => "cyan",
            Category::Documents => "blue",
            Category::Archives => "yellow",
            Category::Code => "green",
            Category::Other => "dark_foreground",
        })
    }
}

const MANIFESTS: &[&str] = &["Cargo.toml", "package.json", "pubspec.yaml", "pyproject.toml", "go.mod", "composer.json", "Gemfile", "pom.xml", "build.gradle", "build.gradle.kts", "CMakeLists.txt", "Makefile", "deno.json"];

fn readme_rank(name: &str) -> Option<u8> {
    match name.to_lowercase().as_str() {
        "readme.md" => Some(0),
        "readme.markdown" | "readme.mdx" => Some(1),
        "readme" | "readme.txt" | "readme.rst" | "readme.org" => Some(2),
        "index.md" | "about.md" | "_index.md" => Some(3),
        _ => None,
    }
}

/// Read the folder (blocking; call from a worker thread).
pub fn scan(path: &Path) -> Scan {
    let (mut dirs, mut files, mut bytes) = (0usize, 0usize, 0u64);
    let mut readme: Option<(u8, PathBuf)> = None;
    let mut manifests: Vec<String> = Vec::new();
    let mut has_git = false;
    let mut media: Vec<(PathBuf, bool)> = Vec::new();
    let mut all: Vec<(PathBuf, u64, i64)> = Vec::new();
    let mut by_kind: Vec<(Category, u64)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten().take(20_000) {
            let name = e.file_name().to_string_lossy().to_string();
            if name == ".git" {
                has_git = true;
                continue;
            }
            let Ok(meta) = std::fs::metadata(e.path()) else { continue };
            if meta.is_dir() {
                dirs += 1;
                continue;
            }
            files += 1;
            bytes += meta.len();
            if let Some(r) = readme_rank(&name) {
                if readme.as_ref().map(|(b, _)| r < *b).unwrap_or(true) {
                    readme = Some((r, e.path()));
                }
            }
            if MANIFESTS.contains(&name.as_str()) {
                manifests.push(name.clone());
            }
            let cat = Category::of(&name);
            match by_kind.iter_mut().find(|(c, _)| *c == cat) {
                Some(entry) => entry.1 += meta.len(),
                None => by_kind.push((cat, meta.len())),
            }
            if matches!(cat, Category::Images | Category::Video) && !name.ends_with(".svg") && !name.ends_with(".ico") {
                media.push((e.path(), cat == Category::Video));
            }
            if !name.starts_with('.') {
                let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
                all.push((e.path(), meta.len(), mtime));
            }
        }
    }
    let readme = readme.map(|(_, p)| p);

    let view = if !manifests.is_empty() || has_git {
        View::Project(project(path, &manifests, has_git))
    } else if media.len() >= 4 && media.len() * 10 >= files * 6 {
        media.sort_by(|a, b| a.0.file_name().cmp(&b.0.file_name()));
        media.truncate(240);
        View::Media(media)
    } else if !all.is_empty() {
        all.sort_by(|a, b| b.2.cmp(&a.2));
        all.truncate(8);
        by_kind.sort_by(|a, b| b.1.cmp(&a.1));
        View::Mixed(Mixed { recent: all, by_kind })
    } else {
        View::Plain
    };
    Scan { dirs, files, bytes, readme, view }
}

// ---------------------------------------------------------------- projects

fn toml_section(text: &str, section: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            inside = l == format!("[{section}]");
            continue;
        }
        if inside {
            if let Some((k, v)) = l.split_once('=') {
                out.push((k.trim().to_string(), v.trim().trim_matches('"').trim_matches('\'').to_string()));
            }
        }
    }
    out
}

fn yaml_top(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}:")))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|v| !v.is_empty() && v != "|" && v != ">")
}

fn yaml_block_len(text: &str, key: &str) -> usize {
    let mut inside = false;
    let mut n = 0;
    for l in text.lines() {
        if l.starts_with(&format!("{key}:")) {
            inside = true;
            continue;
        }
        if inside {
            if !l.starts_with(' ') && !l.trim().is_empty() {
                break;
            }
            // direct children: two-space indent, "name:"
            if l.starts_with("  ") && !l.starts_with("   ") && l.trim_end().contains(':') {
                n += 1;
            }
        }
    }
    n
}

fn project(path: &Path, manifests: &[String], has_git: bool) -> Project {
    let read = |f: &str| std::fs::read_to_string(path.join(f)).unwrap_or_default();
    let mut p = Project::default();
    let has = |f: &str| manifests.iter().any(|m| m == f);
    if has("Cargo.toml") {
        let t = read("Cargo.toml");
        let pkg = toml_section(&t, "package");
        let get = |k: &str| pkg.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        p.stack = if t.contains("[workspace]") && pkg.is_empty() { "Rust workspace".into() } else { "Rust · Cargo".into() };
        p.glyph = "\u{e7a8}".into();
        p.name = get("name");
        p.version = get("version");
        p.description = get("description");
        p.deps = Some(toml_section(&t, "dependencies").len());
        p.scripts = ["build", "run", "test"].iter().map(|s| format!("cargo {s}")).collect();
    } else if has("package.json") {
        let t = read("package.json");
        let j: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
        let s = |k: &str| j.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let deps = j.get("dependencies").and_then(|d| d.as_object()).map(|o| o.len()).unwrap_or(0)
            + j.get("devDependencies").and_then(|d| d.as_object()).map(|o| o.len()).unwrap_or(0);
        let fw = ["next", "react", "vue", "svelte", "@angular/core", "astro", "vite", "express", "electron"]
            .iter()
            .find(|f| j.pointer(&format!("/dependencies/{}", f.replace('/', "~1"))).is_some() || j.pointer(&format!("/devDependencies/{}", f.replace('/', "~1"))).is_some())
            .map(|f| f.trim_start_matches("@angular/").replace("core", "angular"));
        let pm = if path.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if path.join("yarn.lock").exists() {
            "yarn"
        } else if path.join("bun.lockb").exists() || path.join("bun.lock").exists() {
            "bun"
        } else {
            "npm"
        };
        let lang = if path.join("tsconfig.json").exists() { "TypeScript" } else { "JavaScript" };
        p.stack = match fw {
            Some(f) => format!("{lang} · {f} · {pm}"),
            None => format!("{lang} · {pm}"),
        };
        p.glyph = if lang == "TypeScript" { "\u{e628}".into() } else { "\u{e74e}".into() };
        p.name = s("name");
        p.version = s("version");
        p.description = s("description");
        p.deps = Some(deps);
        p.scripts = j.get("scripts").and_then(|v| v.as_object()).map(|o| o.keys().map(|k| format!("{pm} run {k}")).collect()).unwrap_or_default();
    } else if has("pubspec.yaml") {
        let t = read("pubspec.yaml");
        p.stack = if t.contains("flutter:") { "Dart · Flutter".into() } else { "Dart".into() };
        p.glyph = "\u{e798}".into();
        p.name = yaml_top(&t, "name");
        p.version = yaml_top(&t, "version");
        p.description = yaml_top(&t, "description");
        p.deps = Some(yaml_block_len(&t, "dependencies") + yaml_block_len(&t, "dev_dependencies"));
        p.scripts = if t.contains("flutter:") { vec!["flutter run".into(), "flutter test".into()] } else { vec!["dart run".into(), "dart test".into()] };
    } else if has("pyproject.toml") {
        let t = read("pyproject.toml");
        let mut sec = toml_section(&t, "project");
        if sec.is_empty() {
            sec = toml_section(&t, "tool.poetry");
        }
        let get = |k: &str| sec.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        p.stack = if path.join("uv.lock").exists() { "Python · uv".into() } else if t.contains("[tool.poetry]") { "Python · Poetry".into() } else { "Python".into() };
        p.glyph = "\u{e73c}".into();
        p.name = get("name");
        p.version = get("version");
        p.description = get("description");
        p.scripts = toml_section(&t, "project.scripts").into_iter().map(|(k, _)| k).collect();
    } else if has("go.mod") {
        let t = read("go.mod");
        p.stack = "Go".into();
        p.glyph = "\u{e627}".into();
        p.name = t.lines().find_map(|l| l.strip_prefix("module ")).map(|s| s.trim().to_string());
        p.version = t.lines().find_map(|l| l.strip_prefix("go ")).map(|s| format!("go {}", s.trim()));
        p.scripts = vec!["go build ./...".into(), "go test ./...".into()];
    } else if let Some(m) = manifests.first() {
        p.stack = match m.as_str() {
            "composer.json" => "PHP · Composer",
            "Gemfile" => "Ruby · Bundler",
            "pom.xml" => "Java · Maven",
            "build.gradle" | "build.gradle.kts" => "JVM · Gradle",
            "CMakeLists.txt" => "C/C++ · CMake",
            "deno.json" => "Deno",
            _ => "Make",
        }
        .into();
        p.glyph = "\u{f121}".into();
        if m == "Makefile" {
            p.scripts = read("Makefile")
                .lines()
                .filter_map(|l| l.split_once(':').map(|(t, _)| t))
                .filter(|t| !t.is_empty() && !t.starts_with(['.', '\t', ' ', '#']) && !t.contains(['$', '=', '%', ' ']))
                .take(8)
                .map(|t| format!("make {t}"))
                .collect();
        }
    } else {
        p.stack = "Git repository".into();
        p.glyph = "\u{e702}".into();
    }
    if p.name.is_none() {
        p.name = path.file_name().map(|s| s.to_string_lossy().to_string());
    }
    p.scripts.truncate(10);
    if has_git {
        p.git = git_info(path);
    }
    p
}

fn git_info(path: &Path) -> Option<GitInfo> {
    let repo = git2::Repository::open(path).ok()?;
    let branch = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().ok().map(str::to_string))
        .unwrap_or_else(|| "(no commits yet)".into());
    let mut commits = Vec::new();
    if let Ok(mut walk) = repo.revwalk() {
        if walk.push_head().is_ok() {
            for oid in walk.flatten().take(5) {
                if let Ok(c) = repo.find_commit(oid) {
                    let id = oid.to_string()[..7].to_string();
                    let summary = c.summary().ok().flatten().unwrap_or("").to_string();
                    let author = c.author().name().ok().unwrap_or("").to_string();
                    commits.push((id, summary, author, c.time().seconds()));
                }
            }
        }
    }
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(false).include_ignored(false);
    let changes = repo.statuses(Some(&mut opts)).map(|s| s.len()).unwrap_or(0);
    Some(GitInfo { branch, changes, commits })
}

// ---------------------------------------------------------------- widgets

pub fn relative(secs: i64) -> String {
    let now = glib::real_time() / 1_000_000;
    let d = now - secs;
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{} min ago", d / 60)
    } else if d < 86_400 {
        format!("{} h ago", d / 3600)
    } else if d < 2 * 86_400 {
        "yesterday".into()
    } else if d < 30 * 86_400 {
        format!("{} days ago", d / 86_400)
    } else {
        glib::DateTime::from_unix_local(secs).ok().and_then(|t| t.format("%e %b %Y").ok()).map(|s| s.trim().to_string()).unwrap_or_default()
    }
}

fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).build();
    if !class.is_empty() {
        l.add_css_class(class);
    }
    l
}

fn section(title: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(title).xalign(0.0).build();
    l.add_css_class("section-title");
    l.add_css_class("fv-section");
    l
}

/// Callbacks: `pick` selects an entry in the tree, `open` opens it.
pub struct Actions {
    pub pick: Rc<dyn Fn(PathBuf)>,
    pub open: Rc<dyn Fn(PathBuf)>,
}

pub fn build(path: &Path, view: View, actions: Actions, gen: Rc<std::cell::Cell<u64>>, my_gen: u64) -> Option<gtk::Widget> {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("folder-view");
    match view {
        View::Plain => return None,
        View::Project(p) => project_card(&content, p),
        View::Media(items) => media_grid(&content, items, actions, gen, my_gen),
        View::Mixed(m) => mixed(&content, path, m, actions),
    }
    let sc = gtk::ScrolledWindow::builder().child(&content).vexpand(true).hexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    Some(sc.upcast())
}

fn project_card(content: &gtk::Box, p: Project) {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.add_css_class("fv-card");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    let glyph = gtk::Label::new(None);
    glyph.set_markup(&format!("<span size=\"28pt\" foreground=\"{}\">{}</span>", crate::theme::color("accent"), glib::markup_escape_text(&p.glyph)));
    glyph.add_css_class("file-icon");
    top.append(&glyph);
    let titles = gtk::Box::new(gtk::Orientation::Vertical, 2);
    titles.set_valign(gtk::Align::Center);
    let name = p.name.clone().unwrap_or_default();
    let title = match &p.version {
        Some(v) => format!("{}  <span size=\"small\" foreground=\"{}\">{}</span>", glib::markup_escape_text(&name), crate::theme::color("dark_foreground"), glib::markup_escape_text(v)),
        None => glib::markup_escape_text(&name).to_string(),
    };
    let t = gtk::Label::builder().xalign(0.0).build();
    t.set_markup(&title);
    t.add_css_class("title-3");
    titles.append(&t);
    titles.append(&label(&p.stack, "fv-dim"));
    top.append(&titles);
    card.append(&top);
    if let Some(d) = &p.description {
        let d = label(d, "fv-desc");
        d.set_margin_top(6);
        card.append(&d);
    }
    let mut facts: Vec<String> = Vec::new();
    if let Some(n) = p.deps {
        facts.push(format!("{n} dependenc{}", if n == 1 { "y" } else { "ies" }));
    }
    if let Some(g) = &p.git {
        facts.push(format!("\u{e725} {}", g.branch));
        facts.push(if g.changes == 0 { "clean".into() } else { format!("{} changed", g.changes) });
    }
    if !facts.is_empty() {
        let f = label(&facts.join("   ·   "), "fv-dim");
        f.set_margin_top(6);
        card.append(&f);
    }
    content.append(&card);

    if !p.scripts.is_empty() {
        content.append(&section("COMMANDS"));
        let flow = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).column_spacing(6).row_spacing(6).max_children_per_line(6).build();
        flow.add_css_class("fv-chips");
        for s in &p.scripts {
            let chip = gtk::Label::new(Some(s));
            chip.add_css_class("fv-chip");
            chip.set_tooltip_text(Some("Click to copy"));
            let click = gtk::GestureClick::new();
            let s2 = s.clone();
            let c2 = chip.clone();
            click.connect_released(move |_, _, _, _| {
                c2.clipboard().set_text(&s2);
                c2.set_tooltip_text(Some("Copied"));
            });
            chip.add_controller(click);
            flow.insert(&chip, -1);
        }
        content.append(&flow);
    }
    if let Some(g) = p.git.filter(|g| !g.commits.is_empty()) {
        content.append(&section("RECENT COMMITS"));
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("fv-list");
        for (id, summary, author, time) in g.commits {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.add_css_class("fv-row");
            let i = gtk::Label::new(Some(&id));
            i.add_css_class("fv-mono");
            let s = gtk::Label::builder().label(&summary).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
            let a = gtk::Label::new(Some(&format!("{author} · {}", relative(time))));
            a.add_css_class("fv-dim");
            row.append(&i);
            row.append(&s);
            row.append(&a);
            list.append(&row);
        }
        content.append(&list);
    }
}

fn media_grid(content: &gtk::Box, items: Vec<(PathBuf, bool)>, actions: Actions, gen: Rc<std::cell::Cell<u64>>, my_gen: u64) {
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(12)
        .column_spacing(8)
        .row_spacing(8)
        .valign(gtk::Align::Start)
        .build();
    flow.add_css_class("fv-grid");
    let mut pics: Vec<(gtk::Picture, PathBuf, bool)> = Vec::new();
    for (p, video) in &items {
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 4);
        tile.add_css_class("fv-tile");
        let frame = gtk::Overlay::new();
        let pic = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).can_shrink(true).width_request(150).height_request(110).build();
        pic.add_css_class("fv-thumb");
        frame.set_child(Some(&pic));
        if *video {
            let badge = gtk::Label::new(Some("\u{f04b}"));
            badge.add_css_class("fv-play");
            badge.set_halign(gtk::Align::Center);
            badge.set_valign(gtk::Align::Center);
            frame.add_overlay(&badge);
        }
        tile.append(&frame);
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let l = gtk::Label::builder().label(&name).ellipsize(gtk::pango::EllipsizeMode::Middle).max_width_chars(18).build();
        l.add_css_class("fv-dim");
        tile.append(&l);
        tile.set_tooltip_text(Some(&name));
        flow.insert(&tile, -1);
        pics.push((pic, p.clone(), *video));
    }
    let paths: Vec<PathBuf> = items.iter().map(|(p, _)| p.clone()).collect();
    let pick = actions.pick.clone();
    let p2 = paths.clone();
    flow.connect_child_activated(move |_, child| {
        if let Some(p) = p2.get(child.index() as usize) {
            pick(p.clone());
        }
    });
    let dbl = gtk::GestureClick::new();
    let open = actions.open.clone();
    let f2 = flow.clone();
    dbl.connect_pressed(move |_, n, x, y| {
        if n == 2 {
            if let Some(child) = f2.child_at_pos(x as i32, y as i32) {
                if let Some(p) = paths.get(child.index() as usize) {
                    open(p.clone());
                }
            }
        }
    });
    flow.add_controller(dbl);
    content.append(&flow);

    // Load thumbnails one by one in the background; stop when the preview moves on.
    glib::spawn_future_local(async move {
        for (pic, path, video) in pics {
            if gen.get() != my_gen {
                return;
            }
            let p2 = path.clone();
            // Pixbuf can't cross threads: hand back its pixels instead.
            let px = gio::spawn_blocking(move || -> Option<(glib::Bytes, i32, i32, usize, bool)> {
                let src = if video { super::frame::video_still(&p2)? } else { super::frame::thumbnail_cache(&p2).unwrap_or_else(|| p2.clone()) };
                let pb = gdk::gdk_pixbuf::Pixbuf::from_file_at_scale(&src, 320, 320, true).ok()?;
                let pb = pb.apply_embedded_orientation().unwrap_or(pb);
                Some((pb.read_pixel_bytes(), pb.width(), pb.height(), pb.rowstride() as usize, pb.has_alpha()))
            })
            .await
            .ok()
            .flatten();
            match px {
                Some((bytes, w, h, stride, alpha)) => {
                    let fmt = if alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
                    pic.set_paintable(Some(&gdk::MemoryTexture::new(w, h, fmt, &bytes, stride)));
                }
                None => {
                    let glyph = icons::markup(&path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), false, false);
                    let l = gtk::Label::new(None);
                    l.set_markup(&format!("<span size=\"28pt\">{glyph}</span>"));
                    l.add_css_class("file-icon");
                    if let Some(frame) = pic.parent().and_downcast::<gtk::Overlay>() {
                        frame.add_overlay(&l);
                    }
                }
            }
        }
    });
}

fn mixed(content: &gtk::Box, _path: &Path, m: Mixed, actions: Actions) {
    content.append(&section("RECENTLY CHANGED"));
    let list = gtk::ListBox::new();
    list.add_css_class("fv-recent");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(true);
    let paths: Vec<PathBuf> = m.recent.iter().map(|(p, _, _)| p.clone()).collect();
    for (p, size, mtime) in &m.recent {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.add_css_class("fv-row");
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let icon = gtk::Label::new(None);
        icon.set_markup(&icons::markup(&name, false, false));
        icon.add_css_class("file-icon");
        // Name on top, size and age below: stays readable in a narrow pane.
        let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
        text.set_hexpand(true);
        let n = gtk::Label::builder().label(&name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        let d = gtk::Label::builder().label(format!("{} · {}", super::human(*size), relative(*mtime))).xalign(0.0).build();
        d.add_css_class("fv-sub");
        text.append(&n);
        text.append(&d);
        icon.set_valign(gtk::Align::Start);
        row.append(&icon);
        row.append(&text);
        list.append(&row);
    }
    let pick = actions.pick.clone();
    let p2 = paths.clone();
    list.connect_row_activated(move |_, row| {
        if let Some(p) = p2.get(row.index() as usize) {
            pick(p.clone());
        }
    });
    let dbl = gtk::GestureClick::new();
    let open = actions.open.clone();
    let l2 = list.clone();
    dbl.connect_pressed(move |_, n, _, y| {
        if n == 2 {
            if let Some(r) = l2.row_at_y(y as i32) {
                if let Some(p) = paths.get(r.index() as usize) {
                    open(p.clone());
                }
            }
        }
    });
    list.add_controller(dbl);
    content.append(&list);

    let total: u64 = m.by_kind.iter().map(|(_, b)| *b).sum();
    if total == 0 {
        return;
    }
    content.append(&section("SPACE BY TYPE"));
    let kinds = m.by_kind.clone();
    let bar = gtk::DrawingArea::builder().content_height(12).hexpand(true).build();
    bar.add_css_class("fv-bar");
    bar.set_draw_func(move |_, cr, w, h| {
        let mut x = 0.0;
        for (cat, bytes) in &kinds {
            let width = (*bytes as f64 / total as f64) * w as f64;
            if let Ok(c) = gdk::RGBA::parse(cat.color()) {
                cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 1.0);
                cr.rectangle(x, 0.0, width.max(1.0), h as f64);
                let _ = cr.fill();
            }
            x += width;
        }
    });
    content.append(&bar);
    let legend = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).column_spacing(18).row_spacing(4).max_children_per_line(4).build();
    legend.add_css_class("fv-legend");
    for (cat, bytes) in &m.by_kind {
        let l = gtk::Label::new(None);
        let pct = *bytes as f64 * 100.0 / total as f64;
        l.set_markup(&format!(
            "<span foreground=\"{}\">●</span>  {}  <span foreground=\"{}\">{} · {:.0}%</span>",
            cat.color(),
            cat.label(),
            crate::theme::color("dark_foreground"),
            super::human(*bytes),
            pct
        ));
        l.set_xalign(0.0);
        legend.insert(&l, -1);
    }
    content.append(&legend);
}

