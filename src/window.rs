//! The main window: path bar on top, preview on the left, explorer / source
//! control / search on the right. Also used in picker mode for the portal.

use crate::fileops::{self, plural};
use crate::gitpanel::GitPanel;
use crate::pathbar::{self, PathBar};
use crate::preview::Preview;
use crate::search::{QuickOpen, SearchPanel};
use crate::tree::{Node, Tree, TRASH_ROOT};
use crate::{git, state};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

#[derive(Clone, Debug)]
pub struct Filter {
    pub name: String,
    /// (0 = glob, 1 = mime type, pattern)
    pub rules: Vec<(u32, String)>,
}

impl Filter {
    pub fn matches(&self, name: &str, path: &Path) -> bool {
        self.rules.iter().any(|(kind, pat)| {
            if *kind == 0 {
                glob_match(&pat.to_lowercase(), &name.to_lowercase())
            } else {
                let ct = gio::content_type_guess(Some(path), None).0;
                let want = gio::content_type_from_mime_type(pat).unwrap_or_else(|| pat.clone().into());
                gio::content_type_is_a(&ct, &want) || pat == "*/*" || (pat.ends_with("/*") && gio::content_type_get_mime_type(&ct).map(|m| m.starts_with(pat.trim_end_matches('*'))).unwrap_or(false))
            }
        })
    }
}

