//! D-Bus services:
//! - org.freedesktop.impl.portal.FileChooser: replaces the GTK "Open / Save"
//!   dialog that apps (Brave, Chromium, Electron, Firefox) show via the portal.
//! - org.freedesktop.FileManager1: "Show in folder" from browsers and apps.

use crate::window::{Filter, PickerSpec, Win};
use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

pub const PORTAL_NAME: &str = "org.freedesktop.impl.portal.desktop.philly";
pub const FM_NAME: &str = "org.freedesktop.FileManager1";

const PORTAL_XML: &str = r#"
<node>
  <interface name="org.freedesktop.impl.portal.FileChooser">
    <method name="OpenFile">
      <arg type="o" name="handle" direction="in"/>
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/>
      <arg type="a{sv}" name="results" direction="out"/>
    </method>
    <method name="SaveFile">
      <arg type="o" name="handle" direction="in"/>
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/>
      <arg type="a{sv}" name="results" direction="out"/>
    </method>
    <method name="SaveFiles">
      <arg type="o" name="handle" direction="in"/>
      <arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/>
      <arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/>
      <arg type="a{sv}" name="results" direction="out"/>
    </method>
  </interface>
</node>
"#;

const REQUEST_XML: &str = r#"
<node>
  <interface name="org.freedesktop.impl.portal.Request">
    <method name="Close"/>
  </interface>
</node>
"#;

const FM_XML: &str = r#"
<node>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItems">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItemProperties">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
  </interface>
</node>
"#;

thread_local! {
    static OWNERS: RefCell<Vec<gio::OwnerId>> = const { RefCell::new(Vec::new()) };
    static REGS: RefCell<Vec<gio::RegistrationId>> = const { RefCell::new(Vec::new()) };
}

fn iface(xml: &str, name: &str) -> gio::DBusInterfaceInfo {
    gio::DBusNodeInfo::for_xml(xml).expect("bad introspection xml").lookup_interface(name).expect("missing interface")
}

pub fn register(app: &adw::Application) {
    let a = app.downgrade();
    let id = gio::bus_own_name(
        gio::BusType::Session,
        PORTAL_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        move |conn, _| {
            let info = iface(PORTAL_XML, "org.freedesktop.impl.portal.FileChooser");
            let a = a.clone();
            let reg = conn
                .register_object("/org/freedesktop/portal/desktop", &info)
                .method_call(move |conn, _sender, _path, _iface, method, params, inv| {
                    if let Some(app) = a.upgrade() {
                        handle_file_chooser(&app, &conn, method, &params, inv);
                    } else {
                        inv.return_dbus_error("org.freedesktop.DBus.Error.Failed", "app gone");
                    }
                })
                .build();
            if let Ok(r) = reg {
                REGS.with(|v| v.borrow_mut().push(r));
            }
        },
        |_, _| {},
        |_, name| eprintln!("philly: could not own {name} (another file chooser is active)"),
    );
    OWNERS.with(|v| v.borrow_mut().push(id));

    let a = app.downgrade();
    let id = gio::bus_own_name(
        gio::BusType::Session,
        FM_NAME,
        gio::BusNameOwnerFlags::REPLACE | gio::BusNameOwnerFlags::ALLOW_REPLACEMENT,
        move |conn, _| {
            let info = iface(FM_XML, "org.freedesktop.FileManager1");
            let a = a.clone();
            let reg = conn
                .register_object("/org/freedesktop/FileManager1", &info)
                .method_call(move |_conn, _sender, _path, _iface, method, params, inv| {
                    let uris: Vec<String> = params.child_value(0).get().unwrap_or_default();
                    if let Some(app) = a.upgrade() {
                        for u in uris.iter().take(4) {
                            let Some(p) = gio::File::for_uri(u).path() else { continue };
                            let target = if method == "ShowFolders" { p } else { p };
                            crate::open_window(&app, Some(target), None);
                        }
                    }
                    inv.return_value(None);
                })
                .build();
            if let Ok(r) = reg {
                REGS.with(|v| v.borrow_mut().push(r));
            }
        },
        |_, _| {},
        |_, _| {},
    );
    OWNERS.with(|v| v.borrow_mut().push(id));
}

fn lookup<T: glib::variant::FromVariant>(opts: &glib::VariantDict, key: &str) -> Option<T> {
    opts.lookup_value(key, None).and_then(|v| v.get::<T>())
}

fn bytes_path(v: Option<Vec<u8>>) -> Option<PathBuf> {
    let mut b = v?;
    while b.last() == Some(&0) {
        b.pop();
    }
    if b.is_empty() {
        return None;
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(b)))
}

