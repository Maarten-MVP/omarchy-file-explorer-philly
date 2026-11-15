//! Ctrl+P quick open (fuzzy, gitignore-aware) and Ctrl+Shift+F content search (ripgrep).

use crate::icons;
use gtk::{gdk, gio, glib, prelude::*};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

const MAX_INDEX: usize = 200_000;
const MAX_RESULTS: usize = 60;

thread_local! {
    static INDEX: RefCell<HashMap<PathBuf, Rc<Vec<String>>>> = RefCell::new(HashMap::new());
}

fn build_index(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "node_modules" | ".cache" | "target" | ".dart_tool")))
        .build();
    for e in walker.flatten() {
        if e.file_type().map(|t| t.is_file() || t.is_symlink()).unwrap_or(false) {
            if let Ok(rel) = e.path().strip_prefix(root) {
                out.push(rel.to_string_lossy().to_string());
                if out.len() >= MAX_INDEX {
                    break;
                }
            }
        }
    }
    out
}

/// Fuzzy-ranked matches: (rel path, score, matched char indices).
fn rank(items: &[String], query: &str) -> Vec<(String, u32, Vec<u32>)> {
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pat = Pattern::new(query, CaseMatching::Smart, Normalization::Smart, AtomKind::Fuzzy);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = Vec::new();
    for (i, s) in items.iter().enumerate() {
        if let Some(score) = pat.score(Utf32Str::new(s, &mut buf), &mut matcher) {
            // Prefer matches in the file name.
            let name = s.rsplit('/').next().unwrap_or(s);
            let mut nb = Vec::new();
            let bonus = pat.score(Utf32Str::new(name, &mut nb), &mut matcher).map(|s| s / 2).unwrap_or(0);
            scored.push((i, score + bonus));
        }
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| items[a.0].len().cmp(&items[b.0].len())));
    scored.truncate(MAX_RESULTS);
    scored
        .into_iter()
        .map(|(i, score)| {
            let mut idx = Vec::new();
            pat.indices(Utf32Str::new(&items[i], &mut buf), &mut matcher, &mut idx);
            idx.sort_unstable();
            idx.dedup();
            (items[i].clone(), score, idx)
        })
        .collect()
}

fn highlight(s: &str, offset: usize, idx: &[u32], color: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        let esc = glib::markup_escape_text(&c.to_string()).to_string();
        if idx.binary_search(&((i + offset) as u32)).is_ok() {
            out.push_str(&format!("<span foreground=\"{color}\" weight=\"bold\">{esc}</span>"));
        } else {
            out.push_str(&esc);
        }
    }
    out
}

// ---------------------------------------------------------------------------

pub struct QuickOpen {
    pub widget: gtk::Revealer,
    entry: gtk::Entry,
    list: gtk::ListBox,
    root: RefCell<PathBuf>,
    results: RefCell<Vec<String>>,
    gen: Cell<u64>,
    pub on_pick: RefCell<Option<Box<dyn Fn(PathBuf, bool)>>>,
    pub on_close: RefCell<Option<Box<dyn Fn()>>>,
    me: RefCell<Weak<QuickOpen>>,
}

