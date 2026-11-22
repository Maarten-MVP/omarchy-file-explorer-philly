//! Small persisted UI state (window size, pane width, toggles).

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct State {
    pub width: i32,
    pub height: i32,
    /// Width of the left sidebar (favorites / search / git) in pixels.
    pub sidebar: i32,
    /// Width of the preview pane on the right.
    pub preview: i32,
    /// File list sort column and direction.
    pub sort: crate::tree::Sort,
    pub split: bool,
    pub show_hidden: bool,
    /// Sidebar and preview wanted (they also hide when the window is narrow).
    pub show_sidebar: bool,
    pub show_preview: bool,
    pub last_picker_dir: Option<PathBuf>,
    /// UI zoom factor (Ctrl +/−), 1.0 = system size.
    pub zoom: f64,
}

impl Default for State {
    fn default() -> Self {
        State { width: 1280, height: 820, sidebar: 220, preview: 440, sort: crate::tree::Sort::default(), split: false, show_hidden: true, show_sidebar: true, show_preview: true, last_picker_dir: None, zoom: 1.0 }
    }
}

fn file() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| gtk::glib::home_dir().join(".local/state")).join("philly/state")
}

pub fn load() -> State {
    let mut s = State::default();
    let Ok(text) = std::fs::read_to_string(file()) else { return s };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "width" => s.width = v.parse().unwrap_or(s.width),
            "height" => s.height = v.parse().unwrap_or(s.height),
            "sidebar" => s.sidebar = v.parse().unwrap_or(s.sidebar),
            "preview" => s.preview = v.parse().unwrap_or(s.preview),
            "sort" => s.sort.key = crate::tree::SortKey::parse(v),
            "sort_desc" => s.sort.desc = v == "true",
            "split" => s.split = v == "true",
            "show_hidden" => s.show_hidden = v == "true",
            "show_sidebar" => s.show_sidebar = v != "false",
            "show_preview" => s.show_preview = v != "false",
            "zoom" => s.zoom = v.parse().unwrap_or(s.zoom),
            "last_picker_dir" if !v.is_empty() => s.last_picker_dir = Some(PathBuf::from(v)),
            _ => {}
        }
    }
    s
}

pub fn save(s: &State) {
    let f = file();
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let text = format!(
        "width={}\nheight={}\nsidebar={}\npreview={}\nsort={}\nsort_desc={}\nsplit={}\nshow_hidden={}\nshow_sidebar={}\nshow_preview={}\nzoom={}\nlast_picker_dir={}\n",
        s.width,
        s.height,
        s.sidebar,
        s.preview,
        s.sort.key.as_str(),
        s.sort.desc,
        s.split,
        s.show_hidden,
        s.show_sidebar,
        s.show_preview,
        s.zoom,
        s.last_picker_dir.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
    );
    let _ = std::fs::write(f, text);
}
