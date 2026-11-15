//! Source control panel: changed files, stage / unstage / discard, commit.

use crate::git::{self, Change, Commit, Flow, Status};
use crate::icons;
use gtk::{gdk, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

#[derive(Default)]
pub struct Callbacks {
    pub select: Option<Box<dyn Fn(PathBuf, Change)>>,
    pub open: Option<Box<dyn Fn(PathBuf)>>,
    pub toast: Option<Box<dyn Fn(String)>>,
    pub changed: Option<Box<dyn Fn()>>,
    pub confirm: Option<Box<dyn Fn(String, String, Box<dyn Fn()>)>>,
    /// A commit was picked in the commit list.
    pub show_commit: Option<Box<dyn Fn(PathBuf, Commit)>>,
}

pub struct GitPanel {
    pub widget: gtk::Box,
    stack: gtk::Stack,
    branch: gtk::Label,
    msg: gtk::TextView,
    msg_frame: gtk::Box,
    placeholder: gtk::Label,
    commit_btn: gtk::Button,
    list: gtk::ListBox,
    rows: RefCell<Vec<Change>>,
    pull_btn: gtk::Button,
    push_btn: gtk::Button,
    fetch_btn: gtk::Button,
    /// A fetch / pull / push is running.
    busy: Cell<bool>,
    commits: gtk::ListBox,
    commit_rows: RefCell<Vec<Commit>>,
    commits_count: gtk::Label,
    /// (root, HEAD, upstream) the commit list was loaded for.
    log_key: RefCell<String>,
    status: RefCell<Option<Rc<Status>>>,
    /// Latest status received while the panel was hidden; applied on map.
    pending: RefCell<Option<Option<Rc<Status>>>>,
    pub cb: RefCell<Callbacks>,
    me: RefCell<Weak<GitPanel>>,
}

fn header_button(icon: &str, tip: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).tooltip_text(tip).css_classes(["flat"]).build()
}