impl QuickOpen {
    pub fn new() -> Rc<QuickOpen> {
        let entry = gtk::Entry::builder().placeholder_text("Search files by name").hexpand(true).build();
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .propagate_natural_height(true)
            .max_content_height(420)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        b.add_css_class("palette");
        b.append(&entry);
        b.append(&scroll);
        b.set_size_request(620, -1);
        let widget = gtk::Revealer::builder()
            .child(&b)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .transition_duration(120)
            .build();
        widget.set_visible(false);
        let q = Rc::new(QuickOpen {
            widget,
            entry,
            list,
            root: RefCell::new(PathBuf::new()),
            results: RefCell::new(Vec::new()),
            gen: Cell::new(0),
            on_pick: RefCell::new(None),
            on_close: RefCell::new(None),
            me: RefCell::new(Weak::new()),
        });
        *q.me.borrow_mut() = Rc::downgrade(&q);
        let w = Rc::downgrade(&q);
        q.entry.connect_changed(move |_| {
            if let Some(q) = w.upgrade() {
                q.update();
            }
        });
        let w = Rc::downgrade(&q);
        q.entry.connect_activate(move |_| {
            if let Some(q) = w.upgrade() {
                q.pick(false);
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(&q);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(q) = w.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::Escape => {
                    q.close();
                    glib::Propagation::Stop
                }
                gdk::Key::Down | gdk::Key::Up => {
                    q.move_sel(if key == gdk::Key::Down { 1 } else { -1 });
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter if state.contains(gdk::ModifierType::CONTROL_MASK) => {
                    q.pick(true);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        q.entry.add_controller(keys);
        let w = Rc::downgrade(&q);
        q.list.connect_row_activated(move |_, _| {
            if let Some(q) = w.upgrade() {
                q.pick(false);
            }
        });
        q
    }

    pub fn is_open(&self) -> bool {
        self.widget.reveals_child()
    }

    pub fn open(&self, root: &Path) {
        *self.root.borrow_mut() = root.to_path_buf();
        self.widget.set_visible(true);
        self.widget.set_reveal_child(true);
        self.entry.set_text("");
        self.entry.grab_focus();
        // Rebuild the index in the background; show cached results meanwhile.
        let w = self.me.borrow().clone();
        let r = root.to_path_buf();
        glib::spawn_future_local(async move {
            let r2 = r.clone();
            let idx = gio::spawn_blocking(move || build_index(&r2)).await.unwrap_or_default();
            INDEX.with(|i| i.borrow_mut().insert(r.clone(), Rc::new(idx)));
            if let Some(q) = w.upgrade() {
                if *q.root.borrow() == r && q.is_open() {
                    q.update();
                }
            }
        });
        self.update();
    }

    pub fn close(&self) {
        self.widget.set_reveal_child(false);
        let w = self.widget.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(130), move || {
            if !w.reveals_child() {
                w.set_visible(false);
            }
        });
        if let Some(f) = self.on_close.borrow().as_ref() {
            f();
        }
    }

    fn move_sel(&self, d: i32) {
        let n = self.results.borrow().len() as i32;
        if n == 0 {
            return;
        }
        let cur = self.list.selected_row().map(|r| r.index()).unwrap_or(0);
        let next = (cur + d).rem_euclid(n);
        if let Some(r) = self.list.row_at_index(next) {
            self.list.select_row(Some(&r));
            // Keep the row in view without moving focus away from the entry.
            if let Some(adj) = self.list.parent().and_then(|v| v.parent()).and_downcast::<gtk::ScrolledWindow>().map(|s| s.vadjustment()) {
                let (_, y) = r.translate_coordinates(&self.list, 0.0, 0.0).unwrap_or((0.0, 0.0));
                let h = r.height() as f64;
                if y < adj.value() {
                    adj.set_value(y);
                } else if y + h > adj.value() + adj.page_size() {
                    adj.set_value(y + h - adj.page_size());
                }
            }
        }
    }

    fn pick(&self, alt: bool) {
        let idx = self.list.selected_row().map(|r| r.index() as usize).unwrap_or(0);
        let rel = self.results.borrow().get(idx).cloned();
        if let Some(rel) = rel {
            let p = self.root.borrow().join(rel);
            self.close();
            if let Some(f) = self.on_pick.borrow().as_ref() {
                f(p, alt);
            }
        }
    }

    fn update(&self) {
        let gen = self.gen.get() + 1;
        self.gen.set(gen);
        let root = self.root.borrow().clone();
        let q = self.entry.text().to_string();
        let items = INDEX.with(|i| i.borrow().get(&root).cloned());
        let Some(items) = items else {
            self.render(vec![]);
            return;
        };
        if q.trim().is_empty() {
            // Recently changed files first would need history; show a short alphabetical list.
            let mut v: Vec<(String, u32, Vec<u32>)> = items.iter().take(MAX_RESULTS).map(|s| (s.clone(), 0, vec![])).collect();
            v.sort_by(|a, b| a.0.matches('/').count().cmp(&b.0.matches('/').count()).then(a.0.cmp(&b.0)));
            self.render(v);
            return;
        }
        let w = self.me.borrow().clone();
        glib::spawn_future_local(async move {
            let items2 = (*items).clone();
            let res = gio::spawn_blocking(move || rank(&items2, &q)).await.unwrap_or_default();
            if let Some(qo) = w.upgrade() {
                if qo.gen.get() == gen {
                    qo.render(res);
                }
            }
        });
    }

    fn render(&self, res: Vec<(String, u32, Vec<u32>)>) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let accent = crate::theme::color("accent");
        for (rel, _, idx) in &res {
            let (dir, name) = match rel.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), rel.clone()),
            };
            let name_off = if dir.is_empty() { 0 } else { dir.chars().count() + 1 };
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let icon = gtk::Label::new(None);
            icon.set_markup(&icons::markup(&name, false, false));
            icon.add_css_class("file-icon");
            let n = gtk::Label::new(None);
            n.set_markup(&highlight(&name, name_off, idx, &accent));
            let d = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
            d.set_markup(&highlight(&dir, 0, idx, &accent));
            d.add_css_class("path");
            row.append(&icon);
            row.append(&n);
            row.append(&d);
            self.list.append(&row);
        }
        *self.results.borrow_mut() = res.into_iter().map(|r| r.0).collect();
        if let Some(r) = self.list.row_at_index(0) {
            self.list.select_row(Some(&r));
        }
    }
}

// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: PathBuf,
    pub line: u32,
    pub text: String,
    /// Byte ranges of the match within `text`.
    pub ranges: Vec<(usize, usize)>,
}

fn run_rg(root: &Path, query: &str, case: bool, regex: bool, include: &str) -> Result<Vec<Hit>, String> {
    let mut cmd = std::process::Command::new("rg");
    cmd.arg("--json").arg("--max-count=100").arg("--max-columns=300").arg("--hidden").arg("-g").arg("!.git");
    if !case {
        cmd.arg("--smart-case");
    } else {
        cmd.arg("--case-sensitive");
    }
    if !regex {
        cmd.arg("--fixed-strings");
    }
    for g in include.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        cmd.arg("-g").arg(g);
    }
    cmd.arg("--").arg(query).arg(".");
    cmd.current_dir(root);
    let out = cmd.output().map_err(|e| format!("ripgrep not available: {e}"))?;
    let mut hits = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"] != "match" {
            continue;
        }
        let d = &v["data"];
        let Some(path) = d["path"]["text"].as_str() else { continue };
        let text = d["lines"]["text"].as_str().unwrap_or("").trim_end().to_string();
        let ranges = d["submatches"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| Some((m["start"].as_u64()? as usize, m["end"].as_u64()? as usize))).collect())
            .unwrap_or_default();
        hits.push(Hit {
            path: root.join(path.trim_start_matches("./")),
            line: d["line_number"].as_u64().unwrap_or(1) as u32,
            text,
            ranges,
        });
        if hits.len() >= 2000 {
            break;
        }
    }
    if !out.status.success() && hits.is_empty() && out.status.code() != Some(1) {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !err.is_empty() {
            return Err(err.lines().next().unwrap_or("search failed").to_string());
        }
    }
    Ok(hits)
}

enum Row {
    File(PathBuf, usize),
    Hit(Hit),
}

pub struct SearchPanel {
    pub widget: gtk::Box,
    pub entry: gtk::SearchEntry,
    include: gtk::Entry,
    case_btn: gtk::ToggleButton,
    regex_btn: gtk::ToggleButton,
    summary: gtk::Label,
    list: gtk::ListBox,
    rows: RefCell<Vec<Row>>,
    root: RefCell<PathBuf>,
    gen: Cell<u64>,
    pub on_select: RefCell<Option<Box<dyn Fn(PathBuf, u32)>>>,
    pub on_open: RefCell<Option<Box<dyn Fn(PathBuf, u32)>>>,
    me: RefCell<Weak<SearchPanel>>,
}