type FilterTuple = (String, Vec<(u32, String)>);

fn handle_file_chooser(app: &adw::Application, conn: &gio::DBusConnection, method: &str, params: &glib::Variant, inv: gio::DBusMethodInvocation) {
    if std::env::var_os("PHILLY_DEBUG").is_some() {
        eprintln!("philly: portal {method} {params}");
    }
    let handle: String = params.child_value(0).str().unwrap_or("/").to_string();
    let title: String = params.child_value(3).get().unwrap_or_default();
    let options = params.child_value(4);
    let opts = glib::VariantDict::new(Some(&options));

    let save = method != "OpenFile";
    let save_files = method == "SaveFiles";
    let multiple = lookup::<bool>(&opts, "multiple").unwrap_or(false);
    let directory = lookup::<bool>(&opts, "directory").unwrap_or(false) || save_files;
    let accept_label = lookup::<String>(&opts, "accept_label");
    let current_name = lookup::<String>(&opts, "current_name");
    let current_file = bytes_path(lookup::<Vec<u8>>(&opts, "current_file"));
    let current_folder = bytes_path(lookup::<Vec<u8>>(&opts, "current_folder"))
        .or_else(|| current_file.as_ref().and_then(|f| f.parent().map(|p| p.to_path_buf())))
        .filter(|p| p.is_dir());
    let current_name = current_name.or_else(|| current_file.as_ref().and_then(|f| f.file_name()).map(|s| s.to_string_lossy().to_string()));
    let raw_filters: Vec<FilterTuple> = lookup(&opts, "filters").unwrap_or_default();
    let cur_filter: Option<FilterTuple> = lookup(&opts, "current_filter");
    let mut filters: Vec<Filter> = raw_filters.iter().map(|(n, r)| Filter { name: n.clone(), rules: r.clone() }).collect();
    let mut current_filter = cur_filter.as_ref().and_then(|(n, _)| filters.iter().position(|f| &f.name == n));
    if let (None, Some((n, r))) = (current_filter, &cur_filter) {
        filters.insert(0, Filter { name: n.clone(), rules: r.clone() });
        current_filter = Some(0);
    }
    let files_to_save: Vec<Vec<u8>> = lookup(&opts, "files").unwrap_or_default();

    // Register the Request object so the caller can close the dialog.
    let req_info = iface(REQUEST_XML, "org.freedesktop.impl.portal.Request");
    let window_slot: std::rc::Rc<RefCell<Option<glib::WeakRef<adw::ApplicationWindow>>>> = Default::default();
    let ws = window_slot.clone();
    let req_reg = conn
        .register_object(&handle, &req_info)
        .method_call(move |_, _, _, _, _, _, inv| {
            if let Some(w) = ws.borrow().as_ref().and_then(|w| w.upgrade()) {
                w.close();
            }
            inv.return_value(None);
        })
        .build()
        .ok();

    let conn2 = conn.clone();
    let filters_for_reply = filters.clone();
    let reply = move |uris: Option<Vec<String>>, filter_idx: Option<usize>| {
        if let Some(r) = req_reg {
            let _ = conn2.unregister_object(r);
        }
        let mut results: HashMap<String, glib::Variant> = HashMap::new();
        let code: u32 = match uris {
            Some(mut uris) => {
                if save_files {
                    // SaveFiles: the user picked a folder; return one URI per file.
                    let dir = uris.first().and_then(|u| gio::File::for_uri(u).path()).unwrap_or_default();
                    uris = files_to_save
                        .iter()
                        .filter_map(|b| bytes_path(Some(b.clone())))
                        .map(|name| gio::File::for_path(crate::fileops::free_name(&dir, &name.to_string_lossy())).uri().to_string())
                        .collect();
                }
                results.insert("uris".into(), uris.to_variant());
                results.insert("writable".into(), true.to_variant());
                if let Some(f) = filter_idx.and_then(|i| filters_for_reply.get(i)) {
                    results.insert("current_filter".into(), (f.name.clone(), f.rules.clone()).to_variant());
                }
                0
            }
            None => 1,
        };
        inv.return_value(Some(&(code, results).to_variant()));
    };

    let spec = PickerSpec {
        save: save && !save_files,
        multiple: multiple && !save,
        directory,
        title: if title.is_empty() { if save { "Save".into() } else { "Open".into() } } else { title },
        accept_label,
        current_name,
        current_folder,
        filters,
        current_filter,
        reply: RefCell::new(Some(Box::new(reply))),
    };
    let win = Win::new(app, None, Some(spec));
    *window_slot.borrow_mut() = Some(win.window.downgrade());
    crate::keep(win);
}
