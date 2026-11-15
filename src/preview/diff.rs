//! Git diff rendering: inline (unified) or side by side, with syntax
//! highlighting, line numbers and word-level change emphasis.

use gtk::prelude::*;
use similar::{ChangeTag, TextDiff};
use sourceview5::prelude::*;

#[derive(Clone, Copy, PartialEq)]
enum K {
    Eq,
    Add,
    Del,
    Sep,
    Pad,
}

struct Line {
    old: Option<usize>,
    new: Option<usize>,
    kind: K,
    /// (emphasized, text) segments.
    parts: Vec<(bool, String)>,
}

impl Line {
    fn text(&self) -> String {
        let s: String = self.parts.iter().map(|(_, t)| t.as_str()).collect();
        s.trim_end_matches(['\n', '\r']).to_string()
    }
}

pub struct Stats {
    pub added: usize,
    pub removed: usize,
}

fn collect(old: &str, new: &str) -> (Vec<Vec<Line>>, Stats) {
    let diff = TextDiff::configure().algorithm(similar::Algorithm::Patience).diff_lines(old, new);
    let mut groups = Vec::new();
    let mut stats = Stats { added: 0, removed: 0 };
    for group in diff.grouped_ops(4) {
        let mut lines = Vec::new();
        for op in &group {
            for ch in diff.iter_inline_changes(op) {
                let kind = match ch.tag() {
                    ChangeTag::Equal => K::Eq,
                    ChangeTag::Insert => {
                        stats.added += 1;
                        K::Add
                    }
                    ChangeTag::Delete => {
                        stats.removed += 1;
                        K::Del
                    }
                };
                let parts = ch.iter_strings_lossy().map(|(e, s)| (e, s.to_string())).collect();
                lines.push(Line { old: ch.old_index().map(|i| i + 1), new: ch.new_index().map(|i| i + 1), kind, parts });
            }
        }
        groups.push(lines);
    }
    (groups, stats)
}

fn tags(buf: &gtk::TextBuffer) {
    let table = buf.tag_table();
    for n in ["add", "del", "sep", "pad", "add-em", "del-em", "num", "num-add", "num-del"] {
        if let Some(t) = table.lookup(n) {
            table.remove(&t);
        }
    }
    let (add, del, add_em, del_em) = crate::theme::diff_colors();
    let p = crate::theme::palette();
    table.add(&gtk::TextTag::builder().name("add").paragraph_background(&add).build());
    table.add(&gtk::TextTag::builder().name("del").paragraph_background(&del).build());
    table.add(&gtk::TextTag::builder().name("add-em").background(&add_em).build());
    table.add(&gtk::TextTag::builder().name("del-em").background(&del_em).build());
    table.add(&gtk::TextTag::builder()
        .name("sep")
        .paragraph_background(&p.get("dark_background"))
        .foreground(&p.get("dark_foreground"))
        .build());
    table.add(&gtk::TextTag::builder().name("pad").paragraph_background(&p.get("darker_background")).build());
    table.add(&gtk::TextTag::builder().name("num").foreground(&p.get("muted")).build());
    table.add(&gtk::TextTag::builder().name("num-add").foreground(&p.get("green")).paragraph_background(&add).build());
    table.add(&gtk::TextTag::builder().name("num-del").foreground(&p.get("red")).paragraph_background(&del).build());
}

fn code_view(lang: Option<&sourceview5::Language>) -> (sourceview5::View, sourceview5::Buffer) {
    let buf = sourceview5::Buffer::new(None);
    buf.set_language(lang);
    buf.set_highlight_syntax(true);
    if let Some(s) = crate::theme::source_scheme() {
        buf.set_style_scheme(Some(&s));
    }
    let view = sourceview5::View::with_buffer(&buf);
    view.set_editable(false);
    view.set_monospace(true);
    view.set_hexpand(true);
    view.set_top_margin(6);
    view.set_bottom_margin(24);
    view.set_left_margin(10);
    view.add_css_class("code");
    tags(buf.upcast_ref());
    (view, buf)
}

