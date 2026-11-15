//! The path bar: clickable breadcrumbs that turn into a typed path with
//! completion, zoxide-powered fuzzy jumps, pinned folders, drives and trash.

use crate::tree::{shorten_home, TRASH_ROOT};
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub path: PathBuf,
    pub kind: &'static str,
    pub is_dir: bool,
}

pub struct PathBar {
    pub widget: gtk::Box,
    frame: gtk::Box,
    stack: gtk::Stack,
    crumbs: gtk::Box,
    crumb_scroll: gtk::ScrolledWindow,
    pub entry: gtk::Entry,
    pin: gtk::Button,
    pub places_btn: gtk::MenuButton,
    places_box: gtk::Box,
    sugg_pop: gtk::Popover,
    sugg_list: gtk::ListBox,
    suggestions: RefCell<Vec<Suggestion>>,
    current: RefCell<PathBuf>,
    query_gen: Cell<u64>,
    pub on_navigate: RefCell<Option<Box<dyn Fn(PathBuf)>>>,
    pub on_cancel: RefCell<Option<Box<dyn Fn()>>>,
    me: RefCell<Weak<PathBar>>,
}

// ---------- bookmarks (shared with GTK / Nautilus) ----------

fn bookmarks_file() -> PathBuf {
    glib::user_config_dir().join("gtk-3.0/bookmarks")
}

pub fn bookmarks() -> Vec<(PathBuf, String)> {
    let Ok(text) = std::fs::read_to_string(bookmarks_file()) else { return vec![] };
    text.lines()
        .filter_map(|l| {
            let (uri, label) = l.split_once(' ').map(|(u, l)| (u, Some(l))).unwrap_or((l, None));
            let path = gio::File::for_uri(uri).path()?;
            let label = label.map(str::to_string).unwrap_or_else(|| {
                path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string())
            });
            Some((path, label))
        })
        .collect()
}

pub fn is_bookmarked(p: &Path) -> bool {
    bookmarks().iter().any(|(b, _)| b == p)
}

pub fn toggle_bookmark(p: &Path) {
    let file = bookmarks_file();
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let uri = gio::File::for_path(p).uri().to_string();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let before = lines.len();
    lines.retain(|l| l.split(' ').next() != Some(uri.as_str()));
    if lines.len() == before {
        lines.push(uri);
    }
    let _ = std::fs::create_dir_all(file.parent().unwrap());
    let mut out = lines.join("\n");
    out.push('\n');
    let _ = std::fs::write(file, out);
}

// ---------- zoxide ----------

pub fn zoxide_add(p: &Path) {
    let _ = std::process::Command::new("zoxide")
        .arg("add")
        .arg("--")
        .arg(p)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut c| {
            std::thread::spawn(move || c.wait());
        });
}

fn zoxide_query(terms: &str) -> Vec<PathBuf> {
    let words: Vec<&str> = terms.split_whitespace().collect();
    if words.is_empty() {
        return vec![];
    }
    let out = std::process::Command::new("zoxide").args(["query", "--list", "--"]).args(&words).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).lines().take(8).map(PathBuf::from).collect(),
        Err(_) => vec![],
    }
}

pub fn expand(text: &str, base: &Path) -> PathBuf {
    let t = text.trim();
    if t == "~" {
        return glib::home_dir();
    }
    if let Some(rest) = t.strip_prefix("~/") {
        return glib::home_dir().join(rest);
    }
    if let Some(rest) = t.strip_prefix("file://") {
        return gio::File::for_uri(&format!("file://{rest}")).path().unwrap_or_else(|| PathBuf::from(rest));
    }
    let p = PathBuf::from(t);
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

fn looks_like_path(t: &str) -> bool {
    t.starts_with('/') || t.starts_with('~') || t.starts_with("./") || t.starts_with("../") || t.starts_with("file://")
}

fn path_completions(text: &str, base: &Path) -> Vec<Suggestion> {
    let full = expand(text, base);
    let (dir, prefix) = if text.ends_with('/') {
        (full.clone(), String::new())
    } else {
        (
            full.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/")),
            full.file_name().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default(),
        )
    };
    let mut out: Vec<Suggestion> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if prefix.is_empty() && name.starts_with('.') {
                continue;
            }
            if !name.to_lowercase().starts_with(&prefix) {
                continue;
            }
            let path = e.path();
            let is_dir = path.is_dir();
            out.push(Suggestion { path, kind: if is_dir { "folder" } else { "file" }, is_dir });
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.cmp(&b.path)));
    out.truncate(14);
    out
}