/// Shell-style glob: `*`, `?` and `[...]` classes (Chrome sends `*.[mM][pP]4`).
fn glob_match(pat: &str, s: &str) -> bool {
    fn class(p: &[char], c: char) -> Option<(bool, usize)> {
        // p starts after '['; returns (matched, length including ']').
        let mut i = 0;
        let neg = matches!(p.first(), Some('!' | '^'));
        if neg {
            i += 1;
        }
        let mut hit = false;
        let mut first = true;
        while i < p.len() {
            if p[i] == ']' && !first {
                return Some((hit != neg, i + 1));
            }
            first = false;
            if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                hit |= p[i] <= c && c <= p[i + 2];
                i += 3;
            } else {
                hit |= p[i] == c;
                i += 1;
            }
        }
        None
    }
    fn m(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some('*'), _) => m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..])),
            (Some('?'), Some(_)) => m(&p[1..], &s[1..]),
            (Some('['), Some(&c)) => match class(&p[1..], c) {
                Some((true, n)) => m(&p[1 + n..], &s[1..]),
                Some((false, _)) => false,
                None => c == '[' && m(&p[1..], &s[1..]),
            },
            (Some(a), Some(b)) if a == b => m(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    m(&p, &s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn globs() {
        use super::glob_match as g;
        assert!(g("*.[mm][pp]4", "philly-vs-nautilus.mp4"));
        assert!(g("*.[a-z]", "x.c"));
        assert!(!g("*.[!c]", "x.c"));
        assert!(g("*.png", "a.png"));
        assert!(!g("*.png", "a.jpg"));
        assert!(g("*", "anything"));
    }
}

pub const PICKER_APP_ID: &str = "dev.philly.Picker";

pub struct PickerSpec {
    pub save: bool,
    pub multiple: bool,
    pub directory: bool,
    pub title: String,
    pub accept_label: Option<String>,
    pub current_name: Option<String>,
    pub current_folder: Option<PathBuf>,
    pub filters: Vec<Filter>,
    pub current_filter: Option<usize>,
    /// Called once with the chosen URIs (None = cancelled) and chosen filter.
    pub reply: RefCell<Option<Box<dyn FnOnce(Option<Vec<String>>, Option<usize>)>>>,
}

pub struct Win {
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    pathbar: Rc<PathBar>,
    tree: Rc<Tree>,
    preview: Rc<Preview>,
    git: Rc<GitPanel>,
    search: Rc<SearchPanel>,
    quick: Rc<QuickOpen>,
    side_stack: gtk::Stack,
    btn_files: gtk::ToggleButton,
    btn_git: gtk::ToggleButton,
    btn_places: gtk::ToggleButton,
    places: Rc<crate::places::PlacesPanel>,
    btn_search: gtk::ToggleButton,
    git_badge: gtk::Label,
    paned: gtk::Paned,
    status_branch: gtk::Label,
    status_sel: gtk::Label,
    shortcuts: gtk::Revealer,
    back: RefCell<Vec<PathBuf>>,
    forward: RefCell<Vec<PathBuf>>,
    current: RefCell<PathBuf>,
    /// Root to use for the next navigate() instead of the automatic one.
    force_root: RefCell<Option<PathBuf>>,
    /// The current root was chosen by the user (double-click / Open as root):
    /// keep it while navigating inside it.
    root_sticky: Cell<bool>,
    preview_timer: RefCell<Option<glib::SourceId>>,
    picker: Option<Rc<PickerSpec>>,
    picker_name: Option<gtk::Entry>,
    picker_accept: Option<gtk::Button>,
    picker_filter: Option<gtk::DropDown>,
    me: RefCell<Weak<Win>>,
}

fn act_button(glyph: &str, tip: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::builder().tooltip_text(tip).build();
    let l = gtk::Label::new(Some(glyph));
    l.add_css_class("file-icon");
    l.set_markup(&format!("<span size=\"15pt\">{glyph}</span>"));
    b.set_child(Some(&l));
    b
}

impl Win {
    pub fn new(app: &adw::Application, start: Option<PathBuf>, picker: Option<PickerSpec>) -> Rc<Win> {
        let st = state::load();
        let window = adw::ApplicationWindow::builder()
            .default_width(st.width)
            .default_height(st.height)
            .title("Philly")
            .build();
        if picker.is_none() {
            window.set_application(Some(app));
        } else {
            // Picker windows stay out of the GtkApplication: it would overwrite
            // their Wayland app id (see PICKER_APP_ID). Hold the app instead.
            let guard = std::cell::RefCell::new(Some(app.hold()));
            window.connect_destroy(move |_| {
                guard.borrow_mut().take();
            });
        }
        window.add_css_class("philly");

        // ---------- header ----------
        let pathbar = PathBar::new();
        let back_btn = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Back (Alt+Left)").css_classes(["flat"]).build();
        let fwd_btn = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Forward (Alt+Right)").css_classes(["flat"]).build();
        let up_btn = gtk::Button::builder().icon_name("go-up-symbolic").tooltip_text("Parent folder (Alt+Up)").css_classes(["flat"]).build();
        let header = adw::HeaderBar::new();
        header.pack_start(&back_btn);
        header.pack_start(&fwd_btn);
        header.pack_start(&up_btn);
        let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        title_box.set_hexpand(true);
        title_box.append(&pathbar.widget);
        title_box.set_margin_start(6);
        title_box.set_margin_end(6);
        header.set_title_widget(Some(&title_box));
        let help_btn = gtk::Button::builder().icon_name("input-keyboard-symbolic").tooltip_text("Keyboard shortcuts (Ctrl+/)").css_classes(["flat"]).build();
        header.pack_end(&help_btn);
        if picker.is_some() {
            header.set_show_end_title_buttons(false);
        }

        // ---------- panels ----------
        let tree = Tree::new();
        let preview = Preview::new();
        let git = GitPanel::new();
        let search = SearchPanel::new();
        let quick = QuickOpen::new();
        preview.split.set(st.split);
        tree.show_hidden.set(st.show_hidden);

        let files_header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        files_header.add_css_class("section-header");
        files_header.set_margin_start(10);
        files_header.set_margin_top(4);
        let hb = |icon: &str, tip: &str, action: &str| {
            gtk::Button::builder().icon_name(icon).tooltip_text(tip).css_classes(["flat"]).action_name(action).build()
        };
        let up = hb("go-up-symbolic", "Parent folder (Alt+↑)", "win.up");
        up.add_css_class("tree-up");
        files_header.append(&up);
        files_header.append(&tree.header_label);
        files_header.append(&hb("document-new-symbolic", "New file (Ctrl+N)", "win.new-file"));
        files_header.append(&hb("folder-new-symbolic", "New folder (Ctrl+Shift+N)", "win.new-folder"));
        files_header.append(&hb("view-refresh-symbolic", "Refresh (F5)", "win.refresh"));
        files_header.append(&hb("view-list-symbolic", "Collapse folders", "win.collapse"));
        let files_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
        files_view.add_css_class("sidebar-pane");
        files_view.append(&files_header);
        files_view.append(&tree.widget);

        let side_stack = gtk::Stack::new();
        side_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        side_stack.set_transition_duration(80);
        side_stack.set_hexpand(true);
        side_stack.add_named(&files_view, Some("files"));
        side_stack.add_named(&git.widget, Some("git"));
        side_stack.add_named(&search.widget, Some("search"));
        let places = crate::places::PlacesPanel::new();
        side_stack.add_named(&places.widget, Some("places"));

        let btn_files = act_button("\u{f0c5}", "Explorer (Ctrl+Shift+E)");
        let btn_search = act_button("\u{f002}", "Search (Ctrl+Shift+F)");
        let btn_git = act_button("\u{e725}", "Source control (Ctrl+Shift+G)");
        btn_search.set_group(Some(&btn_files));
        btn_git.set_group(Some(&btn_files));
        let btn_places = act_button("\u{f005}", "Places & pinned folders (Ctrl+Shift+B)");
        btn_places.set_group(Some(&btn_files));
        btn_files.set_active(true);
        let git_badge = gtk::Label::new(None);
        git_badge.add_css_class("badge");
        git_badge.set_halign(gtk::Align::End);
        git_badge.set_valign(gtk::Align::End);
        git_badge.set_can_target(false);
        git_badge.set_visible(false);
        let git_overlay = gtk::Overlay::new();
        git_overlay.set_child(Some(&btn_git));
        git_overlay.add_overlay(&git_badge);
        let activity = gtk::Box::new(gtk::Orientation::Vertical, 0);
        activity.add_css_class("activitybar");
        activity.append(&btn_files);
        activity.append(&btn_places);
        activity.append(&btn_search);
        activity.append(&git_overlay);

        let side = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        side.add_css_class("sidebar-pane");
        side.append(&side_stack);
        side.append(&activity);

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.add_css_class("main");
        paned.set_start_child(Some(&preview.widget));
        paned.set_end_child(Some(&side));
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(false);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        preview.widget.set_size_request(260, -1);
        side.set_size_request(260, -1);
        // Restore the sidebar width (not the divider position): windows come in
        // very different sizes when tiled, and the sidebar is the fixed part.
        let side_w = st.side;
        let applied = Rc::new(Cell::new(false));
        paned.connect_notify_local(Some("max-position"), move |p, _| {
            let total = p.width();
            if total > 0 && !applied.replace(true) {
                let side = side_w.clamp(240, (total as f64 * 0.6) as i32);
                p.set_position(total - side);
            }
        });

        // ---------- shortcuts sheet ----------
        let shortcuts = gtk::Revealer::builder()
            .child(&shortcuts_sheet())
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .build();
        shortcuts.set_visible(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&paned));
        overlay.add_overlay(&quick.widget);
        overlay.add_overlay(&shortcuts);
        overlay.set_vexpand(true);

        // ---------- status bar ----------
        let status_branch = gtk::Label::new(None);
        status_branch.add_css_class("branch-pill");
        let status_sel = gtk::Label::builder().hexpand(true).xalign(0.0).margin_start(16).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let status_hint = gtk::Label::new(Some("Ctrl+/ shortcuts"));
        let status = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        status.add_css_class("statusbar");
        status.append(&status_branch);
        status.append(&status_sel);
        status.append(&status_hint);
        let branch_click = gtk::GestureClick::new();
        status_branch.add_controller(branch_click.clone());

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&overlay);

        // ---------- picker bar ----------
        let picker = picker.map(Rc::new);
        let mut picker_name = None;
        let mut picker_accept = None;
        let mut picker_filter = None;
        if let Some(spec) = &picker {
            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            bar.add_css_class("picker-bar");
            if spec.save {
                let l = gtk::Label::new(Some("Name"));
                l.add_css_class("dim");
                bar.append(&l);
                let e = gtk::Entry::builder().hexpand(true).text(spec.current_name.clone().unwrap_or_default()).build();
                bar.append(&e);
                picker_name = Some(e);
            } else {
                let hint = gtk::Label::builder()
                    .label(if spec.directory { "Select a folder" } else if spec.multiple { "Select one or more files" } else { "Select a file" })
                    .xalign(0.0)
                    .hexpand(true)
                    .build();
                hint.add_css_class("dim");
                bar.append(&hint);
            }
            if !spec.filters.is_empty() {
                let names: Vec<&str> = spec.filters.iter().map(|f| f.name.as_str()).collect();
                let dd = gtk::DropDown::from_strings(&names);
                dd.set_selected(spec.current_filter.unwrap_or(0) as u32);
                bar.append(&dd);
                picker_filter = Some(dd);
            }
            let cancel = gtk::Button::with_label("Cancel");
            cancel.set_action_name(Some("win.picker-cancel"));
            let accept_text = spec.accept_label.clone().map(|s| s.replace('_', "")).unwrap_or_else(|| {
                if spec.save { "Save".into() } else if spec.directory { "Select".into() } else { "Open".into() }
            });
            let accept = gtk::Button::with_label(&accept_text);
            accept.add_css_class("suggested-action");
            accept.set_action_name(Some("win.picker-accept"));
            bar.append(&cancel);
            bar.append(&accept);
            picker_accept = Some(accept);
            content.append(&bar);
            window.set_title(Some(&format!("Philly — {}", spec.title)));
            window.set_default_size(st.width.min(1200), st.height.min(760));
            if !spec.multiple {
                // Single selection still uses MultiSelection; we only take the first.
            }
        } else {
            content.append(&status);
        }

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&content));
        window.set_content(Some(&toasts));

        let win = Rc::new(Win {
            window,
            toasts,
            pathbar,
            tree,
            preview,
            git,
            search,
            quick,
            side_stack,
            btn_files,
            btn_git,
            btn_places,
            places,
            btn_search,
            git_badge,
            paned,
            status_branch,
            status_sel,
            shortcuts,
            back: RefCell::new(Vec::new()),
            forward: RefCell::new(Vec::new()),
            current: RefCell::new(PathBuf::new()),
            force_root: RefCell::new(None),
            root_sticky: Cell::new(false),
            preview_timer: RefCell::new(None),
            picker,
            picker_name,
            picker_accept,
            picker_filter,
            me: RefCell::new(Weak::new()),
        });
        *win.me.borrow_mut() = Rc::downgrade(&win);

        let w = Rc::downgrade(&win);
        back_btn.connect_clicked(move |_| w.upgrade().map(|w| w.go_back()).unwrap_or(()));
        let w = Rc::downgrade(&win);
        fwd_btn.connect_clicked(move |_| w.upgrade().map(|w| w.go_forward()).unwrap_or(()));
        let w = Rc::downgrade(&win);
        up_btn.connect_clicked(move |_| w.upgrade().map(|w| w.go_up()).unwrap_or(()));
        let w = Rc::downgrade(&win);
        help_btn.connect_clicked(move |_| w.upgrade().map(|w| w.toggle_shortcuts()).unwrap_or(()));
        let w = Rc::downgrade(&win);
        branch_click.connect_released(move |_, _, _, _| w.upgrade().map(|w| w.show_side("git")).unwrap_or(()));

        win.wire();
        win.setup_actions();
        win.setup_keys();
        win.setup_picker();

        let start = start.unwrap_or_else(|| {
            win.picker
                .as_ref()
                .and_then(|p| p.current_folder.clone())
                .or_else(|| if win.picker.is_some() { state::load().last_picker_dir.filter(|p| p.is_dir()) } else { None })
                .unwrap_or_else(glib::home_dir)
        });
        win.navigate(&start, true);
        if win.picker.is_some() {
            // GTK takes a window's Wayland app id from the program name when it
            // first shows the window. A separate id lets Hyprland float pickers.
            let prev = glib::prgname();
            glib::set_prgname(Some(PICKER_APP_ID));
            win.window.present();
            glib::set_prgname(prev.as_deref());
        } else {
            win.window.present();
        }
        if let (Some(spec), Some(e)) = (&win.picker, &win.picker_name) {
            if spec.save {
                e.grab_focus();
                let name = e.text().to_string();
                let stem = name.rsplit_once('.').map(|(s, _)| s.chars().count() as i32).unwrap_or(-1);
                e.select_region(0, stem);
            }
        } else {
            win.tree.focus();
        }
        win
    }

    fn weak(&self) -> Weak<Win> {
        self.me.borrow().clone()
    }

    // ---------------------------------------------------------------- wiring

    fn wire(self: &Rc<Self>) {
        let w = self.weak();
        *self.pathbar.on_navigate.borrow_mut() = Some(Box::new(move |p| {
            if let Some(w) = w.upgrade() {
                w.navigate(&p, true);
                w.tree.focus();
            }
        }));
        let w = self.weak();
        *self.pathbar.on_cancel.borrow_mut() = Some(Box::new(move || {
            if let Some(w) = w.upgrade() {
                w.tree.focus();
            }
        }));

        let w = self.weak();
        self.tree.cb.borrow_mut().selection = Some(Box::new(move |sel| {
            if let Some(w) = w.upgrade() {
                w.on_selection(sel);
            }
        }));
        let w = self.weak();
        self.tree.cb.borrow_mut().open_root = Some(Box::new(move |p| {
            if let Some(w) = w.upgrade() {
                w.open_as_root(&p);
            }
        }));
        let w = self.weak();
        self.tree.cb.borrow_mut().activate = Some(Box::new(move |n| {
            if let Some(w) = w.upgrade() {
                w.on_activate(n);
            }
        }));
        let w = self.weak();
        self.tree.cb.borrow_mut().context = Some(Box::new(move |n, x, y| {
            if let Some(w) = w.upgrade() {
                w.context_menu(n, x, y);
            }
        }));
        let w = self.weak();
        self.tree.cb.borrow_mut().dropped = Some(Box::new(move |files, dest, action| {
            if let Some(w) = w.upgrade() {
                let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
                w.transfer(paths, dest, action == gdk::DragAction::MOVE);
            }
        }));
        let w = self.weak();
        self.tree.cb.borrow_mut().status_changed = Some(Box::new(move || {
            if let Some(w) = w.upgrade() {
                w.on_git_status();
            }
        }));

        let w = self.weak();
        self.git.cb.borrow_mut().select = Some(Box::new(move |root, ch| {
            if let Some(w) = w.upgrade() {
                w.preview.show_diff(&root, &ch);
            }
        }));
        let w = self.weak();
        self.git.cb.borrow_mut().open = Some(Box::new(move |p| {
            if let Some(w) = w.upgrade() {
                w.open_editor(&p, None);
            }
        }));
        let w = self.weak();
        self.git.cb.borrow_mut().toast = Some(Box::new(move |s| {
            if let Some(w) = w.upgrade() {
                w.toast(&s);
            }
        }));
        let w = self.weak();
        self.git.cb.borrow_mut().changed = Some(Box::new(move || {
            if let Some(w) = w.upgrade() {
                w.tree.reload_git();
            }
        }));
        let w = self.weak();
        self.git.cb.borrow_mut().confirm = Some(Box::new(move |title, body, action| {
            if let Some(w) = w.upgrade() {
                w.confirm(&title, &body, "Discard", action);
            }
        }));

        let w = self.weak();
        *self.search.on_select.borrow_mut() = Some(Box::new(move |p, line| {
            if let Some(w) = w.upgrade() {
                if line > 0 {
                    w.preview.show_file_at(&p, line);
                } else {
                    w.preview.show_file(&p);
                }
            }
        }));
        let w = self.weak();
        *self.search.on_open.borrow_mut() = Some(Box::new(move |p, line| {
            if let Some(w) = w.upgrade() {
                w.open_editor(&p, Some(line));
            }
        }));

        let w = self.weak();
        *self.quick.on_pick.borrow_mut() = Some(Box::new(move |p, alt| {
            if let Some(w) = w.upgrade() {
                w.show_side("files");
                w.navigate(&p, true);
                w.tree.focus();
                if alt {
                    w.open_editor(&p, None);
                }
            }
        }));
        let w = self.weak();
        *self.quick.on_close.borrow_mut() = Some(Box::new(move || {
            if let Some(w) = w.upgrade() {
                w.tree.focus();
            }
        }));

        let w = self.weak();
        let w = self.weak();
        *self.preview.on_pick.borrow_mut() = Some(Box::new(move |p| {
            if let Some(w) = w.upgrade() {
                if !w.tree.reveal(&p) {
                    w.navigate(&p, true);
                }
            }
        }));
        let w = self.weak();
        *self.preview.open_cb.borrow_mut() = Some(Box::new(move |p| {
            if let Some(w) = w.upgrade() {
                w.open_default(&p);
            }
        }));
        let w = self.weak();
        *self.preview.on_open.borrow_mut() = Some(Box::new(move |p| {
            if let Some(w) = w.upgrade() {
                w.open_default(&p);
            }
        }));

        let w = self.weak();
        *self.places.on_pick.borrow_mut() = Some(Box::new(move |p| {
            if let Some(w) = w.upgrade() {
                w.navigate(&p, true);
                w.show_side("files");
            }
        }));

        for (btn, name) in [(&self.btn_files, "files"), (&self.btn_git, "git"), (&self.btn_search, "search"), (&self.btn_places, "places")] {
            let w = self.weak();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    if let Some(w) = w.upgrade() {
                        w.side_stack.set_visible_child_name(name);
                        w.on_side_changed(name);
                    }
                }
            });
        }

        let w = self.weak();
        self.window.connect_close_request(move |win| {
            if let Some(w) = w.upgrade() {
                w.save_state();
                w.picker_finish(None);
            }
            let _ = win;
            glib::Propagation::Proceed
        });
        let w = self.weak();
        crate::theme::on_change(move || {
            if let Some(w) = w.upgrade() {
                w.tree.restyle();
                w.on_git_status();
            }
        });
    }

    fn save_state(&self) {
        let mut st = state::load();
        if self.picker.is_none() {
            let (w, h) = self.window.default_size();
            st.width = w;
            st.height = h;
            let total = self.paned.width();
            if total > 0 {
                st.side = total - self.paned.position();
            }
            st.split = self.preview.split.get();
            st.show_hidden = self.tree.show_hidden.get();
        } else {
            let r = self.tree.root();
            if r.as_os_str() != TRASH_ROOT {
                st.last_picker_dir = Some(r);
            }
        }
        state::save(&st);
    }

    fn on_side_changed(&self, name: &str) {
        match name {
            "git" => {
                self.tree.reload_git();
                self.git.focus_list();
            }
            "search" => {
                self.search.set_root(&self.tree.root());
                self.search.focus();
            }
            "places" => self.places.focus(),
            _ => self.tree.focus(),
        }
    }

    pub fn show_side(&self, name: &str) {
        let btn = match name {
            "git" => &self.btn_git,
            "search" => &self.btn_search,
            "places" => &self.btn_places,
            _ => &self.btn_files,
        };
        if btn.is_active() {
            self.on_side_changed(name);
        } else {
            btn.set_active(true);
        }
    }

    fn on_git_status(&self) {
        let st = self.tree.status.borrow().clone();
        match &st {
            Some(s) => {
                let n = s.count();
                self.git_badge.set_text(&n.to_string());
                self.git_badge.set_visible(n > 0);
                let mut b = format!("\u{e725} {}", s.branch);
                if s.ahead > 0 {
                    b.push_str(&format!(" ↑{}", s.ahead));
                }
                if s.behind > 0 {
                    b.push_str(&format!(" ↓{}", s.behind));
                }
                if n > 0 {
                    b.push_str(&format!("  ·  {} changed", n));
                }
                self.status_branch.set_text(&b);
            }
            None => {
                self.git_badge.set_visible(false);
                self.status_branch.set_text(if self.tree.is_trash() { "\u{f1f8} Trash" } else { "" });
            }
        }
        self.git.set_status(st);
    }

    fn on_selection(&self, sel: Vec<Node>) {
        if let Some(id) = self.preview_timer.borrow_mut().take() {
            id.remove();
        }
        let info = match sel.len() {
            0 => String::new(),
            1 => {
                let n = &sel[0];
                if let Some(t) = &n.trash {
                    format!("Deleted {} from {}", t.deleted, crate::tree::shorten_home(&t.orig))
                } else {
                    crate::tree::shorten_home(&n.path)
                }
            }
            k => format!("{k} items selected"),
        };
        self.status_sel.set_text(&info);
        if let Some(first) = sel.first() {
            if first.trash.is_none() {
                self.pathbar.set_path(&first.path);
                *self.current.borrow_mut() = first.path.clone();
            }
            if let (Some(spec), Some(e)) = (&self.picker, &self.picker_name) {
                if spec.save && !first.is_dir {
                    e.set_text(&first.name);
                }
            }
        }
        let w = self.weak();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(45), move || {
            let Some(w) = w.upgrade() else { return };
            *w.preview_timer.borrow_mut() = None;
            match sel.len() {
                0 => w.preview.clear(),
                1 => w.preview.show_file(&sel[0].path),
                n => {
                    let total: u64 = sel.iter().filter(|n| !n.is_dir).filter_map(|n| std::fs::metadata(&n.path).ok()).map(|m| m.len()).sum();
                    w.preview.show_multi(n, total);
                }
            }
        });
        *self.preview_timer.borrow_mut() = Some(id);
    }

    fn on_activate(&self, n: Node) {
        if n.trash.is_some() {
            return;
        }
        if let Some(spec) = &self.picker {
            if !n.is_dir || (spec.directory && false) {
                self.picker_accept();
            }
            return;
        }
        if !n.is_dir {
            self.open_default(&n.path);
        }
    }

    // ------------------------------------------------------------- navigation

    /// Show `target` (folder or file). Re-roots the tree when needed.
    pub fn navigate(&self, target: &Path, record: bool) {
        let is_trash = target.as_os_str() == TRASH_ROOT;
        let target = if is_trash { target.to_path_buf() } else { target.canonicalize().unwrap_or_else(|_| target.to_path_buf()) };
        if !is_trash && !target.exists() {
            self.toast(&format!("{} doesn't exist", target.display()));
            return;
        }
        let prev = self.current.borrow().clone();
        let root = self.tree.root();
        let dir = if is_trash || target.is_dir() { target.clone() } else { target.parent().map(Path::to_path_buf).unwrap_or_else(|| target.clone()) };
        let is_dir = !is_trash && target.is_dir();
        let inside = !is_trash && !self.tree.is_trash() && root.as_os_str() != "" && dir.starts_with(&root);
        // Opening a folder shows its contents: the tree is rooted at the folder
        // (or at its git repo, with the folder expanded). Opening a file keeps
        // the current tree when the file is inside it.
        let forced = self.force_root.borrow_mut().take();
        let sticky = self.root_sticky.get() && inside && forced.is_none();
        self.root_sticky.set(forced.is_some() || sticky);
        let new_root = if is_trash {
            target.clone()
        } else if let Some(r) = forced {
            r
        } else if sticky {
            root.clone()
        } else if is_dir || !inside {
            self.root_for(&dir)
        } else {
            root.clone()
        };
        if new_root != root || is_trash != self.tree.is_trash() {
            self.tree.set_root(&new_root);
            self.search.set_root(&new_root);
            let name = if is_trash { "Trash".to_string() } else { new_root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "/".into()) };
            if self.picker.is_none() {
                self.window.set_title(Some(&format!("{name} — Philly")));
            }
        }
        if !is_trash && target != self.tree.root() {
            self.tree.reveal(&target);
            if is_dir {
                self.tree.expand(&target);
            }
        } else {
            // Going up: select the folder we came from.
            let child = prev.strip_prefix(&target).ok().and_then(|r| r.components().next()).map(|c| target.join(c));
            match child {
                Some(c) if !is_trash && c != target && self.tree.reveal(&c) => {}
                _ => {
                    self.tree.selection.unselect_all();
                    if is_dir {
                        self.preview.show_file(&target);
                    } else {
                        self.preview.clear();
                    }
                }
            }
        }
        self.pathbar.set_path(&target);
        self.places.set_current(&dir);
        *self.current.borrow_mut() = target.clone();
        if record && prev.as_os_str() != "" && prev != target {
            self.back.borrow_mut().push(prev);
            self.forward.borrow_mut().clear();
        }
        if !is_trash {
            pathbar::zoxide_add(&dir);
        }
        self.on_git_status();
    }

    /// Root folder for showing `dir`: its git repo root (VS Code style), unless
    /// in picker mode or the repo is the whole home folder.
    fn root_for(&self, dir: &Path) -> PathBuf {
        if self.picker.is_some() {
            return dir.to_path_buf();
        }
        let home = glib::home_dir();
        match git::discover(dir) {
            Some(r) if r != home && r != Path::new("/") => r,
            _ => dir.to_path_buf(),
        }
    }

    fn go_back(&self) {
        let p = self.back.borrow_mut().pop();
        if let Some(p) = p {
            let cur = self.current.borrow().clone();
            self.forward.borrow_mut().push(cur);
            self.navigate(&p, false);
        }
    }

    fn go_forward(&self) {
        let p = self.forward.borrow_mut().pop();
        if let Some(p) = p {
            let cur = self.current.borrow().clone();
            self.back.borrow_mut().push(cur);
            self.navigate(&p, false);
        }
    }

    fn go_up(&self) {
        if self.tree.is_trash() {
            self.navigate(&glib::home_dir(), true);
            return;
        }
        let root = self.tree.root();
        if let Some(parent) = root.parent() {
            // navigate() roots the tree at the parent (or its repo) and selects
            // the folder we came from.
            let parent = parent.to_path_buf();
            *self.current.borrow_mut() = root.clone();
            self.navigate(&parent, true);
            self.tree.focus();
        }
    }

    // ---------------------------------------------------------------- actions

    fn selection_paths(&self) -> Vec<PathBuf> {
        self.tree.selected().into_iter().filter(|n| n.trash.is_none()).map(|n| n.path).collect()
    }

    pub fn toast(&self, msg: &str) {
        let t = adw::Toast::new(&glib::markup_escape_text(msg));
        t.set_timeout(3);
        self.toasts.add_toast(t);
    }

    fn open_default(&self, p: &Path) {
        if p.is_dir() {
            self.navigate(p, true);
            return;
        }
        if let Err(e) = fileops::open_default(&self.window, p) {
            self.toast(&e);
        }
    }

    fn open_editor(&self, p: &Path, line: Option<u32>) {
        let ws = self.tree.repo_root();
        let ws = ws.as_deref().filter(|r| p.starts_with(r) && p != *r);
        if let Err(e) = fileops::open_in_editor(p, line, ws) {
            self.toast(&e);
        }
    }

    fn transfer(&self, srcs: Vec<PathBuf>, dest: PathBuf, mv: bool) {
        if srcs.is_empty() {
            return;
        }
        let w = self.weak();
        let n = srcs.len();
        glib::spawn_future_local(async move {
            let res = fileops::transfer(srcs, dest.clone(), mv).await;
            let Some(w) = w.upgrade() else { return };
            match res {
                Ok(out) => {
                    if mv {
                        fileops::clear_cut();
                        *w.tree.cut.borrow_mut() = Default::default();
                        w.tree.restyle();
                    }
                    w.tree.refresh_dir(&dest);
                    if let Some(first) = out.first() {
                        w.tree.reveal(first);
                    }
                    w.toast(&format!("{} {}", if mv { "Moved" } else { "Copied" }, plural(n, "item")));
                }
                Err(e) => w.toast(&e),
            }
        });
    }

    fn paste(&self) {
        let dest = self.tree.target_dir();
        let w = self.weak();
        let display = gtk::prelude::WidgetExt::display(&self.window);
        glib::spawn_future_local(async move {
            let Some((paths, cut)) = fileops::read_clipboard(&display).await else {
                if let Some(w) = w.upgrade() {
                    w.toast("Nothing to paste");
                }
                return;
            };
            if let Some(w) = w.upgrade() {
                w.transfer(paths, dest, cut);
            }
        });
    }

    fn trash_selected(&self) {
        let nodes = self.tree.selected();
        if nodes.is_empty() {
            return;
        }
        if self.tree.is_trash() {
            let uris: Vec<String> = nodes.iter().filter_map(|n| n.trash.as_ref().map(|t| t.uri.clone())).collect();
            let w = self.weak();
            self.confirm(
                &format!("Permanently delete {}?", plural(uris.len(), "item")),
                "This can't be undone.",
                "Delete",
                Box::new(move || {
                    if let Some(w) = w.upgrade() {
                        if let Err(e) = fileops::delete_permanently(&uris) {
                            w.toast(&e);
                        }
                        w.tree.refresh_dir(Path::new(TRASH_ROOT));
                    }
                }),
            );
            return;
        }
        let paths: Vec<PathBuf> = nodes.iter().map(|n| n.path.clone()).collect();
        // Select a neighbour so the cursor doesn't jump to the top.
        let next = self.tree.cursor_pos();
        match fileops::trash(&paths) {
            Ok(()) => {
                let msg = if paths.len() == 1 { format!("Moved \"{}\" to trash", nodes[0].name) } else { format!("Moved {} to trash", plural(paths.len(), "item")) };
                self.toast_undo(&msg);
                for p in &paths {
                    if let Some(d) = p.parent() {
                        self.tree.refresh_dir(d);
                    }
                }
                if let Some(pos) = next {
                    let n = self.tree.selection.n_items();
                    if n > 0 {
                        self.tree.select_pos(pos.min(n - 1));
                    }
                }
            }
            Err(e) => self.toast(&e),
        }
    }

    fn toast_undo(&self, msg: &str) {
        let t = adw::Toast::new(&glib::markup_escape_text(msg));
        t.set_button_label(Some("Undo"));
        t.set_action_name(Some("win.undo"));
        t.set_timeout(5);
        self.toasts.add_toast(t);
    }

    fn undo(&self) {
        match fileops::undo() {
            Ok(m) => {
                self.toast(&m);
                self.tree.refresh_all();
            }
            Err(e) => self.toast(&e),
        }
    }

    fn rename_selected(&self) {
        let Some(n) = self.tree.selected().into_iter().next() else { return };
        if n.trash.is_some() {
            return;
        }
        let stem_len = if n.is_dir { -1 } else { n.name.rsplit_once('.').filter(|(s, _)| !s.is_empty()).map(|(s, _)| s.chars().count() as i32).unwrap_or(-1) };
        let w = self.weak();
        let path = n.path.clone();
        self.prompt("Rename", &n.name, stem_len, "Rename", move |name| {
            let w = w.upgrade()?;
            match fileops::rename(&path, &name) {
                Ok(to) => {
                    if let Some(d) = to.parent() {
                        w.tree.refresh_dir(d);
                    }
                    w.tree.reveal(&to);
                    None
                }
                Err(e) => Some(e),
            }
        });
    }

    fn new_item(&self, folder: bool) {
        if self.tree.is_trash() {
            return;
        }
        let dir = self.tree.target_dir();
        let w = self.weak();
        let title = if folder { "New folder" } else { "New file" };
        let d2 = dir.clone();
        self.prompt(title, "", -1, "Create", move |name| {
            let w = w.upgrade()?;
            match fileops::create(&d2, &name, folder) {
                Ok(p) => {
                    let mut d = p.parent().map(Path::to_path_buf);
                    while let Some(dd) = d {
                        w.tree.refresh_dir(&dd);
                        if dd == d2 {
                            break;
                        }
                        d = dd.parent().map(Path::to_path_buf);
                    }
                    w.tree.refresh_dir(&d2);
                    w.tree.reveal(&p);
                    None
                }
                Err(e) => Some(e),
            }
        });
        let _ = dir;
    }

    /// Text prompt dialog. `on_ok` returns an error message to keep it open.
    fn prompt(&self, title: &str, initial: &str, select_to: i32, ok: &str, on_ok: impl Fn(String) -> Option<String> + 'static) {
        let d = adw::AlertDialog::new(Some(title), None);
        let entry = gtk::Entry::builder().text(initial).activates_default(true).build();
        entry.set_width_chars(36);
        d.set_extra_child(Some(&entry));
        d.add_responses(&[("cancel", "Cancel"), ("ok", ok)]);
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("ok"));
        d.set_close_response("cancel");
        d.set_response_enabled("ok", !initial.is_empty());
        let d2 = d.clone();
        entry.connect_changed(move |e| d2.set_response_enabled("ok", !e.text().trim().is_empty()));
        let e2 = entry.clone();
        let w = self.weak();
        d.connect_response(None, move |dlg, resp| {
            if resp != "ok" {
                if let Some(w) = w.upgrade() {
                    w.tree.focus();
                }
                return;
            }
            let name = e2.text().trim().to_string();
            if let Some(err) = on_ok(name) {
                dlg.set_body(&err);
                dlg.present(Some(&e2.root().and_downcast::<gtk::Window>().unwrap_or_default()));
            } else if let Some(w) = w.upgrade() {
                w.tree.focus();
            }
        });
        d.present(Some(&self.window));
        let e3 = entry.clone();
        glib::idle_add_local_once(move || {
            e3.grab_focus();
            e3.select_region(0, select_to);
        });
    }

    fn confirm(&self, title: &str, body: &str, ok: &str, action: Box<dyn Fn()>) {
        let d = adw::AlertDialog::new(Some(title), Some(body));
        d.add_responses(&[("cancel", "Cancel"), ("ok", ok)]);
        d.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        d.set_default_response(Some("cancel"));
        d.set_close_response("cancel");
        d.connect_response(None, move |_, r| {
            if r == "ok" {
                action();
            }
        });
        d.present(Some(&self.window));
    }

    fn open_with_dialog(&self) {
        let paths = self.selection_paths();
        let Some(first) = paths.first().cloned() else { return };
        let apps = fileops::apps_for(&first);
        if apps.is_empty() {
            self.toast("No applications found for this file type");
            return;
        }
        let d = adw::AlertDialog::new(Some("Open with"), Some(&first.file_name().unwrap_or_default().to_string_lossy()));
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        for a in &apps {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(8);
            let img = gtk::Image::builder().pixel_size(24).build();
            if let Some(icon) = a.icon() {
                img.set_from_gicon(&icon);
            }
            row.append(&img);
            row.append(&gtk::Label::new(Some(&a.display_name())));
            list.append(&row);
        }
        let sc = gtk::ScrolledWindow::builder().child(&list).propagate_natural_height(true).max_content_height(360).min_content_width(320).build();
        d.set_extra_child(Some(&sc));
        d.add_responses(&[("cancel", "Cancel"), ("default", "Always use"), ("ok", "Open")]);
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("ok"));
        d.set_close_response("cancel");
        if let Some(r) = list.row_at_index(0) {
            list.select_row(Some(&r));
        }
        let d2 = d.clone();
        list.connect_row_activated(move |_, _| d2.emit_by_name::<()>("response", &[&"ok"]));
        let w = self.weak();
        let l2 = list.clone();
        d.connect_response(None, move |_, r| {
            let Some(w) = w.upgrade() else { return };
            let Some(i) = l2.selected_row().map(|r| r.index() as usize) else { return };
            let Some(app) = apps.get(i) else { return };
            match r {
                "ok" => {
                    if let Err(e) = fileops::open_with(&w.window, app, &paths) {
                        w.toast(&e);
                    }
                }
                "default" => {
                    let ct = if first.is_dir() { "inode/directory".to_string() } else { gio::content_type_guess(Some(&first), None).0.to_string() };
                    let _ = app.set_as_default_for_type(&ct);
                    if let Err(e) = fileops::open_with(&w.window, app, &paths) {
                        w.toast(&e);
                    }
                }
                _ => {}
            }
            w.tree.focus();
        });
        d.present(Some(&self.window));
    }

    fn copy_path(&self, relative: bool) {
        let paths = self.selection_paths();
        let paths = if paths.is_empty() { vec![self.tree.root()] } else { paths };
        let base = self.tree.repo_root().unwrap_or_else(|| self.tree.root());
        let text: Vec<String> = paths
            .iter()
            .map(|p| if relative { p.strip_prefix(&base).map(|r| r.to_string_lossy().to_string()).unwrap_or_else(|_| p.to_string_lossy().to_string()) } else { p.to_string_lossy().to_string() })
            .collect();
        fileops::copy_text(&gtk::prelude::WidgetExt::display(&self.window), &text.join("\n"));
        self.toast(&format!("Copied {}", if text.len() == 1 { text[0].clone() } else { plural(text.len(), "path") }));
    }

    fn toggle_preview(&self) {
        let v = !self.preview.widget.is_visible();
        self.preview.widget.set_visible(v);
    }

    fn toggle_shortcuts(&self) {
        let open = !self.shortcuts.reveals_child();
        if open {
            self.shortcuts.set_visible(true);
        }
        self.shortcuts.set_reveal_child(open);
        if !open {
            let s = self.shortcuts.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                if !s.reveals_child() {
                    s.set_visible(false);
                }
            });
        }
    }

    /// Make `dir` the tree root, also inside a git repo.
    fn open_as_root(&self, dir: &Path) {
        *self.force_root.borrow_mut() = Some(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()));
        self.navigate(dir, true);
        self.tree.focus();
    }

    /// Pin or unpin the selected folder, or the current folder.
    fn toggle_pin(&self) {
        let sel = self.tree.selected().into_iter().find(|n| n.is_dir && n.trash.is_none()).map(|n| n.path);
        let cur = self.current.borrow().clone();
        let cur_dir = if cur.is_dir() { cur.clone() } else { cur.parent().map(Path::to_path_buf).unwrap_or(cur.clone()) };
        let target = sel.unwrap_or(cur_dir);
        if target.as_os_str() == TRASH_ROOT || target.as_os_str().is_empty() {
            return;
        }
        let was = pathbar::is_bookmarked(&target);
        pathbar::toggle_bookmark(&target);
        self.pathbar.set_path(&cur);
        self.places.set_current(&self.tree.root());
        let name = target.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| target.display().to_string());
        self.toast(&if was { format!("Unpinned {name}") } else { format!("Pinned {name}") });
    }

    fn context_menu(&self, node: Option<Node>, x: f64, y: f64) {
        let menu = gio::Menu::new();
        let item = |m: &gio::Menu, label: &str, action: &str, accel: Option<&str>| {
            let i = gio::MenuItem::new(Some(label), Some(action));
            if let Some(a) = accel {
                i.set_attribute_value("accel", Some(&a.to_variant()));
            }
            m.append_item(&i);
        };
        if self.tree.is_trash() {
            let s = gio::Menu::new();
            if node.is_some() {
                item(&s, "Restore", "win.restore", None);
                item(&s, "Delete permanently", "win.trash", Some("Delete"));
            }
            item(&s, "Empty trash", "win.empty-trash", None);
            menu.append_section(None, &s);
        } else {
            let s1 = gio::Menu::new();
            if let Some(n) = &node {
                if n.is_dir {
                    item(&s1, "Open as root (double-click)", "win.open-root", None);
                    let label = if pathbar::is_bookmarked(&n.path) { "Unpin folder" } else { "Pin folder" };
                    item(&s1, label, "win.pin", Some("<Control><Shift>d"));
                } else {
                    item(&s1, "Open", "win.open", Some("Return"));
                }
                item(&s1, "Open with…", "win.open-with", None);
                item(&s1, "Open in VS Code", "win.open-editor", Some("<Control>Return"));
            }
            item(&s1, "Open terminal here", "win.terminal", Some("<Control><Shift>c"));
            menu.append_section(None, &s1);
            let s2 = gio::Menu::new();
            item(&s2, "New file", "win.new-file", Some("<Control>n"));
            item(&s2, "New folder", "win.new-folder", Some("<Control><Shift>n"));
            menu.append_section(None, &s2);
            let s3 = gio::Menu::new();
            if node.is_some() {
                item(&s3, "Cut", "win.cut", Some("<Control>x"));
                item(&s3, "Copy", "win.copy", Some("<Control>c"));
            }
            item(&s3, "Paste", "win.paste", Some("<Control>v"));
            if node.is_some() {
                item(&s3, "Duplicate", "win.duplicate", None);
            }
            menu.append_section(None, &s3);
            let s4 = gio::Menu::new();
            item(&s4, "Copy path", "win.copy-path", Some("<Shift><Alt>c"));
            item(&s4, "Copy relative path", "win.copy-rel-path", Some("<Control><Shift><Alt>c"));
            menu.append_section(None, &s4);
            if node.is_some() {
                let s5 = gio::Menu::new();
                item(&s5, "Rename…", "win.rename", Some("F2"));
                item(&s5, "Move to trash", "win.trash", Some("Delete"));
                menu.append_section(None, &s5);
            }
            if node.as_ref().map(|n| on_removable(&n.path)).unwrap_or(false) || on_removable(&self.tree.root()) {
                let s6 = gio::Menu::new();
                item(&s6, "Eject drive", "win.eject", None);
                menu.append_section(None, &s6);
            }
        }
        let pop = gtk::PopoverMenu::from_model(Some(&menu));
        pop.set_parent(&self.tree.list);
        pop.set_has_arrow(false);
        pop.set_halign(gtk::Align::Start);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    }

    fn setup_actions(self: &Rc<Self>) {
        let add = |name: &str, f: Box<dyn Fn(&Win)>| {
            let a = gio::SimpleAction::new(name, None);
            let w = self.weak();
            a.connect_activate(move |_, _| {
                if let Some(w) = w.upgrade() {
                    f(&w);
                }
            });
            self.window.add_action(&a);
        };
        add("open", Box::new(|w| {
            if let Some(n) = w.tree.selected().into_iter().next() {
                w.open_default(&n.path);
            }
        }));
        add("open-root", Box::new(|w| {
            if let Some(n) = w.tree.selected().into_iter().find(|n| n.is_dir) {
                w.open_as_root(&n.path);
            }
        }));
        add("open-with", Box::new(|w| w.open_with_dialog()));
        add("open-editor", Box::new(|w| {
            let p = w.selection_paths().into_iter().next().unwrap_or_else(|| w.tree.root());
            w.open_editor(&p, None);
        }));
        add("terminal", Box::new(|w| {
            let d = if w.tree.is_trash() { glib::home_dir() } else { w.tree.target_dir() };
            if let Err(e) = fileops::open_terminal(&d) {
                w.toast(&e);
            }
        }));
        add("new-file", Box::new(|w| w.new_item(false)));
        add("new-folder", Box::new(|w| w.new_item(true)));
        add("cut", Box::new(|w| {
            let p = w.selection_paths();
            if !p.is_empty() {
                fileops::set_clipboard(&gtk::prelude::WidgetExt::display(&w.window), &p, true);
                *w.tree.cut.borrow_mut() = fileops::cut_set();
                w.tree.restyle();
                w.toast(&format!("Cut {}", plural(p.len(), "item")));
            }
        }));
        add("copy", Box::new(|w| {
            let p = w.selection_paths();
            if !p.is_empty() {
                fileops::set_clipboard(&gtk::prelude::WidgetExt::display(&w.window), &p, false);
                *w.tree.cut.borrow_mut() = Default::default();
                w.tree.restyle();
                w.toast(&format!("Copied {}", plural(p.len(), "item")));
            }
        }));
        add("paste", Box::new(|w| w.paste()));
        add("duplicate", Box::new(|w| {
            for p in w.selection_paths() {
                match fileops::duplicate(&p) {
                    Ok(d) => {
                        w.tree.refresh_dir(d.parent().unwrap());
                        w.tree.reveal(&d);
                    }
                    Err(e) => w.toast(&e),
                }
            }
        }));
        add("copy-path", Box::new(|w| w.copy_path(false)));
        add("copy-rel-path", Box::new(|w| w.copy_path(true)));
        add("rename", Box::new(|w| w.rename_selected()));
        add("trash", Box::new(|w| w.trash_selected()));
        add("undo", Box::new(|w| w.undo()));
        add("refresh", Box::new(|w| {
            w.tree.refresh_all();
            w.preview.reload();
        }));
        add("collapse", Box::new(|w| w.tree.collapse_all()));
        add("up", Box::new(|w| w.go_up()));
        add("pin", Box::new(|w| w.toggle_pin()));
        add("restore", Box::new(|w| {
            let mut n = 0;
            for node in w.tree.selected() {
                if let Some(t) = node.trash {
                    match fileops::restore(&t.uri, &t.orig) {
                        Ok(()) => n += 1,
                        Err(e) => w.toast(&e),
                    }
                }
            }
            if n > 0 {
                w.toast(&format!("Restored {}", plural(n, "item")));
            }
            w.tree.refresh_dir(Path::new(TRASH_ROOT));
        }));
        add("empty-trash", Box::new(|w| {
            let ww = w.weak();
            w.confirm(
                "Empty trash?",
                "All items in the trash will be permanently deleted.",
                "Empty trash",
                Box::new(move || {
                    let Some(w) = ww.upgrade() else { return };
                    let n = w.tree.selection.n_items();
                    let mut uris = Vec::new();
                    // Collect all top-level trash items (not just visible ones).
                    if let Ok(en) = gio::File::for_uri(TRASH_ROOT).enumerate_children("standard::name", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE) {
                        while let Ok(Some(i)) = en.next_file(gio::Cancellable::NONE) {
                            uris.push(gio::File::for_uri(TRASH_ROOT).child(i.name()).uri().to_string());
                        }
                    }
                    let _ = n;
                    if let Err(e) = fileops::delete_permanently(&uris) {
                        w.toast(&e);
                    }
                    w.tree.refresh_dir(Path::new(TRASH_ROOT));
                }),
            );
        }));
        add("eject", Box::new(|w| {
            let p = w.tree.selected().first().map(|n| n.path.clone()).unwrap_or_else(|| w.tree.root());
            let ww = w.weak();
            pathbar::eject_for(&p, move |r| {
                if let Some(w) = ww.upgrade() {
                    match r {
                        Ok(()) => {
                            w.toast("Drive ejected, safe to remove");
                            w.navigate(&glib::home_dir(), true);
                        }
                        Err(e) => w.toast(&e),
                    }
                }
            });
        }));
        add("picker-accept", Box::new(|w| w.picker_accept()));
        add("picker-cancel", Box::new(|w| {
            w.picker_finish(None);
            w.window.close();
        }));
    }

    fn setup_keys(self: &Rc<Self>) {
        // Global shortcuts: work wherever focus is.
        if std::env::var_os("PHILLY_DEBUG").is_some() {
            let dbg = gtk::EventControllerKey::new();
            dbg.set_propagation_phase(gtk::PropagationPhase::Capture);
            dbg.connect_key_pressed(|c, key, code, state| {
                let consumed = c.current_event().and_then(|e| e.downcast::<gdk::KeyEvent>().ok()).map(|e| e.consumed_modifiers());
                eprintln!("philly: key {:?} code {code} state {state:?} consumed {consumed:?}", key.name());
                glib::Propagation::Proceed
            });
            self.window.add_controller(dbg);
        }
        // Our own matcher instead of GtkShortcutController: compare the lower-case
        // key and only Ctrl/Shift/Alt/Super, so Ctrl+Shift+F matches whether the
        // keyboard reports "F" or "f" with Shift.
        type Handler = Box<dyn Fn(&Win) -> bool>;
        let table: Rc<RefCell<Vec<(gdk::Key, gdk::ModifierType, Handler)>>> = Default::default();
        let add = |trigger: &str, f: Handler| {
            match gtk::accelerator_parse(trigger) {
                Some((key, mods)) => table.borrow_mut().push((key.to_lower(), mods, f)),
                None => eprintln!("philly: bad shortcut {trigger}"),
            }
        };
        add("<Control>p", Box::new(|w| {
            w.quick.open(&w.tree.root());
            true
        }));
        add("<Control><Shift>f", Box::new(|w| {
            w.show_side("search");
            true
        }));
        add("<Control><Shift>e", Box::new(|w| {
            w.show_side("files");
            true
        }));
        add("<Control><Shift>g", Box::new(|w| {
            w.show_side("git");
            true
        }));
        add("<Control>l", Box::new(|w| {
            w.pathbar.start_edit(None);
            true
        }));
        add("<Alt>d", Box::new(|w| {
            w.pathbar.start_edit(None);
            true
        }));
        add("<Control>d", Box::new(|w| {
            w.pathbar.open_places();
            true
        }));
        add("<Control><Shift>d", Box::new(|w| {
            w.toggle_pin();
            true
        }));
        add("<Control><Shift>b", Box::new(|w| {
            w.show_side("places");
            true
        }));
        add("<Control>b", Box::new(|w| {
            w.toggle_preview();
            true
        }));
        add("<Control>backslash", Box::new(|w| {
            w.preview.toggle_split();
            true
        }));
        add("<Control><Shift>v", Box::new(|w| {
            w.preview.toggle_source();
            true
        }));
        add("<Control>h", Box::new(|w| {
            let v = !w.tree.show_hidden.get();
            w.tree.show_hidden.set(v);
            w.tree.refilter();
            w.toast(if v { "Showing hidden files" } else { "Hiding hidden files" });
            true
        }));
        add("<Alt>Left", Box::new(|w| {
            w.go_back();
            true
        }));
        add("<Alt>Right", Box::new(|w| {
            w.go_forward();
            true
        }));
        // Shift+Enter on a folder in the tree: make it the root (like double-click).
        add("<Shift>Return", Box::new(|w| {
            let in_tree = gtk::prelude::GtkWindowExt::focus(&w.window).map(|f| f.is_ancestor(&w.tree.list) || f == *w.tree.list.upcast_ref::<gtk::Widget>()).unwrap_or(false);
            if !in_tree || focus_in_text(&w.window) {
                return false;
            }
            match w.tree.selected().into_iter().find(|n| n.is_dir && n.trash.is_none()) {
                Some(n) => {
                    w.open_as_root(&n.path);
                    true
                }
                None => false,
            }
        }));
        add("<Alt>Up", Box::new(|w| {
            w.go_up();
            true
        }));
        add("F5", Box::new(|w| {
            gio::prelude::ActionGroupExt::activate_action(&w.window, "refresh", None);
            true
        }));
        add("<Control>r", Box::new(|w| {
            gio::prelude::ActionGroupExt::activate_action(&w.window, "refresh", None);
            true
        }));
        add("<Control>w", Box::new(|w| {
            w.window.close();
            true
        }));
        add("<Control><Alt>n", Box::new(|w| {
            if w.picker.is_none() {
                if let Some(app) = w.window.application().and_downcast::<adw::Application>() {
                    Win::new(&app, Some(w.tree.root()), None);
                }
            }
            true
        }));
        add("<Control>slash", Box::new(|w| {
            w.toggle_shortcuts();
            true
        }));
        add("F1", Box::new(|w| {
            w.toggle_shortcuts();
            true
        }));
        add("<Control><Shift>c", Box::new(|w| {
            gio::prelude::ActionGroupExt::activate_action(&w.window, "terminal", None);
            true
        }));
        add("<Control>grave", Box::new(|w| {
            gio::prelude::ActionGroupExt::activate_action(&w.window, "terminal", None);
            true
        }));
        add("<Shift><Alt>c", Box::new(|w| {
            w.copy_path(false);
            true
        }));
        add("<Control><Shift><Alt>c", Box::new(|w| {
            w.copy_path(true);
            true
        }));
        add("Escape", Box::new(|w| {
            if w.shortcuts.reveals_child() {
                w.toggle_shortcuts();
                return true;
            }
            if w.quick.is_open() {
                w.quick.close();
                return true;
            }
            if w.pathbar.is_editing() {
                return false;
            }
            if w.picker.is_some() && !w.side_stack.visible_child_name().map(|n| n != "files").unwrap_or(false) {
                gio::prelude::ActionGroupExt::activate_action(&w.window, "picker-cancel", None);
                return true;
            }
            false
        }));
        let sc = gtk::EventControllerKey::new();
        sc.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = self.weak();
        sc.connect_key_pressed(move |_, key, _, state| {
            let Some(w) = w.upgrade() else { return glib::Propagation::Proceed };
            let mask = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK;
            let (key, mods) = (key.to_lower(), state & mask);
            let table = table.borrow();
            match table.iter().find(|(k, m, _)| *k == key && *m == mods) {
                Some((_, _, f)) if f(&w) => glib::Propagation::Stop,
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(sc);

        // File shortcuts: only when no text field consumed the key (bubble phase).
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Bubble);
        let w = self.weak();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(w) = w.upgrade() else { return glib::Propagation::Proceed };
            if focus_in_text(&w.window) {
                return glib::Propagation::Proceed;
            }
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            let alt = state.contains(gdk::ModifierType::ALT_MASK);
            let in_files = w.side_stack.visible_child_name().as_deref() == Some("files");
            let act = |name: &str| gio::prelude::ActionGroupExt::activate_action(&w.window, name, None);
            let handled = match (key, ctrl, shift, alt) {
                (gdk::Key::c, true, false, false) if in_files => {
                    act("copy");
                    true
                }
                (gdk::Key::x, true, false, false) if in_files => {
                    act("cut");
                    true
                }
                (gdk::Key::v, true, false, false) if in_files => {
                    act("paste");
                    true
                }
                (gdk::Key::z, true, false, false) => {
                    act("undo");
                    true
                }
                (gdk::Key::n, true, false, false) => {
                    act("new-file");
                    true
                }
                (gdk::Key::N, true, true, false) | (gdk::Key::n, true, true, false) => {
                    act("new-folder");
                    true
                }
                (gdk::Key::Delete, false, _, false) | (gdk::Key::KP_Delete, false, _, false) if in_files => {
                    act("trash");
                    true
                }
                (gdk::Key::F2, false, false, false) if in_files => {
                    act("rename");
                    true
                }
                (gdk::Key::Return | gdk::Key::KP_Enter, true, false, false) => {
                    act("open-editor");
                    true
                }
                (gdk::Key::Menu, _, _, _) | (gdk::Key::F10, false, true, false) if in_files => {
                    let (x, y) = w
                        .tree
                        .cursor_pos()
                        .and_then(|_| w.tree.list.focus_child())
                        .and_then(|c| c.compute_bounds(&w.tree.list))
                        .map(|b| (b.x() as f64 + 40.0, b.y() as f64 + b.height() as f64))
                        .unwrap_or((40.0, 40.0));
                    let n = w.tree.selected().into_iter().next();
                    w.context_menu(n, x, y);
                    true
                }
                (gdk::Key::BackSpace, false, false, false) if in_files => {
                    w.go_up();
                    true
                }
                _ => false,
            };
            if handled {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.window.add_controller(keys);
    }

    // ----------------------------------------------------------------- picker

    fn setup_picker(self: &Rc<Self>) {
        let Some(spec) = self.picker.clone() else { return };
        if spec.directory {
            self.tree.dirs_only.set(true);
        }
        if !spec.multiple {
            // Treat extra selections as single by only using the first.
        }
        let apply_filter = {
            let w = self.weak();
            let spec = spec.clone();
            move |idx: Option<usize>| {
                let Some(w) = w.upgrade() else { return };
                let f = idx.and_then(|i| spec.filters.get(i).cloned());
                *w.tree.file_filter.borrow_mut() = f.map(|f| Box::new(move |n: &Node| f.matches(&n.name, &n.path)) as Box<dyn Fn(&Node) -> bool>);
                w.tree.refilter();
            }
        };
        apply_filter(if spec.filters.is_empty() { None } else { Some(spec.current_filter.unwrap_or(0)) });
        if let Some(dd) = &self.picker_filter {
            dd.connect_selected_notify(move |dd| apply_filter(Some(dd.selected() as usize)));
        }
        if let Some(e) = &self.picker_name {
            let w = self.weak();
            e.connect_activate(move |_| {
                if let Some(w) = w.upgrade() {
                    w.picker_accept();
                }
            });
        }
        // Hide panels the picker doesn't need.
        self.btn_git.set_visible(false);
        if let Some(o) = self.btn_git.parent() {
            o.set_visible(false);
        }
    }

    fn picker_accept(&self) {
        let Some(spec) = self.picker.clone() else { return };
        let sel = self.tree.selected();
        if spec.save {
            let name = self.picker_name.as_ref().map(|e| e.text().trim().to_string()).unwrap_or_default();
            if name.is_empty() {
                self.toast("Type a file name");
                return;
            }
            let dir = match sel.first() {
                Some(n) if n.is_dir => n.path.clone(),
                Some(n) => n.path.parent().unwrap().to_path_buf(),
                None => self.tree.root(),
            };
            let dest = if name.starts_with('/') || name.starts_with('~') { pathbar::expand(&name, &dir) } else { dir.join(&name) };
            if dest.is_dir() {
                self.navigate(&dest, true);
                return;
            }
            let uri = gio::File::for_path(&dest).uri().to_string();
            if dest.exists() {
                let w = self.weak();
                self.confirm(
                    &format!("Replace \"{}\"?", dest.file_name().unwrap_or_default().to_string_lossy()),
                    "A file with this name already exists. Replacing it will overwrite its contents.",
                    "Replace",
                    Box::new(move || {
                        if let Some(w) = w.upgrade() {
                            w.picker_finish(Some(vec![uri.clone()]));
                            w.window.close();
                        }
                    }),
                );
                return;
            }
            self.picker_finish(Some(vec![uri]));
            self.window.close();
            return;
        }
        let uris: Vec<String> = if spec.directory {
            let p = sel.iter().find(|n| n.is_dir).map(|n| n.path.clone()).unwrap_or_else(|| self.tree.root());
            vec![gio::File::for_path(p).uri().to_string()]
        } else {
            let files: Vec<&Node> = sel.iter().filter(|n| !n.is_dir).collect();
            // Picked in the folder view (or hidden by a filter): the file in the path bar.
            let cur = self.current.borrow().clone();
            if files.is_empty() && sel.is_empty() && cur.is_file() {
                self.picker_finish(Some(vec![gio::File::for_path(&cur).uri().to_string()]));
                self.window.close();
                return;
            }
            if files.is_empty() {
                // A folder is selected: step into it instead.
                if let Some(n) = sel.first() {
                    if let Some(pos) = self.tree.cursor_pos() {
                        if let Some(lr) = self.tree.row_at(pos) {
                            lr.set_expanded(true);
                            self.tree.select_pos(pos + 1);
                        }
                    }
                    let _ = n;
                }
                return;
            }
            let take = if spec.multiple { files.len() } else { 1 };
            files.iter().take(take).map(|n| gio::File::for_path(&n.path).uri().to_string()).collect()
        };
        self.picker_finish(Some(uris));
        self.window.close();
    }

    fn picker_finish(&self, uris: Option<Vec<String>>) {
        let Some(spec) = &self.picker else { return };
        let f = spec.reply.borrow_mut().take();
        if let Some(f) = f {
            if uris.is_some() {
                self.save_state();
            }
            let filter = self.picker_filter.as_ref().map(|d| d.selected() as usize);
            f(uris, filter);
        }
    }
}

fn focus_in_text(window: &adw::ApplicationWindow) -> bool {
    let Some(f) = gtk::prelude::GtkWindowExt::focus(window) else { return false };
    f.is::<gtk::Text>() || f.is::<gtk::TextView>() || f.ancestor(gtk::Entry::static_type()).is_some() || f.ancestor(gtk::SearchEntry::static_type()).is_some()
}

fn on_removable(p: &Path) -> bool {
    gio::VolumeMonitor::get()
        .mounts()
        .into_iter()
        .any(|m| m.can_unmount() && m.root().path().map(|r| r != Path::new("/") && p.starts_with(r)).unwrap_or(false))
}

fn shortcuts_sheet() -> gtk::Widget {
    let groups: &[(&str, &[(&str, &str)])] = &[
        (
            "NAVIGATE",
            &[
                ("Ctrl P", "Go to file"),
                ("Ctrl L", "Type a path / jump"),
                ("Ctrl D", "Places, drives, trash"),
                ("Ctrl Shift D", "Pin / unpin folder"),
                ("Ctrl Shift B", "Places & pinned folders"),
                ("Shift ⏎", "Folder as root (or double-click)"),
                ("Alt ← / →", "Back / forward"),
                ("Alt ↑  ⌫", "Parent folder"),
                ("← / →", "Collapse / expand"),
                ("type", "Find in tree"),
            ],
        ),
        (
            "PANELS",
            &[
                ("Ctrl Shift E", "Explorer"),
                ("Ctrl Shift F", "Search in files"),
                ("Ctrl Shift G", "Source control"),
                ("Ctrl B", "Toggle preview"),
                ("Ctrl \\", "Diff side by side"),
                ("Ctrl Shift V", "Markdown source"),
                ("Ctrl H", "Hidden files"),
            ],
        ),
        (
            "FILES",
            &[
                ("Enter", "Open / toggle folder"),
                ("Ctrl Enter", "Open in VS Code"),
                ("Ctrl N", "New file"),
                ("Ctrl Shift N", "New folder"),
                ("F2", "Rename"),
                ("Delete", "Move to trash"),
                ("Ctrl C X V", "Copy, cut, paste"),
                ("Ctrl Z", "Undo"),
            ],
        ),
        (
            "MORE",
            &[
                ("Space", "Stage / unstage (git)"),
                ("Ctrl Enter", "Commit (message box)"),
                ("Shift Alt C", "Copy path"),
                ("Ctrl Shift Alt C", "Copy relative path"),
                ("Ctrl Shift C", "Terminal here"),
                ("Ctrl Alt N", "New window"),
                ("F5", "Refresh"),
                ("Ctrl W", "Close window"),
            ],
        ),
    ];
    let grid = gtk::Box::new(gtk::Orientation::Horizontal, 28);
    for (title, items) in groups {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let t = gtk::Label::builder().label(*title).xalign(0.0).build();
        t.add_css_class("group");
        col.append(&t);
        for (k, d) in *items {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let keys = gtk::Box::new(gtk::Orientation::Horizontal, 3);
            keys.set_size_request(130, -1);
            for part in k.split(' ') {
                let l = gtk::Label::new(Some(part));
                l.add_css_class("keycap");
                keys.append(&l);
            }
            let dl = gtk::Label::builder().label(*d).xalign(0.0).build();
            dl.add_css_class("desc");
            row.append(&keys);
            row.append(&dl);
            col.append(&row);
        }
        grid.append(&col);
    }
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.add_css_class("shortcuts-sheet");
    b.append(&grid);
    b.upcast()
}
