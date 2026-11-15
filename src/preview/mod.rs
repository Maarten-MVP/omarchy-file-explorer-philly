//! The left preview panel.

pub mod diff;
mod folderview;
mod frame;
pub mod markdown;

use crate::{git, icons, theme, tree::shorten_home};
use gtk::{gdk, gio, glib, prelude::*};
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

const MAX_TEXT: usize = 2 * 1024 * 1024;
const MAX_PDF_PAGES: i32 = 60;

#[derive(Clone)]
enum Shown {
    Nothing,
    File(PathBuf),
    Diff(PathBuf, git::Change),
}

pub struct Preview {
    pub widget: gtk::Box,
    icon: gtk::Label,
    title: gtk::Label,
    meta: gtk::Label,
    split_btn: gtk::ToggleButton,
    source_btn: gtk::ToggleButton,
    open_btn: gtk::Button,
    stack: gtk::Stack,
    code_buf: sourceview5::Buffer,
    code_view: sourceview5::View,
    md_view: gtk::TextView,
    image: gtk::Picture,
    pdf_box: gtk::Box,
    info_holder: gtk::Box,
    diff_holder: gtk::Box,
    gen: Rc<Cell<u64>>,
    shown: RefCell<Shown>,
    pub split: Cell<bool>,
    pending_line: Rc<Cell<u32>>,
    pub on_open: RefCell<Option<Box<dyn Fn(PathBuf)>>>,
    /// Select an entry in the tree (from a folder view).
    pub on_pick: Rc<RefCell<Option<Box<dyn Fn(PathBuf)>>>>,
    pub open_cb: Rc<RefCell<Option<Box<dyn Fn(PathBuf)>>>>,
    folder_holder: gtk::Box,
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder().child(child).vexpand(true).hexpand(true).build()
}