impl SearchPanel {
    pub fn new() -> Rc<SearchPanel> {
        let title = gtk::Label::builder().label("SEARCH").xalign(0.0).hexpand(true).build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.add_css_class("section-header");
        header.set_margin_start(10);
        header.set_margin_top(4);
        header.set_size_request(-1, 30);
        header.append(&title);

        let entry = gtk::SearchEntry::builder().placeholder_text("Search").hexpand(true).search_delay(220).build();
        let case_btn = gtk::ToggleButton::builder().label("Aa").tooltip_text("Match case").css_classes(["flat"]).build();
        let regex_btn = gtk::ToggleButton::builder().label(".*").tooltip_text("Use regular expression").css_classes(["flat"]).build();
        let row1 = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        row1.add_css_class("search-box");
        row1.append(&entry);
        row1.append(&case_btn);
        row1.append(&regex_btn);
        let include = gtk::Entry::builder().placeholder_text("files to include  (e.g. *.rs, src/**)").build();
        let row2 = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row2.add_css_class("search-box");
        include.set_hexpand(true);
        row2.append(&include);
        let summary = gtk::Label::builder().xalign(0.0).build();
        summary.add_css_class("search-summary");

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("navigation-sidebar");
        list.set_activate_on_single_click(false);
        let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("sidebar-pane");
        widget.append(&header);
        widget.append(&row1);
        widget.append(&row2);
        widget.append(&summary);
        widget.append(&scroll);

        let s = Rc::new(SearchPanel {
            widget,
            entry,
            include,
            case_btn,
            regex_btn,
            summary,
            list,
            rows: RefCell::new(Vec::new()),
            root: RefCell::new(PathBuf::new()),
            gen: Cell::new(0),
            on_select: RefCell::new(None),
            on_open: RefCell::new(None),
            me: RefCell::new(Weak::new()),
        });
        *s.me.borrow_mut() = Rc::downgrade(&s);
        let w = Rc::downgrade(&s);
        s.entry.connect_search_changed(move |_| {
            if let Some(s) = w.upgrade() {
                s.run();
            }
        });
        for b in [&s.case_btn, &s.regex_btn] {
            let w = Rc::downgrade(&s);
            b.connect_toggled(move |_| {
                if let Some(s) = w.upgrade() {
                    s.run();
                }
            });
        }
        let w = Rc::downgrade(&s);
        s.include.connect_activate(move |_| {
            if let Some(s) = w.upgrade() {
                s.run();
            }
        });
        let w = Rc::downgrade(&s);
        s.entry.connect_activate(move |_| {
            if let Some(s) = w.upgrade() {
                s.focus_results();
            }
        });
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&s);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Down {
                if let Some(s) = w.upgrade() {
                    s.focus_results();
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        s.entry.add_controller(keys);
        let w = Rc::downgrade(&s);
        s.list.connect_row_selected(move |_, row| {
            let (Some(s), Some(row)) = (w.upgrade(), row) else { return };
            let target = match s.rows.borrow().get(row.index() as usize) {
                Some(Row::Hit(h)) => Some((h.path.clone(), h.line)),
                Some(Row::File(p, _)) => Some((p.clone(), 0)),
                None => None,
            };
            if let (Some((p, l)), Some(f)) = (target, s.on_select.borrow().as_ref()) {
                f(p, l);
            };
        });
        let w = Rc::downgrade(&s);
        s.list.connect_row_activated(move |_, row| {
            let Some(s) = w.upgrade() else { return };
            let target = match s.rows.borrow().get(row.index() as usize) {
                Some(Row::Hit(h)) => Some((h.path.clone(), h.line)),
                Some(Row::File(p, _)) => Some((p.clone(), 1)),
                None => None,
            };
            if let (Some((p, l)), Some(f)) = (target, s.on_open.borrow().as_ref()) {
                f(p, l);
            };
        });
        s
    }

    pub fn set_root(&self, root: &Path) {
        if *self.root.borrow() != root {
            *self.root.borrow_mut() = root.to_path_buf();
            if !self.entry.text().is_empty() {
                self.run();
            }
        }
    }

    pub fn focus(&self) {
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    fn focus_results(&self) {
        if let Some(r) = self.list.row_at_index(1).or_else(|| self.list.row_at_index(0)) {
            self.list.select_row(Some(&r));
            r.grab_focus();
        }
    }

    fn run(&self) {
        let gen = self.gen.get() + 1;
        self.gen.set(gen);
        let q = self.entry.text().to_string();
        if q.is_empty() {
            self.render(vec![], None);
            return;
        }
        let root = self.root.borrow().clone();
        let case = self.case_btn.is_active();
        let regex = self.regex_btn.is_active();
        let include = self.include.text().to_string();
        self.summary.set_text("Searching…");
        let w = self.me.borrow().clone();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || run_rg(&root, &q, case, regex, &include)).await.unwrap_or(Err("failed".into()));
            if let Some(s) = w.upgrade() {
                if s.gen.get() == gen {
                    match res {
                        Ok(h) => s.render(h, None),
                        Err(e) => s.render(vec![], Some(e)),
                    }
                }
            }
        });
    }

    fn render(&self, hits: Vec<Hit>, err: Option<String>) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let mut rows = Vec::new();
        let root = self.root.borrow().clone();
        let mut files = 0;
        let total = hits.len();
        let mut i = 0;
        let accent = crate::theme::color("accent");
        let hl_bg = crate::theme::color("selection");
        while i < hits.len() {
            let p = hits[i].path.clone();
            let n = hits[i..].iter().take_while(|h| h.path == p).count();
            files += 1;
            let rel = p.strip_prefix(&root).unwrap_or(&p).to_string_lossy().to_string();
            let (dir, name) = rel.rsplit_once('/').map(|(d, n)| (d.to_string(), n.to_string())).unwrap_or((String::new(), rel.clone()));
            let fr = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            fr.add_css_class("tree-row");
            let icon = gtk::Label::new(None);
            icon.set_markup(&icons::markup(&name, false, false));
            icon.add_css_class("file-icon");
            let nl = gtk::Label::builder().label(&name).xalign(0.0).build();
            nl.add_css_class("name");
            let dl = gtk::Label::builder().label(&dir).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
            dl.add_css_class("detail");
            let cl = gtk::Label::new(Some(&n.to_string()));
            cl.add_css_class("badge");
            cl.add_css_class("dim");
            fr.append(&icon);
            fr.append(&nl);
            fr.append(&dl);
            fr.append(&cl);
            self.list.append(&fr);
            rows.push(Row::File(p.clone(), n));
            for h in &hits[i..i + n] {
                let hr = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                hr.set_margin_start(28);
                let ln = gtk::Label::builder().label(h.line.to_string()).xalign(1.0).build();
                ln.add_css_class("match-lnum");
                let tl = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
                tl.add_css_class("match-line");
                tl.set_markup(&match_markup(h, &accent, &hl_bg));
                hr.append(&ln);
                hr.append(&tl);
                self.list.append(&hr);
                rows.push(Row::Hit(h.clone()));
            }
            i += n;
        }
        *self.rows.borrow_mut() = rows;
        let summary = match err {
            Some(e) => e,
            None if self.entry.text().is_empty() => String::new(),
            None if total == 0 => "No results".into(),
            None => format!("{total}{} results in {files} files", if total >= 2000 { "+" } else { "" }),
        };
        self.summary.set_text(&summary);
    }
}

fn match_markup(h: &Hit, color: &str, bg: &str) -> String {
    // Trim leading whitespace but keep byte offsets right.
    let lead = h.text.len() - h.text.trim_start().len();
    let text = &h.text;
    let mut out = String::new();
    let mut pos = lead;
    for &(s, e) in &h.ranges {
        if s < pos || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        out.push_str(&glib::markup_escape_text(&text[pos..s]));
        out.push_str(&format!(
            "<span foreground=\"{color}\" background=\"{bg}\" weight=\"bold\">{}</span>",
            glib::markup_escape_text(&text[s..e])
        ));
        pos = e;
    }
    if pos <= text.len() {
        out.push_str(&glib::markup_escape_text(&text[pos..]));
    }
    out
}
