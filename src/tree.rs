//! The file list: a GtkColumnView (name, size, modified, kind) over a lazily
//! loaded GtkTreeListModel, with live file watching, git decorations, ignore
//! dimming, sorting by column and type-to-filter.

use crate::git;
use crate::icons;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

#[derive(Clone, Debug)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub ignored: bool,
    /// Size in bytes (files).
    pub size: u64,
    /// Modification time (unix seconds, 0 = unknown); deletion time in the trash.
    pub mtime: i64,
    /// Set for items in the trash view.
    pub trash: Option<TrashInfo>,
}

#[derive(Clone, Debug)]
pub struct TrashInfo {
    pub uri: String,
    pub orig: PathBuf,
    pub deleted: String,
}

pub const TRASH_ROOT: &str = "trash:///";

pub fn node_of(obj: &glib::Object) -> Option<Node> {
    let obj = if let Some(row) = obj.downcast_ref::<gtk::TreeListRow>() { row.item()? } else { obj.clone() };
    obj.downcast_ref::<glib::BoxedAnyObject>().map(|b| b.borrow::<Node>().clone())
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let o = na.len().cmp(&nb.len()).then(na.cmp(&nb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_lowercase().cmp(y.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// The column the file list is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Kind,
}

impl SortKey {
    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Kind => "kind",
        }
    }
    pub fn parse(s: &str) -> SortKey {
        match s {
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            "kind" => SortKey::Kind,
            _ => SortKey::Name,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub desc: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort { key: SortKey::Name, desc: false }
    }
}

/// Folders always come first; ties are broken by name.
fn sort_nodes(v: &mut [Node], sort: Sort) {
    use std::cmp::Ordering;
    v.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let o = match sort.key {
                SortKey::Name => Ordering::Equal,
                // Folders have no size of their own: keep them by name.
                SortKey::Size if a.is_dir => Ordering::Equal,
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.mtime.cmp(&b.mtime),
                SortKey::Kind => kind_of(&a.name, a.is_dir).cmp(&kind_of(&b.name, b.is_dir)),
            };
            let o = o.then_with(|| natural_cmp(&a.name, &b.name));
            if sort.desc {
                o.reverse()
            } else {
                o
            }
        })
    });
}

thread_local! {
    static KINDS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// Human description of a file type ("Rust source code", "PNG image"), cached
/// per extension.
pub fn kind_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".into();
    }
    let key = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
        _ => format!("/{name}"),
    };
    KINDS.with(|k| {
        k.borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                let (ct, _) = gio::content_type_guess(Some(name), None);
                let mut d = gio::content_type_get_description(&ct).to_string();
                if let Some(c) = d.get(..1) {
                    d = c.to_uppercase() + &d[1..];
                }
                d
            })
            .clone()
    })
}

/// "Today 14:32", "Mon 09:10", "12 Nov", "3 Feb 2024".
pub fn format_time(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let Ok(t) = glib::DateTime::from_unix_local(secs) else { return String::new() };
    let Ok(now) = glib::DateTime::now_local() else { return String::new() };
    let days = |d: &glib::DateTime| d.year() as i64 * 400 + d.day_of_year() as i64;
    let ago = days(&now) - days(&t);
    let fmt = if ago == 0 {
        "Today %H:%M"
    } else if ago == 1 {
        "Yesterday %H:%M"
    } else if (2..7).contains(&ago) {
        "%a %H:%M"
    } else if t.year() == now.year() {
        "%e %b"
    } else {
        "%e %b %Y"
    };
    t.format(fmt).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn mtime_of(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Directories never shown in the tree (like VS Code's files.exclude defaults).
fn excluded(name: &str) -> bool {
    matches!(name, ".git" | ".hg" | ".svn" | ".DS_Store")
}

pub fn read_dir(dir: &Path, ignore: &git::IgnoreChecker, parent_ignored: bool, sort: Sort) -> Vec<Node> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if excluded(&name) {
            continue;
        }
        let path = e.path();
        let ft = e.file_type().ok();
        let is_link = ft.map(|f| f.is_symlink()).unwrap_or(false);
        let is_dir = if is_link { path.is_dir() } else { ft.map(|f| f.is_dir()).unwrap_or(false) };
        let ignored = parent_ignored || ignore.is_ignored(&path);
        let meta = if is_link { std::fs::metadata(&path).ok() } else { e.metadata().ok() };
        let size = meta.as_ref().filter(|m| !m.is_dir()).map(|m| m.len()).unwrap_or(0);
        let mtime = meta.as_ref().map(mtime_of).unwrap_or(0);
        out.push(Node { path, name, is_dir, is_link, ignored, size, mtime, trash: None });
    }
    sort_nodes(&mut out, sort);
    out
}

fn read_trash(sort: Sort) -> Vec<Node> {
    let mut out = Vec::new();
    let trash = gio::File::for_uri(TRASH_ROOT);
    let home_files = glib::user_data_dir().join("Trash/files");
    if let Ok(en) = trash.enumerate_children(
        "standard::name,standard::display-name,standard::type,trash::orig-path,trash::deletion-date",
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        gio::Cancellable::NONE,
    ) {
        while let Ok(Some(info)) = en.next_file(gio::Cancellable::NONE) {
            let raw = info.name();
            let display = info.display_name().to_string();
            let orig = info
                .attribute_byte_string("trash::orig-path")
                .map(|s| PathBuf::from(s.to_string()))
                .unwrap_or_default();
            let deleted = info.attribute_string("trash::deletion-date").map(|s| s.replace('T', " ")).unwrap_or_default();
            let raw_name = raw.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let uri = trash.child(&raw).uri().to_string();
            let real = home_files.join(raw_name.trim_start_matches('\\'));
            let real = if real.exists() { real } else { home_files.join(&display) };
            let size = std::fs::symlink_metadata(&real).ok().filter(|m| !m.is_dir()).map(|m| m.len()).unwrap_or(0);
            let mtime = glib::DateTime::from_iso8601(&deleted.replace(' ', "T"), Some(&glib::TimeZone::local())).map(|d| d.to_unix()).unwrap_or(0);
            out.push(Node {
                path: real,
                name: display,
                is_dir: info.file_type() == gio::FileType::Directory,
                is_link: false,
                ignored: false,
                size,
                mtime,
                trash: Some(TrashInfo { uri, orig, deleted }),
            });
        }
    }
    sort_nodes(&mut out, sort);
    out
}

/// One row widget, recycled by the list view.
struct Row {
    root: gtk::Box,
    expander: gtk::TreeExpander,
    icon: gtk::Label,
    name: gtk::Label,
    detail: gtk::Label,
    badge: gtk::Label,
    node: RefCell<Option<Node>>,
    expanded_handler: RefCell<Option<(gtk::TreeListRow, glib::SignalHandlerId)>>,
}

/// The columns next to the name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Size,
    Modified,
    Kind,
}

/// One size / modified / kind cell, recycled by the column view.
struct AuxCell {
    label: gtk::Label,
    col: Col,
    cell: glib::WeakRef<gtk::ListItem>,
}

/// Marks a cell's child widget with its list item, to map widgets to rows.
const CELL_KEY: &str = "philly-cell";