fn gutter_view() -> (gtk::TextView, gtk::TextBuffer) {
    let buf = gtk::TextBuffer::new(None);
    tags(&buf);
    let view = gtk::TextView::with_buffer(&buf);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_can_focus(false);
    view.set_top_margin(6);
    view.set_bottom_margin(24);
    view.set_left_margin(6);
    view.set_right_margin(6);
    view.add_css_class("code");
    view.add_css_class("diff-gutter");
    (view, buf)
}

fn append(buf: &gtk::TextBuffer, text: &str, tag: Option<&str>) {
    let mut end = buf.end_iter();
    match tag.and_then(|t| buf.tag_table().lookup(t)) {
        Some(t) => buf.insert_with_tags(&mut end, text, &[&t]),
        None => buf.insert(&mut end, text),
    }
}

fn line_tag(k: K) -> Option<&'static str> {
    match k {
        K::Add => Some("add"),
        K::Del => Some("del"),
        K::Sep => Some("sep"),
        K::Pad => Some("pad"),
        K::Eq => None,
    }
}

fn write_line(buf: &gtk::TextBuffer, l: &Line, show_em: bool) {
    let start = buf.end_iter().offset();
    if l.kind == K::Sep {
        append(buf, &l.parts[0].1, None);
    } else {
        let full: String = l.text();
        let em_tag = match l.kind {
            K::Add => Some("add-em"),
            K::Del => Some("del-em"),
            _ => None,
        };
        // Only emphasize when part of the line changed (not whole-line edits).
        let partial = show_em && l.parts.iter().any(|(e, _)| *e) && l.parts.iter().any(|(e, s)| !*e && !s.trim().is_empty());
        if partial && em_tag.is_some() {
            let mut used = 0;
            for (e, s) in &l.parts {
                let s = s.trim_end_matches(['\n', '\r']);
                if used >= full.len() {
                    break;
                }
                append(buf, s, if *e { em_tag } else { None });
                used += s.len();
            }
        } else {
            append(buf, &full, None);
        }
    }
    append(buf, "\n", None);
    if let Some(t) = line_tag(l.kind).and_then(|t| buf.tag_table().lookup(t)) {
        let s = buf.iter_at_offset(start);
        let e = buf.end_iter();
        buf.apply_tag(&t, &s, &e);
    }
}

fn num(n: Option<usize>, w: usize) -> String {
    n.map(|n| format!("{n:>w$}")).unwrap_or_else(|| " ".repeat(w))
}

fn sep_line(prev_end: usize, next_start: usize) -> Line {
    let hidden = next_start.saturating_sub(prev_end + 1);
    let text = if hidden > 0 { format!("  ⋯  {hidden} unchanged lines") } else { "  ⋯".into() };
    Line { old: None, new: None, kind: K::Sep, parts: vec![(false, text)] }
}

fn hscroll(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(child)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .hexpand(true)
        .valign(gtk::Align::Start)
        .build()
}

