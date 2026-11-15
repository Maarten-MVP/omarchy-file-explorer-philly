//! Source control panel: changed files, stage / unstage / discard, commit.

use crate::git::{self, Change, Status};
use crate::icons;
use gtk::{gdk, glib, prelude::*};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

#[derive(Default)]
pub struct Callbacks {
    pub select: Option<Box<dyn Fn(PathBuf, Change)>>,
    pub open: Option<Box<dyn Fn(PathBuf)>>,
    pub toast: Option<Box<dyn Fn(String)>>,
    pub changed: Option<Box<dyn Fn()>>,
    pub confirm: Option<Box<dyn Fn(String, String, Box<dyn Fn()>)>>,
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
        let placeholder = gtk::Label::builder().xalign(0.0).yalign(0.0).can_target(false).build();
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
        list.set_activate_on_single_click(false);
        let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

        let repo_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
        repo_view.append(&branch);
        repo_view.append(&commit_box);
        repo_view.append(&commit_btn);
        repo_view.append(&scroll);

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
        let mut b = format!("\u{e725}  {}", st.branch);
        if st.ahead > 0 {
            b.push_str(&format!("  ↑{}", st.ahead));
        }
        if st.behind > 0 {
            b.push_str(&format!("  ↓{}", st.behind));
        }
        self.branch.set_text(&b);
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