impl GitPanel {
    pub fn new() -> Rc<GitPanel> {
        let title = gtk::Label::builder().label("SOURCE CONTROL").xalign(0.0).hexpand(true).build();
        let refresh = header_button("view-refresh-symbolic", "Refresh (F5)");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.add_css_class("section-header");
        header.set_margin_start(10);
        header.set_margin_top(4);
        header.append(&title);
        header.append(&refresh);

        let branch = gtk::Label::builder().xalign(0.0).build();
        branch.add_css_class("branch");

        let msg = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(2)
            .bottom_margin(2)
            .build();
        msg.set_size_request(-1, 54);
        let placeholder = gtk::Label::builder()
            .xalign(0.0)
            .yalign(0.0)
            .can_target(false)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        placeholder.set_margin_top(2);
        placeholder.add_css_class("dim");
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&msg));
        overlay.add_overlay(&placeholder);
        let msg_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        msg_frame.add_css_class("commit-frame");
        msg_frame.append(&overlay);
        let commit_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        commit_box.add_css_class("commit-box");
        commit_box.append(&msg_frame);

        let commit_btn = gtk::Button::builder().css_classes(["suggested-action", "commit-btn"]).build();
        let commit_lbl = gtk::Label::new(Some("\u{f00c}  Commit"));
        commit_btn.set_child(Some(&commit_lbl));

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("navigation-sidebar");
        list.add_css_class("git-list");
        list.set_activate_on_single_click(false);
        let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

        // Push / pull state and buttons.
        let pull_btn = gtk::Button::builder().css_classes(["sync-btn"]).tooltip_text("Pull (fast-forward only)").hexpand(true).build();
        let push_btn = gtk::Button::builder().css_classes(["sync-btn"]).hexpand(true).build();
        let sync_btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sync_btns.add_css_class("sync-btns");
        sync_btns.set_homogeneous(true);
        sync_btns.append(&pull_btn);
        sync_btns.append(&push_btn);

        // Recent commits, like VS Code's graph.
        let commits_title = gtk::Label::builder().label("COMMITS").xalign(0.0).build();
        let commits_count = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        commits_count.add_css_class("commits-count");
        let fetch_btn = header_button("mail-send-receive-symbolic", "Fetch from the remote");
        let commits_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        commits_header.add_css_class("section-header");
        commits_header.add_css_class("commits-header");
        commits_header.set_margin_start(10);
        commits_header.append(&commits_title);
        commits_header.append(&commits_count);
        commits_header.append(&fetch_btn);
        let commits = gtk::ListBox::new();
        commits.set_selection_mode(gtk::SelectionMode::Single);
        commits.add_css_class("navigation-sidebar");
        commits.add_css_class("git-list");
        commits.add_css_class("commit-list");
        let commits_scroll = gtk::ScrolledWindow::builder().child(&commits).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        let commits_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        commits_box.append(&commits_header);
        commits_box.append(&commits_scroll);

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&scroll)
            .end_child(&commits_box)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .wide_handle(false)
            .vexpand(true)
            .build();
        paned.add_css_class("git-paned");
        // Start with the changes on top and the commits in the lower 40%.
        let placed = Cell::new(false);
        paned.connect_notify_local(Some("max-position"), move |p, _| {
            if !placed.get() && p.max_position() > 200 {
                placed.set(true);
                p.set_position(p.max_position() * 55 / 100);
            }
        });

        let repo_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
        repo_view.append(&branch);
        repo_view.append(&sync_btns);
        repo_view.append(&commit_box);
        repo_view.append(&commit_btn);
        repo_view.append(&paned);

        let none = gtk::Label::builder()
            .label("This folder isn't inside a git repository.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .valign(gtk::Align::Start)
            .build();
        none.add_css_class("git-empty");

        let stack = gtk::Stack::new();
        stack.add_named(&repo_view, Some("repo"));
        stack.add_named(&none, Some("none"));
        stack.set_vexpand(true);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("sidebar-pane");
        widget.append(&header);
        widget.append(&stack);

        let p = Rc::new(GitPanel {
            widget,
            stack,
            branch,
            msg,
            msg_frame,
            placeholder,
            commit_btn,
            list,
            rows: RefCell::new(Vec::new()),
            pull_btn,
            push_btn,
            fetch_btn,
            busy: Cell::new(false),
            commits,
            commit_rows: RefCell::new(Vec::new()),
            commits_count,
            log_key: RefCell::new(String::new()),
            status: RefCell::new(None),
            pending: RefCell::new(None),
            cb: RefCell::new(Callbacks::default()),
            me: RefCell::new(Weak::new()),
        });
        *p.me.borrow_mut() = Rc::downgrade(&p);
        let w = Rc::downgrade(&p);
        p.widget.connect_map(move |_| {
            if let Some(p) = w.upgrade() {
                if let Some(st) = p.pending.borrow_mut().take() {
                    p.set_status(st);
                }
                p.auto_fetch();
            }
        });
        let w = Rc::downgrade(&p);
        p.fetch_btn.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.remote("Fetching…", "Fetched", git::fetch);
            }
        });
        let w = Rc::downgrade(&p);
        p.pull_btn.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.remote("Pulling…", "Pulled", git::pull);
            }
        });
        let w = Rc::downgrade(&p);
        p.push_btn.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                let publish = p.status.borrow().as_ref().map(|s| s.upstream.is_none()).unwrap_or(false);
                p.remote(if publish { "Publishing…" } else { "Pushing…" }, if publish { "Published" } else { "Pushed" }, move |r| git::push(r, publish));
            }
        });
        let w = Rc::downgrade(&p);
        p.commits.connect_row_selected(move |_, row| {
            let (Some(p), Some(row)) = (w.upgrade(), row) else { return };
            let c = p.commit_rows.borrow().get(row.index() as usize).cloned();
            if let (Some(c), Some(root)) = (c, p.root()) {
                if let Some(f) = p.cb.borrow().show_commit.as_ref() {
                    f(root, c);
                }
            }
        });

        let w = Rc::downgrade(&p);
        refresh.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.notify_changed();
            }
        });
        let w = Rc::downgrade(&p);
        p.commit_btn.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.commit();
            }
        });
        let w = Rc::downgrade(&p);
        p.msg.buffer().connect_changed(move |b| {
            if let Some(p) = w.upgrade() {
                p.placeholder.set_visible(b.char_count() == 0);
            }
        });
        let focus = gtk::EventControllerFocus::new();
        let mf = p.msg_frame.clone();
        focus.connect_enter(move |_| mf.add_css_class("focused"));
        let mf = p.msg_frame.clone();
        focus.connect_leave(move |_| mf.remove_css_class("focused"));
        p.msg.add_controller(focus);
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&p);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(p) = w.upgrade() else { return glib::Propagation::Proceed };
            if state.contains(gdk::ModifierType::CONTROL_MASK) && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
                p.commit();
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::Escape || (key == gdk::Key::Down && p.cursor_on_last_line()) {
                p.focus_list();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        p.msg.add_controller(keys);

        let w = Rc::downgrade(&p);
        p.list.connect_row_selected(move |_, row| {
            let (Some(p), Some(row)) = (w.upgrade(), row) else { return };
            let ch = p.rows.borrow().get(row.index() as usize).cloned();
            let root = p.root();
            if let (Some(ch), Some(root)) = (ch, root) {
                if let Some(f) = p.cb.borrow().select.as_ref() {
                    f(root, ch);
                }
            }
        });
        let w = Rc::downgrade(&p);
        p.list.connect_row_activated(move |_, row| {
            let Some(p) = w.upgrade() else { return };
            let ch = p.rows.borrow().get(row.index() as usize).cloned();
            if let (Some(ch), Some(root)) = (ch, p.root()) {
                if let Some(f) = p.cb.borrow().open.as_ref() {
                    f(root.join(&ch.rel));
                }
            }
        });
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&p);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(p) = w.upgrade() else { return glib::Propagation::Proceed };
            let Some(ch) = p.selected() else { return glib::Propagation::Proceed };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::space => {
                    p.toggle_stage(&ch);
                    glib::Propagation::Stop
                }
                gdk::Key::Delete | gdk::Key::BackSpace if !ch.staged => {
                    p.discard(&ch);
                    glib::Propagation::Stop
                }
                gdk::Key::Up if p.list.selected_row().map(|r| r.index() == 0).unwrap_or(false) && !ctrl => {
                    p.msg.grab_focus();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        p.list.add_controller(keys);
        p.set_status(None);
        p
    }

    fn cursor_on_last_line(&self) -> bool {
        let b = self.msg.buffer();
        let it = b.iter_at_mark(&b.get_insert());
        it.line() == b.end_iter().line()
    }

    fn root(&self) -> Option<PathBuf> {
        self.status.borrow().as_ref().map(|s| s.root.clone())
    }

    fn selected(&self) -> Option<Change> {
        let row = self.list.selected_row()?;
        self.rows.borrow().get(row.index() as usize).cloned()
    }

    pub fn focus_list(&self) {
        if self.list.selected_row().is_none() {
            if let Some(r) = self.list.row_at_index(0) {
                self.list.select_row(Some(&r));
            }
        }
        match self.list.selected_row() {
            Some(r) => {
                r.grab_focus();
            }
            None => {
                self.msg.grab_focus();
            }
        }
    }

    pub fn focus_message(&self) {
        self.msg.grab_focus();
    }

    fn toast(&self, s: impl Into<String>) {
        if let Some(f) = self.cb.borrow().toast.as_ref() {
            f(s.into());
        }
    }

    fn notify_changed(&self) {
        if let Some(f) = self.cb.borrow().changed.as_ref() {
            f();
        }
    }

    pub fn set_status(&self, st: Option<Rc<Status>>) {
        if !self.widget.is_mapped() {
            *self.pending.borrow_mut() = Some(st);
            return;
        }
        let same = match (&st, self.status.borrow().as_ref()) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b) || **a == **b,
            _ => false,
        };
        if same {
            return;
        }
        let Some(st) = st else {
            *self.status.borrow_mut() = None;
            self.stack.set_visible_child_name("none");
            return;
        };
        self.stack.set_visible_child_name("repo");
        let b = match &st.upstream {
            Some(up) => format!("\u{e725}  {}  →  {up}", st.branch),
            None => format!("\u{e725}  {}", st.branch),
        };
        self.branch.set_text(&b);
        self.update_sync(&st);
        self.load_log(&st);
        self.placeholder.set_text(&format!("Message (Ctrl+Enter to commit on \"{}\")", st.branch));
        let label = if st.staged.is_empty() && !st.unstaged.is_empty() { "\u{f00c}  Commit All" } else { "\u{f00c}  Commit" };
        if let Some(l) = self.commit_btn.child().and_downcast::<gtk::Label>() {
            l.set_text(label);
        }
        self.commit_btn.set_sensitive(!st.staged.is_empty() || !st.unstaged.is_empty());

        let prev = self.selected().map(|c| (c.rel, c.staged));
        let had_focus = self.list.focus_child().is_some();
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let mut rows = Vec::new();
        rows.extend(st.staged.iter().cloned());
        rows.extend(st.unstaged.iter().cloned());
        let n_staged = st.staged.len();
        let n_unstaged = st.unstaged.len();
        for ch in &rows {
            self.list.append(&self.make_row(ch));
        }
        *self.rows.borrow_mut() = rows.clone();
        *self.status.borrow_mut() = Some(st);

        let w = self.me.borrow().clone();
        self.list.set_header_func(move |row, before| {
            let i = row.index() as usize;
            let staged_header = i == 0 && n_staged > 0;
            let changes_header = i == n_staged && n_unstaged > 0;
            if !(staged_header || changes_header) {
                row.set_header(None::<&gtk::Widget>);
                return;
            }
            let _ = before;
            let (title, count, staged) =
                if staged_header { ("STAGED CHANGES", n_staged, true) } else { ("CHANGES", n_unstaged, false) };
            let h = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            h.add_css_class("section-header");
            h.set_margin_start(6);
            let l = gtk::Label::builder().label(title).xalign(0.0).hexpand(true).build();
            let c = gtk::Label::new(Some(&count.to_string()));
            c.add_css_class("badge");
            c.add_css_class("dim");
            let btn = header_button(
                if staged { "list-remove-symbolic" } else { "list-add-symbolic" },
                if staged { "Unstage all" } else { "Stage all" },
            );
            let w2 = w.clone();
            btn.connect_clicked(move |_| {
                if let Some(p) = w2.upgrade() {
                    p.stage_all(!staged);
                }
            });
            h.append(&l);
            h.append(&btn);
            h.append(&c);
            row.set_header(Some(&h));
        });
        // Keep the selection on the same file when possible.
        let idx = prev.and_then(|(rel, staged)| {
            rows.iter().position(|c| c.rel == rel && c.staged == staged).or_else(|| rows.iter().position(|c| c.rel == rel))
        });
        if let Some(i) = idx {
            if let Some(r) = self.list.row_at_index(i as i32) {
                self.list.select_row(Some(&r));
                if had_focus {
                    r.grab_focus();
                }
            }
        }
    }

    /// The push / pull line and buttons.
    fn update_sync(&self, st: &Status) {
        let plural = |n: usize| if n == 1 { "commit" } else { "commits" };
        let text = match &st.upstream {
            None => "Not published: this branch has no upstream yet".to_string(),
            Some(_) => {
                let push = if st.ahead > 0 { format!("{} {} to push", st.ahead, plural(st.ahead)) } else { "Nothing to push".into() };
                let pull = if st.behind > 0 { format!("{} to pull", st.behind) } else { "nothing to pull".into() };
                let fetched = match st.fetched {
                    Some(t) => format!("fetched {}", rel_time(t)),
                    None => "never fetched".into(),
                };
                format!("{push} · {pull} · {fetched}")
            }
        };
        // The counts are on the buttons; the rest goes in the tooltips.
        self.pull_btn.set_tooltip_text(Some(&format!("Pull (fast-forward only)\n{text}")));
        self.fetch_btn.set_tooltip_text(Some(&match st.fetched {
            Some(t) => format!("Fetch from the remote (last fetch {})", rel_time(t)),
            None => "Fetch from the remote (never fetched)".into(),
        }));
        let busy = self.busy.get();
        self.pull_btn.set_label(&format!("↓  Pull{}", if st.behind > 0 { format!(" {}", st.behind) } else { String::new() }));
        self.pull_btn.set_sensitive(!busy && st.upstream.is_some() && st.behind > 0);
        if st.upstream.is_none() {
            self.push_btn.set_label("↑  Publish");
            self.push_btn.set_tooltip_text(Some("Push this branch and track it (git push -u)"));
        } else {
            self.push_btn.set_label(&format!("↑  Push{}", if st.ahead > 0 { format!(" {}", st.ahead) } else { String::new() }));
            self.push_btn.set_tooltip_text(Some(&format!("Push\n{text}")));
        }
        self.push_btn.set_sensitive(!busy && (st.upstream.is_none() || st.ahead > 0));
        self.pull_btn.set_css_classes(if st.behind > 0 { &["sync-btn", "pending"] } else { &["sync-btn"] });
        self.push_btn.set_css_classes(if st.ahead > 0 || st.upstream.is_none() { &["sync-btn", "pending"] } else { &["sync-btn"] });
        self.fetch_btn.set_sensitive(!busy && st.upstream.is_some());
    }

    /// Run a fetch / pull / push in the background, then refresh.
    fn remote(&self, doing: &str, done: &'static str, f: impl FnOnce(&Path) -> Result<(), String> + Send + 'static) {
        let Some(root) = self.root() else { return };
        if self.busy.replace(true) {
            return;
        }
        self.pull_btn.set_label(doing);
        for b in [&self.pull_btn, &self.push_btn, &self.fetch_btn] {
            b.set_sensitive(false);
        }
        let w = self.me.borrow().clone();
        glib::spawn_future_local(async move {
            let res = gtk::gio::spawn_blocking(move || f(&root)).await.unwrap_or(Err("failed".into()));
            let Some(p) = w.upgrade() else { return };
            p.busy.set(false);
            match res {
                Ok(()) => p.toast(done),
                Err(e) => p.toast(e),
            }
            // Restore the sync line now; the refresh updates it if anything moved.
            if let Some(st) = p.status.borrow().clone() {
                p.update_sync(&st);
            }
            p.notify_changed();
        });
    }

    /// Fetch quietly when the panel opens and the last fetch is over 5 minutes old.
    fn auto_fetch(&self) {
        let Some(st) = self.status.borrow().clone() else { return };
        if st.upstream.is_none() || self.busy.get() {
            return;
        }
        let now = glib::real_time() / 1_000_000;
        if st.fetched.map(|t| now - t < 300).unwrap_or(false) {
            return;
        }
        let root = st.root.clone();
        self.busy.set(true);
        let w = self.me.borrow().clone();
        glib::spawn_future_local(async move {
            let res = gtk::gio::spawn_blocking(move || git::fetch(&root)).await;
            let Some(p) = w.upgrade() else { return };
            p.busy.set(false);
            if let Some(st) = p.status.borrow().clone() {
                p.update_sync(&st);
            }
            if matches!(res, Ok(Ok(()))) {
                p.notify_changed();
            }
        });
    }

    /// Load the commit list when HEAD or the upstream moved.
    fn load_log(&self, st: &Status) {
        let key = format!("{}|{}|{}", st.root.display(), st.head_id, st.upstream_id);
        if *self.log_key.borrow() == key {
            return;
        }
        *self.log_key.borrow_mut() = key.clone();
        let root = st.root.clone();
        let w = self.me.borrow().clone();
        glib::spawn_future_local(async move {
            let commits = gtk::gio::spawn_blocking(move || git::log(&root, 200)).await.unwrap_or_default();
            let Some(p) = w.upgrade() else { return };
            if *p.log_key.borrow() != key {
                return;
            }
            p.fill_commits(commits);
        });
    }

    fn fill_commits(&self, commits: Vec<Commit>) {
        while let Some(c) = self.commits.first_child() {
            self.commits.remove(&c);
        }
        let n = commits.len();
        for (i, c) in commits.iter().enumerate() {
            self.commits.append(&commit_row(c, i == 0, i + 1 == n));
        }
        let out = commits.iter().filter(|c| c.flow == Flow::Outgoing).count();
        let inc = commits.iter().filter(|c| c.flow == Flow::Incoming).count();
        let mut count = String::new();
        if out > 0 {
            count.push_str(&format!("↑{out} "));
        }
        if inc > 0 {
            count.push_str(&format!("↓{inc}"));
        }
        self.commits_count.set_text(count.trim());
        *self.commit_rows.borrow_mut() = commits;
    }

    fn make_row(&self, ch: &Change) -> gtk::Box {
        let path = Path::new(&ch.rel);
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let dir = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.add_css_class("tree-row");
        row.add_css_class(ch.kind.class());
        row.set_margin_start(10);
        let icon = gtk::Label::new(None);
        icon.set_markup(&icons::markup(&name, false, false));
        icon.add_css_class("file-icon");
        let n = gtk::Label::builder().label(&name).xalign(0.0).build();
        n.add_css_class("name");
        let d = gtk::Label::builder().label(&dir).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
        d.add_css_class("detail");
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        actions.set_opacity(0.0);
        let w = self.me.borrow().clone();
        if !ch.staged {
            let discard = header_button("edit-undo-symbolic", "Discard changes (Delete)");
            let c = ch.clone();
            let w2 = w.clone();
            discard.connect_clicked(move |_| {
                if let Some(p) = w2.upgrade() {
                    p.discard(&c);
                }
            });
            actions.append(&discard);
        }
        let toggle = header_button(
            if ch.staged { "list-remove-symbolic" } else { "list-add-symbolic" },
            if ch.staged { "Unstage (Space)" } else { "Stage (Space)" },
        );
        let c = ch.clone();
        toggle.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.toggle_stage(&c);
            }
        });
        actions.append(&toggle);
        let badge = gtk::Label::new(Some(ch.kind.letter()));
        badge.add_css_class("badge");
        badge.set_margin_start(4);
        badge.set_margin_end(8);
        row.append(&icon);
        row.append(&n);
        row.append(&d);
        row.append(&actions);
        row.append(&badge);
        let motion = gtk::EventControllerMotion::new();
        let a = actions.clone();
        motion.connect_enter(move |_, _, _| a.set_opacity(1.0));
        let a = actions.clone();
        motion.connect_leave(move |_| a.set_opacity(0.0));
        row.add_controller(motion);
        row
    }

    fn run(&self, f: impl FnOnce(&Path) -> Result<(), String> + Send + 'static) {
        let Some(root) = self.root() else { return };
        let w = self.me.borrow().clone();
        glib::spawn_future_local(async move {
            let res = gtk::gio::spawn_blocking(move || f(&root)).await.unwrap_or(Err("failed".into()));
            if let Some(p) = w.upgrade() {
                if let Err(e) = res {
                    p.toast(e);
                }
                p.notify_changed();
            }
        });
    }

    pub fn toggle_stage(&self, ch: &Change) {
        let rel = vec![ch.rel.clone()];
        if ch.staged {
            self.run(move |r| git::unstage(r, &rel));
        } else {
            self.run(move |r| git::stage(r, &rel));
        }
    }

    pub fn stage_all(&self, stage: bool) {
        let Some(st) = self.status.borrow().clone() else { return };
        let rels: Vec<String> = if stage { st.unstaged.iter().map(|c| c.rel.clone()).collect() } else { st.staged.iter().map(|c| c.rel.clone()).collect() };
        if rels.is_empty() {
            return;
        }
        if stage {
            self.run(move |r| git::stage(r, &rels));
        } else {
            self.run(move |r| git::unstage(r, &rels));
        }
    }

    pub fn discard(&self, ch: &Change) {
        let c = ch.clone();
        let w = self.me.borrow().clone();
        let what = if ch.kind == git::Kind::Untracked {
            format!("\"{}\" is untracked. It will be moved to the trash.", ch.rel)
        } else {
            format!("Your changes to \"{}\" will be lost. This can't be undone.", ch.rel)
        };
        let action: Box<dyn Fn()> = Box::new(move || {
            if let Some(p) = w.upgrade() {
                let c = c.clone();
                p.run(move |r| git::discard(r, &c));
            }
        });
        if let Some(f) = self.cb.borrow().confirm.as_ref() {
            f("Discard changes?".into(), what, action);
        }
    }

    pub fn commit(&self) {
        let Some(st) = self.status.borrow().clone() else { return };
        let buf = self.msg.buffer();
        let msg = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
        if msg.is_empty() {
            self.msg.grab_focus();
            self.toast("Write a commit message first");
            return;
        }
        if st.staged.is_empty() && st.unstaged.is_empty() {
            self.toast("Nothing to commit");
            return;
        }
        let stage_all = st.staged.is_empty();
        let rels: Vec<String> = st.unstaged.iter().map(|c| c.rel.clone()).collect();
        let root = st.root.clone();
        let w = self.me.borrow().clone();
        self.commit_btn.set_sensitive(false);
        glib::spawn_future_local(async move {
            let res = gtk::gio::spawn_blocking(move || {
                if stage_all {
                    git::stage(&root, &rels)?;
                }
                git::commit(&root, &msg, false)
            })
            .await
            .unwrap_or(Err("failed".into()));
            let Some(p) = w.upgrade() else { return };
            p.commit_btn.set_sensitive(true);
            match res {
                Ok(out) => {
                    p.msg.buffer().set_text("");
                    let first = out.lines().next().unwrap_or("Committed").to_string();
                    p.toast(first);
                }
                Err(e) => p.toast(e.lines().last().unwrap_or("Commit failed").to_string()),
            }
            p.notify_changed();
        });
    }
}