#[derive(Default)]
pub struct Callbacks {
    pub selection: Option<Box<dyn Fn(Vec<Node>)>>,
    pub activate: Option<Box<dyn Fn(Node)>>,
    pub context: Option<Box<dyn Fn(Option<Node>, f64, f64)>>,
    pub dropped: Option<Box<dyn Fn(Vec<gio::File>, PathBuf, gdk::DragAction)>>,
    pub status_changed: Option<Box<dyn Fn()>>,
    /// Enter or double-click on a folder: open it.
    pub open_root: Option<Box<dyn Fn(PathBuf)>>,
}

pub struct Tree {
    pub widget: gtk::Box,
    pub list: gtk::ColumnView,
    pub header_label: gtk::Label,
    root: RefCell<PathBuf>,
    repo_root: RefCell<Option<PathBuf>>,
    pub status: RefCell<Option<Rc<git::Status>>>,
    stores: RefCell<HashMap<PathBuf, glib::WeakRef<gio::ListStore>>>,
    monitors: RefCell<HashMap<PathBuf, gio::FileMonitor>>,
    pending_dirs: RefCell<HashSet<PathBuf>>,
    git_pending: Cell<bool>,
    git_running: Cell<bool>,
    git_again: Cell<bool>,
    git_last: Cell<Option<std::time::Instant>>,
    git_monitors: RefCell<Vec<gio::FileMonitor>>,
    rows: RefCell<Vec<Rc<Row>>>,
    aux: RefCell<Vec<Rc<AuxCell>>>,
    /// Item counts of folders (all, not hidden), for the size column.
    dir_counts: RefCell<HashMap<PathBuf, (usize, usize)>>,
    counting: RefCell<HashSet<PathBuf>>,
    sort: Cell<Sort>,
    columns: Vec<(gtk::ColumnViewColumn, SortKey)>,
    /// Set while the sort indicator is changed from code.
    sort_quiet: Cell<bool>,
    cells_queued: Cell<bool>,
    pub on_sort_changed: RefCell<Option<Box<dyn Fn(Sort)>>>,
    pub model: RefCell<Option<gtk::TreeListModel>>,
    pub selection: gtk::MultiSelection,
    filter: gtk::CustomFilter,
    pub show_hidden: Cell<bool>,
    ignore: RefCell<Rc<git::IgnoreChecker>>,
    /// Picker file-type filter: returns true when a file may be shown.
    pub file_filter: RefCell<Option<Box<dyn Fn(&Node) -> bool>>>,
    pub dirs_only: Cell<bool>,
    pub cut: RefCell<HashSet<PathBuf>>,
    find_query: RefCell<String>,
    find_filter_mode: Cell<bool>,
    find_bar: gtk::Revealer,
    find_entry: gtk::Entry,
    find_toggle: gtk::ToggleButton,
    sticky: gtk::Box,
    scroller: gtk::ScrolledWindow,
    sticky_queued: Cell<bool>,
    sticky_paths: RefCell<Vec<PathBuf>>,
    pub cb: RefCell<Callbacks>,
    me: RefCell<Weak<Tree>>,
}

impl Tree {
    pub fn new() -> Rc<Tree> {
        let filter = gtk::CustomFilter::new(|_| true);
        let selection = gtk::MultiSelection::new(None::<gio::ListModel>);
        let list = gtk::ColumnView::new(Some(selection.clone()));
        list.add_css_class("tree");
        list.set_vexpand(true);
        list.set_single_click_activate(false);
        list.set_reorderable(false);
        list.set_show_column_separators(false);
        list.set_show_row_separators(false);
        let mut columns = Vec::new();
        for (title, key, width) in [("Name", SortKey::Name, -1), ("Size", SortKey::Size, 90), ("Modified", SortKey::Modified, 140), ("Kind", SortKey::Kind, 150)] {
            let c = gtk::ColumnViewColumn::new(Some(title), None::<gtk::ListItemFactory>);
            c.set_resizable(true);
            if width > 0 {
                c.set_fixed_width(crate::theme::px(width));
            } else {
                c.set_expand(true);
            }
            // The sorter only drives the header arrows: the folder stores are
            // sorted by us (see resort), which keeps list positions simple.
            c.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
            list.append_column(&c);
            columns.push((c, key));
        }

        let header_label = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Middle).build();

        let find_entry = gtk::Entry::builder().placeholder_text("Find in tree").hexpand(true).build();
        let find_toggle = gtk::ToggleButton::builder()
            .icon_name("view-list-bullet-symbolic")
            .tooltip_text("Filter (hide non-matching)")
            .css_classes(["flat"])
            .build();
        let find_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        find_box.add_css_class("search-box");
        find_box.append(&find_entry);
        find_box.append(&find_toggle);
        let find_bar = gtk::Revealer::builder().child(&find_box).reveal_child(false).build();

