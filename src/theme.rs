//! Omarchy theme integration: reads the active theme's colors.toml, turns it into
//! GTK CSS plus a GtkSourceView style scheme, and reloads both live when the
//! user switches theme.

use gtk::{gdk, gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Palette {
    pub dark: bool,
    c: HashMap<String, String>,
}

impl Palette {
    pub fn get(&self, key: &str) -> String {
        self.c
            .get(key)
            .cloned()
            .or_else(|| DEFAULTS.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()))
            .unwrap_or_else(|| "#ff00ff".into())
    }

    fn load() -> Palette {
        let mut c = HashMap::new();
        let mut dark = true;
        if let Ok(text) = std::fs::read_to_string(theme_dir().join("colors.toml")) {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || !line.contains('=') {
                    continue;
                }
                let (k, v) = line.split_once('=').unwrap();
                let v = v.trim().trim_matches('"').to_string();
                let k = k.trim().to_string();
                if k == "mode" {
                    dark = v != "light";
                } else if v.starts_with('#') {
                    c.insert(k, v);
                }
            }
        }
        Palette { dark, c }
    }
}

/// Tokyo Night, used for any key a theme does not define.
const DEFAULTS: &[(&str, &str)] = &[
    ("accent", "#7aa2f7"),
    ("selection", "#292e42"),
    ("muted", "#414868"),
    ("background", "#1a1b26"),
    ("dark_background", "#16161e"),
    ("darker_background", "#0e0e14"),
    ("lighter_background", "#24283b"),
    ("foreground", "#a9b1d6"),
    ("dark_foreground", "#565f89"),
    ("light_foreground", "#b4bee6"),
    ("bright_foreground", "#c0caf5"),
    ("red", "#f7768e"),
    ("yellow", "#e0af68"),
    ("orange", "#ff9e64"),
    ("green", "#9ece6a"),
    ("cyan", "#7dcfff"),
    ("blue", "#7aa2f7"),
    ("magenta", "#bb9af7"),
];

pub fn theme_dir() -> PathBuf {
    glib::home_dir().join(".local/state/omarchy/current/theme")
}

thread_local! {
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static PALETTE: RefCell<Option<Palette>> = const { RefCell::new(None) };
    static LISTENERS: RefCell<Vec<Box<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
    static MONITORS: RefCell<Vec<gio::FileMonitor>> = const { RefCell::new(Vec::new()) };
    static ZOOM_LISTENERS: RefCell<Vec<Box<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
    static ZOOM: Cell<f64> = const { Cell::new(1.0) };
    /// gtk-xft-dpi as the system set it, before any zoom.
    static BASE_DPI: Cell<i32> = const { Cell::new(0) };
}

pub const ZOOM_MIN: f64 = 0.6;
pub const ZOOM_MAX: f64 = 2.5;