/// "just now", "5 min ago", "3 h ago", "2 days ago", or the date.
pub fn rel_time(t: i64) -> String {
    let now = glib::real_time() / 1_000_000;
    let d = (now - t).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        86_400..=604_799 => {
            let n = d / 86_400;
            format!("{n} {} ago", if n == 1 { "day" } else { "days" })
        }
        _ => glib::DateTime::from_unix_local(t).and_then(|dt| dt.format("%-d %b %Y")).map(|s| s.to_string()).unwrap_or_default(),
    }
}

fn short_time(t: i64) -> String {
    let now = glib::real_time() / 1_000_000;
    let d = (now - t).max(0);
    match d {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", d / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        86_400..=2_591_999 => format!("{}d", d / 86_400),
        _ => glib::DateTime::from_unix_local(t).and_then(|dt| dt.format("%-d %b")).map(|s| s.to_string()).unwrap_or_default(),
    }
}

/// One commit: graph dot and line, subject, branch / tag badges, age.
fn commit_row(c: &Commit, first: bool, last: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("commit-row");
    row.add_css_class(match c.flow {
        Flow::Shared => "shared",
        Flow::Outgoing => "outgoing",
        Flow::Incoming => "incoming",
    });
    let graph = gtk::DrawingArea::new();
    graph.add_css_class("commit-graph");
    graph.set_content_width(14);
    let (merge, flow, head) = (c.merge, c.flow, c.head);
    graph.set_draw_func(move |a, cr, w, h| {
        let col = a.color();
        cr.set_source_rgba(col.red() as f64, col.green() as f64, col.blue() as f64, 0.55);
        cr.set_line_width(2.0);
        let x = w as f64 / 2.0;
        let y = h as f64 / 2.0;
        cr.move_to(x, if first { y } else { 0.0 });
        cr.line_to(x, if last { y } else { h as f64 });
        let _ = cr.stroke();
        let r = if merge || head { 4.5 } else { 3.5 };
        cr.set_source_rgba(col.red() as f64, col.green() as f64, col.blue() as f64, 1.0);
        if flow == Flow::Shared && !head {
            cr.arc(x, y, r, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        } else {
            // Not on both sides yet, or HEAD: a ring over the line.
            cr.set_operator(gtk::cairo::Operator::Clear);
            cr.arc(x, y, r, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_operator(gtk::cairo::Operator::Over);
            cr.set_line_width(2.0);
            cr.arc(x, y, r - 1.0, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        }
    });
    let summary = gtk::Label::builder().label(&c.summary).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    summary.add_css_class("commit-summary");
    row.append(&graph);
    row.append(&summary);
    for r in c.refs.iter().take(2) {
        let b = gtk::Label::new(Some(r));
        b.add_css_class("ref-badge");
        if r.contains('/') {
            b.add_css_class("remote");
        }
        b.set_ellipsize(gtk::pango::EllipsizeMode::End);
        b.set_max_width_chars(14);
        row.append(&b);
    }
    let age = gtk::Label::new(Some(&short_time(c.time)));
    age.add_css_class("commit-age");
    row.append(&age);
    let state = match c.flow {
        Flow::Outgoing => "\nNot pushed yet",
        Flow::Incoming => "\nOn the remote, not pulled yet",
        Flow::Shared => "",
    };
    let when = glib::DateTime::from_unix_local(c.time).and_then(|d| d.format("%-d %B %Y %H:%M")).map(|s| s.to_string()).unwrap_or_default();
    row.set_tooltip_text(Some(&format!("{}\n\n{} · {} · {}{state}", c.message, &c.id[..7.min(c.id.len())], c.author, when)));
    row
}