        let scrolled = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&find_bar);
        // Sticky folders (VS Code "sticky scroll"): the parent folders of the
        // top row stay visible above the list while scrolling.
        let sticky = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sticky.add_css_class("tree-sticky");
        sticky.set_valign(gtk::Align::Start);
        sticky.set_visible(false);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scrolled));
        overlay.add_overlay(&sticky);
        overlay.set_vexpand(true);
        widget.append(&overlay);

        let t = Rc::new(Tree {
            widget,
            list,
            header_label,
            root: RefCell::new(PathBuf::new()),
            repo_root: RefCell::new(None),
            status: RefCell::new(None),
            stores: RefCell::new(HashMap::new()),
            monitors: RefCell::new(HashMap::new()),
            pending_dirs: RefCell::new(HashSet::new()),
            git_pending: Cell::new(false),
            git_running: Cell::new(false),
            git_again: Cell::new(false),
            git_last: Cell::new(None),
            git_monitors: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
            aux: RefCell::new(Vec::new()),
            dir_counts: RefCell::new(HashMap::new()),
            counting: RefCell::new(HashSet::new()),
            sort: Cell::new(Sort::default()),
            columns,
            sort_quiet: Cell::new(false),
            cells_queued: Cell::new(false),
            on_sort_changed: RefCell::new(None),
            model: RefCell::new(None),
            selection,
            filter,
            show_hidden: Cell::new(true),
            ignore: RefCell::new(Rc::new(git::IgnoreChecker::new(None))),
            file_filter: RefCell::new(None),
            dirs_only: Cell::new(false),
            cut: RefCell::new(HashSet::new()),
            find_query: RefCell::new(String::new()),
            find_filter_mode: Cell::new(false),
            find_bar,
            find_entry,
            find_toggle,
            sticky,
            scroller: scrolled,
            sticky_queued: Cell::new(false),
            sticky_paths: RefCell::new(Vec::new()),
            cb: RefCell::new(Callbacks::default()),
            me: RefCell::new(Weak::new()),
        });
        *t.me.borrow_mut() = Rc::downgrade(&t);
        t.setup_filter();
        t.setup_factory();
        t.setup_signals();
        t.setup_keys();
        t.setup_find();
        t.setup_dnd();
        t.setup_sticky();
        t.setup_sort();
        t
    }

    fn setup_sort(self: &Rc<Self>) {
        self.show_sort();
        let Some(sorter) = self.list.sorter() else { return };
        let w = Rc::downgrade(self);
        sorter.connect_changed(move |s, _| {
            let Some(t) = w.upgrade() else { return };
            if t.sort_quiet.get() {
                return;
            }
            let Some(cs) = s.downcast_ref::<gtk::ColumnViewSorter>() else { return };
            let sort = match cs.primary_sort_column() {
                Some(c) => Sort {
                    key: t.columns.iter().find(|(col, _)| *col == c).map(|(_, k)| *k).unwrap_or(SortKey::Name),
                    desc: cs.primary_sort_order() == gtk::SortType::Descending,
                },
                None => Sort::default(),
            };
            t.set_sort(sort);
            if let Some(f) = t.on_sort_changed.borrow().as_ref() {
                f(sort);
            };
        });
    }

    /// Put the header arrow on the sort column.
    fn show_sort(&self) {
        let sort = self.sort.get();
        let col = self.columns.iter().find(|(_, k)| *k == sort.key).map(|(c, _)| c.clone());
        self.sort_quiet.set(true);
        // Clear first: the sorter keeps earlier columns as secondary keys and
        // would show their arrows too.
        self.list.sort_by_column(None, gtk::SortType::Ascending);
        self.list.sort_by_column(col.as_ref(), if sort.desc { gtk::SortType::Descending } else { gtk::SortType::Ascending });
        self.sort_quiet.set(false);
    }

    /// Sort the list by a column, keeping expanded folders and the selection.
    pub fn set_sort(&self, sort: Sort) {
        if self.sort.replace(sort) == sort {
            return;
        }
        self.show_sort();
        if let Some(c) = self.columns.iter().find(|(_, k)| *k == sort.key).map(|(c, _)| c) {
            c.set_visible(true);
        }
        let root = self.root();
        let store = self.stores.borrow().get(&root).and_then(|w| w.upgrade());
        let Some(store) = store else { return };
        let n = self.selection.n_items();
        let mut expanded = Vec::new();
        for i in 0..n {
            if let Some(lr) = self.row_at(i) {
                if lr.is_expanded() {
                    if let Some(nd) = node_of(lr.upcast_ref()) {
                        expanded.push(nd.path);
                    }
                }
            }
        }
        let selected: Vec<PathBuf> = self.selected().into_iter().map(|n| n.path).collect();
        let mut nodes: Vec<Node> = (0..store.n_items()).filter_map(|i| store.item(i).and_then(|o| node_of(&o))).collect();
        sort_nodes(&mut nodes, sort);
        let objs: Vec<glib::BoxedAnyObject> = nodes.into_iter().map(glib::BoxedAnyObject::new).collect();
        // Child folders are read again (sorted) when their rows expand.
        self.stores.borrow_mut().retain(|p, _| *p == root);
        store.splice(0, store.n_items(), &objs);
        for p in expanded {
            self.expand(&p);
        }
        if let Some(first) = selected.first() {
            self.reveal(first);
            for p in &selected[1..] {
                if let Some(pos) = self.find_pos(p, 0) {
                    self.selection.select_item(pos, false);
                }
            }
        } else {
            self.list.scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
        }
    }

    /// Show only the columns that fit next to a readable name column.
    pub fn fit_columns(&self, width: i32) {
        let z = crate::theme::zoom();
        let need = |px: f64| (px * z) as i32;
        // (column index, minimum list width to show it); Kind goes first.
        for (i, min) in [(1, need(300.0)), (2, need(400.0)), (3, need(560.0))] {
            let (c, key) = &self.columns[i];
            // The sort column stays: its header arrow tells how the list is sorted.
            let show = width >= min || *key == self.sort.get().key;
            if c.is_visible() != show {
                c.set_visible(show);
            }
        }
    }

    fn scroll_to(&self, pos: u32, flags: gtk::ListScrollFlags) {
        self.list.scroll_to(pos, None, flags, None);
    }

    /// List position of the row a widget inside the list belongs to.
    fn pos_of_widget(&self, mut w: gtk::Widget) -> Option<u32> {
        let list: &gtk::Widget = self.list.upcast_ref();
        loop {
            if &w == list {
                return None;
            }
            if let Some(p) = cell_pos(&w) {
                return Some(p);
            }
            if w.css_name() == "row" {
                // A row: look at the cells' children.
                let mut c = w.first_child();
                while let Some(cell) = c {
                    if let Some(p) = cell.first_child().and_then(|ch| cell_pos(&ch)) {
                        return Some(p);
                    }
                    c = cell.next_sibling();
                }
                return None;
            }
            w = w.parent()?;
        }
    }

    /// The column header (its height offsets the rows).
    fn header_height(&self) -> f64 {
        let mut c = self.list.first_child();
        while let Some(w) = c {
            if w.css_name() == "header" {
                return w.height() as f64;
            }
            c = w.next_sibling();
        }
        0.0
    }

    fn setup_sticky(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.scroller.vadjustment().connect_value_changed(move |_| {
            if let Some(t) = w.upgrade() {
                t.queue_sticky();
            }
        });
        let w = Rc::downgrade(self);
        self.selection.connect_items_changed(move |_, _, _, _| {
            if let Some(t) = w.upgrade() {
                t.queue_sticky();
            }
        });
    }

    fn queue_sticky(&self) {
        if self.sticky_queued.replace(true) {
            return;
        }
        let w = self.weak();
        glib::idle_add_local_once(move || {
            if let Some(t) = w.upgrade() {
                t.sticky_queued.set(false);
                t.update_sticky();
            }
        });
    }

    /// Height of one tree row in pixels.
    fn row_height(&self) -> f64 {
        // Rows live in the column view's inner list view.
        let mut c = self.list.first_child();
        while let Some(w) = c {
            let mut r = w.first_child();
            while let Some(row) = r {
                if row.css_name() == "row" && row.height() > 0 {
                    return row.height() as f64;
                }
                r = row.next_sibling();
            }
            c = w.next_sibling();
        }
        24.0
    }

    fn update_sticky(&self) {
        const MAX: usize = 4;
        let root = self.root();
        let scrolled_top = self.scroller.vadjustment().value() > 1.0;
        let mut chain: Vec<PathBuf> = Vec::new();
        let top = self.header_height();
        self.sticky.set_margin_top(top as i32);
        if scrolled_top && !self.is_trash() {
            let rh = self.row_height();
            // The rows under the sticky header are hidden by it, so look below
            // it; repeat until the number of sticky rows is stable.
            let mut k = 0usize;
            for _ in 0..MAX + 1 {
                let Some(pos) = self.pos_at(12.0, top + k as f64 * rh + rh * 0.5) else { break };
                let Some(n) = self.row_at(pos).and_then(|lr| node_of(lr.upcast_ref())) else { break };
                let mut anc: Vec<PathBuf> = n.path.ancestors().skip(1).take_while(|p| *p != root.as_path() && p.starts_with(&root)).map(Path::to_path_buf).collect();
                anc.reverse();
                if anc.len() > MAX {
                    anc.drain(..anc.len() - MAX);
                }
                let done = anc.len() == k;
                k = anc.len();
                chain = anc;
                if done {
                    break;
                }
            }
        }
        if *self.sticky_paths.borrow() == chain {
            return;
        }
        while let Some(c) = self.sticky.first_child() {
            self.sticky.remove(&c);
        }
        let base_depth = root.components().count();
        for p in &chain {
            let depth = p.components().count().saturating_sub(base_depth + 1);
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            row.add_css_class("tree-row");
            row.add_css_class("sticky-row");
            row.set_margin_start(crate::theme::px(4) + depth as i32 * crate::theme::px(14));
            let chevron = gtk::Image::from_icon_name("pan-down-symbolic");
            chevron.add_css_class("sticky-chevron");
            let icon = gtk::Label::new(None);
            icon.set_markup(&icons::markup(&name, true, true));
            icon.add_css_class("file-icon");
            let label = gtk::Label::builder().label(&name).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
            label.add_css_class("name");
            row.append(&chevron);
            row.append(&icon);
            row.append(&label);
            let click = gtk::GestureClick::new();
            let w = self.weak();
            let target = p.clone();
            click.connect_released(move |_, _, _, _| {
                if let Some(t) = w.upgrade() {
                    if let Some(pos) = t.find_pos(&target, 0) {
                        t.scroll_to(pos, gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT);
                    }
                }
            });
            row.add_controller(click);
            row.set_cursor_from_name(Some("pointer"));
            self.sticky.append(&row);
        }
        self.sticky.set_visible(!chain.is_empty());
        *self.sticky_paths.borrow_mut() = chain;
    }

    fn weak(&self) -> Weak<Tree> {
        self.me.borrow().clone()
    }

    pub fn root(&self) -> PathBuf {
        self.root.borrow().clone()
    }
    pub fn repo_root(&self) -> Option<PathBuf> {
        self.repo_root.borrow().clone()
    }
    pub fn is_trash(&self) -> bool {
        self.root.borrow().as_os_str() == TRASH_ROOT
    }

    fn setup_filter(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.filter.set_filter_func(move |obj| {
            let Some(t) = w.upgrade() else { return true };
            let Some(n) = node_of(obj) else { return true };
            t.passes(&n)
        });
    }

    fn passes(&self, n: &Node) -> bool {
        if n.trash.is_some() {
            return true;
        }
        if !self.show_hidden.get() && n.name.starts_with('.') {
            return false;
        }
        if !n.is_dir {
            if self.dirs_only.get() {
                return false;
            }
            if let Some(f) = self.file_filter.borrow().as_ref() {
                if !f(n) {
                    return false;
                }
            }
        }
        if self.find_filter_mode.get() {
            let q = self.find_query.borrow();
            if !q.is_empty() {
                return fuzzy_contains(&n.name, &q) || (n.is_dir && self.loaded_descendant_matches(&n.path, &q, 0));
            }
        }
        true
    }

    fn loaded_descendant_matches(&self, dir: &Path, q: &str, depth: usize) -> bool {
        if depth > 12 {
            return false;
        }
        let store = self.stores.borrow().get(dir).and_then(|w| w.upgrade());
        let Some(store) = store else { return false };
        for i in 0..store.n_items() {
            if let Some(n) = store.item(i).and_then(|o| node_of(&o)) {
                if fuzzy_contains(&n.name, q) || (n.is_dir && self.loaded_descendant_matches(&n.path, q, depth + 1)) {
                    return true;
                }
            }
        }
        false
    }

    pub fn refilter(&self) {
        self.filter.changed(gtk::FilterChange::Different);
        self.refresh_cells();
    }

    fn make_store(&self, dir: &Path, parent_ignored: bool) -> gio::ListModel {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let nodes = if dir.as_os_str() == TRASH_ROOT {
            read_trash(self.sort.get())
        } else {
            read_dir(dir, &self.ignore.borrow(), parent_ignored, self.sort.get())
        };
        let objs: Vec<glib::BoxedAnyObject> = nodes.into_iter().map(glib::BoxedAnyObject::new).collect();
        store.extend_from_slice(&objs);
        self.stores.borrow_mut().insert(dir.to_path_buf(), store.downgrade());
        self.watch_dir(dir);
        gtk::FilterListModel::new(Some(store), Some(self.filter.clone())).upcast()
    }

    fn watch_dir(&self, dir: &Path) {
        if self.monitors.borrow().contains_key(dir) {
            return;
        }
        let file = if dir.as_os_str() == TRASH_ROOT {
            gio::File::for_uri(TRASH_ROOT)
        } else {
            gio::File::for_path(dir)
        };
        let Ok(mon) = file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) else {
            return;
        };
        let w = self.weak();
        let d = dir.to_path_buf();
        mon.connect_changed(move |_, _, _, ev| {
            if matches!(ev, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::AttributeChanged) {
                return;
            }
            if let Some(t) = w.upgrade() {
                t.queue_refresh(&d);
            }
        });
        self.monitors.borrow_mut().insert(dir.to_path_buf(), mon);
    }

    fn queue_refresh(&self, dir: &Path) {
        let first = self.pending_dirs.borrow().is_empty();
        self.pending_dirs.borrow_mut().insert(dir.to_path_buf());
        if first {
            let w = self.weak();
            glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
                if let Some(t) = w.upgrade() {
                    let dirs: Vec<PathBuf> = t.pending_dirs.borrow_mut().drain().collect();
                    for d in dirs {
                        t.refresh_dir(&d);
                    }
                }
            });
        }
        self.queue_git_refresh();
    }

    /// Re-read one directory and merge changes into its store, keeping
    /// expansion state of unchanged entries.
    pub fn refresh_dir(&self, dir: &Path) {
        let store = self.stores.borrow().get(dir).and_then(|w| w.upgrade());
        let Some(store) = store else {
            self.stores.borrow_mut().remove(dir);
            self.monitors.borrow_mut().remove(dir);
            return;
        };
        let parent_ignored = self.ignore.borrow().is_ignored(dir);
        let new = if dir.as_os_str() == TRASH_ROOT {
            read_trash(self.sort.get())
        } else {
            read_dir(dir, &self.ignore.borrow(), parent_ignored, self.sort.get())
        };
        // Its item count changed (shown in the parent's size column).
        self.dir_counts.borrow_mut().remove(dir);
        let new_names: HashSet<&str> = new.iter().map(|n| n.name.as_str()).collect();
        // Remove vanished entries (back to front).
        let mut i = store.n_items();
        while i > 0 {
            i -= 1;
            if let Some(n) = store.item(i).and_then(|o| node_of(&o)) {
                if !new_names.contains(n.name.as_str()) {
                    store.remove(i);
                    if n.is_dir {
                        self.forget_subtree(&n.path);
                    }
                }
            }
        }
        // Insert new entries in sorted position; update changed ones. When
        // sorted by size or date an entry can also move.
        let present: HashSet<String> = (0..store.n_items()).filter_map(|i| store.item(i).and_then(|o| node_of(&o))).map(|n| n.name).collect();
        let mut meta_changed = false;
        for (pos, n) in new.into_iter().enumerate() {
            let pos = pos as u32;
            let cur = store.item(pos).and_then(|o| node_of(&o));
            match cur {
                Some(c) if c.name == n.name => {
                    if c.is_dir != n.is_dir || c.ignored != n.ignored {
                        store.splice(pos, 1, &[glib::BoxedAnyObject::new(n)]);
                    } else if c.size != n.size || c.mtime != n.mtime || c.is_link != n.is_link {
                        // Update in place: replacing the item would collapse a
                        // folder and drop the selection.
                        if let Some(obj) = store.item(pos).and_downcast::<glib::BoxedAnyObject>() {
                            *obj.borrow_mut::<Node>() = n;
                            meta_changed = true;
                        }
                    }
                }
                _ => {
                    if present.contains(&n.name) {
                        let old = (pos + 1..store.n_items()).find(|&i| store.item(i).and_then(|o| node_of(&o)).map(|c| c.name == n.name).unwrap_or(false));
                        if let Some(i) = old {
                            store.remove(i);
                        }
                    }
                    store.insert(pos, &glib::BoxedAnyObject::new(n));
                }
            }
        }
        if meta_changed {
            self.refresh_cells();
        }
        if dir.as_os_str() == TRASH_ROOT {
            if let Some(f) = self.cb.borrow().status_changed.as_ref() {
                f();
            }
        }
    }

    /// Re-read the size / modified / kind cells from their items.
    pub fn refresh_cells(&self) {
        self.aux.borrow_mut().retain(|a| a.cell.upgrade().is_some());
        let cells: Vec<Rc<AuxCell>> = self.aux.borrow().clone();
        for a in cells {
            if let Some(li) = a.cell.upgrade() {
                if let Some(n) = li.item().and_then(|o| node_of(&o)) {
                    self.fill_cell(&a, &n);
                }
            }
        }
    }

    fn fill_cell(&self, a: &AuxCell, n: &Node) {
        let text = match a.col {
            Col::Size if n.trash.is_some() && n.is_dir => String::new(),
            Col::Size if n.is_dir => match self.dir_count(&n.path) {
                Some(c) => crate::preview::items_label(c),
                None => String::new(),
            },
            Col::Size => crate::preview::human(n.size),
            Col::Modified => format_time(n.mtime),
            Col::Kind => kind_of(&n.name, n.is_dir),
        };
        a.label.set_text(&text);
        a.label.set_opacity(if n.ignored { 0.45 } else { 1.0 });
    }

    /// Item count of a folder, honouring "show hidden". Counted in the
    /// background the first time; cells refresh when it is known.
    fn dir_count(&self, dir: &Path) -> Option<usize> {
        if let Some((all, visible)) = self.dir_counts.borrow().get(dir) {
            return Some(if self.show_hidden.get() { *all } else { *visible });
        }
        if self.counting.borrow_mut().insert(dir.to_path_buf()) {
            let w = self.weak();
            let d = dir.to_path_buf();
            glib::spawn_future_local(async move {
                let d2 = d.clone();
                let counts = gio::spawn_blocking(move || {
                    let rd = std::fs::read_dir(&d2).ok()?;
                    let (mut all, mut visible) = (0usize, 0usize);
                    for e in rd.flatten() {
                        let name = e.file_name();
                        let name = name.to_string_lossy();
                        if excluded(&name) {
                            continue;
                        }
                        all += 1;
                        if !name.starts_with('.') {
                            visible += 1;
                        }
                    }
                    Some((all, visible))
                })
                .await
                .ok()
                .flatten();
                let Some(t) = w.upgrade() else { return };
                t.counting.borrow_mut().remove(&d);
                if let Some(c) = counts {
                    t.dir_counts.borrow_mut().insert(d, c);
                    t.queue_cells();
                }
            });
        }
        None
    }

    /// Refresh the cells once after a burst of folder counts.
    fn queue_cells(&self) {
        if self.cells_queued.replace(true) {
            return;
        }
        let w = self.weak();
        glib::idle_add_local_once(move || {
            if let Some(t) = w.upgrade() {
                t.cells_queued.set(false);
                t.refresh_cells();
            }
        });
    }

    fn forget_subtree(&self, dir: &Path) {
        self.stores.borrow_mut().retain(|p, _| !p.starts_with(dir));
        self.monitors.borrow_mut().retain(|p, _| !p.starts_with(dir));
    }

    pub fn refresh_all(&self) {
        let dirs: Vec<PathBuf> = self.stores.borrow().keys().cloned().collect();
        *self.ignore.borrow_mut() = Rc::new(git::IgnoreChecker::new(self.repo_root.borrow().as_deref()));
        for d in dirs {
            self.refresh_dir(&d);
        }
        self.queue_git_refresh();
    }

    pub fn queue_git_refresh(&self) {
        if self.repo_root.borrow().is_none() || self.git_pending.replace(true) {
            return;
        }
        // Coalesce bursts: wait 300 ms after the first change, and run at most
        // once per 1.5 s while changes keep coming (git status is not cheap).
        let since = self.git_last.get().map(|t| t.elapsed()).unwrap_or(std::time::Duration::MAX);
        let delay = std::time::Duration::from_millis(1500).saturating_sub(since).max(std::time::Duration::from_millis(300));
        let w = self.weak();
        glib::timeout_add_local_once(delay, move || {
            if let Some(t) = w.upgrade() {
                t.git_pending.set(false);
                if t.git_running.get() {
                    t.git_again.set(true);
                } else {
                    t.reload_git();
                }
            }
        });
    }

    pub fn reload_git(&self) {
        let Some(root) = self.repo_root.borrow().clone() else { return };
        let w = self.weak();
        self.git_running.set(true);
        glib::spawn_future_local(async move {
            let r2 = root.clone();
            let st = gio::spawn_blocking(move || git::status(&r2)).await.ok().flatten();
            let Some(t) = w.upgrade() else { return };
            t.git_running.set(false);
            t.git_last.set(Some(std::time::Instant::now()));
            if t.git_again.replace(false) {
                t.queue_git_refresh();
            }
            if t.repo_root.borrow().as_ref() != Some(&root) {
                return;
            }
            if t.status.borrow().as_deref() == st.as_ref() {
                return; // nothing changed: skip restyling and panel rebuilds
            }
            *t.status.borrow_mut() = st.map(Rc::new);
            t.restyle_rows();
            if let Some(f) = t.cb.borrow().status_changed.as_ref() {
                f();
            };
        });
    }

    fn watch_git(&self, repo: &Path) {
        self.git_monitors.borrow_mut().clear();
        let gitdir = repo.join(".git");
        for p in [gitdir.join("index"), gitdir.join("HEAD"), gitdir.join("refs/heads")] {
            let f = gio::File::for_path(&p);
            let mon = if p.is_dir() {
                f.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
            } else {
                f.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
            };
            if let Ok(mon) = mon {
                let w = self.weak();
                mon.connect_changed(move |_, _, _, _| {
                    if let Some(t) = w.upgrade() {
                        t.queue_git_refresh();
                    }
                });
                self.git_monitors.borrow_mut().push(mon);
            }
        }
    }

    /// Show `dir` as the tree root.
    pub fn set_root(&self, dir: &Path) {
        let is_trash = dir.as_os_str() == TRASH_ROOT;
        *self.root.borrow_mut() = dir.to_path_buf();
        self.stores.borrow_mut().clear();
        self.monitors.borrow_mut().clear();
        self.dir_counts.borrow_mut().clear();
        let repo = if is_trash { None } else { git::discover(dir) };
        *self.ignore.borrow_mut() = Rc::new(git::IgnoreChecker::new(repo.as_deref()));
        let repo_changed = *self.repo_root.borrow() != repo;
        *self.repo_root.borrow_mut() = repo.clone();
        if repo_changed {
            *self.status.borrow_mut() = None;
            match &repo {
                Some(r) => self.watch_git(r),
                None => self.git_monitors.borrow_mut().clear(),
            }
        }
        let title = if is_trash {
            "TRASH".to_string()
        } else {
            dir.file_name().map(|s| s.to_string_lossy().to_uppercase()).unwrap_or_else(|| "/".into())
        };
        self.header_label.set_text(&title);
        self.header_label.set_tooltip_text(Some(&dir.to_string_lossy()));

        let w = self.weak();
        let root_ignored = self.ignore.borrow().is_ignored(dir);
        let root_model = self.make_store(dir, root_ignored);
        let model = gtk::TreeListModel::new(root_model, false, false, move |obj| {
            let t = w.upgrade()?;
            let n = node_of(obj)?;
            if !n.is_dir || n.trash.is_some() {
                return None;
            }
            Some(t.make_store(&n.path, n.ignored))
        });
        self.selection.set_model(Some(&model));
        *self.model.borrow_mut() = Some(model);
        self.reload_git();
        if self.selection.n_items() > 0 {
            self.selection.unselect_all();
        }
    }

    pub fn git_kind(&self, path: &Path) -> Option<git::Kind> {
        self.status.borrow().as_ref().and_then(|s| s.files.get(path).copied())
    }

    fn style_row(&self, row: &Row, n: &Node, expanded: bool) {
        row.icon.set_markup(&icons::markup(&n.name, n.is_dir, expanded));
        row.name.set_text(&n.name);
        for c in ["git-M", "git-A", "git-U", "git-D", "git-R", "git-C", "git-dir", "ignored", "cut"] {
            row.root.remove_css_class(c);
        }
        if n.ignored {
            row.root.add_css_class("ignored");
        }
        if self.cut.borrow().contains(&n.path) {
            row.root.add_css_class("cut");
        }
        row.badge.set_text("");
        row.detail.set_text("");
        if let Some(t) = &n.trash {
            row.detail.set_text(&t.orig.parent().map(|p| shorten_home(p)).unwrap_or_default());
            return;
        }
        if n.is_link {
            row.detail.set_text("\u{f0c1}");
        }
        let st = self.status.borrow();
        if let Some(st) = st.as_ref() {
            if let Some(k) = st.files.get(&n.path) {
                row.root.add_css_class(k.class());
                row.badge.set_text(k.letter());
            } else if n.is_dir && st.dirs.contains(&n.path) {
                row.root.add_css_class("git-dir");
                row.badge.set_text("\u{2022}");
            }
        }
    }

    fn restyle_rows(&self) {
        for r in self.rows.borrow().iter() {
            let n = r.node.borrow().clone();
            if let Some(n) = n {
                let expanded = r.expander.list_row().map(|lr| lr.is_expanded()).unwrap_or(false);
                self.style_row(r, &n, expanded);
            }
        }
    }

    /// After a theme or zoom change: rows and the sticky header carry colours
    /// and pixel sizes set from code.
    pub fn restyle(&self) {
        for (i, base) in [(1, 90), (2, 140), (3, 150)] {
            self.columns[i].0.set_fixed_width(crate::theme::px(base));
        }
        self.restyle_rows();
        self.sticky_paths.borrow_mut().clear();
        self.update_sticky();
    }

    fn setup_factory(self: &Rc<Self>) {
        let factory = gtk::SignalListItemFactory::new();
        let w = Rc::downgrade(self);
        factory.connect_setup(move |_, li| {
            let li = li.downcast_ref::<gtk::ListItem>().unwrap();
            let icon = gtk::Label::new(None);
            icon.add_css_class("file-icon");
            let name = gtk::Label::builder().xalign(0.0).hexpand(true).width_chars(12).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
            name.add_css_class("name");
            let detail = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::Start).max_width_chars(30).build();
            detail.add_css_class("detail");
            let badge = gtk::Label::new(None);
            badge.add_css_class("badge");
            let inner = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            inner.append(&icon);
            inner.append(&name);
            inner.append(&detail);
            inner.append(&badge);
            let expander = gtk::TreeExpander::new();
            expander.set_indent_for_icon(true);
            expander.set_indent_for_depth(true);
            expander.set_child(Some(&inner));
            expander.set_hexpand(true);
            let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            root.add_css_class("tree-row");
            root.append(&expander);
            li.set_child(Some(&root));
            unsafe { root.set_data(CELL_KEY, li.downgrade()) };
            let row = Rc::new(Row {
                root,
                expander,
                icon,
                name,
                detail,
                badge,
                node: RefCell::new(None),
                expanded_handler: RefCell::new(None),
            });
            if let Some(t) = w.upgrade() {
                t.rows.borrow_mut().push(row.clone());
            }
            unsafe { li.set_data("philly-row", row) };
        });
        let w = Rc::downgrade(self);
        factory.connect_bind(move |_, li| {
            let li = li.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(t) = w.upgrade() else { return };
            let row: Rc<Row> = unsafe { li.data::<Rc<Row>>("philly-row").unwrap().as_ref().clone() };
            let Some(lr) = li.item().and_downcast::<gtk::TreeListRow>() else { return };
            row.expander.set_list_row(Some(&lr));
            let Some(n) = node_of(lr.upcast_ref()) else { return };
            t.style_row(&row, &n, lr.is_expanded());
            row.root.set_tooltip_text(None);
            *row.node.borrow_mut() = Some(n.clone());
            let wt = Rc::downgrade(&t);
            let rw = Rc::downgrade(&row);
            let id = lr.connect_expanded_notify(move |lr| {
                if let (Some(t), Some(r)) = (wt.upgrade(), rw.upgrade()) {
                    if let Some(n) = r.node.borrow().clone() {
                        r.icon.set_markup(&icons::markup(&n.name, n.is_dir, lr.is_expanded()));
                    }
                    let _ = t;
                }
            });
            *row.expanded_handler.borrow_mut() = Some((lr, id));
        });
        factory.connect_unbind(move |_, li| {
            let li = li.downcast_ref::<gtk::ListItem>().unwrap();
            let row: Rc<Row> = unsafe { li.data::<Rc<Row>>("philly-row").unwrap().as_ref().clone() };
            if let Some((lr, id)) = row.expanded_handler.borrow_mut().take() {
                lr.disconnect(id);
            }
            *row.node.borrow_mut() = None;
            row.expander.set_list_row(None);
        });
        self.columns[0].0.set_factory(Some(&factory));

        for (i, col) in [(1, Col::Size), (2, Col::Modified), (3, Col::Kind)] {
            let factory = gtk::SignalListItemFactory::new();
            let w = Rc::downgrade(self);
            factory.connect_setup(move |_, li| {
                let li = li.downcast_ref::<gtk::ListItem>().unwrap();
                let label = gtk::Label::builder()
                    .xalign(if col == Col::Size { 1.0 } else { 0.0 })
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                label.add_css_class("cell-detail");
                if col == Col::Size {
                    label.add_css_class("numeric");
                }
                li.set_child(Some(&label));
                unsafe { label.set_data(CELL_KEY, li.downgrade()) };
                let a = Rc::new(AuxCell { label, col, cell: li.downgrade() });
                if let Some(t) = w.upgrade() {
                    t.aux.borrow_mut().push(a.clone());
                }
                unsafe { li.set_data("philly-aux", a) };
            });
            let w = Rc::downgrade(self);
            factory.connect_bind(move |_, li| {
                let li = li.downcast_ref::<gtk::ListItem>().unwrap();
                let Some(t) = w.upgrade() else { return };
                let a: Rc<AuxCell> = unsafe { li.data::<Rc<AuxCell>>("philly-aux").unwrap().as_ref().clone() };
                match li.item().and_then(|o| node_of(&o)) {
                    Some(n) => t.fill_cell(&a, &n),
                    None => a.label.set_text(""),
                }
            });
            self.columns[i].0.set_factory(Some(&factory));
        }
    }

    fn setup_signals(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(t) = w.upgrade() {
                let sel = t.selected();
                if let Some(f) = t.cb.borrow().selection.as_ref() {
                    f(sel);
                }
            }
        });
        let w = Rc::downgrade(self);
        self.list.connect_activate(move |_, pos| {
            let Some(t) = w.upgrade() else { return };
            let Some(lr) = t.row_at(pos) else { return };
            let Some(n) = node_of(lr.upcast_ref()) else { return };
            if n.is_dir && n.trash.is_none() {
                // Enter or double-click on a folder: go into it.
                if let Some(f) = t.cb.borrow().open_root.as_ref() {
                    f(n.path.clone());
                }
                return;
            }
            if let Some(f) = t.cb.borrow().activate.as_ref() {
                f(n);
            };
        });

        let right = gtk::GestureClick::builder().button(3).build();
        let w = Rc::downgrade(self);
        right.connect_pressed(move |g, _, x, y| {
            let Some(t) = w.upgrade() else { return };
            g.set_state(gtk::EventSequenceState::Claimed);
            let node = t.pos_at(x, y).and_then(|pos| {
                if !t.selection.is_selected(pos) {
                    t.selection.select_item(pos, true);
                }
                t.row_at(pos).and_then(|lr| node_of(lr.upcast_ref()))
            });
            if let Some(f) = t.cb.borrow().context.as_ref() {
                f(node, x, y);
            };
        });
        self.list.add_controller(right);
    }

    /// List position under widget coordinates.
    fn pos_at(&self, x: f64, y: f64) -> Option<u32> {
        let w = self.list.pick(x, y, gtk::PickFlags::DEFAULT)?;
        self.pos_of_widget(w)
    }

    pub fn row_at(&self, pos: u32) -> Option<gtk::TreeListRow> {
        self.selection.item(pos).and_downcast::<gtk::TreeListRow>()
    }

    pub fn selected(&self) -> Vec<Node> {
        let bs = self.selection.selection();
        let mut out = Vec::new();
        if let Some((iter, first)) = gtk::BitsetIter::init_first(&bs) {
            for pos in std::iter::once(first).chain(iter) {
                if let Some(n) = self.row_at(pos).and_then(|lr| node_of(lr.upcast_ref())) {
                    out.push(n);
                }
            }
        }
        out
    }

    /// Position of the keyboard focus row (falls back to first selected).
    pub fn cursor_pos(&self) -> Option<u32> {
        let bs = self.selection.selection();
        if bs.is_empty() {
            return None;
        }
        Some(bs.minimum())
    }

    fn setup_keys(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(t) = w.upgrade() else { return glib::Propagation::Proceed };
            let mods = state & (gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
            if !mods.is_empty() {
                return glib::Propagation::Proceed;
            }
            let focus_pos = t.focus_pos();
            match key {
                gdk::Key::Right => {
                    if let Some(pos) = focus_pos {
                        if let Some(lr) = t.row_at(pos) {
                            if lr.is_expandable() && !lr.is_expanded() {
                                lr.set_expanded(true);
                            } else if lr.is_expanded() {
                                t.select_pos(pos + 1);
                            }
                        }
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Left => {
                    if let Some(pos) = focus_pos {
                        if let Some(lr) = t.row_at(pos) {
                            if lr.is_expanded() {
                                lr.set_expanded(false);
                            } else if let Some(parent) = lr.parent() {
                                t.select_pos(parent.position());
                            }
                        }
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Escape if t.find_bar.reveals_child() => {
                    t.close_find();
                    glib::Propagation::Stop
                }
                _ => {
                    if state.contains(gdk::ModifierType::SHIFT_MASK) && key.to_unicode().map(|c| !c.is_alphanumeric()).unwrap_or(true) {
                        return glib::Propagation::Proceed;
                    }
                    if let Some(c) = key.to_unicode() {
                        if !c.is_control() && c != ' ' {
                            t.open_find(Some(c));
                            return glib::Propagation::Stop;
                        }
                    }
                    glib::Propagation::Proceed
                }
            }
        });
        self.list.add_controller(keys);
    }

    fn focus_pos(&self) -> Option<u32> {
        self.focused_row_pos().or_else(|| self.cursor_pos())
    }

    /// The focused row widget tells us the keyboard cursor position.
    fn focused_row_pos(&self) -> Option<u32> {
        let root = self.list.root()?;
        let f = gtk::prelude::RootExt::focus(&root)?;
        if !f.is_ancestor(&self.list) {
            return None;
        }
        self.pos_of_widget(f)
    }

    pub fn select_pos(&self, pos: u32) {
        if pos >= self.selection.n_items() {
            return;
        }
        self.scroll_to(pos, gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT);
    }

    pub fn focus(&self) {
        if let Some(p) = self.cursor_pos() {
            self.scroll_to(p, gtk::ListScrollFlags::FOCUS);
        } else if self.selection.n_items() > 0 {
            self.scroll_to(0, gtk::ListScrollFlags::FOCUS);
        }
        self.list.grab_focus();
    }

    /// Expand ancestors of `path` and select it.
    pub fn reveal(&self, path: &Path) -> bool {
        let root = self.root();
        let Ok(rel) = path.strip_prefix(&root) else { return false };
        let mut cur = root.clone();
        let comps: Vec<_> = rel.components().collect();
        let mut pos: Option<u32> = None;
        let mut start = 0u32;
        for (i, c) in comps.iter().enumerate() {
            cur.push(c);
            let found = self.find_pos(&cur, start);
            let Some(p) = found else { return false };
            pos = Some(p);
            if i + 1 < comps.len() {
                if let Some(lr) = self.row_at(p) {
                    lr.set_expanded(true);
                }
                start = p + 1;
            }
        }
        if let Some(p) = pos {
            self.scroll_to(p, gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT);
            return true;
        }
        false
    }

    /// Expand a folder row that is currently visible in the tree.
    pub fn expand(&self, path: &Path) {
        if let Some(lr) = self.find_pos(path, 0).and_then(|p| self.row_at(p)) {
            lr.set_expanded(true);
        }
    }

    fn find_pos(&self, path: &Path, start: u32) -> Option<u32> {
        let n = self.selection.n_items();
        (start..n).find(|&i| self.row_at(i).and_then(|lr| node_of(lr.upcast_ref())).map(|nd| nd.path == path).unwrap_or(false))
    }

    pub fn collapse_all(&self) {
        let n = self.selection.n_items();
        for i in (0..n).rev() {
            if let Some(lr) = self.row_at(i) {
                if lr.depth() == 0 && lr.is_expanded() {
                    lr.set_expanded(false);
                }
            }
        }
    }

    /// Directory where a new file would be created: the selected folder, or the
    /// selected file's folder, or the root.
    pub fn target_dir(&self) -> PathBuf {
        match self.selected().first() {
            Some(n) if n.is_dir => n.path.clone(),
            Some(n) => n.path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.root()),
            None => self.root(),
        }
    }

    // ---------- find / filter ----------

    fn setup_find(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.find_entry.connect_changed(move |e| {
            let Some(t) = w.upgrade() else { return };
            *t.find_query.borrow_mut() = e.text().to_lowercase();
            if t.find_filter_mode.get() {
                t.refilter();
            }
            t.find_next(0, true);
        });
        let w = Rc::downgrade(self);
        self.find_toggle.connect_toggled(move |b| {
            if let Some(t) = w.upgrade() {
                t.find_filter_mode.set(b.is_active());
                t.refilter();
            }
        });
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(t) = w.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::Escape => {
                    t.close_find();
                    glib::Propagation::Stop
                }
                gdk::Key::Down => {
                    t.find_next(1, false);
                    glib::Propagation::Stop
                }
                gdk::Key::Up => {
                    t.find_next(-1, false);
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    t.close_find_keep();
                    if let Some(p) = t.cursor_pos() {
                        t.list.emit_by_name::<()>("activate", &[&p]);
                    }
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.find_entry.add_controller(keys);
    }

    pub fn open_find(&self, initial: Option<char>) {
        self.find_bar.set_reveal_child(true);
        self.find_entry.grab_focus();
        if let Some(c) = initial {
            self.find_entry.set_text(&c.to_string());
            self.find_entry.set_position(-1);
        }
    }

    fn close_find_keep(&self) {
        self.find_bar.set_reveal_child(false);
        self.find_entry.set_text("");
        self.find_query.borrow_mut().clear();
        if self.find_filter_mode.get() {
            self.refilter();
        }
        self.focus();
    }

    pub fn close_find(&self) {
        self.close_find_keep();
    }

    /// Select the next visible row matching the query.
    fn find_next(&self, dir: i32, include_current: bool) {
        let q = self.find_query.borrow().clone();
        if q.is_empty() {
            return;
        }
        let n = self.selection.n_items();
        if n == 0 {
            return;
        }
        let cur = self.cursor_pos().unwrap_or(0) as i64;
        let step = if dir < 0 { -1i64 } else { 1 };
        let first = if include_current { 0 } else { 1 };
        for k in first..=(n as i64) {
            let p = (cur + step * k).rem_euclid(n as i64) as u32;
            if let Some(nd) = self.row_at(p).and_then(|lr| node_of(lr.upcast_ref())) {
                if fuzzy_contains(&nd.name, &q) {
                    self.scroll_to(p, gtk::ListScrollFlags::SELECT);
                    return;
                }
            }
        }
    }

    // ---------- drag & drop ----------

    fn setup_dnd(self: &Rc<Self>) {
        let src = gtk::DragSource::new();
        // Copy only: Hyprland cancels drags offering more than one action
        // (other apps never see them). Moves within philly are decided by
        // philly's own drop handler.
        src.set_actions(gdk::DragAction::COPY);
        let w = Rc::downgrade(self);
        src.connect_prepare(move |_, x, y| {
            let t = w.upgrade()?;
            let pos = t.pos_at(x, y)?;
            if !t.selection.is_selected(pos) {
                t.selection.select_item(pos, true);
            }
            let files: Vec<gio::File> = t.selected().iter().filter(|n| n.trash.is_none()).map(|n| gio::File::for_path(&n.path)).collect();
            if files.is_empty() {
                return None;
            }
            if std::env::var_os("PHILLY_DEBUG").is_some() {
                eprintln!("drag prepare: {} files", files.len());
            }
            let list = gdk::FileList::from_array(&files);
            let uris: String = files.iter().map(|f| format!("{}\r\n", f.uri())).collect();
            let bytes = glib::Bytes::from_owned(uris.into_bytes());
            Some(gdk::ContentProvider::new_union(&[
                gdk::ContentProvider::for_value(&list.to_value()),
                gdk::ContentProvider::for_bytes("text/uri-list", &bytes),
            ]))
        });
        let w = Rc::downgrade(self);
        src.connect_drag_begin(move |s, _| {
            if let Some(t) = w.upgrade() {
                let n = t.selected().len();
                let label = if n == 1 { t.selected()[0].name.clone() } else { format!("{n} items") };
                let icon = gtk::Label::new(Some(&label));
                let paintable = gtk::WidgetPaintable::new(Some(&icon));
                s.set_icon(Some(&paintable), 0, 0);
            }
        });
        if std::env::var_os("PHILLY_DEBUG").is_some() {
            src.connect_drag_end(|_, d, del| eprintln!("drag end: action={:?} delete={del}", d.selected_action()));
            src.connect_drag_cancel(|_, _, r| {
                eprintln!("drag cancel: {r:?}");
                false
            });
        }
        self.list.add_controller(src);

        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY | gdk::DragAction::MOVE);
        drop.set_preload(true);
        let w = Rc::downgrade(self);
        drop.connect_drop(move |dt, value, x, y| {
            let Some(t) = w.upgrade() else { return false };
            let Ok(list) = value.get::<gdk::FileList>() else { return false };
            let dest = t
                .pos_at(x, y)
                .and_then(|p| t.row_at(p))
                .and_then(|lr| node_of(lr.upcast_ref()))
                .map(|n| if n.is_dir { n.path } else { n.path.parent().unwrap().to_path_buf() })
                .unwrap_or_else(|| t.root());
            let action = dt.current_drop().map(|d| d.actions()).unwrap_or(gdk::DragAction::COPY);
            // Default to move within the same filesystem tree, copy from elsewhere.
            let shift = dt.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            let ctrl = dt.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
            let files = list.files();
            let internal = files.iter().all(|f| f.path().map(|p| p.starts_with(t.root())).unwrap_or(false));
            let from_philly = dt.current_drop().and_then(|d| d.drag()).is_some();
            let act = if ctrl {
                gdk::DragAction::COPY
            } else if shift || (internal && (from_philly || action.contains(gdk::DragAction::MOVE))) {
                gdk::DragAction::MOVE
            } else {
                gdk::DragAction::COPY
            };
            if let Some(f) = t.cb.borrow().dropped.as_ref() {
                f(files, dest, act);
            }
            true
        });
        let w = Rc::downgrade(self);
        drop.connect_enter(move |_, _, _| {
            if let Some(t) = w.upgrade() {
                t.list.add_css_class("drop-target");
            }
            gdk::DragAction::COPY
        });
        let w = Rc::downgrade(self);
        drop.connect_leave(move |_| {
            if let Some(t) = w.upgrade() {
                t.list.remove_css_class("drop-target");
            }
        });
        self.list.add_controller(drop);
    }
}

fn cell_pos(w: &gtk::Widget) -> Option<u32> {
    // SAFETY: CELL_KEY is only ever set to a WeakRef<ListItem>.
    let li = unsafe { w.data::<glib::WeakRef<gtk::ListItem>>(CELL_KEY) }?;
    let li = unsafe { li.as_ref() }.upgrade()?;
    let p = li.position();
    (p != gtk::INVALID_LIST_POSITION).then_some(p)
}

pub fn fuzzy_contains(name: &str, q: &str) -> bool {
    let name = name.to_lowercase();
    if name.contains(q) {
        return true;
    }
    let mut it = name.chars();
    q.chars().all(|c| it.any(|n| n == c))
}

pub fn shorten_home(p: &Path) -> String {
    let home = glib::home_dir();
    match p.strip_prefix(&home) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}