pub fn build(old: &str, new: &str, lang: Option<&sourceview5::Language>, split: bool) -> (gtk::Widget, Stats) {
    let (groups, stats) = collect(old, new);
    if groups.is_empty() {
        let l = gtk::Label::new(Some("No textual changes"));
        l.add_css_class("preview-empty");
        l.set_vexpand(true);
        return (l.upcast(), stats);
    }
    let max = old.lines().count().max(new.lines().count()).max(1);
    let w = max.to_string().len();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.set_valign(gtk::Align::Start);

    if !split {
        let (gv, gb) = gutter_view();
        let (cv, cb) = code_view(lang);
        let mut last_old = 0;
        for (gi, g) in groups.iter().enumerate() {
            let first_old = g.first().and_then(|l| l.old.or(l.new)).unwrap_or(1);
            if gi > 0 || first_old > 1 {
                let s = sep_line(last_old, first_old);
                write_line(cb.upcast_ref(), &s, false);
                append(&gb, &format!("{} {}\n", " ".repeat(w), " ".repeat(w)), Some("sep"));
            }
            for l in g {
                write_line(cb.upcast_ref(), l, true);
                let tag = match l.kind {
                    K::Add => "num-add",
                    K::Del => "num-del",
                    _ => "num",
                };
                append(&gb, &format!("{} {}\n", num(l.old, w), num(l.new, w)), Some(tag));
                if let Some(o) = l.old {
                    last_old = o;
                }
            }
        }
        content.append(&gv);
        content.append(&hscroll(&cv));
    } else {
        let (lg, lgb) = gutter_view();
        let (lv, lb) = code_view(lang);
        let (rg, rgb) = gutter_view();
        let (rv, rb) = code_view(lang);
        let mut last_old = 0;
        for (gi, g) in groups.iter().enumerate() {
            let first_old = g.first().and_then(|l| l.old.or(l.new)).unwrap_or(1);
            if gi > 0 || first_old > 1 {
                let s = sep_line(last_old, first_old);
                write_line(lb.upcast_ref(), &s, false);
                write_line(rb.upcast_ref(), &Line { old: None, new: None, kind: K::Sep, parts: vec![(false, String::new())] }, false);
                append(&lgb, &format!("{}\n", " ".repeat(w)), Some("sep"));
                append(&rgb, &format!("{}\n", " ".repeat(w)), Some("sep"));
            }
            let mut i = 0;
            while i < g.len() {
                if g[i].kind == K::Eq {
                    write_line(lb.upcast_ref(), &g[i], false);
                    write_line(rb.upcast_ref(), &g[i], false);
                    append(&lgb, &format!("{}\n", num(g[i].old, w)), Some("num"));
                    append(&rgb, &format!("{}\n", num(g[i].new, w)), Some("num"));
                    if let Some(o) = g[i].old {
                        last_old = o;
                    }
                    i += 1;
                    continue;
                }
                let mut dels = Vec::new();
                let mut adds = Vec::new();
                while i < g.len() && g[i].kind != K::Eq {
                    if g[i].kind == K::Del {
                        dels.push(&g[i]);
                    } else {
                        adds.push(&g[i]);
                    }
                    i += 1;
                }
                let n = dels.len().max(adds.len());
                for j in 0..n {
                    match dels.get(j) {
                        Some(l) => {
                            write_line(lb.upcast_ref(), l, true);
                            append(&lgb, &format!("{}\n", num(l.old, w)), Some("num-del"));
                            if let Some(o) = l.old {
                                last_old = o;
                            }
                        }
                        None => {
                            write_line(lb.upcast_ref(), &Line { old: None, new: None, kind: K::Pad, parts: vec![] }, false);
                            append(&lgb, &format!("{}\n", " ".repeat(w)), Some("pad"));
                        }
                    }
                    match adds.get(j) {
                        Some(l) => {
                            write_line(rb.upcast_ref(), l, true);
                            append(&rgb, &format!("{}\n", num(l.new, w)), Some("num-add"));
                        }
                        None => {
                            write_line(rb.upcast_ref(), &Line { old: None, new: None, kind: K::Pad, parts: vec![] }, false);
                            append(&rgb, &format!("{}\n", " ".repeat(w)), Some("pad"));
                        }
                    }
                }
            }
        }
        let left = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        left.append(&lg);
        left.append(&hscroll(&lv));
        let right = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        right.append(&rg);
        right.append(&hscroll(&rv));
        content.set_homogeneous(true);
        let sep = gtk::Separator::new(gtk::Orientation::Vertical);
        let l = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        l.append(&left);
        l.append(&sep);
        left.set_hexpand(true);
        content.append(&l);
        content.append(&right);
    }

    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    (scroller.upcast(), stats)
}