impl PathBar {
    pub fn new() -> Rc<PathBar> {
        let places_btn = gtk::MenuButton::builder().css_classes(["flat", "crumb"]).tooltip_text("Places (Ctrl+D)").build();
        let pl = gtk::Label::new(Some("\u{f0219}"));
        pl.set_markup(&format!("<span foreground=\"{}\">\u{f4d3}</span>", crate::theme::color("accent")));
        pl.add_css_class("file-icon");
        places_btn.set_child(Some(&pl));
        let places_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        places_box.add_css_class("suggestions");
        let places_pop = gtk::Popover::builder().child(&places_box).has_arrow(false).build();
        places_btn.set_popover(Some(&places_pop));

        let crumbs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        crumbs.set_valign(gtk::Align::Center);
        let crumb_scroll = gtk::ScrolledWindow::builder()
            .child(&crumbs)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hscrollbar_policy(gtk::PolicyType::External)
            .hexpand(true)
            .build();
        let entry = gtk::Entry::builder().hexpand(true).placeholder_text("Type a path, or a few letters to jump…").build();
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.add_named(&crumb_scroll, Some("crumbs"));
        stack.add_named(&entry, Some("entry"));
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(80);

        let pin = gtk::Button::builder().css_classes(["flat", "crumb", "pin"]).tooltip_text("Pin folder (Ctrl+Shift+D)").build();
        pin.set_child(Some(&gtk::Label::new(Some("\u{f005}"))));

        let frame = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        frame.add_css_class("pathbar");
        frame.set_hexpand(true);
        frame.append(&places_btn);
        frame.append(&stack);
        frame.append(&pin);

        let sugg_list = gtk::ListBox::new();
        sugg_list.set_selection_mode(gtk::SelectionMode::Single);
        sugg_list.add_css_class("suggestions");
        let sugg_pop = gtk::Popover::builder()
            .child(&sugg_list)
            .has_arrow(false)
            .autohide(false)
            .can_focus(false)
            .position(gtk::PositionType::Bottom)
            .halign(gtk::Align::Start)
            .build();
        sugg_pop.set_parent(&frame);

        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        widget.append(&frame);
        widget.set_hexpand(true);

        let pb = Rc::new(PathBar {
            widget,
            frame,
            stack,
            crumbs,
            crumb_scroll,
            entry,
            pin,
            places_btn,
            places_box,
            sugg_pop,
            sugg_list,
            suggestions: RefCell::new(Vec::new()),
            current: RefCell::new(PathBuf::new()),
            query_gen: Cell::new(0),
            on_navigate: RefCell::new(None),
            on_cancel: RefCell::new(None),
            me: RefCell::new(Weak::new()),
        });
        *pb.me.borrow_mut() = Rc::downgrade(&pb);
        pb.setup();
        pb
    }

    fn navigate(&self, p: PathBuf) {
        self.stop_edit();
        if let Some(f) = self.on_navigate.borrow().as_ref() {
            f(p);
        }
    }

