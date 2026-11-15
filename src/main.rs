//! philly: a fast, keyboard-first file explorer with a git panel, built for Omarchy.
//!
//! Usage:
//!   philly [PATH]      open a window (reuses the running instance)
//!   philly --service   stay resident in the background (instant windows, file picker)

mod fileops;
mod git;
mod gitpanel;
mod icons;
mod pathbar;
mod places;
mod portal;
mod preview;
mod search;
mod state;
mod theme;
mod tree;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use window::Win;

pub const APP_ID: &str = "dev.philly.Philly";

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<Win>>> = const { RefCell::new(Vec::new()) };
    static HELD: Cell<bool> = const { Cell::new(false) };
}

/// Keep a window's controller alive until the window is closed.
///
/// GTK only destroys a window once its last reference is gone, and `Win` holds
/// one; so release it on close-request (not on destroy, which would never come).
pub fn keep(win: Rc<Win>) {
    let w = Rc::downgrade(&win);
    win.window.connect_close_request(move |_| {
        let w = w.clone();
        // After the other close-request handlers ran (they save state, reply to the portal).
        glib::idle_add_local_once(move || {
            if let Some(w) = w.upgrade() {
                WINDOWS.with(|v| v.borrow_mut().retain(|x| !Rc::ptr_eq(x, &w)));
            }
        });
        glib::Propagation::Proceed
    });
    WINDOWS.with(|v| v.borrow_mut().push(win));
}

pub fn open_window(app: &adw::Application, path: Option<PathBuf>, picker: Option<window::PickerSpec>) {
    let win = Win::new(app, path, picker);
    keep(win);
}

fn main() -> glib::ExitCode {
    // philly *is* a portal backend (FileChooser). GTK and libadwaita read desktop
    // settings from xdg-desktop-portal synchronously during startup, while the
    // portal waits for philly to come up: a 25 s deadlock at login. philly takes
    // its look from the Omarchy theme, so it doesn't need those settings.
    std::env::set_var("ADW_DISABLE_PORTAL", "1");
    let gdk_debug = std::env::var("GDK_DEBUG").unwrap_or_default();
    if !gdk_debug.contains("no-portals") {
        std::env::set_var("GDK_DEBUG", if gdk_debug.is_empty() { "no-portals".into() } else { format!("{gdk_debug},no-portals") });
    }
    let app = adw::Application::builder()
        .application_id(if std::env::var_os("PHILLY_NO_DBUS").is_some() { "dev.philly.PhillyDev" } else { APP_ID })
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    app.connect_startup(|app| {
        theme::init(state::load().zoom);
        if std::env::var_os("PHILLY_NO_DBUS").is_none() {
            portal::register(app);
        }
    });

    app.connect_command_line(|app, cmd| {
        let args: Vec<String> = cmd.arguments().iter().skip(1).map(|a| a.to_string_lossy().to_string()).collect();
        let cwd = cmd.cwd().unwrap_or_else(glib::home_dir);
        let mut service = false;
        let mut paths = Vec::new();
        for a in &args {
            match a.as_str() {
                "--service" | "--gapplication-service" => service = true,
                "--new-window" | "-w" => {}
                "-h" | "--help" => {
                    cmd.print_literal("Usage: philly [PATH...]\n       philly --service\n");
                    return glib::ExitCode::SUCCESS;
                }
                s if s.starts_with("file://") => {
                    if let Some(p) = gio::File::for_uri(s).path() {
                        paths.push(p);
                    }
                }
                s if s.starts_with('-') => {}
                s => paths.push(if s.starts_with('/') { PathBuf::from(s) } else if s == "~" { glib::home_dir() } else { cwd.join(s) }),
            }
        }
        if service {
            if !HELD.with(|h| h.replace(true)) {
                // The guard lives for the rest of the process.
                std::mem::forget(app.hold());
            }
            if paths.is_empty() {
                return glib::ExitCode::SUCCESS;
            }
        }
        if paths.is_empty() {
            open_window(app, None, None);
        }
        for p in paths {
            open_window(app, Some(p), None);
        }
        glib::ExitCode::SUCCESS
    });

    app.connect_open(|app, files, _| {
        for f in files {
            open_window(app, f.path(), None);
        }
    });

    app.run()
}
