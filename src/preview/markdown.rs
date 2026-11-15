//! Lightweight markdown rendering into a GtkTextBuffer (no web engine).

use gtk::{pango, prelude::*};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

pub fn ensure_tags(buf: &gtk::TextBuffer) {
    let table = buf.tag_table();
    if table.lookup("h1").is_some() {
        return;
    }
    let p = crate::theme::palette();
    let add = |t: gtk::TextTag| {
        table.add(&t);
    };
    let bright = p.get("bright_foreground");
    add(gtk::TextTag::builder().name("h1").scale(1.75).weight(800).foreground(&bright).pixels_above_lines(14).pixels_below_lines(8).build());
    add(gtk::TextTag::builder().name("h2").scale(1.4).weight(700).foreground(&bright).pixels_above_lines(14).pixels_below_lines(6).build());
    add(gtk::TextTag::builder().name("h3").scale(1.2).weight(700).foreground(&bright).pixels_above_lines(10).pixels_below_lines(4).build());
    add(gtk::TextTag::builder().name("h4").scale(1.05).weight(700).foreground(&bright).pixels_above_lines(8).pixels_below_lines(2).build());
    add(gtk::TextTag::builder().name("bold").weight(700).foreground(&bright).build());
    add(gtk::TextTag::builder().name("italic").style(pango::Style::Italic).build());
    add(gtk::TextTag::builder().name("strike").strikethrough(true).build());
    add(gtk::TextTag::builder()
        .name("code")
        .family("JetBrainsMono Nerd Font")
        .foreground(&p.get("green"))
        .background(&p.get("lighter_background"))
        .scale(0.92)
        .build());
    add(gtk::TextTag::builder()
        .name("codeblock")
        .family("JetBrainsMono Nerd Font")
        .scale(0.92)
        .foreground(&p.get("foreground"))
        .paragraph_background(&p.get("dark_background"))
        .left_margin(28)
        .right_margin(16)
        .build());
    add(gtk::TextTag::builder().name("link").foreground(&p.get("blue")).underline(pango::Underline::Single).build());
    add(gtk::TextTag::builder()
        .name("quote")
        .foreground(&p.get("dark_foreground"))
        .style(pango::Style::Italic)
        .left_margin(36)
        .build());
    add(gtk::TextTag::builder().name("dim").foreground(&p.get("dark_foreground")).build());
    add(gtk::TextTag::builder().name("bullet").foreground(&p.get("accent")).weight(700).build());
    add(gtk::TextTag::builder().name("hr").foreground(&p.get("muted")).justification(gtk::Justification::Center).build());
    add(gtk::TextTag::builder().name("para").pixels_below_lines(8).build());
    add(gtk::TextTag::builder().name("table").family("JetBrainsMono Nerd Font").scale(0.92).build());
}

