//! Places: home and user folders, pinned folders (GTK bookmarks, shared with
//! Nautilus and the GTK file chooser), drives and trash.
//!
//! `build_list` is used by the path bar's places popover and by the Places
//! panel in the sidebar.

use crate::pathbar::{bookmarks, toggle_bookmark};
use crate::tree::{shorten_home, TRASH_ROOT};
use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

#[derive(Clone)]
enum Target {
    Path(PathBuf),
    Mount(PathBuf),
    Volume(gio::Volume),
}

impl Target {
    fn path(&self) -> Option<&Path> {
        match self {
            Target::Path(p) | Target::Mount(p) => Some(p),
            Target::Volume(_) => None,
        }
    }
}

pub type Pick = Rc<dyn Fn(PathBuf)>;

fn section(title: &str) -> gtk::Widget {
    if title.is_empty() {
        return gtk::Separator::new(gtk::Orientation::Horizontal).upcast();
    }
    let l = gtk::Label::builder().label(title).xalign(0.0).build();
    l.add_css_class("section-title");
    l.upcast()
}

/// Build the places list. `on_pick` gets the folder to open (drives are
/// mounted first). In the sidebar, pinned rows get an unpin button and
/// `on_changed` runs after a pin is removed.
pub fn build_list(on_pick: Pick, sidebar: bool, current: Option<&Path>, on_changed: Option<Rc<dyn Fn()>>) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class(if sidebar { "places-list" } else { "suggestions" });
    let targets: Rc<RefCell<Vec<Target>>> = Default::default();

    let add_row = |glyph: String, label: &str, detail: &str, t: Target, unpin: bool| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.add_css_class("place-row");
        let icon = gtk::Label::new(None);
        icon.set_markup(&glyph);
        icon.add_css_class("file-icon");
        icon.add_css_class("sugg-icon");
        let l = gtk::Label::builder().label(label).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let d = gtk::Label::builder().label(detail).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
        d.add_css_class("kind");
        row.append(&icon);
        row.append(&l);
        row.append(&d);
        if unpin {
            if let Target::Path(p) = &t {
                let b = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Unpin").css_classes(["flat", "row-action"]).build();
                let p = p.clone();
                let changed = on_changed.clone();
                b.connect_clicked(move |_| {
                    toggle_bookmark(&p);
                    if let Some(f) = changed.as_ref() {
                        f();
                    }
                });
                row.append(&b);
            }
        }
        list.append(&row);
        if let (Some(cur), Some(p)) = (current, t.path()) {
            if cur == p {
                if let Some(r) = list.last_child().and_downcast::<gtk::ListBoxRow>() {
                    list.select_row(Some(&r));
                }
            }
        }
        targets.borrow_mut().push(t);
    };

    let acc = crate::theme::color("accent");
    let home = glib::home_dir();
    let mut places: Vec<(String, PathBuf)> = vec![("Home".into(), home.clone())];
    for (d, label) in [
        (glib::UserDirectory::Desktop, "Desktop"),
        (glib::UserDirectory::Downloads, "Downloads"),
        (glib::UserDirectory::Documents, "Documents"),
        (glib::UserDirectory::Pictures, "Pictures"),
        (glib::UserDirectory::Videos, "Videos"),
        (glib::UserDirectory::Music, "Music"),
    ] {
        if let Some(p) = glib::user_special_dir(d) {
            if p != home && p.is_dir() && !(sidebar == false && matches!(label, "Desktop" | "Videos" | "Music")) {
                places.push((label.into(), p));
            }
        }
    }
    let standard: std::collections::HashSet<PathBuf> = places.iter().map(|(_, p)| p.clone()).collect();
    for (label, p) in places {
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let glyph = if label == "Home" { format!("<span foreground=\"{acc}\">\u{f015}</span>") } else { crate::icons::markup(&name, true, false) };
        add_row(glyph, &label, "", Target::Path(p), false);
    }

    let pinned_start = targets.borrow().len();
    let star = crate::theme::color("yellow");
    // Pins of a folder already listed under PLACES only show there.
    for (p, label) in bookmarks().into_iter().filter(|(p, _)| !standard.contains(p)) {
        let detail = shorten_home(p.parent().unwrap_or(&p));
        add_row(format!("<span foreground=\"{star}\">\u{f005}</span>"), &label, &detail, Target::Path(p), sidebar);
    }

    let drives_start = targets.borrow().len();
    let vm = gio::VolumeMonitor::get();
    let mut seen = std::collections::HashSet::new();
    for m in vm.mounts() {
        let Some(root) = m.root().path() else { continue };
        if m.is_shadowed() {
            continue;
        }
        seen.insert(m.name().to_string());
        let detail = if m.can_unmount() { "mounted · ⏏" } else { "mounted" };
        add_row(format!("<span foreground=\"{}\">\u{f0a0}</span>", crate::theme::color("cyan")), &m.name(), detail, Target::Mount(root), false);
    }
    for v in vm.volumes() {
        if v.get_mount().is_some() || seen.contains(v.name().as_str()) {
            continue;
        }
        add_row(
            format!("<span foreground=\"{}\">\u{f0a0}</span>", crate::theme::color("dark_foreground")),
            &v.name(),
            "click to mount",
            Target::Volume(v.clone()),
            false,
        );
    }

    let trash_start = targets.borrow().len();
    let trash_count = gio::File::for_uri(TRASH_ROOT)
        .query_info("trash::item-count", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
        .map(|i| i.attribute_uint32("trash::item-count"))
        .unwrap_or(0);
    let detail = if trash_count > 0 { format!("{trash_count} items") } else { "empty".into() };
    add_row(format!("<span foreground=\"{}\">\u{f1f8}</span>", crate::theme::color("red")), "Trash", &detail, Target::Path(PathBuf::from(TRASH_ROOT)), false);

    let has_pins = pinned_start < drives_start;
    list.set_header_func(move |row, _| {
        let i = row.index() as usize;
        let title = if i == 0 {
            Some("PLACES")
        } else if i == pinned_start && has_pins {
            Some("PINNED")
        } else if i == drives_start && drives_start < trash_start {
            Some("DRIVES")
        } else if i == trash_start {
            Some("")
        } else {
            None
        };
        row.set_header(title.map(section).as_ref());
    });

    list.connect_row_activated(move |list, row| {
        let Some(t) = targets.borrow().get(row.index() as usize).cloned() else { return };
        match t {
            Target::Path(p) | Target::Mount(p) => on_pick(p),
            Target::Volume(v) => {
                let op = gtk::MountOperation::new(list.root().and_downcast_ref::<gtk::Window>());
                let v2 = v.clone();
                let pick = on_pick.clone();
                v.mount(gio::MountMountFlags::NONE, Some(&op), gio::Cancellable::NONE, move |res| {
                    if res.is_ok() {
                        if let Some(p) = v2.get_mount().and_then(|m| m.root().path()) {
                            pick(p);
                        }
                    }
                });
            }
        }
    });
    list
}