    fn setup(self: &Rc<Self>) {
        // Clicking empty space in the crumb bar starts editing.
        let click = gtk::GestureClick::new();
        let w = Rc::downgrade(self);
        click.connect_released(move |g, _, x, y| {
            let Some(pb) = w.upgrade() else { return };
            if let Some(picked) = pb.crumb_scroll.pick(x, y, gtk::PickFlags::DEFAULT) {
                if picked.ancestor(gtk::Button::static_type()).is_some() || picked.is::<gtk::Button>() {
                    return;
                }
            }
            g.set_state(gtk::EventSequenceState::Claimed);
            pb.start_edit(None);
        });
        self.crumb_scroll.add_controller(click);

        let w = Rc::downgrade(self);
        self.pin.connect_clicked(move |_| {
            if let Some(pb) = w.upgrade() {
                pb.toggle_pin();
            }
        });

        let w = Rc::downgrade(self);
        self.places_btn.popover().unwrap().connect_show(move |_| {
            if let Some(pb) = w.upgrade() {
                pb.fill_places();
            }
        });

        let w = Rc::downgrade(self);
        self.entry.connect_changed(move |_| {
            if let Some(pb) = w.upgrade() {
                pb.update_suggestions();
            }
        });
        let w = Rc::downgrade(self);
        self.entry.connect_activate(move |e| {
            let Some(pb) = w.upgrade() else { return };
            let text = e.text().to_string();
            let selected = pb.sugg_list.selected_row().map(|r| r.index() as usize);
            let sugg = pb.suggestions.borrow().clone();
            if let Some(i) = selected {
                if let Some(s) = sugg.get(i) {
                    return pb.navigate(s.path.clone());
                }
            }
            if text.trim().is_empty() {
                return pb.stop_edit();
            }
            let p = expand(&text, &pb.current.borrow());
            if p.exists() {
                pb.navigate(p);
            } else if let Some(s) = sugg.first() {
                pb.navigate(s.path.clone());
            } else {
                pb.entry.add_css_class("error");
                let e2 = pb.entry.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || e2.remove_css_class("error"));
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(pb) = w.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::Escape => {
                    pb.stop_edit();
                    if let Some(f) = pb.on_cancel.borrow().as_ref() {
                        f();
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Down | gdk::Key::Up => {
                    pb.move_selection(if key == gdk::Key::Down { 1 } else { -1 });
                    glib::Propagation::Stop
                }
                gdk::Key::Tab => {
                    pb.complete();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.entry.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let w = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(pb) = w.upgrade() {
                let w2 = Rc::downgrade(&pb);
                // Defer so clicks on suggestions still register.
                glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                    if let Some(pb) = w2.upgrade() {
                        if !pb.entry.has_focus() && pb.stack.visible_child_name().as_deref() == Some("entry") {
                            pb.stop_edit();
                        }
                    }
                });
            }
        });
        self.entry.add_controller(focus);

        let w = Rc::downgrade(self);
        self.sugg_list.connect_row_activated(move |_, row| {
            if let Some(pb) = w.upgrade() {
                let s = pb.suggestions.borrow().get(row.index() as usize).cloned();
                if let Some(s) = s {
                    pb.navigate(s.path);
                }
            }
        });
    }

    pub fn is_editing(&self) -> bool {
        self.stack.visible_child_name().as_deref() == Some("entry")
    }

    pub fn start_edit(&self, initial: Option<&str>) {
        let cur = self.current.borrow().clone();
        let text = match initial {
            Some(t) => t.to_string(),
            None if cur.as_os_str() == TRASH_ROOT => String::new(),
            None => {
                let s = shorten_home(&cur);
                if s.ends_with('/') { s } else { format!("{s}/") }
            }
        };
        self.frame.add_css_class("editing");
        self.stack.set_visible_child_name("entry");
        self.entry.set_text(&text);
        self.entry.grab_focus();
        if initial.is_none() {
            self.entry.select_region(0, -1);
        } else {
            self.entry.set_position(-1);
        }
    }

    pub fn stop_edit(&self) {
        self.frame.remove_css_class("editing");
        self.sugg_pop.popdown();
        self.stack.set_visible_child_name("crumbs");
    }

    fn move_selection(&self, d: i32) {
        let n = self.suggestions.borrow().len() as i32;
        if n == 0 {
            return;
        }
        let cur = self.sugg_list.selected_row().map(|r| r.index()).unwrap_or(-1);
        let next = if cur < 0 { if d > 0 { 0 } else { n - 1 } } else { (cur + d).rem_euclid(n) };
        if let Some(row) = self.sugg_list.row_at_index(next) {
            self.sugg_list.select_row(Some(&row));
        }
    }

    fn complete(&self) {
        let sugg = self.suggestions.borrow().clone();
        let idx = self.sugg_list.selected_row().map(|r| r.index() as usize).unwrap_or(0);
        if let Some(s) = sugg.get(idx) {
            let mut t = shorten_home(&s.path);
            if s.is_dir {
                t.push('/');
            }
            self.entry.set_text(&t);
            self.entry.set_position(-1);
        }
    }

    fn update_suggestions(&self) {
        let text = self.entry.text().to_string();
        let base = self.current.borrow().clone();
        let gen = self.query_gen.get() + 1;
        self.query_gen.set(gen);
        if !self.is_editing() {
            return;
        }
        if text.trim().is_empty() {
            self.show_suggestions(vec![]);
            return;
        }
        if looks_like_path(&text) {
            self.show_suggestions(path_completions(&text, &base));
            return;
        }
        // Fuzzy jump: bookmarks + zoxide frecency.
        let lower = text.to_lowercase();
        let mut out: Vec<Suggestion> = bookmarks()
            .into_iter()
            .filter(|(p, l)| crate::tree::fuzzy_contains(l, &lower) || crate::tree::fuzzy_contains(&p.to_string_lossy(), &lower))
            .map(|(p, _)| Suggestion { path: p, kind: "pinned", is_dir: true })
            .collect();
        let w = self.me.borrow().clone();
        glib::spawn_future_local(async move {
            let t2 = text.clone();
            let z = gio::spawn_blocking(move || zoxide_query(&t2)).await.unwrap_or_default();
            let Some(pb) = w.upgrade() else { return };
            if pb.query_gen.get() != gen {
                return;
            }
            for p in z {
                if !out.iter().any(|s| s.path == p) {
                    out.push(Suggestion { path: p, kind: "recent", is_dir: true });
                }
            }
            // Children of the current folder matching the text.
            if let Ok(rd) = std::fs::read_dir(&base) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_lowercase();
                    if name.starts_with(&lower) && out.len() < 14 {
                        let path = e.path();
                        let is_dir = path.is_dir();
                        out.push(Suggestion { path, kind: if is_dir { "folder" } else { "file" }, is_dir });
                    }
                }
            }
            pb.show_suggestions(out);
        });
    }

    fn show_suggestions(&self, list: Vec<Suggestion>) {
        while let Some(c) = self.sugg_list.first_child() {
            self.sugg_list.remove(&c);
        }
        if list.is_empty() {
            self.sugg_pop.popdown();
            *self.suggestions.borrow_mut() = list;
            return;
        }
        for s in &list {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let name = s.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let icon = gtk::Label::new(None);
            icon.set_markup(&crate::icons::markup(&name, s.is_dir, false));
            icon.add_css_class("file-icon");
            icon.add_css_class("sugg-icon");
            let path = gtk::Label::builder().label(shorten_home(&s.path)).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
            let kind = gtk::Label::new(Some(s.kind));
            kind.add_css_class("kind");
            row.append(&icon);
            row.append(&path);
            row.append(&kind);
            self.sugg_list.append(&row);
        }
        *self.suggestions.borrow_mut() = list;
        self.sugg_list.unselect_all();
        let w = self.frame.width();
        self.sugg_list.set_size_request(w.max(300) - 8, -1);
        self.sugg_pop.popup();
    }

    /// Update the breadcrumbs. `focus` is the selected item (file or folder).
    pub fn set_path(&self, focus: &Path) {
        *self.current.borrow_mut() = if focus.is_dir() || focus.as_os_str() == TRASH_ROOT {
            focus.to_path_buf()
        } else {
            focus.parent().map(Path::to_path_buf).unwrap_or_else(|| focus.to_path_buf())
        };
        while let Some(c) = self.crumbs.first_child() {
            self.crumbs.remove(&c);
        }
        let pinned = is_bookmarked(&self.current.borrow());
        if pinned {
            self.pin.add_css_class("pinned");
            self.pin.set_tooltip_text(Some("Unpin folder (Ctrl+Shift+D)"));
        } else {
            self.pin.remove_css_class("pinned");
            self.pin.set_tooltip_text(Some("Pin folder (Ctrl+Shift+D)"));
        }
        if focus.as_os_str() == TRASH_ROOT {
            let b = gtk::Button::builder().label("Trash").css_classes(["flat", "crumb", "last"]).build();
            self.crumbs.append(&b);
            return;
        }
        let home = glib::home_dir();
        let (start, rest): (PathBuf, PathBuf) = match focus.strip_prefix(&home) {
            Ok(r) => (home.clone(), r.to_path_buf()),
            Err(_) => (PathBuf::from("/"), focus.strip_prefix("/").map(Path::to_path_buf).unwrap_or_default()),
        };
        let mut segs: Vec<(String, PathBuf)> = vec![(if start == home { "~".into() } else { "/".into() }, start.clone())];
        let mut acc = start;
        for c in rest.components() {
            acc.push(c);
            segs.push((c.as_os_str().to_string_lossy().to_string(), acc.clone()));
        }
        let n = segs.len();
        for (i, (label, path)) in segs.into_iter().enumerate() {
            let last = i + 1 == n;
            let b = gtk::Button::builder().label(&label).css_classes(["flat", "crumb"]).tooltip_text(path.to_string_lossy()).build();
            if last {
                b.add_css_class("last");
            }
            let w = self.me.borrow().clone();
            let p = path.clone();
            b.connect_clicked(move |_| {
                if let Some(pb) = w.upgrade() {
                    pb.navigate(p.clone());
                }
            });
            self.crumbs.append(&b);
            if path.is_dir() && !(last && !focus.is_dir()) {
                let sep = gtk::MenuButton::builder().css_classes(["flat", "crumb-sep"]).tooltip_text("Subfolders").build();
                sep.set_child(Some(&gtk::Label::new(Some("\u{f105}"))));
                let pop = gtk::Popover::builder().has_arrow(false).build();
                sep.set_popover(Some(&pop));
                let w = self.me.borrow().clone();
                let p = path.clone();
                pop.connect_show(move |pop| {
                    if let Some(pb) = w.upgrade() {
                        pb.fill_subdirs(pop, &p);
                    }
                });
                self.crumbs.append(&sep);
            } else if !last {
                let l = gtk::Label::new(Some("\u{f105}"));
                l.add_css_class("dim");
                self.crumbs.append(&l);
            }
        }
        // Keep the deepest segment visible.
        let sc = self.crumb_scroll.clone();
        glib::idle_add_local_once(move || {
            let adj = sc.hadjustment();
            adj.set_value(adj.upper());
        });
    }

    fn fill_subdirs(&self, pop: &gtk::Popover, dir: &Path) {
        let list = gtk::ListBox::new();
        list.add_css_class("suggestions");
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        dirs.sort_by_key(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()));
        let hidden_last: Vec<PathBuf> = dirs.iter().filter(|p| !is_hidden(p)).chain(dirs.iter().filter(|p| is_hidden(p))).cloned().collect();
        if hidden_last.is_empty() {
            let l = gtk::Label::new(Some("No subfolders"));
            l.add_css_class("dim");
            l.set_margin_top(8);
            l.set_margin_bottom(8);
            l.set_margin_start(12);
            l.set_margin_end(12);
            pop.set_child(Some(&l));
            return;
        }
        for p in &hidden_last {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let icon = gtk::Label::new(None);
            icon.set_markup(&crate::icons::markup(&name, true, false));
            icon.add_css_class("file-icon");
            icon.add_css_class("sugg-icon");
            let l = gtk::Label::builder().label(&name).xalign(0.0).build();
            if is_hidden(p) {
                l.add_css_class("dim");
            }
            row.append(&icon);
            row.append(&l);
            list.append(&row);
        }
        let w = self.me.borrow().clone();
        let popc = pop.clone();
        list.connect_row_activated(move |_, row| {
            if let (Some(pb), Some(p)) = (w.upgrade(), hidden_last.get(row.index() as usize)) {
                popc.popdown();
                pb.navigate(p.clone());
            }
        });
        let sc = gtk::ScrolledWindow::builder().child(&list).propagate_natural_height(true).max_content_height(480).min_content_width(220).build();
        pop.set_child(Some(&sc));
        if let Some(r) = list.row_at_index(0) {
            list.select_row(Some(&r));
            r.grab_focus();
        }
    }

    pub fn toggle_pin(&self) {
        let cur = self.current.borrow().clone();
        if cur.as_os_str() == TRASH_ROOT {
            return;
        }
        toggle_bookmark(&cur);
        let c2 = cur.clone();
        self.set_path(&c2);
    }

    pub fn open_places(&self) {
        self.places_btn.popup();
    }

    fn fill_places(&self) {
        while let Some(c) = self.places_box.first_child() {
            self.places_box.remove(&c);
        }
        let w = self.me.borrow().clone();
        let pop = self.places_btn.popover().unwrap();
        let pick: crate::places::Pick = Rc::new(move |p| {
            pop.popdown();
            if let Some(pb) = w.upgrade() {
                pb.navigate(p);
            }
        });
        let cur = self.current.borrow().clone();
        let list = crate::places::build_list(pick, false, Some(&cur), None);
        let sc = gtk::ScrolledWindow::builder().child(&list).propagate_natural_height(true).max_content_height(560).min_content_width(300).build();
        self.places_box.append(&sc);
    }
}


fn is_hidden(p: &Path) -> bool {
    p.file_name().map(|s| s.to_string_lossy().starts_with('.')).unwrap_or(false)
}

/// Unmount / eject a mounted drive that contains `path`.
pub fn eject_for(path: &Path, done: impl FnOnce(Result<(), String>) + 'static) {
    let vm = gio::VolumeMonitor::get();
    let Some(m) = vm.mounts().into_iter().find(|m| m.root().path().map(|r| path.starts_with(&r) && r != Path::new("/")).unwrap_or(false)) else {
        return done(Err("Not on a removable drive".into()));
    };
    let cb = move |r: Result<(), glib::Error>| done(r.map_err(|e| e.to_string()));
    if m.can_eject() {
        m.eject_with_operation(gio::MountUnmountFlags::NONE, gio::MountOperation::NONE, gio::Cancellable::NONE, cb);
    } else {
        m.unmount_with_operation(gio::MountUnmountFlags::NONE, gio::MountOperation::NONE, gio::Cancellable::NONE, cb);
    }
}