fn clamp_zoom(z: f64) -> f64 {
    if !z.is_finite() {
        return 1.0;
    }
    ((z * 10.0).round() / 10.0).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// Current UI zoom factor (1.0 = system size). Shared by every window.
pub fn zoom() -> f64 {
    ZOOM.with(|z| z.get())
}

/// Scale the whole UI: text through the font DPI, and the pixel sizes in our
/// own stylesheet (icons, row heights, paddings) to match. Returns the zoom
/// actually applied.
pub fn set_zoom(z: f64) -> f64 {
    let z = clamp_zoom(z);
    if (z - zoom()).abs() > 1e-6 {
        ZOOM.with(|c| c.set(z));
        apply_dpi();
        let css = build_css(&palette());
        PROVIDER.with(|p| {
            if let Some(prov) = p.borrow().as_ref() {
                prov.load_from_string(&css);
            }
        });
        ZOOM_LISTENERS.with(|l| l.borrow().iter().for_each(|f| f()));
    }
    z
}

/// Register a callback that runs after the zoom changed (for pixel sizes set
/// from code; the stylesheet and text follow by themselves).
pub fn on_zoom(f: impl Fn() + 'static) {
    ZOOM_LISTENERS.with(|l| l.borrow_mut().push(Box::new(f)));
}

/// A pixel size from code, scaled like the stylesheet's.
pub fn px(v: i32) -> i32 {
    (v as f64 * zoom()).round() as i32
}

fn apply_dpi() {
    let Some(settings) = gtk::Settings::default() else { return };
    let base = BASE_DPI.with(|b| {
        if b.get() == 0 {
            let v = settings.gtk_xft_dpi();
            b.set(if v > 0 { v } else { 96 * 1024 });
        }
        b.get()
    });
    settings.set_gtk_xft_dpi((base as f64 * zoom()).round() as i32);
}

/// Multiply every `<n>px` length in the stylesheet by `z`.
fn scale_px(css: &str, z: f64) -> String {
    if (z - 1.0).abs() < 1e-6 {
        return css.to_string();
    }
    let b = css.as_bytes();
    let mut out = String::with_capacity(css.len() + 256);
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if b[i] == b'#' {
            // Hex colour: not a length, whatever digits it holds.
            i += 1;
            while i < b.len() && b[i].is_ascii_alphanumeric() {
                i += 1;
            }
        } else if b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.' || b[i - 1] == b'_')) {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            let is_px = css[i..].starts_with("px") && !b.get(i + 2).map(|c| c.is_ascii_alphanumeric()).unwrap_or(false);
            if let (true, Ok(v)) = (is_px, css[start..i].parse::<f64>()) {
                // Whole pixels, and a hairline never scales away.
                let scaled = if v == 0.0 { 0.0 } else { (v * z).round().max(1.0) };
                out.push_str(&css[copied..start]);
                out.push_str(&format!("{scaled}"));
                copied = i;
            }
        } else {
            i += 1;
        }
    }
    out.push_str(&css[copied..]);
    out
}

pub fn palette() -> Palette {
    PALETTE.with(|p| p.borrow().clone()).unwrap_or_else(Palette::load)
}

/// Register a callback that runs after every theme reload.
pub fn on_change(f: impl Fn() + 'static) {
    LISTENERS.with(|l| l.borrow_mut().push(Box::new(f)));
}

pub fn init(zoom: f64) {
    let zoom = clamp_zoom(zoom);
    if zoom != 1.0 {
        ZOOM.with(|z| z.set(zoom));
        apply_dpi();
    }
    apply();
    // Omarchy swaps the theme by rewriting theme.name and the theme dir.
    let state = glib::home_dir().join(".local/state/omarchy/current");
    let watch = |path: PathBuf| {
        let file = gio::File::for_path(path);
        if let Ok(mon) = file.monitor(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) {
            mon.set_rate_limit(300);
            let pending = std::rc::Rc::new(std::cell::Cell::new(false));
            mon.connect_changed(move |_, _, _, _| {
                if pending.replace(true) {
                    return;
                }
                let pending = pending.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                    pending.set(false);
                    apply();
                });
            });
            MONITORS.with(|m| m.borrow_mut().push(mon));
        }
    };
    watch(state.clone());
    watch(state.join("theme.name"));
}

fn apply() {
    let pal = Palette::load();
    let display = gdk::Display::default().expect("no display");
    adw::StyleManager::default().set_color_scheme(if pal.dark {
        adw::ColorScheme::ForceDark
    } else {
        adw::ColorScheme::ForceLight
    });
    let css = build_css(&pal);
    PROVIDER.with(|p| {
        let mut p = p.borrow_mut();
        let provider = p.get_or_insert_with(|| {
            let prov = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(
                &display,
                &prov,
                gtk::STYLE_PROVIDER_PRIORITY_USER + 10,
            );
            prov
        });
        provider.load_from_string(&css);
    });
    write_source_scheme(&pal);
    PALETTE.with(|p| *p.borrow_mut() = Some(pal));
    LISTENERS.with(|l| l.borrow().iter().for_each(|f| f()));
}

fn mix(a: &str, b: &str, t: f32) -> String {
    let p = |s: &str| {
        let s = s.trim_start_matches('#');
        let v = u32::from_str_radix(&s[..6.min(s.len())], 16).unwrap_or(0);
        [(v >> 16) as f32, ((v >> 8) & 0xff) as f32, (v & 0xff) as f32]
    };
    let (x, y) = (p(a), p(b));
    let m: Vec<u8> = (0..3).map(|i| (x[i] * (1.0 - t) + y[i] * t).round() as u8).collect();
    format!("#{:02x}{:02x}{:02x}", m[0], m[1], m[2])
}