pub fn render(buf: &gtk::TextBuffer, text: &str) {
    // Tags capture colors, so rebuild them for the current theme.
    let table = buf.tag_table();
    let names = ["h1", "h2", "h3", "h4", "bold", "italic", "strike", "code", "codeblock", "link", "quote", "dim", "bullet", "hr", "para", "table"];
    for n in names {
        if let Some(t) = table.lookup(n) {
            table.remove(&t);
        }
    }
    for i in 0..10 {
        if let Some(t) = table.lookup(&format!("indent{i}")) {
            table.remove(&t);
        }
    }
    ensure_tags(buf);
    buf.set_text("");

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(text, opts);

    let mut active: Vec<String> = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut in_code_block = false;
    let mut quote_depth = 0;
    let mut table_row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut in_table = false;

    let insert = |buf: &gtk::TextBuffer, s: &str, tags: &[String]| {
        let mut end = buf.end_iter();
        let refs: Vec<gtk::TextTag> = tags.iter().filter_map(|n| buf.tag_table().lookup(n)).collect();
        let refs: Vec<&gtk::TextTag> = refs.iter().collect();
        buf.insert_with_tags(&mut end, s, &refs);
    };
    let indent_tag = |buf: &gtk::TextBuffer, depth: usize| -> String {
        let name = format!("indent{depth}");
        if buf.tag_table().lookup(&name).is_none() {
            buf.tag_table().add(&gtk::TextTag::builder().name(&name).left_margin(14 + 22 * depth as i32).build());
        }
        name
    };
    let newline_if_needed = |buf: &gtk::TextBuffer| {
        let end = buf.end_iter();
        if end.offset() > 0 {
            let mut prev = end;
            prev.backward_char();
            if prev.char() != '\n' {
                let mut e = buf.end_iter();
                buf.insert(&mut e, "\n");
            }
        }
    };

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    newline_if_needed(buf);
                    active.push(match level {
                        HeadingLevel::H1 => "h1",
                        HeadingLevel::H2 => "h2",
                        HeadingLevel::H3 => "h3",
                        _ => "h4",
                    }
                    .into());
                }
                Tag::Paragraph => {
                    if !in_table && lists.is_empty() {
                        newline_if_needed(buf);
                    }
                    active.push("para".into());
                }
                Tag::Emphasis => active.push("italic".into()),
                Tag::Strong => active.push("bold".into()),
                Tag::Strikethrough => active.push("strike".into()),
                Tag::Link { .. } => active.push("link".into()),
                Tag::BlockQuote(_) => {
                    quote_depth += 1;
                    active.push("quote".into());
                }
                Tag::CodeBlock(_) => {
                    newline_if_needed(buf);
                    in_code_block = true;
                    active.push("codeblock".into());
                    insert(buf, "\n", &active);
                }
                Tag::List(start) => {
                    newline_if_needed(buf);
                    lists.push(start);
                }
                Tag::Item => {
                    newline_if_needed(buf);
                    let depth = lists.len().saturating_sub(1);
                    let ind = indent_tag(buf, depth);
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => if depth % 2 == 0 { "•  ".to_string() } else { "◦  ".to_string() },
                    };
                    insert(buf, &marker, &[ind.clone(), "bullet".into()]);
                    active.push(ind);
                }
                Tag::Table(_) => {
                    newline_if_needed(buf);
                    in_table = true;
                }
                Tag::TableHead | Tag::TableRow => table_row.clear(),
                Tag::TableCell => cell.clear(),
                Tag::Image { .. } => {
                    insert(buf, "\u{f03e} ", &["dim".to_string()]);
                    active.push("dim".into());
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Heading(_) => {
                    active.pop();
                    insert(buf, "\n", &[]);
                }
                TagEnd::Paragraph => {
                    active.pop();
                    if !in_table {
                        insert(buf, "\n", &active);
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link | TagEnd::Image => {
                    active.pop();
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth -= 1;
                    active.pop();
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    insert(buf, "\n", &active);
                    active.pop();
                    insert(buf, "\n", &[]);
                }
                TagEnd::List(_) => {
                    lists.pop();
                    if lists.is_empty() {
                        insert(buf, "\n", &[]);
                    }
                }
                TagEnd::Item => {
                    active.pop();
                }
                TagEnd::Table => {
                    in_table = false;
                    insert(buf, "\n", &[]);
                }
                TagEnd::TableCell => table_row.push(std::mem::take(&mut cell)),
                TagEnd::TableHead | TagEnd::TableRow => {
                    let is_head = matches!(end, TagEnd::TableHead);
                    let line = format!("│ {} │\n", table_row.join(" │ "));
                    let mut tags = vec!["table".to_string()];
                    if is_head {
                        tags.push("bold".into());
                    }
                    insert(buf, &line, &tags);
                    if is_head {
                        let sep: Vec<String> = table_row.iter().map(|c| "─".repeat(c.chars().count())).collect();
                        insert(buf, &format!("├─{}─┤\n", sep.join("─┼─")), &["table".into(), "dim".into()]);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_table {
                    cell.push_str(&t);
                } else if in_code_block {
                    insert(buf, &t, &active);
                } else {
                    insert(buf, &t, &active);
                }
            }
            Event::Code(t) => {
                if in_table {
                    cell.push_str(&t);
                } else {
                    let mut tags = active.clone();
                    tags.push("code".into());
                    insert(buf, &format!(" {t} "), &tags);
                }
            }
            Event::SoftBreak => insert(buf, " ", &active),
            Event::HardBreak => insert(buf, "\n", &active),
            Event::Rule => {
                newline_if_needed(buf);
                insert(buf, &"─".repeat(48), &["hr".into()]);
                insert(buf, "\n", &[]);
            }
            Event::TaskListMarker(done) => {
                insert(buf, if done { "\u{f0132} " } else { "\u{f0131} " }, &["bullet".into()]);
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                let s = h.trim();
                if !s.starts_with("<!--") && !s.is_empty() && !s.starts_with('<') {
                    insert(buf, s, &["dim".into()]);
                }
            }
            _ => {}
        }
    }
    let _ = quote_depth;
}
