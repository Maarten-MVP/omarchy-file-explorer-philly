//! File-type icons as Nerd Font glyphs, colored per type (VS Code / devicons style).

/// Returns (glyph, palette color key).
pub fn for_file(name: &str, is_dir: bool, expanded: bool) -> (&'static str, &'static str) {
    if is_dir {
        let special = match name {
            ".git" => Some(("\u{e5fb}", "orange")),
            ".github" => Some(("\u{e5fd}", "foreground")),
            "node_modules" => Some(("\u{e5fa}", "green")),
            ".vscode" => Some(("\u{e70c}", "blue")),
            "src" | "lib" => Some(("\u{f07b}", "blue")),
            "test" | "tests" | "__tests__" | "spec" => Some(("\u{f07b}", "green")),
            "docs" | "doc" => Some(("\u{f07b}", "cyan")),
            "assets" | "images" | "img" | "public" | "static" => Some(("\u{f07b}", "magenta")),
            ".config" | "config" => Some(("\u{e5fc}", "foreground")),
            "Downloads" => Some(("\u{f498}", "cyan")),
            "Pictures" => Some(("\u{f03e}", "magenta")),
            "Music" => Some(("\u{f001}", "red")),
            "Videos" => Some(("\u{f03d}", "orange")),
            "Documents" => Some(("\u{f0219}", "blue")),
            _ => None,
        };
        if let Some(s) = special {
            if !expanded || s.0 != "\u{f07b}" {
                return s;
            }
            return ("\u{f07c}", s.1);
        }
        return if expanded { ("\u{f07c}", "blue") } else { ("\u{f07b}", "blue") };
    }

    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cargo.toml" | "cargo.lock" => return ("\u{e7a8}", "orange"),
        "package.json" | "package-lock.json" => return ("\u{e71e}", "red"),
        "pubspec.yaml" | "pubspec.lock" => return ("\u{e798}", "cyan"),
        "dockerfile" | "docker-compose.yml" | "docker-compose.yaml" | "compose.yaml" => {
            return ("\u{f308}", "blue")
        }
        "makefile" | "justfile" => return ("\u{e779}", "orange"),
        ".gitignore" | ".gitattributes" | ".gitmodules" => return ("\u{e702}", "orange"),
        "license" | "license.md" | "license.txt" => return ("\u{f0219}", "yellow"),
        "readme.md" | "readme" => return ("\u{f00ba}", "cyan"),
        ".env" | ".env.local" | ".env.example" => return ("\u{f462}", "yellow"),
        "go.mod" | "go.sum" => return ("\u{e627}", "cyan"),
        "tsconfig.json" => return ("\u{e628}", "blue"),
        "claude.md" | "agents.md" => return ("\u{f0219}", "orange"),
        _ => {}
    }
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "rs" => ("\u{e7a8}", "orange"),
        "ts" | "mts" | "cts" => ("\u{e628}", "blue"),
        "tsx" | "jsx" => ("\u{e7ba}", "cyan"),
        "js" | "mjs" | "cjs" => ("\u{e74e}", "yellow"),
        "json" | "jsonc" => ("\u{e60b}", "yellow"),
        "py" | "pyi" => ("\u{e73c}", "yellow"),
        "go" => ("\u{e627}", "cyan"),
        "dart" => ("\u{e798}", "cyan"),
        "c" | "h" => ("\u{e61e}", "blue"),
        "cpp" | "cc" | "hpp" | "cxx" => ("\u{e61d}", "blue"),
        "java" => ("\u{e738}", "red"),
        "kt" | "kts" => ("\u{e634}", "magenta"),
        "swift" => ("\u{e755}", "orange"),
        "rb" => ("\u{e739}", "red"),
        "php" => ("\u{e73d}", "magenta"),
        "lua" => ("\u{e620}", "blue"),
        "zig" => ("\u{e6a9}", "orange"),
        "sh" | "bash" | "zsh" | "fish" => ("\u{f489}", "green"),
        "html" | "htm" => ("\u{e736}", "orange"),
        "css" => ("\u{e749}", "blue"),
        "scss" | "sass" => ("\u{e603}", "magenta"),
        "vue" => ("\u{e6a0}", "green"),
        "svelte" => ("\u{e697}", "orange"),
        "md" | "markdown" | "mdx" => ("\u{e73e}", "foreground"),
        "toml" => ("\u{e6b2}", "foreground"),
        "yml" | "yaml" => ("\u{e6a8}", "magenta"),
        "xml" => ("\u{f05c0}", "orange"),
        "sql" | "db" | "sqlite" => ("\u{f1c0}", "cyan"),
        "lock" => ("\u{f023}", "dark_foreground"),
        "txt" | "log" => ("\u{f15c}", "foreground"),
        "csv" | "tsv" | "xlsx" | "xls" | "ods" => ("\u{f0219}", "green"),
        "pdf" => ("\u{f1c1}", "red"),
        "doc" | "docx" | "odt" => ("\u{f1c2}", "blue"),
        "ppt" | "pptx" | "odp" => ("\u{f1c4}", "orange"),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "avif" | "heic" => {
            ("\u{f1c5}", "magenta")
        }
        "svg" => ("\u{f0721}", "yellow"),
        "mp4" | "mkv" | "webm" | "mov" | "avi" => ("\u{f1c8}", "orange"),
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "opus" => ("\u{f1c7}", "red"),
        "zip" | "tar" | "gz" | "xz" | "zst" | "7z" | "rar" | "bz2" => ("\u{f1c6}", "yellow"),
        "ttf" | "otf" | "woff" | "woff2" => ("\u{f031}", "foreground"),
        "iso" | "img" | "deb" | "rpm" | "appimage" => ("\u{f0a0}", "dark_foreground"),
        "key" | "pem" | "crt" | "pub" | "gpg" | "asc" => ("\u{f084}", "yellow"),
        "desktop" | "service" | "conf" | "ini" | "cfg" => ("\u{f013}", "dark_foreground"),
        _ => ("\u{f15b}", "dark_foreground"),
    }
}

/// Markup span for an icon, colored with the current theme.
pub fn markup(name: &str, is_dir: bool, expanded: bool) -> String {
    let (glyph, key) = for_file(name, is_dir, expanded);
    format!("<span foreground=\"{}\">{}</span>", crate::theme::color(key), glyph)
}