fn build_css(p: &Palette) -> String {
    let g = |k: &str| p.get(k);
    let bg = g("background");
    let sidebar = g("dark_background");
    let darker = g("darker_background");
    let lighter = g("lighter_background");
    let fg = g("foreground");
    let dim = g("dark_foreground");
    let bright = g("bright_foreground");
    let accent = g("accent");
    let sel = g("selection");
    let muted = g("muted");
    let border = mix(&darker, &sidebar, 0.3);
    let hover = mix(&sidebar, &lighter, 0.55);
    let sel_inactive = mix(&sidebar, &sel, 0.6);
    let accent_soft = mix(&sidebar, &accent, 0.18);
    let red = g("red");
    let yellow = g("yellow");
    let green = g("green");
    let magenta = g("magenta");
    let cyan = g("cyan");
    let orange = g("orange");
    let css = format!(
        r#"
:root {{
  --window-bg-color: {bg};
  --window-fg-color: {fg};
  --view-bg-color: {bg};
  --view-fg-color: {fg};
  --headerbar-bg-color: {sidebar};
  --headerbar-fg-color: {fg};
  --headerbar-border-color: {border};
  --headerbar-backdrop-color: {sidebar};
  --popover-bg-color: {lighter};
  --popover-fg-color: {fg};
  --dialog-bg-color: {lighter};
  --dialog-fg-color: {fg};
  --card-bg-color: {lighter};
  --card-fg-color: {fg};
  --sidebar-bg-color: {sidebar};
  --accent-bg-color: {accent};
  --accent-fg-color: {darker};
  --accent-color: {accent};
  --destructive-bg-color: {red};
  --destructive-color: {red};
  --success-color: {green};
  --warning-color: {yellow};
  --error-color: {red};
  --border-color: {border};
}}
window.philly {{ background: {bg}; color: {fg}; }}
window.philly headerbar {{ background: {sidebar}; box-shadow: inset 0 -1px {border}; min-height: 42px; padding: 0 4px; }}
window.philly headerbar button.flat {{ color: {dim}; min-width: 28px; min-height: 28px; padding: 0 4px; }}
window.philly headerbar button.flat:hover {{ color: {bright}; background: {hover}; }}

/* ---------- path bar ---------- */
.pathbar {{ background: {darker}; border: 1px solid {border}; border-radius: 7px; min-height: 30px; padding: 0 2px; }}
.pathbar:hover {{ border-color: {muted}; }}
.pathbar.editing {{ border-color: {accent}; box-shadow: 0 0 0 1px {accent_soft}; }}
.pathbar .crumb {{ padding: 2px 6px; min-height: 22px; border-radius: 5px; color: {fg}; font-weight: 500; }}
.pathbar .crumb:hover {{ background: {hover}; color: {bright}; }}
.pathbar .crumb.last {{ color: {bright}; font-weight: 600; }}
.pathbar .crumb-sep {{ padding: 0; min-width: 14px; min-height: 22px; color: {dim}; border-radius: 4px; }}
.pathbar .crumb-sep:hover {{ background: {hover}; color: {bright}; }}
.pathbar .crumb-icon {{ color: {accent}; margin: 0 2px 0 6px; }}
.pathbar entry {{ background: none; border: none; box-shadow: none; outline: none; min-height: 26px; font-family: monospace; color: {bright}; }}
.pathbar .pin {{ color: {dim}; }}
.pathbar .pin.pinned {{ color: {yellow}; }}
.suggestions {{ padding: 4px; }}
.suggestions row {{ border-radius: 5px; padding: 4px 8px; min-height: 24px; }}
.suggestions row:selected {{ background: {sel}; color: {bright}; }}
.suggestions .kind {{ color: {dim}; font-size: 0.85em; }}
.suggestions .sugg-icon {{ margin-right: 8px; }}
.places-list {{ background: transparent; padding: 0 4px 8px 4px; }}
.places-list row {{ border-radius: 5px; padding: 3px 6px 3px 10px; min-height: 22px; }}
.places-list row:hover {{ background: alpha({fg}, 0.06); }}
.places-list row:selected {{ background: {sel}; color: {bright}; }}
.places-list .file-icon {{ font-family: "JetBrainsMono Nerd Font", "Symbols Nerd Font", monospace; font-size: 14px; min-width: 18px; margin-right: 8px; }}
.places-list .kind {{ color: {dim}; font-size: 0.85em; margin-left: 8px; }}
.places-list .row-action {{ opacity: 0; min-height: 18px; min-width: 18px; padding: 0; }}
.places-list row:hover .row-action {{ opacity: 1; }}
.places-hint {{ font-size: 0.85em; padding: 10px 14px 14px 14px; }}
.tree-sticky {{ background: {sidebar}; border-bottom: 1px solid {border}; box-shadow: 0 3px 6px -2px alpha(black, 0.45); }}
.sticky-row {{ min-height: 22px; }}
.sticky-row:hover {{ background: {hover}; }}
.sticky-chevron {{ -gtk-icon-size: 12px; min-width: 14px; color: {dim}; }}
.tree-up {{ min-width: 22px; min-height: 22px; padding: 0; margin-right: 2px; }}
.folder-view {{ padding: 18px 22px 24px 22px; }}
.fv-card {{ background: {sidebar}; border-radius: 10px; padding: 18px 22px; }}
.fv-dim {{ color: {dim}; }}
.fv-desc {{ color: {fg}; }}
.fv-section {{ padding: 20px 2px 8px 2px; }}
.fv-chip {{ background: {lighter}; border-radius: 6px; padding: 3px 9px; font-family: "JetBrainsMono Nerd Font", monospace; font-size: 0.88em; color: {bright}; }}
.fv-chip:hover {{ background: {accent_soft}; }}
.fv-chips flowboxchild {{ padding: 0; }}
.fv-row {{ padding: 4px 6px; min-height: 22px; }}
.fv-row .file-icon {{ font-family: "JetBrainsMono Nerd Font", "Symbols Nerd Font", monospace; font-size: 14px; min-width: 18px; margin-right: 10px; }}
.fv-row .fv-dim {{ margin-left: 14px; font-size: 0.9em; font-feature-settings: "tnum"; }}
.fv-sub {{ color: {dim}; font-size: 0.85em; font-feature-settings: "tnum"; }}
.fv-mono {{ font-family: "JetBrainsMono Nerd Font", monospace; color: {accent}; font-size: 0.9em; }}
.fv-recent {{ background: transparent; }}
.fv-recent row {{ border-radius: 5px; }}
.fv-recent row:hover {{ background: alpha({fg}, 0.05); }}
.fv-bar {{ border-radius: 4px; margin: 2px 2px 10px 2px; }}
.fv-legend {{ font-size: 0.9em; }}
.fv-grid flowboxchild {{ border-radius: 8px; padding: 6px; }}
.fv-grid flowboxchild:hover {{ background: alpha({fg}, 0.05); }}
.fv-grid flowboxchild:selected {{ background: {sel}; }}
.fv-thumb {{ border-radius: 6px; background: {sidebar}; }}
.fv-play {{ font-family: "JetBrainsMono Nerd Font", monospace; font-size: 20px; color: white; background: alpha(black, 0.5); border-radius: 99px; padding: 6px 9px 6px 11px; }}

/* ---------- layout ---------- */
.preview-pane {{ background: {bg}; }}
.sidebar-pane {{ background: {sidebar}; }}
paned.main > separator {{ background: {border}; min-width: 1px; }}
.activitybar {{ background: {darker}; border-left: 1px solid {border}; padding: 6px 0; }}
.activitybar button {{ min-width: 44px; min-height: 44px; padding: 0; border-radius: 0; color: {dim}; background: none; box-shadow: none; border-left: 2px solid transparent; }}
.activitybar button:hover {{ color: {bright}; }}
.activitybar button:checked {{ color: {bright}; border-left-color: {accent}; }}
.activitybar .badge {{ background: {accent}; color: {darker}; border-radius: 99px; font-size: 0.65em; font-weight: 700; min-width: 15px; min-height: 15px; padding: 0 3px; margin: 6px 4px; }}
.section-title {{ font-size: 0.75em; font-weight: 700; letter-spacing: 0.08em; color: {dim}; padding: 10px 14px 6px 14px; }}
.section-header {{ padding: 0 6px 0 4px; min-height: 26px; }}
.section-header label {{ font-size: 0.75em; font-weight: 700; letter-spacing: 0.06em; color: {fg}; }}
.section-header button {{ min-width: 22px; min-height: 22px; padding: 0; color: {dim}; }}
.section-header button:hover {{ color: {bright}; }}

/* ---------- tree ---------- */
listview.tree, listview.flat {{ background: {sidebar}; color: {fg}; }}
listview.tree > row, listview.flat > row {{ padding: 0; min-height: 24px; border-radius: 0; outline: none; }}
listview.tree > row:hover, listview.flat > row:hover {{ background: {hover}; }}
listview.tree > row:selected, listview.flat > row:selected {{ background: {sel_inactive}; color: {bright}; }}
listview.tree:focus-within > row:selected, listview.flat:focus-within > row:selected {{ background: {sel}; box-shadow: inset 0 0 0 1px {accent}; }}
listview.tree.drop-target {{ box-shadow: inset 0 0 0 2px {accent}; }}
treeexpander {{ border-spacing: 0; }}
treeexpander indent {{ -gtk-icon-size: 12px; min-width: 14px; }}
treeexpander expander {{ min-width: 16px; color: {dim}; -gtk-icon-size: 12px; }}
.tree-row {{ padding: 0 8px 0 4px; }}
.tree-row .name {{ color: {fg}; }}
.tree-row .file-icon {{ font-family: "JetBrainsMono Nerd Font", "Symbols Nerd Font", monospace; font-size: 14px; min-width: 18px; margin-right: 6px; }}
.tree-row .badge {{ font-family: monospace; font-size: 0.85em; font-weight: 700; min-width: 14px; }}
.tree-row .detail {{ color: {dim}; font-size: 0.85em; margin-left: 6px; }}
.tree-row.ignored .name, .tree-row.ignored .file-icon {{ opacity: 0.45; }}
.tree-row.cut {{ opacity: 0.5; }}
.git-M .name, .git-M .badge {{ color: {yellow}; }}
.git-A .name, .git-A .badge, .git-U .name, .git-U .badge {{ color: {green}; }}
.git-D .name, .git-D .badge {{ color: {red}; }}
.git-D .name {{ text-decoration: line-through; }}
.git-R .name, .git-R .badge {{ color: {cyan}; }}
.git-C .name, .git-C .badge {{ color: {magenta}; }}
.git-dir .name {{ color: {yellow}; }}
.git-dir .badge {{ color: {yellow}; }}

/* ---------- git panel ---------- */
.commit-box {{ margin: 8px 12px 6px 12px; }}
.commit-box textview, .commit-box textview text {{ background: {darker}; color: {bright}; }}
.commit-frame {{ border: 1px solid {border}; border-radius: 6px; background: {darker}; padding: 6px 8px; }}
.commit-frame.focused {{ border-color: {accent}; }}
.commit-btn {{ margin: 0 12px 10px 12px; min-height: 30px; border-radius: 6px; font-weight: 600; }}
.branch {{ color: {dim}; font-size: 0.85em; padding: 0 14px 4px 14px; }}
.git-empty {{ color: {dim}; padding: 24px; }}
.navigation-sidebar.git-list > row {{ min-height: 22px; padding: 0; margin: 0 4px; border-radius: 4px; }}
.navigation-sidebar.git-list {{ padding: 2px 0; }}
.git-list .tree-row button {{ min-width: 20px; min-height: 20px; padding: 0; margin: 0; }}
.sync-btns {{ margin: 0 12px 4px 12px; }}
.sync-btn {{ min-height: 26px; padding: 0 8px; border-radius: 6px; font-size: 0.9em; }}
.sync-btn.pending {{ color: {accent}; box-shadow: inset 0 0 0 1px {accent}; }}
.sync-all {{ min-width: 26px; min-height: 26px; padding: 0 4px; border-radius: 6px; font-size: 0.9em; color: {dim}; }}
.sync-all:hover {{ color: {bright}; }}
.git-paned > separator {{ background: {border}; min-height: 1px; }}
.commits-header {{ margin-top: 2px; }}
.commits-count {{ color: {accent}; font-size: 0.8em; font-weight: 700; }}
.commit-row {{ padding: 0 6px 0 2px; min-height: 22px; }}
.commit-row .commit-graph {{ color: {dim}; }}
.commit-row.outgoing .commit-graph {{ color: {accent}; }}
.commit-row.incoming .commit-graph {{ color: {cyan}; }}
.commit-row.incoming .commit-summary {{ color: {dim}; font-style: italic; }}
.commit-row .commit-summary {{ color: {fg}; font-size: 0.92em; }}
.commit-row .commit-age {{ color: {dim}; font-size: 0.8em; }}
.ref-badge {{ background: {accent}; color: {darker}; border-radius: 99px; padding: 0 6px; font-size: 0.75em; font-weight: 700; }}
.ref-badge.remote {{ background: none; color: {cyan}; box-shadow: inset 0 0 0 1px {cyan}; }}

/* ---------- search ---------- */
.search-box {{ margin: 6px 12px; }}
.search-box entry {{ background: {darker}; border: 1px solid {border}; border-radius: 6px; min-height: 30px; }}
.search-box entry:focus-within {{ border-color: {accent}; box-shadow: none; outline: none; }}
.search-summary {{ color: {dim}; font-size: 0.85em; padding: 0 14px 6px 14px; }}
.match-line {{ font-family: monospace; font-size: 0.9em; color: {fg}; }}
.match-lnum {{ font-family: monospace; font-size: 0.85em; color: {dim}; min-width: 34px; }}

/* ---------- preview ---------- */
.preview-header {{ background: {sidebar}; border-bottom: 1px solid {border}; min-height: 34px; padding: 0 10px; }}
.preview-header .title {{ color: {bright}; font-weight: 600; }}
.preview-header .meta {{ color: {dim}; font-size: 0.85em; }}
.preview-header .file-icon {{ font-family: "JetBrainsMono Nerd Font", monospace; font-size: 15px; margin-right: 8px; }}
.preview-header button {{ min-height: 24px; min-width: 24px; padding: 0 6px; color: {dim}; }}
.preview-header button:hover, .preview-header button:checked {{ color: {bright}; }}
.preview-empty {{ color: {dim}; }}
.preview-empty .big {{ font-size: 64px; font-family: "JetBrainsMono Nerd Font", monospace; opacity: 0.35; }}
.preview-empty .hint {{ font-size: 0.9em; }}
.keycap {{ font-family: monospace; font-size: 0.85em; background: {lighter}; color: {fg}; border-radius: 4px; padding: 1px 6px; border-bottom: 2px solid {muted}; }}
textview.code, textview.code text {{ background: {bg}; }}
textview.markdown, textview.markdown text {{ background: {bg}; color: {fg}; }}
.info-card {{ background: {sidebar}; border-radius: 10px; padding: 20px 26px; }}
.info-card .k {{ color: {dim}; }}
.info-card .v {{ color: {bright}; }}
.pdf-page {{ background: white; box-shadow: 0 2px 10px alpha(black, 0.45); margin: 10px; }}
.diff-gutter {{ background: {sidebar}; }}
.image-stage {{ background-image: linear-gradient(45deg, {lighter} 25%, transparent 25%), linear-gradient(-45deg, {lighter} 25%, transparent 25%), linear-gradient(45deg, transparent 75%, {lighter} 75%), linear-gradient(-45deg, transparent 75%, {lighter} 75%); background-size: 20px 20px; background-position: 0 0, 0 10px, 10px -10px, -10px 0; background-color: {bg}; }}

/* ---------- palette (quick open) ---------- */
.palette {{ background: {lighter}; border: 1px solid {muted}; border-radius: 10px; box-shadow: 0 10px 40px alpha(black, 0.55); margin-top: 8px; padding: 6px; }}
.palette entry {{ background: {darker}; border: 1px solid {accent}; border-radius: 6px; min-height: 32px; box-shadow: none; outline: none; }}
.palette list, .palette listview {{ background: none; }}
.palette row {{ border-radius: 5px; padding: 3px 8px; min-height: 26px; }}
.palette row:selected {{ background: {sel}; }}
.palette .path {{ color: {dim}; font-size: 0.85em; margin-left: 8px; }}
.palette .hl {{ color: {accent}; }}
.palette .file-icon {{ font-family: "JetBrainsMono Nerd Font", monospace; min-width: 18px; margin-right: 8px; }}

/* ---------- status bar ---------- */
.statusbar {{ background: {darker}; border-top: 1px solid {border}; min-height: 24px; padding: 0 10px; font-size: 0.82em; color: {dim}; }}
.statusbar .branch-pill {{ color: {fg}; }}
.statusbar .accent {{ color: {accent}; }}

/* ---------- picker ---------- */
.picker-bar {{ background: {sidebar}; border-top: 1px solid {border}; padding: 10px 12px; }}
.picker-bar entry {{ background: {darker}; border: 1px solid {border}; border-radius: 6px; min-height: 32px; }}
.picker-bar entry:focus-within {{ border-color: {accent}; outline: none; }}
.picker-bar .suggested-action {{ min-height: 32px; padding: 0 18px; border-radius: 6px; font-weight: 600; }}
.picker-bar button {{ min-height: 32px; border-radius: 6px; }}

/* ---------- misc ---------- */
/* The stock symbolic icon size, stated here so it follows the zoom. */
image {{ -gtk-icon-size: 16px; }}
popover.menu contents, popover contents {{ border-radius: 8px; }}
.shortcuts-sheet {{ background: {lighter}; border-radius: 12px; border: 1px solid {muted}; padding: 18px 22px; box-shadow: 0 10px 40px alpha(black, 0.55); }}
.shortcuts-sheet .group {{ color: {accent}; font-weight: 700; font-size: 0.8em; letter-spacing: 0.08em; margin-top: 10px; }}
.shortcuts-sheet .desc {{ color: {fg}; }}
toast {{ background: {lighter}; color: {bright}; border: 1px solid {muted}; }}
scrollbar slider {{ background: alpha({fg}, 0.25); min-width: 6px; min-height: 6px; }}
scrollbar slider:hover {{ background: alpha({fg}, 0.4); }}
.dim {{ color: {dim}; }}
.orange {{ color: {orange}; }}
"#
    );
    scale_px(&css, zoom())
}