fn human_size(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

pub fn human(n: u64) -> String {
    human_size(n)
}

fn keycap_row(keys: &str, desc: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    b.set_halign(gtk::Align::Center);
    let k = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    k.set_size_request(150, -1);
    k.set_halign(gtk::Align::End);
    for part in keys.split(' ') {
        let l = gtk::Label::new(Some(part));
        l.add_css_class("keycap");
        k.append(&l);
    }
    let d = gtk::Label::builder().label(desc).xalign(0.0).width_chars(18).build();
    d.add_css_class("hint");
    b.append(&k);
    b.append(&d);
    b
}

impl Preview {
    pub fn new() -> Rc<Preview> {
        // Header
        let icon = gtk::Label::new(None);
        icon.add_css_class("file-icon");
        let title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Start).build();
        title.add_css_class("title");
        let meta = gtk::Label::builder().xalign(0.0).hexpand(true).margin_start(10).ellipsize(gtk::pango::EllipsizeMode::End).build();
        meta.add_css_class("meta");
        let split_btn = gtk::ToggleButton::builder()
            .icon_name("view-dual-symbolic")
            .tooltip_text("Side-by-side diff (Ctrl+\\)")
            .css_classes(["flat"])
            .visible(false)
            .build();
        let source_btn = gtk::ToggleButton::builder()
            .icon_name("text-x-generic-symbolic")
            .tooltip_text("Show source (Ctrl+Shift+V)")
            .css_classes(["flat"])
            .visible(false)
            .build();
        let open_btn = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text("Open in default app (Enter)")
            .css_classes(["flat"])
            .visible(false)
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.add_css_class("preview-header");
        header.append(&icon);
        header.append(&title);
        header.append(&meta);
        header.append(&source_btn);
        header.append(&split_btn);
        header.append(&open_btn);

        // Code
        let code_buf = sourceview5::Buffer::new(None);
        code_buf.set_highlight_matching_brackets(false);
        let code_view = sourceview5::View::with_buffer(&code_buf);
        code_view.set_editable(false);
        code_view.set_monospace(true);
        code_view.set_show_line_numbers(true);
        code_view.set_highlight_current_line(true);
        code_view.set_top_margin(8);
        code_view.set_bottom_margin(40);
        code_view.set_left_margin(8);
        code_view.add_css_class("code");

        // Markdown
        let md_view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(36)
            .right_margin(36)
            .top_margin(24)
            .bottom_margin(48)
            .pixels_below_lines(2)
            .build();
        md_view.add_css_class("markdown");

        let image = gtk::Picture::builder().can_shrink(true).content_fit(gtk::ContentFit::ScaleDown).build();
        image.set_margin_top(16);
        image.set_margin_bottom(16);
        image.set_margin_start(16);
        image.set_margin_end(16);
        let image_stage = gtk::Box::new(gtk::Orientation::Vertical, 0);
        image_stage.add_css_class("image-stage");
        image.set_vexpand(true);
        image_stage.append(&image);

        let pdf_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pdf_box.set_halign(gtk::Align::Center);

        let info_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let diff_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        diff_holder.set_vexpand(true);

        // Empty state with the main shortcuts
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 8);
        empty.add_css_class("preview-empty");
        empty.set_valign(gtk::Align::Center);
        let big = gtk::Label::new(Some("\u{f07b}"));
        big.add_css_class("big");
        big.set_margin_bottom(16);
        empty.append(&big);
        for (k, d) in [
            ("Ctrl P", "Go to file"),
            ("Ctrl L", "Type a path"),
            ("Ctrl Shift F", "Search in files"),
            ("Ctrl Shift G", "Source control"),
            ("Ctrl /", "All shortcuts"),
        ] {
            empty.append(&keycap_row(k, d));
        }

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(90);
        stack.set_vexpand(true);
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scrolled(&code_view), Some("code"));
        stack.add_named(&scrolled(&md_view), Some("markdown"));
        stack.add_named(&image_stage, Some("image"));
        stack.add_named(&scrolled(&pdf_box), Some("pdf"));
        stack.add_named(&scrolled(&info_holder), Some("info"));
        stack.add_named(&diff_holder, Some("diff"));
        let folder_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stack.add_named(&folder_holder, Some("folder"));

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("preview-pane");
        widget.append(&header);
        widget.append(&stack);

        let p = Rc::new(Preview {
            widget,
            icon,
            title,
            meta,
            split_btn,
            source_btn,
            open_btn,
            stack,
            code_buf,
            code_view,
            md_view,
            image,
            pdf_box,
            info_holder,
            diff_holder,
            gen: Rc::new(Cell::new(0)),
            shown: RefCell::new(Shown::Nothing),
            split: Cell::new(false),
            pending_line: Rc::new(Cell::new(0)),
            on_open: RefCell::new(None),
            on_pick: Rc::new(RefCell::new(None)),
            open_cb: Rc::new(RefCell::new(None)),
            folder_holder,
        });
        let w = Rc::downgrade(&p);
        p.split_btn.connect_toggled(move |b| {
            if let Some(p) = w.upgrade() {
                if p.split.get() != b.is_active() {
                    p.split.set(b.is_active());
                    p.reload();
                }
            }
        });
        let w = Rc::downgrade(&p);
        p.source_btn.connect_toggled(move |_| {
            if let Some(p) = w.upgrade() {
                p.reload();
            }
        });
        let w = Rc::downgrade(&p);
        p.open_btn.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                if let Shown::File(path) = p.shown.borrow().clone() {
                    if let Some(f) = p.on_open.borrow().as_ref() {
                        f(path);
                    }
                }
            }
        });
        let w = Rc::downgrade(&p);
        theme::on_change(move || {
            if let Some(p) = w.upgrade() {
                if let Some(s) = theme::source_scheme() {
                    p.code_buf.set_style_scheme(Some(&s));
                }
                p.reload();
            }
        });
        p.clear();
        p
    }

    fn next_gen(&self) -> u64 {
        let g = self.gen.get() + 1;
        self.gen.set(g);
        g
    }

    fn set_header(&self, name: &str, is_dir: bool, title: &str, meta: &str) {
        self.icon.set_markup(&icons::markup(name, is_dir, false));
        self.title.set_text(title);
        self.meta.set_text(meta);
    }

    pub fn clear(&self) {
        self.next_gen();
        *self.shown.borrow_mut() = Shown::Nothing;
        self.icon.set_text("");
        self.title.set_text("");
        self.meta.set_text("");
        self.split_btn.set_visible(false);
        self.source_btn.set_visible(false);
        self.open_btn.set_visible(false);
        self.stack.set_visible_child_name("empty");
    }

    pub fn reload(&self) {
        match self.shown.borrow().clone() {
            Shown::Nothing => {}
            Shown::File(p) => self.show_file_inner(&p, true),
            Shown::Diff(root, ch) => self.show_diff(&root, &ch),
        }
    }

    pub fn toggle_split(&self) {
        self.split_btn.set_active(!self.split_btn.is_active());
    }

    pub fn toggle_source(&self) {
        if self.source_btn.is_visible() {
            self.source_btn.set_active(!self.source_btn.is_active());
        }
    }

    pub fn shown_path(&self) -> Option<PathBuf> {
        match &*self.shown.borrow() {
            Shown::File(p) => Some(p.clone()),
            Shown::Diff(root, ch) => Some(root.join(&ch.rel)),
            Shown::Nothing => None,
        }
    }

    pub fn show_multi(&self, count: usize, total: u64) {
        self.clear();
        self.icon.set_text("\u{f0c5}");
        self.title.set_text(&format!("{count} items selected"));
        self.meta.set_text(&human_size(total));
    }

    pub fn show_file(&self, path: &Path) {
        if let Shown::File(p) = &*self.shown.borrow() {
            if p == path {
                return;
            }
        }
        self.source_btn.set_active(false);
        self.show_file_inner(path, false);
    }

    /// Show a file and highlight a line (1-based) once it is loaded.
    pub fn show_file_at(&self, path: &Path, line: u32) {
        self.pending_line.set(line);
        self.source_btn.set_active(true);
        self.show_file_inner(path, false);
    }

    fn show_file_inner(&self, path: &Path, keep_scroll: bool) {
        let _ = keep_scroll;
        let gen = self.next_gen();
        *self.shown.borrow_mut() = Shown::File(path.to_path_buf());
        self.split_btn.set_visible(false);
        self.source_btn.set_visible(false);
        self.open_btn.set_visible(true);
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
        let meta = std::fs::metadata(path).ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        self.set_header(&name, is_dir, &name, "");

        if is_dir {
            self.show_dir(path, gen);
            return;
        }
        let Some(meta) = meta else {
            self.show_info(path, "File not found");
            return;
        };
        let size = meta.len();
        let ctype = gio::content_type_guess(Some(path), None).0.to_string();
        let mime = gio::content_type_get_mime_type(&ctype).map(|s| s.to_string()).unwrap_or_default();
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();

        if mime.starts_with("image/") && !matches!(ext.as_str(), "psd" | "xcf") {
            self.meta.set_text(&human_size(size));
            self.load_image(path, gen, size);
        } else if mime == "application/pdf" {
            self.load_pdf(path, gen);
        } else if mime.starts_with("video/") {
            self.meta.set_text(&format!("{} · {}", human_size(size), mime));
            self.load_video_still(path, gen, size, &mime);
        } else if mime.starts_with("audio/") {
            self.show_info(path, "");
        } else {
            self.load_text(path, gen, size, &ext);
        }
    }

    fn load_text(&self, path: &Path, gen: u64, size: u64, ext: &str) {
        let p = path.to_path_buf();
        let is_md = matches!(ext, "md" | "markdown" | "mdx");
        let code_buf = self.code_buf.clone();
        let code_view = self.code_view.clone();
        let md_view = self.md_view.clone();
        let stack = self.stack.clone();
        let meta = self.meta.clone();
        let source_btn = self.source_btn.clone();
        let gen_ref = self.gen_ref();
        let pending_line = self.pending_line.clone();
        glib::spawn_future_local(async move {
            let p2 = p.clone();
            let data = gio::spawn_blocking(move || -> Option<Vec<u8>> {
                use std::io::Read;
                let mut f = std::fs::File::open(&p2).ok()?;
                let mut buf = Vec::with_capacity(size.min(MAX_TEXT as u64) as usize);
                f.by_ref().take(MAX_TEXT as u64).read_to_end(&mut buf).ok()?;
                Some(buf)
            })
            .await
            .ok()
            .flatten();
            if gen_ref.get() != gen {
                return;
            }
            let Some(data) = data else {
                meta.set_text("Can't read file");
                stack.set_visible_child_name("info");
                return;
            };
            let sniff = &data[..data.len().min(8192)];
            if sniff.contains(&0) {
                // Binary: show an info card.
                return show_info_static(&p, &stack, &meta, None);
            }
            let text = String::from_utf8_lossy(&data).to_string();
            let lines = text.lines().count();
            let trunc = if size as usize > MAX_TEXT { " · truncated" } else { "" };
            if is_md && !source_btn.is_active() {
                source_btn.set_visible(true);
                meta.set_text(&format!("{} · Markdown{trunc}", human_size(size)));
                markdown::render(&md_view.buffer(), &text);
                stack.set_visible_child_name("markdown");
                if let Some(adj) = md_view.parent().and_downcast::<gtk::ScrolledWindow>().map(|s| s.vadjustment()) {
                    adj.set_value(0.0);
                }
                return;
            }
            if is_md {
                source_btn.set_visible(true);
            }
            let lm = sourceview5::LanguageManager::default();
            let fname = p.file_name().map(|s| s.to_string_lossy().to_string());
            let ctype = gio::content_type_guess(fname.as_deref(), sniff).0;
            let lang = lm.guess_language(fname.as_deref(), Some(ctype.as_str()));
            code_buf.set_language(lang.as_ref());
            code_buf.set_highlight_syntax(lines < 20000);
            if let Some(s) = theme::source_scheme() {
                code_buf.set_style_scheme(Some(&s));
            }
            code_buf.set_text(&text);
            let start = code_buf.start_iter();
            code_buf.place_cursor(&start);
            let lang_name = lang.map(|l| l.name().to_string()).unwrap_or_else(|| "Plain text".into());
            meta.set_text(&format!("{lines} lines · {} · {lang_name}{trunc}", human_size(size)));
            stack.set_visible_child_name("code");
            if let Some(adj) = code_view.parent().and_downcast::<gtk::ScrolledWindow>().map(|s| s.vadjustment()) {
                adj.set_value(0.0);
            }
            let line = pending_line.replace(0);
            if line > 0 {
                glib::idle_add_local_once(move || {
                    if let Some(mut it) = code_buf.iter_at_line(line as i32 - 1) {
                        let mut end = it;
                        end.forward_to_line_end();
                        code_buf.select_range(&it, &end);
                        code_view.scroll_to_iter(&mut it, 0.0, true, 0.0, 0.3);
                    }
                });
            }
        });
    }

    fn gen_ref(&self) -> Rc<Cell<u64>> {
        self.gen.clone()
    }

    fn load_image(&self, path: &Path, gen: u64, size: u64) {
        let p = path.to_path_buf();
        let image = self.image.clone();
        let stack = self.stack.clone();
        let meta = self.meta.clone();
        let gen_ref = self.gen_ref();
        image.set_paintable(None::<&gdk::Paintable>);
        stack.set_visible_child_name("image");
        glib::spawn_future_local(async move {
            let p2 = p.clone();
            let tex = gio::spawn_blocking(move || gdk::Texture::from_filename(&p2).map_err(|e| e.to_string())).await;
            if gen_ref.get() != gen {
                return;
            }
            match tex {
                Ok(Ok(t)) => {
                    meta.set_text(&format!("{} × {} · {}", t.width(), t.height(), human_size(size)));
                    image.set_paintable(Some(&t));
                }
                _ => show_info_static(&p, &stack, &meta, Some("Can't decode this image")),
            }
        });
    }

    /// One frame of a video; playing it is the video player's job.
    fn load_video_still(&self, path: &Path, gen: u64, size: u64, mime: &str) {
        let p = path.to_path_buf();
        let image = self.image.clone();
        let stack = self.stack.clone();
        let meta = self.meta.clone();
        let gen_ref = self.gen_ref();
        let mime = mime.to_string();
        image.set_paintable(None::<&gdk::Paintable>);
        stack.set_visible_child_name("image");
        glib::spawn_future_local(async move {
            let p2 = p.clone();
            let tex = gio::spawn_blocking(move || {
                let still = frame::video_still(&p2)?;
                gdk::Texture::from_filename(&still).ok()
            })
            .await
            .ok()
            .flatten();
            if gen_ref.get() != gen {
                return;
            }
            match tex {
                Some(t) => {
                    meta.set_text(&format!("{} × {} · {} · {mime} · Enter to play", t.width(), t.height(), human_size(size)));
                    image.set_paintable(Some(&t));
                }
                None => show_info_static(&p, &stack, &meta, Some("No preview frame (is ffmpegthumbnailer installed?)")),
            }
        });
    }

    fn load_pdf(&self, path: &Path, gen: u64) {
        while let Some(c) = self.pdf_box.first_child() {
            self.pdf_box.remove(&c);
        }
        self.stack.set_visible_child_name("pdf");
        let uri = gio::File::for_path(path).uri();
        let doc = match poppler::Document::from_file(&uri, None) {
            Ok(d) => d,
            Err(e) => {
                show_info_static(path, &self.stack, &self.meta, Some(&format!("Can't open PDF: {e}")));
                return;
            }
        };
        let pages = doc.n_pages();
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        self.meta.set_text(&format!("{pages} pages · {} · PDF", human_size(size)));
        let scale = self.widget.scale_factor() as f64;
        let target_w = (self.widget.width().max(500) - 60).min(1100) as f64;
        let pdf_box = self.pdf_box.clone();
        let gen_ref = self.gen_ref();
        let next = Rc::new(Cell::new(0));
        glib::idle_add_local(move || {
            if gen_ref.get() != gen {
                return glib::ControlFlow::Break;
            }
            let i = next.get();
            if i >= pages.min(MAX_PDF_PAGES) {
                if pages > MAX_PDF_PAGES {
                    let l = gtk::Label::new(Some(&format!("… {} more pages", pages - MAX_PDF_PAGES)));
                    l.add_css_class("dim");
                    l.set_margin_bottom(20);
                    pdf_box.append(&l);
                }
                return glib::ControlFlow::Break;
            }
            next.set(i + 1);
            if let Some(page) = doc.page(i) {
                let (pw, ph) = page.size();
                let s = target_w / pw;
                let w = (pw * s * scale) as i32;
                let h = (ph * s * scale) as i32;
                if let Ok(mut surf) = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, w, h) {
                    {
                        let cr = gtk::cairo::Context::new(&surf).unwrap();
                        cr.set_source_rgb(1.0, 1.0, 1.0);
                        let _ = cr.paint();
                        cr.scale(s * scale, s * scale);
                        page.render(&cr);
                    }
                    surf.flush();
                    let stride = surf.stride() as usize;
                    if let Ok(data) = surf.data() {
                        let bytes = glib::Bytes::from(&data[..]);
                        let tex = gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::B8g8r8a8Premultiplied, &bytes, stride);
                        let pic = gtk::Picture::for_paintable(&tex);
                        pic.set_can_shrink(true);
                        pic.set_size_request((w as f64 / scale) as i32, (h as f64 / scale) as i32);
                        pic.add_css_class("pdf-page");
                        pdf_box.append(&pic);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    fn show_dir(&self, path: &Path, gen: u64) {
        let p = path.to_path_buf();
        let gen_ref = self.gen_ref();
        let meta = self.meta.clone();
        let stack = self.stack.clone();
        let md_view = self.md_view.clone();
        let holder = self.folder_holder.clone();
        let pick_cb = self.on_pick.clone();
        let open_cb = self.open_cb.clone();
        meta.set_text("");
        glib::spawn_future_local(async move {
            let p2 = p.clone();
            let Ok(scan) = gio::spawn_blocking(move || folderview::scan(&p2)).await else { return };
            if gen_ref.get() != gen {
                return;
            }
            let summary = format!(
                "{} folder{} · {} file{} · {}",
                scan.dirs,
                if scan.dirs == 1 { "" } else { "s" },
                scan.files,
                if scan.files == 1 { "" } else { "s" },
                human_size(scan.bytes)
            );
            meta.set_text(&summary);
            if let Some(text) = scan.readme.as_ref().and_then(|r| std::fs::read_to_string(r).ok()) {
                let name = scan.readme.as_ref().and_then(|r| r.file_name()).map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                meta.set_text(&format!("{summary} · {name}"));
                markdown::render(&md_view.buffer(), &text);
                stack.set_visible_child_name("markdown");
                return;
            }
            let actions = folderview::Actions {
                pick: Rc::new(move |p| {
                    if let Some(f) = pick_cb.borrow().as_ref() {
                        f(p);
                    }
                }),
                open: Rc::new(move |p| {
                    if let Some(f) = open_cb.borrow().as_ref() {
                        f(p);
                    }
                }),
            };
            match folderview::build(&p, scan.view, actions, gen_ref.clone(), gen) {
                Some(w) => {
                    while let Some(c) = holder.first_child() {
                        holder.remove(&c);
                    }
                    holder.append(&w);
                    stack.set_visible_child_name("folder");
                }
                None => show_info_static(&p, &stack, &meta, None),
            }
            if gen_ref.get() == gen {
                // show_info_static overwrites the meta line; keep the counts.
                meta.set_text(&summary);
            }
        });
    }

    fn show_info(&self, path: &Path, note: &str) {
        show_info_static(path, &self.stack, &self.meta, if note.is_empty() { None } else { Some(note) });
        let _ = &self.info_holder;
    }

    /// A commit from the source control panel: message, stats and patch.
    pub fn show_commit(&self, root: &Path, c: &git::Commit) {
        let gen = self.next_gen();
        *self.shown.borrow_mut() = Shown::Nothing;
        self.icon.set_text("\u{f417}");
        self.title.set_text(&c.summary);
        let when = glib::DateTime::from_unix_local(c.time).and_then(|d| d.format("%-d %b %Y %H:%M")).map(|s| s.to_string()).unwrap_or_default();
        self.meta.set_text(&format!("{} · {} · {when}", &c.id[..7.min(c.id.len())], c.author));
        self.split_btn.set_visible(false);
        self.source_btn.set_visible(false);
        self.open_btn.set_visible(false);
        let (root, id) = (root.to_path_buf(), c.id.clone());
        let code_buf = self.code_buf.clone();
        let code_view = self.code_view.clone();
        let stack = self.stack.clone();
        let gen_ref = self.gen_ref();
        glib::spawn_future_local(async move {
            let text = gio::spawn_blocking(move || git::show(&root, &id)).await.ok().map(|r| r.unwrap_or_else(|e| e));
            if gen_ref.get() != gen {
                return;
            }
            let mut text = text.unwrap_or_default();
            if text.len() > MAX_TEXT {
                let mut cut = MAX_TEXT;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
            }
            let lm = sourceview5::LanguageManager::default();
            code_buf.set_language(lm.language("diff").as_ref());
            code_buf.set_highlight_syntax(true);
            if let Some(s) = theme::source_scheme() {
                code_buf.set_style_scheme(Some(&s));
            }
            code_buf.set_text(&text);
            let start = code_buf.start_iter();
            code_buf.place_cursor(&start);
            stack.set_visible_child_name("code");
            if let Some(adj) = code_view.parent().and_downcast::<gtk::ScrolledWindow>().map(|s| s.vadjustment()) {
                adj.set_value(0.0);
            }
        });
    }

    pub fn show_diff(&self, root: &Path, ch: &git::Change) {
        let gen = self.next_gen();
        *self.shown.borrow_mut() = Shown::Diff(root.to_path_buf(), ch.clone());
        let name = Path::new(&ch.rel).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let side = if ch.staged { "Staged" } else { "Working tree" };
        self.set_header(&name, false, &ch.rel, &format!("{} · {side}", ch.kind.describe()));
        self.split_btn.set_visible(true);
        self.split_btn.set_active(self.split.get());
        self.source_btn.set_visible(false);
        self.open_btn.set_visible(false);
        let root = root.to_path_buf();
        let ch = ch.clone();
        let holder = self.diff_holder.clone();
        let stack = self.stack.clone();
        let meta = self.meta.clone();
        let split = self.split.get();
        let gen_ref = self.gen_ref();
        glib::spawn_future_local(async move {
            let (r2, c2) = (root.clone(), ch.clone());
            let res = gio::spawn_blocking(move || git::change_contents(&r2, &c2)).await;
            if gen_ref.get() != gen {
                return;
            }
            let Ok((old, new)) = res else { return };
            while let Some(c) = holder.first_child() {
                holder.remove(&c);
            }
            let binary = old[..old.len().min(8192)].contains(&0) || new[..new.len().min(8192)].contains(&0);
            if binary {
                let l = gtk::Label::new(Some("Binary file changed"));
                l.add_css_class("preview-empty");
                l.set_vexpand(true);
                holder.append(&l);
            } else {
                let old = String::from_utf8_lossy(&old);
                let new = String::from_utf8_lossy(&new);
                let lm = sourceview5::LanguageManager::default();
                let lang = lm.guess_language(Some(&ch.rel), None);
                let (w, stats) = diff::build(&old, &new, lang.as_ref(), split);
                let side = if ch.staged { "Staged" } else { "Working tree" };
                meta.set_markup(&format!(
                    "{} · {side} · <span foreground=\"{}\">+{}</span> <span foreground=\"{}\">−{}</span>",
                    ch.kind.describe(),
                    theme::color("green"),
                    stats.added,
                    theme::color("red"),
                    stats.removed
                ));
                holder.append(&w);
            }
            stack.set_visible_child_name("diff");
        });
    }
}


fn show_info_static(path: &Path, stack: &gtk::Stack, meta: &gtk::Label, note: Option<&str>) {
    let holder = stack.child_by_name("info").and_then(|s| s.first_child()).and_then(|vp| vp.first_child()).and_downcast::<gtk::Box>();
    let Some(holder) = holder else { return };
    while let Some(c) = holder.first_child() {
        holder.remove(&c);
    }
    let card = gtk::Grid::builder().row_spacing(8).column_spacing(18).halign(gtk::Align::Center).valign(gtk::Align::Center).build();
    card.add_css_class("info-card");
    card.set_margin_top(48);
    let file = gio::File::for_path(path);
    let info = file.query_info("standard::*,time::modified,unix::mode,owner::user", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE).ok();
    let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let is_dir = info.as_ref().map(|i| i.file_type() == gio::FileType::Directory).unwrap_or(false);
    let big = gtk::Label::new(None);
    big.set_markup(&format!("<span size=\"36pt\">{}</span>", icons::markup(&name, is_dir, false)));
    big.add_css_class("file-icon");
    card.attach(&big, 0, 0, 2, 1);
    let title = gtk::Label::new(Some(&name));
    title.add_css_class("title-3");
    title.set_wrap(true);
    card.attach(&title, 0, 1, 2, 1);
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Some(n) = note {
        rows.push(("Note".into(), n.into()));
    }
    if let Some(i) = &info {
        let ct = i.content_type().map(|c| gio::content_type_get_description(&c).to_string()).unwrap_or_default();
        rows.push(("Type".into(), ct));
        if !is_dir {
            rows.push(("Size".into(), human_size(i.size() as u64)));
        }
        if let Some(dt) = i.modification_date_time() {
            if let Ok(local) = dt.to_local() {
                rows.push(("Modified".into(), local.format("%e %b %Y, %H:%M").map(|s| s.to_string()).unwrap_or_default()));
            }
        }
        let mode = i.attribute_uint32("unix::mode");
        if mode != 0 {
            rows.push(("Permissions".into(), format!("{:o}", mode & 0o7777)));
        }
        if let Some(o) = i.attribute_string("owner::user") {
            rows.push(("Owner".into(), o.to_string()));
        }
    }
    rows.push(("Location".into(), path.parent().map(shorten_home).unwrap_or_default()));
    for (i, (k, v)) in rows.iter().enumerate() {
        let kl = gtk::Label::builder().label(k).xalign(1.0).build();
        kl.add_css_class("k");
        let vl = gtk::Label::builder().label(v).xalign(0.0).selectable(true).wrap(true).max_width_chars(48).build();
        vl.add_css_class("v");
        card.attach(&kl, 0, i as i32 + 2, 1, 1);
        card.attach(&vl, 1, i as i32 + 2, 1, 1);
    }
    holder.append(&card);
    if meta.text().is_empty() {
        if let Some(i) = &info {
            meta.set_text(&human_size(i.size() as u64));
        }
    }
    stack.set_visible_child_name("info");
}