/// The Places panel in the sidebar.
pub struct PlacesPanel {
    pub widget: gtk::Box,
    scroller: gtk::ScrolledWindow,
    list: RefCell<Option<gtk::ListBox>>,
    current: RefCell<PathBuf>,
    pub on_pick: RefCell<Option<Box<dyn Fn(PathBuf)>>>,
    _monitors: RefCell<Vec<gio::FileMonitor>>,
    me: RefCell<Weak<PlacesPanel>>,
}

impl PlacesPanel {
    pub fn new() -> Rc<PlacesPanel> {
        let title = gtk::Label::builder().label("FAVORITES").xalign(0.0).hexpand(true).build();
        let pin = gtk::Button::builder()
            .icon_name("starred-symbolic")
            .tooltip_text("Pin or unpin this folder (Ctrl+Shift+D)")
            .css_classes(["flat"])
            .action_name("win.pin")
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.add_css_class("section-header");
        header.set_margin_start(10);
        header.append(&title);
        header.append(&pin);
        let scroller = gtk::ScrolledWindow::builder().vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("sidebar-pane");
        widget.append(&header);
        widget.append(&scroller);
        let hint = gtk::Label::builder()
            .label("Pin a folder with ★ in the path bar, Ctrl+Shift+D, or right-click → Pin folder.")
            .wrap(true)
            .xalign(0.0)
            .width_chars(16)
            .max_width_chars(28)
            .build();
        hint.add_css_class("dim-label");
        hint.add_css_class("places-hint");
        widget.append(&hint);

        let p = Rc::new(PlacesPanel {
            widget,
            scroller,
            list: RefCell::new(None),
            current: RefCell::new(PathBuf::new()),
            on_pick: RefCell::new(None),
            _monitors: RefCell::new(Vec::new()),
            me: RefCell::new(Weak::new()),
        });
        *p.me.borrow_mut() = Rc::downgrade(&p);

        // Rebuild when shown, when pins change (here, in Nautilus or the GTK
        // file chooser) and when drives come and go.
        let w = Rc::downgrade(&p);
        p.widget.connect_map(move |_| {
            if let Some(p) = w.upgrade() {
                p.refresh();
            }
        });
        let bm = glib::user_config_dir().join("gtk-3.0/bookmarks");
        if let Ok(mon) = gio::File::for_path(&bm).monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
            let w = Rc::downgrade(&p);
            mon.connect_changed(move |_, _, _, _| {
                if let Some(p) = w.upgrade() {
                    p.refresh_if_visible();
                }
            });
            p._monitors.borrow_mut().push(mon);
        }
        let vm = gio::VolumeMonitor::get();
        for sig in ["mount-added", "mount-removed", "volume-added", "volume-removed"] {
            let w = Rc::downgrade(&p);
            let id = vm.connect_local(sig, false, move |_| {
                if let Some(p) = w.upgrade() {
                    p.refresh_if_visible();
                }
                None
            });
            let vm2 = vm.clone();
            let id = RefCell::new(Some(id));
            p.widget.connect_destroy(move |_| {
                if let Some(id) = id.borrow_mut().take() {
                    vm2.disconnect(id);
                }
            });
        }
        p
    }

    pub fn set_current(&self, path: &Path) {
        *self.current.borrow_mut() = path.to_path_buf();
        self.refresh_if_visible();
    }

    fn refresh_if_visible(&self) {
        if self.widget.is_mapped() {
            self.refresh();
        }
    }

    pub fn refresh(&self) {
        let w = self.me.borrow().clone();
        let pick: Pick = Rc::new(move |p| {
            if let Some(s) = w.upgrade() {
                if let Some(f) = s.on_pick.borrow().as_ref() {
                    f(p);
                }
            }
        });
        let w = self.me.borrow().clone();
        let changed: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(s) = w.upgrade() {
                s.refresh();
            }
        });
        let cur = self.current.borrow().clone();
        let list = build_list(pick, true, Some(&cur), Some(changed));
        self.scroller.set_child(Some(&list));
        *self.list.borrow_mut() = Some(list);
    }

    pub fn focus(&self) {
        if let Some(l) = self.list.borrow().as_ref() {
            let row = l.selected_row().or_else(|| l.row_at_index(0));
            if let Some(r) = row {
                r.grab_focus();
            }
        }
    }
}