#[cfg(test)]
mod tests {
    use super::scale_px;

    #[test]
    fn scales_px_lengths_only() {
        let css = ".a { color: #14px0; min-height: 24px; margin: 0 -2px 1px 0.5em; font-size: 0.85em; border: 1px solid #1a1b26; }";
        assert_eq!(scale_px(css, 1.0), css);
        assert_eq!(
            scale_px(css, 1.5),
            ".a { color: #14px0; min-height: 36px; margin: 0 -3px 2px 0.5em; font-size: 0.85em; border: 2px solid #1a1b26; }"
        );
        assert_eq!(scale_px("x { min-width: 1px; padding: 0px 3px; }", 0.6), "x { min-width: 1px; padding: 0px 2px; }");
    }
}

/// GtkSourceView style scheme generated from the palette, so code previews
/// follow the theme too.
fn write_source_scheme(p: &Palette) {
    let dir = scheme_dir();
    let _ = std::fs::create_dir_all(&dir);
    let g = |k: &str| p.get(k);
    let bg = g("background");
    let line_hl = mix(&bg, &g("lighter_background"), 0.5);
    let add_bg = mix(&bg, &g("green"), 0.14);
    let del_bg = mix(&bg, &g("red"), 0.14);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<style-scheme id="philly" name="Philly (Omarchy)" version="1.0" kind="{kind}">
  <style name="text" foreground="{fg}" background="{bg}"/>
  <style name="selection" background="{sel}"/>
  <style name="current-line" background="{line_hl}"/>
  <style name="line-numbers" foreground="{muted}" background="{bg}"/>
  <style name="current-line-number" foreground="{fgb}" background="{bg}"/>
  <style name="bracket-match" background="{muted}" bold="true"/>
  <style name="search-match" background="{selb}" foreground="{bright}"/>
  <style name="draw-spaces" foreground="{muted}"/>
  <style name="def:comment" foreground="{comment}" italic="true"/>
  <style name="def:shebang" foreground="{comment}" bold="true"/>
  <style name="def:doc-comment-element" foreground="{comment}" bold="true"/>
  <style name="def:constant" foreground="{orange}"/>
  <style name="def:special-char" foreground="{cyan}"/>
  <style name="def:string" foreground="{green}"/>
  <style name="def:number" foreground="{orange}"/>
  <style name="def:boolean" foreground="{orange}"/>
  <style name="def:keyword" foreground="{magenta}" italic="true"/>
  <style name="def:statement" foreground="{magenta}"/>
  <style name="def:builtin" foreground="{red}"/>
  <style name="def:type" foreground="{cyan2}"/>
  <style name="def:identifier" foreground="{fgb}"/>
  <style name="def:function" foreground="{blue}"/>
  <style name="def:preprocessor" foreground="{cyan}"/>
  <style name="def:operator" foreground="{cyan}"/>
  <style name="def:emphasis" italic="true"/>
  <style name="def:strong-emphasis" bold="true"/>
  <style name="def:heading" foreground="{blue}" bold="true"/>
  <style name="def:link-text" foreground="{blue}"/>
  <style name="def:inline-code" foreground="{green}"/>
  <style name="def:error" foreground="{red}" underline="error"/>
  <style name="def:note" foreground="{yellow}" bold="true"/>
  <style name="def:net-address" foreground="{blue}" underline="single"/>
  <style name="xml:attribute-name" foreground="{magenta}"/>
  <style name="xml:element-name" foreground="{red}"/>
  <style name="json:keyname" foreground="{blue}"/>
  <style name="diff:added-line" foreground="{green}" background="{add_bg}"/>
  <style name="diff:removed-line" foreground="{red}" background="{del_bg}"/>
  <style name="diff:changed-line" foreground="{yellow}"/>
  <style name="diff:location" foreground="{cyan}" bold="true"/>
  <style name="diff:diff-file" foreground="{magenta}" bold="true"/>
  <style name="diff:special-case" foreground="{dim}"/>
</style-scheme>
"#,
        kind = if p.dark { "dark" } else { "light" },
        fg = g("foreground"),
        fgb = g("bright_foreground"),
        bright = g("bright_foreground"),
        bg = bg,
        sel = g("selection"),
        selb = mix(&g("selection"), &g("accent"), 0.3),
        line_hl = line_hl,
        muted = g("muted"),
        comment = mix(&g("dark_foreground"), &g("foreground"), 0.15),
        dim = g("dark_foreground"),
        orange = g("orange"),
        cyan = g("cyan"),
        cyan2 = mix(&g("cyan"), &g("blue"), 0.3),
        green = g("green"),
        magenta = g("magenta"),
        red = g("red"),
        blue = g("blue"),
        yellow = g("yellow"),
        add_bg = add_bg,
        del_bg = del_bg,
    );
    let _ = std::fs::write(dir.join("philly.xml"), xml);
    let mgr = sourceview5::StyleSchemeManager::default();
    let path = dir.to_string_lossy().to_string();
    if !mgr.search_path().iter().any(|p| p.as_str() == path) {
        mgr.prepend_search_path(&path);
    }
    mgr.force_rescan();
}

pub fn scheme_dir() -> PathBuf {
    glib::user_cache_dir().join("philly/styles")
}

pub fn source_scheme() -> Option<sourceview5::StyleScheme> {
    sourceview5::StyleSchemeManager::default().scheme("philly")
}

/// Background tint used for added / removed lines in diffs.
pub fn diff_colors() -> (String, String, String, String) {
    let p = palette();
    let bg = p.get("background");
    (
        mix(&bg, &p.get("green"), 0.16),
        mix(&bg, &p.get("red"), 0.16),
        mix(&bg, &p.get("green"), 0.38),
        mix(&bg, &p.get("red"), 0.38),
    )
}

pub fn color(key: &str) -> String {
    palette().get(key)
}
