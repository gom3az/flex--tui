//! Notification center engine, persistence, entity extractors, and quick control probes.
//!
//! Provides safe, high-performance state management for `flex-notify`, including:
//! - In-flight progress updates and notification threading
//! - Regex-free / robust entity extraction (2FA/OTP codes, URLs, hex colors)
//! - Quick system toggles (DND presets, Night Light, Caffeine, Mic mute)
//! - MPRIS track info and playback commands
//! - Persistent runtime store at `$XDG_RUNTIME_DIR/flex-notify.json`

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Urgency level matching the `FreeDesktop` notification specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Critical => "critical",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "critical" | "crit" => Self::Critical,
            _ => Self::Normal,
        }
    }
}

/// Action button attached to a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationAction {
    pub id: String,
    pub title: String,
}

/// A rich notification item stored in the feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationItem {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub timestamp: u64,
    pub progress: Option<f32>,
    pub app_icon: Option<String>,
    pub image_path: Option<String>,
    pub actions: Vec<NotificationAction>,
    pub is_pinned: bool,
    pub is_snoozed: bool,
    pub snooze_until: Option<u64>,
    pub is_dismissed: bool,
    #[serde(default)]
    pub group: Option<String>,
}

impl NotificationItem {
    /// Create a new notification with default flags.
    #[must_use]
    pub fn new(
        id: u32,
        app_name: impl Into<String>,
        summary: impl Into<String>,
        body: impl Into<String>,
        urgency: Urgency,
    ) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self {
            id,
            app_name: app_name.into(),
            summary: summary.into(),
            body: body.into(),
            urgency,
            timestamp,
            progress: None,
            app_icon: None,
            image_path: None,
            actions: Vec::new(),
            is_pinned: urgency == Urgency::Critical,
            is_snoozed: false,
            snooze_until: None,
            is_dismissed: false,
            group: None,
        }
    }

    /// Set optional group name.
    #[must_use]
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Set optional preview image path.
    #[must_use]
    pub fn with_image(mut self, image_path: impl Into<String>) -> Self {
        self.image_path = Some(image_path.into());
        self
    }
}

/// Extracted actionable entity from notification text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractedEntity {
    OtpCode(String),
    Url(String),
    FilePath(String),
    HexColor(String),
}

/// File extensions that mark a word as a file path (OPT-8: hoisted `const`
/// so the table is not rebuilt per word per tick).
const KNOWN_FILE_EXTS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".pdf", ".txt", ".md", ".rs", ".py", ".js",
    ".ts", ".json", ".yaml", ".toml", ".sh", ".csv", ".log", ".zip", ".tar.gz",
];

/// URL scheme prefixes checked byte-first (OPT-8: `starts_with` before any
/// case folding; schemes are lowercase ASCII on the wire).
const URL_SCHEMES: &[&str] = &["https://", "http://", "mailto:", "ftp://", "ssh://"];

/// Extract actionable entities (OTP 2FA codes, URLs, file paths, hex color codes) from text.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn extract_entities(text: &str) -> Vec<ExtractedEntity> {
    let mut entities = Vec::new();

    // OPT-8: single `split_whitespace` pass feeds the URL + file-path
    // scanners (was 2× passes); the hex + OTP passes stay separate because
    // they need different trimming rules.
    let words: Vec<&str> = text.split_whitespace().collect();

    // 1. Scan for URLs (https://, http://, mailto:, ftp://, www., or bare domain with path)
    for word in &words {
        let trimmed = word.trim_matches(|c: char| {
            c == '('
                || c == ')'
                || c == '<'
                || c == '>'
                || c == '"'
                || c == '\''
                || c == ','
                || c == '.'
                || c == '`'
        });
        // OPT-8: byte-level scheme check first (no `to_lowercase` on the
        // hot path; schemes are matched case-sensitively as emitted).
        if URL_SCHEMES.iter().any(|s| trimmed.starts_with(s)) {
            entities.push(ExtractedEntity::Url(trimmed.to_string()));
        } else if trimmed.starts_with("www.")
            || (trimmed.contains('/')
                && (trimmed.contains(".com/")
                    || trimmed.contains(".org/")
                    || trimmed.contains(".net/")
                    || trimmed.contains(".io/")
                    || trimmed.contains(".dev/")
                    || trimmed.contains(".app/")
                    || trimmed.contains(".ai/")))
        {
            entities.push(ExtractedEntity::Url(format!("https://{trimmed}")));
        }
    }

    // 2. Scan for File Paths (/..., ~/..., or known file extensions)
    for word in &words {
        let trimmed = word.trim_matches(|c: char| {
            c == '('
                || c == ')'
                || c == '<'
                || c == '>'
                || c == '"'
                || c == '\''
                || c == ','
                || c == '`'
                || c == ':'
        });
        if trimmed.is_empty() {
            continue;
        }
        let is_abs_or_home = trimmed.starts_with('/') || trimmed.starts_with("~/");
        let has_known_ext = KNOWN_FILE_EXTS.iter().any(|ext| trimmed.ends_with(ext));

        if is_abs_or_home || has_known_ext {
            let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    format!("{home}/{rest}")
                } else {
                    trimmed.to_string()
                }
            } else {
                trimmed.to_string()
            };

            if !expanded.starts_with("http://")
                && !expanded.starts_with("https://")
                && !entities
                    .iter()
                    .any(|e| matches!(e, ExtractedEntity::FilePath(p) if p == &expanded))
            {
                entities.push(ExtractedEntity::FilePath(expanded));
            }
        }
    }

    // 3. Scan for Hex color codes (#RRGGBB)
    for word in &words {
        let trimmed = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '#');
        if trimmed.starts_with('#') && (trimmed.len() == 7 || trimmed.len() == 4) {
            let hex_part = &trimmed[1..];
            if hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
                entities.push(ExtractedEntity::HexColor(trimmed.to_string()));
            }
        }
    }

    // 4. Scan for OTP / 2FA verification codes (4 to 8 consecutive digits)
    // OPT-8: `words` is reused from above (was a second `split_whitespace`
    // collection); the previous-word context check runs byte-level first.
    for (i, &word) in words.iter().enumerate() {
        let clean = word.trim_matches(|c: char| !c.is_ascii_digit());
        if (4..=8).contains(&clean.len()) && clean.chars().all(|c| c.is_ascii_digit()) {
            let is_contextual = if i > 0 {
                contains_otp_context_ascii(words[i - 1])
            } else {
                false
            };
            if (is_contextual || clean.len() == 6)
                && !entities
                    .iter()
                    .any(|e| matches!(e, ExtractedEntity::OtpCode(c) if c == clean))
            {
                entities.push(ExtractedEntity::OtpCode(clean.to_string()));
            }
        }
    }

    entities
}

/// OPT-8 byte-level OTP context check: ASCII case-insensitive substring
/// search for `code`/`otp`/`pin`/`is`/`verification` without the
/// `to_lowercase` allocation (same predicate, no intermediate `String`).
fn contains_otp_context_ascii(prev: &str) -> bool {
    const NEEDLES: &[&[u8]] = &[b"code", b"otp", b"pin", b"is", b"verification"];
    let bytes = prev.as_bytes();
    for needle in NEEDLES {
        if bytes.len() < needle.len() {
            continue;
        }
        for window in bytes.windows(needle.len()) {
            let mut hit = true;
            for (a, b) in window.iter().zip(needle.iter()) {
                if a.to_ascii_lowercase() != *b {
                    hit = false;
                    break;
                }
            }
            if hit {
                return true;
            }
        }
    }
    false
}

/// Open a URL using `xdg-open` detached via `setsid -f`.
pub fn open_url(url: &str) {
    let _ = crate::spawn::spawn_detached(Path::new("setsid"), Path::new("xdg-open"), &[url]);
}

/// Open a file path using `xdg-open` detached via `setsid -f`.
pub fn open_file(path_str: &str) {
    let expanded = if let Some(rest) = path_str.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            format!("{home}/{rest}")
        } else {
            path_str.to_string()
        }
    } else {
        path_str.to_string()
    };
    let _ = crate::spawn::spawn_detached(Path::new("setsid"), Path::new("xdg-open"), &[&expanded]);
}

/// Managers that can highlight a file (`--select <file>`).
const SELECT_MANAGERS: &[&str] = &["dolphin", "nautilus"];
/// Managers that can open a folder (fallback when no `--select` candidate).
const BROWSE_MANAGERS: &[&str] = &["dolphin", "nautilus", "thunar", "pcmanfm", "nemo"];

/// First installed manager from `candidates` (`None` when none is on `PATH`).
fn first_manager<'a>(candidates: &'a [&str], path_env: &str) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|m| crate::tools::resolve_tool(m, path_env).is_some())
}

/// Split `path_str` into the expanded path and the folder to reveal.
///
/// Returns `(path, dir)`: when the path is a directory both are the directory,
/// otherwise `dir` is the non-empty parent. `None` for empty input,
/// `file://`-only input, and bare filenames (no parent to reveal).
fn reveal_targets(path_str: &str) -> Option<(PathBuf, PathBuf)> {
    let trimmed = path_str.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded = expand_home(trimmed);
    let stripped = expanded.strip_prefix("file://").unwrap_or(&expanded);
    if stripped.trim().is_empty() {
        return None;
    }
    let path = PathBuf::from(stripped);
    if path.is_dir() {
        return Some((path.clone(), path));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())?
        .to_path_buf();
    Some((path, parent))
}

/// Open containing folder of `path_str` in a GUI file manager.
///
/// Prefers `--select <file>` (Dolphin, Nautilus) so the file is highlighted
/// instead of just opening the folder, then a plain folder open in the first
/// installed manager, then `xdg-open` as a last resort. The `xdg-open`
/// fallback is last because a terminal emulator can claim `inode/directory`
/// (e.g. `kitty-open.desktop`), which would open a terminal instead of the
/// file manager. Silent no-op when nothing exists to reveal.
pub fn open_dir(path_str: &str) {
    let path_env = std::env::var("PATH").unwrap_or_default();
    open_dir_with_path(path_str, &path_env);
}

/// [`open_dir`] with an explicit `PATH` (test seam for manager lookup).
fn open_dir_with_path(path_str: &str, path_env: &str) {
    let Some((file, dir)) = reveal_targets(path_str) else {
        return;
    };
    if !dir.is_dir() {
        return;
    }
    if file.is_file() {
        let file_str = file.to_string_lossy();
        if let Some(mgr) = first_manager(SELECT_MANAGERS, path_env) {
            let _ = crate::spawn::spawn_detached(
                Path::new("setsid"),
                Path::new(mgr),
                &["--select", &file_str],
            );
            return;
        }
    }
    let dir_str = dir.to_string_lossy();
    if let Some(mgr) = first_manager(BROWSE_MANAGERS, path_env) {
        let _ = crate::spawn::spawn_detached(Path::new("setsid"), Path::new(mgr), &[&dir_str]);
        return;
    }
    let _ = crate::spawn::spawn_detached(
        Path::new("setsid"),
        Path::new("xdg-open"),
        &[dir_str.as_ref()],
    );
}

/// Expand a leading `~/` against `$HOME` (the same rule as [`open_file`]).
fn expand_home(path_str: &str) -> String {
    if let Some(rest) = path_str.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path_str.to_string()
}

/// Read up to the first 16 bytes of a file (`None` when it cannot be read).
fn read_head(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 16];
    let mut len = 0;
    while len < head.len() {
        match file.read(&mut head[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(_) => return None,
        }
    }
    head.truncate(len);
    Some(head)
}

/// Sniff the image MIME type of a file: magic bytes first (PNG, JPEG, GIF,
/// WebP, BMP), then the extension (covers SVG, which has no magic). Returns
/// `None` for unreadable files and non-images — any `image/*`, never text.
///
/// A readable file with an image extension but unrecognized contents still
/// resolves by extension: `wl-copy` just offers the bytes and the pasting
/// app decides what to do with them.
#[must_use]
pub fn image_mime(path: &Path) -> Option<&'static str> {
    let head = read_head(path)?;
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if head.len() >= 12 && head[..4] == *b"RIFF" && head[8..12] == *b"WEBP" {
        return Some("image/webp");
    }
    if head.starts_with(b"BM") {
        return Some("image/bmp");
    }
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("svg") => Some("image/svg+xml"),
        _ => None,
    }
}

/// Copy an image file's bytes to the Wayland clipboard with its MIME type
/// (`wl-copy --type <mime> < file`, `xclip -t <mime>` fallback).
///
/// Fire-and-forget like the drawer's text copy: `wl-copy` reads stdin fully,
/// forks, and serves from memory, so the paste survives the drawer exiting.
/// Returns whether the copy was handed off; `false` for missing files,
/// non-images, or a failed spawn (the caller reports it).
#[must_use]
pub fn copy_image_file(path_str: &str, mime: &str) -> bool {
    let expanded = expand_home(path_str);
    let path = Path::new(&expanded);
    if !path.is_file() || image_mime(path).is_none() {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    if {
        let mut cmd = Command::new("wl-copy");
        cmd.args(["--type", mime])
            .stdin(Stdio::from(file))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::spawn::spawn_and_reap(&mut cmd)
    }
    .is_ok()
    {
        return true;
    }
    let mut fallback = Command::new("xclip");
    fallback
        .args(["-selection", "clipboard", "-t", mime])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::spawn::spawn_and_reap(&mut fallback).is_ok()
}

/// Persistent trace logger for debugging notify focus and application launch execution steps.
pub fn log_notify_trace(msg: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/flex-notify-exec.log")
    {
        use std::io::Write as _;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = writeln!(file, "[{now}] {msg}");
    }
}

/// Dynamically match a notification target to an open Hyprland client window.
///
/// Uses a 3-tier matching algorithm:
/// 1. Exact match on class or initialClass (case-insensitive).
/// 2. Substring & normalized (alphanumeric-only) match on class, initialClass, or title.
/// 3. Explicit terminal / system app target rules.
///
/// Returns `None` if no matching client is open, allowing `open_application` to trigger
/// the detached launcher (`setsid -f`).
fn find_matching_client<'a>(
    clients: &'a [serde_json::Value],
    clean: &str,
) -> Option<(&'a str, Option<i64>)> {
    let clean = clean.trim().to_lowercase();
    if clean.is_empty() {
        return None;
    }
    let norm_clean: String = clean.chars().filter(|c| c.is_alphanumeric()).collect();

    log_notify_trace(&format!(
        "find_matching_client: searching for clean='{clean}', norm='{norm_clean}'"
    ));

    // Pass 1: Exact match on class or initialClass
    for client in clients {
        let class = client["class"].as_str().unwrap_or("").to_lowercase();
        let initial_class = client["initialClass"].as_str().unwrap_or("").to_lowercase();
        if class == clean || initial_class == clean {
            if let Some(addr) = client["address"].as_str() {
                let ws = client["workspace"]["id"].as_i64();
                log_notify_trace(&format!(
                    "[MATCH Pass 1 Exact] class='{class}', address='{addr}', ws={ws:?}"
                ));
                return Some((addr, ws));
            }
        }
    }

    // Pass 2: Substring & Normalized matching (e.g. google-chrome vs google chrome)
    for client in clients {
        let class = client["class"].as_str().unwrap_or("").to_lowercase();
        let initial_class = client["initialClass"].as_str().unwrap_or("").to_lowercase();
        let title = client["title"].as_str().unwrap_or("").to_lowercase();

        let norm_class: String = class.chars().filter(|c| c.is_alphanumeric()).collect();
        let norm_init: String = initial_class
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();

        let match_class = !class.is_empty()
            && (class.contains(&clean)
                || clean.contains(&class)
                || (!norm_clean.is_empty()
                    && (norm_class == norm_clean || norm_class.contains(&norm_clean))));
        let match_init = !initial_class.is_empty()
            && (initial_class.contains(&clean)
                || clean.contains(&initial_class)
                || (!norm_clean.is_empty()
                    && (norm_init == norm_clean || norm_init.contains(&norm_clean))));
        let match_title = !title.is_empty()
            && (title.contains(&clean)
                || clean.contains(&title)
                || (clean.contains("antigrav") && (title.contains("agy") || class == "kitty")));

        if match_class || match_init || match_title {
            if let Some(addr) = client["address"].as_str() {
                let ws = client["workspace"]["id"].as_i64();
                log_notify_trace(&format!(
                    "[MATCH Pass 2 Substring/Norm] class='{class}', title='{title}', address='{addr}', ws={ws:?}"
                ));
                return Some((addr, ws));
            }
        }
    }

    // Pass 3: Explicit Terminal / System app targets
    let is_terminal_or_agent = clean == "system"
        || clean == "packagekit"
        || clean == "terminal"
        || clean == "notify"
        || clean == "notify-send"
        || clean == "cargo"
        || clean == "bash"
        || clean == "zsh"
        || clean == "fish"
        || clean == "flex"
        || clean.contains("antigrav")
        || clean.contains("agy")
        || clean.contains("agent")
        || clean.contains("term");

    if is_terminal_or_agent {
        for client in clients {
            let class = client["class"].as_str().unwrap_or("").to_lowercase();
            let initial_class = client["initialClass"].as_str().unwrap_or("").to_lowercase();
            if class == "kitty" || initial_class == "kitty" {
                if let Some(addr) = client["address"].as_str() {
                    let ws = client["workspace"]["id"].as_i64();
                    log_notify_trace(&format!(
                        "[MATCH Pass 3 Terminal/System] class='{class}', address='{addr}', ws={ws:?}"
                    ));
                    return Some((addr, ws));
                }
            }
        }
    }

    log_notify_trace(&format!(
        "[MATCH None] No matching client window found for '{clean}'"
    ));
    None
}

/// Open or focus a desktop application and switch workspace.
pub fn open_application(app_name: &str) {
    let clean = app_name.trim().to_lowercase();
    log_notify_trace(&format!(
        "[OPEN_APP] app_name='{app_name}', clean='{clean}'"
    ));
    if clean.is_empty() || clean == "wiremix" {
        return;
    }

    // 0. Unset any active or global fullscreen lock across clients so window focus/launch is visible
    if let Ok(output) = Command::new("hyprctl")
        .args(["clients", "-j"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if output.status.success() {
            if let Ok(json_str) = String::from_utf8(output.stdout) {
                if let Ok(clients) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
                    for client in &clients {
                        let fs = client["fullscreen"].as_i64().unwrap_or(0);
                        let fs_client = client["fullscreenClient"].as_i64().unwrap_or(0);
                        if fs != 0 || fs_client != 0 {
                            if let Some(addr) = client["address"].as_str() {
                                log_notify_trace(&format!(
                                    "[FULLSCREEN] Unsetting fullscreen for client address='{addr}'"
                                ));
                                let lua_cmd = format!(
                                    "hl.dsp.window.fullscreen({{ action = \"unset\", window = \"address:{addr}\" }})"
                                );
                                let _ = Command::new("hyprctl")
                                    .args(["dispatch", &lua_cmd])
                                    .stdin(Stdio::null())
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null())
                                    .status();
                            }
                        }
                    }
                }
            }
        }
    }

    // 1. Query hyprctl clients -j to locate matching window and switch focus
    if let Ok(output) = Command::new("hyprctl")
        .args(["clients", "-j"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if output.status.success() {
            if let Ok(json_str) = String::from_utf8(output.stdout) {
                if let Ok(clients) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
                    if let Some((address, ws_id)) = find_matching_client(&clients, &clean) {
                        let address_owned = address.to_string();
                        log_notify_trace(&format!(
                            "[FOCUS_DISPATCH_SYNC] Dispatching window focus for address='{address_owned}', ws={ws_id:?}"
                        ));

                        let lua_win =
                            format!("hl.dsp.focus({{ window = \"address:{address_owned}\" }})");
                        let _ = Command::new("hyprctl")
                            .args(["dispatch", &lua_win])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();

                        std::thread::sleep(std::time::Duration::from_millis(50));
                        return;
                    }
                }
            }
        }
    }

    // 2. Fallback: try Lua class focus dispatch
    log_notify_trace(&format!(
        "[CLASS_FOCUS] Trying fallback Lua class focus dispatch for '{clean}'"
    ));
    let lua_class_cmd = format!("hl.dsp.focus({{ window = \"class:{clean}\" }})");
    let _ = Command::new("hyprctl")
        .args(["dispatch", &lua_class_cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // 3. Fallback: launch application detached via setsid -f
    let launch_target = if clean == "system" || clean == "packagekit" {
        "kitty"
    } else {
        &clean
    };

    log_notify_trace(&format!(
        "[LAUNCH_SPAWN] Spawning detached setsid -f '{launch_target}'"
    ));
    let _ = crate::spawn::spawn_detached(Path::new("setsid"), Path::new(launch_target), &[]);
}

/// Do Not Disturb state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DndState {
    #[default]
    Off,
    Indefinite,
    Timed {
        until: u64,
        total_secs: u64,
    },
}

impl DndState {
    #[must_use]
    pub fn is_active(self, now: u64) -> bool {
        match self {
            Self::Off => false,
            Self::Indefinite => true,
            Self::Timed { until, .. } => now < until,
        }
    }

    #[must_use]
    pub fn remaining_secs(self, now: u64) -> Option<u64> {
        match self {
            Self::Timed { until, .. } if now < until => Some(until - now),
            _ => None,
        }
    }

    #[must_use]
    pub fn fraction_remaining(self, now: u64) -> Option<f32> {
        match self {
            Self::Timed { until, total_secs } if total_secs > 0 && now < until => {
                let remaining = until.saturating_sub(now);
                #[allow(clippy::cast_precision_loss)]
                Some(((remaining as f32) / (total_secs as f32)).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }
}

/// MPRIS Now Playing track status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MprisTrack {
    pub player: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub position_secs: u64,
    pub length_secs: u64,
    pub is_playing: bool,
    pub is_live: bool,
    pub art_url: Option<String>,
}

/// Status of connected peripheral batteries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeripheralDevice {
    pub name: String,
    pub battery: u8,
    pub icon: String,
}

/// Quick controls shelf snapshot.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct QuickControls {
    pub dnd: DndState,
    pub wifi_ssid: Option<String>,
    pub bluetooth_status: Option<String>,
    pub peripherals: Vec<PeripheralDevice>,
    pub mpris: Option<MprisTrack>,
}

/// Parse a raw metadata line formatted by `playerctl`.
#[must_use]
pub fn parse_mpris_line(line: &str) -> Option<MprisTrack> {
    let parts: Vec<&str> = line.split(";;").collect();
    if parts.len() < 7 {
        return None;
    }

    let player = parts[0].trim().to_string();
    let status = parts[1].trim();
    let artist = parts[2].trim().to_string();
    let title = parts[3].trim().to_string();
    let album_raw = parts[4].trim();
    let album = if album_raw.is_empty() {
        None
    } else {
        Some(album_raw.to_string())
    };

    let pos_micro: u64 = parts[5].trim().parse().unwrap_or(0);
    let len_micro: u64 = parts[6].trim().parse().unwrap_or(0);

    let art_url = if parts.len() > 7 {
        let raw_art = parts[7].trim();
        if raw_art.is_empty() {
            None
        } else {
            Some(
                raw_art
                    .strip_prefix("file://")
                    .unwrap_or(raw_art)
                    .to_string(),
            )
        }
    } else {
        None
    };

    let position_secs = pos_micro / 1_000_000;
    // Chromium and Twitch send i64::MAX (9223372036854775807) for live streams.
    let is_live = len_micro > (86400 * 7 * 1_000_000) || len_micro == 0;
    let length_secs = if is_live { 0 } else { len_micro / 1_000_000 };

    if title.is_empty() && artist.is_empty() {
        return None;
    }

    Some(MprisTrack {
        player,
        title: if title.is_empty() {
            "Unknown Title".to_string()
        } else {
            title
        },
        artist: if artist.is_empty() {
            "Unknown Artist".to_string()
        } else {
            artist
        },
        album,
        position_secs,
        length_secs,
        is_playing: status.eq_ignore_ascii_case("Playing"),
        is_live,
        art_url,
    })
}

/// Query `playerctl` for the active media player metadata.
#[must_use]
pub fn probe_mpris() -> Option<MprisTrack> {
    let output = std::process::Command::new("playerctl")
        .args([
            "metadata",
            "--format",
            "{{playerName}};;{{status}};;{{artist}};;{{title}};;{{album}};;{{position}};;{{mpris:length}};;{{mpris:artUrl}}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }

    parse_mpris_line(first_line)
}

/// Update MPRIS status in `QuickControls` while preserving active DND state.
pub fn probe_quick_controls(controls: &mut QuickControls) {
    controls.mpris = probe_mpris();
}

/// Complete runtime state stored in `$XDG_RUNTIME_DIR/flex-notify.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NotifyState {
    pub notifications: Vec<NotificationItem>,
    pub controls: QuickControls,
    pub muted_apps: Vec<String>,
    pub priority_apps: Vec<String>,
}

/// Resolve the path to the runtime state JSON file.
#[must_use]
pub fn state_file_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("flex-notify.json")
    } else {
        std::env::temp_dir().join("flex-notify.json")
    }
}

/// Load state from JSON file, returning default state if missing or corrupted.
#[must_use]
pub fn load_state(path: Option<&Path>) -> NotifyState {
    let default_path = state_file_path();
    let file = path.unwrap_or(&default_path);

    if let Ok(content) = std::fs::read_to_string(file) {
        if let Ok(state) = serde_json::from_str::<NotifyState>(&content) {
            return state;
        }
    }
    NotifyState::default()
}

/// Save state to JSON file with atomic rename.
///
/// # Errors
/// Returns error if file cannot be created or serialized.
pub fn save_state(state: &NotifyState, path: Option<&Path>) -> anyhow::Result<()> {
    let default_path = state_file_path();
    let file = path.unwrap_or(&default_path);

    let json = serde_json::to_string_pretty(state)?;
    let tmp_file = file.with_extension("tmp");
    std::fs::write(&tmp_file, json)?;
    std::fs::rename(tmp_file, file)?;
    Ok(())
}

/// Append a new notification to the active state store.
///
/// # Errors
/// Returns error if state cannot be loaded or saved.
pub fn post_notification(item: NotificationItem, path: Option<&Path>) -> anyhow::Result<u32> {
    let mut state = load_state(path);
    let next_id = state.notifications.iter().map(|n| n.id).max().unwrap_or(0) + 1;
    let mut final_item = item;
    if final_item.id == 0 {
        final_item.id = next_id;
    }
    let assigned_id = final_item.id;
    state.notifications.push(final_item);
    save_state(&state, path)?;
    Ok(assigned_id)
}

/// Format relative time (e.g. "Just Now", "2m ago", "1h ago", "2d ago").
#[must_use]
pub fn format_relative_time(timestamp: u64, now: u64) -> String {
    if now <= timestamp || now - timestamp < 45 {
        return String::from("Just Now");
    }
    let diff = now - timestamp;
    if diff < 3600 {
        let mins = diff / 60;
        format!("{mins}m ago")
    } else if diff < 86400 {
        let hours = diff / 3600;
        format!("{hours}h ago")
    } else {
        let days = diff / 86400;
        format!("{days}d ago")
    }
}

/// Format duration in mm:ss.
#[must_use]
pub fn format_duration(seconds: u64) -> String {
    let mins = seconds / 60;
    let secs = seconds % 60;
    format!("{mins:02}:{secs:02}")
}
/// Parse urgency from a D-Bus variant hint.
#[must_use]
pub fn parse_hint_urgency(val: &zbus::zvariant::Value) -> Urgency {
    match val {
        zbus::zvariant::Value::U8(0)
        | zbus::zvariant::Value::I16(0)
        | zbus::zvariant::Value::I32(0)
        | zbus::zvariant::Value::I64(0) => Urgency::Low,
        zbus::zvariant::Value::U8(2)
        | zbus::zvariant::Value::I16(2)
        | zbus::zvariant::Value::I32(2)
        | zbus::zvariant::Value::I64(2) => Urgency::Critical,
        zbus::zvariant::Value::Str(s) => Urgency::parse(s.as_str()),
        zbus::zvariant::Value::Value(inner) => parse_hint_urgency(inner),
        _ => Urgency::Normal,
    }
}

/// Parse progress fraction (0.0 to 1.0) from a D-Bus variant hint.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn parse_hint_progress(val: &zbus::zvariant::Value) -> Option<f32> {
    let num = match val {
        zbus::zvariant::Value::U8(v) => Some(f64::from(*v)),
        zbus::zvariant::Value::I16(v) => Some(f64::from(*v)),
        zbus::zvariant::Value::I32(v) => Some(f64::from(*v)),
        zbus::zvariant::Value::I64(v) => Some(*v as f64),
        zbus::zvariant::Value::U32(v) => Some(f64::from(*v)),
        zbus::zvariant::Value::U64(v) => Some(*v as f64),
        zbus::zvariant::Value::F64(v) => Some(*v),
        zbus::zvariant::Value::Value(inner) => return parse_hint_progress(inner),
        _ => None,
    };
    num.map(|n| {
        if n > 1.0 {
            ((n / 100.0) as f32).clamp(0.0, 1.0)
        } else {
            (n as f32).clamp(0.0, 1.0)
        }
    })
}

/// Play a subtle notification audio cue based on urgency, respecting DND mode.
pub fn play_notification_sound(urgency: Urgency, dnd_active: bool) {
    play_notification_sound_with_path(urgency, dnd_active, None);
}

/// [`play_notification_sound`] with an explicit `PATH` override (test seam).
///
/// `None` inherits the ambient `PATH`; `Some` shadows it, so tests exercise
/// the player-fallback spawn against stub players without touching the
/// process env or any live audio tool.
pub fn play_notification_sound_with_path(
    urgency: Urgency,
    dnd_active: bool,
    path_override: Option<&str>,
) {
    if dnd_active {
        return;
    }

    let sound_paths = match urgency {
        Urgency::Critical => [
            "/usr/share/sounds/freedesktop/stereo/dialog-warning.oga",
            "/usr/share/sounds/freedesktop/stereo/bell.oga",
            "/usr/share/sounds/freedesktop/stereo/dialog-error.oga",
        ],
        Urgency::Normal | Urgency::Low => [
            "/usr/share/sounds/freedesktop/stereo/message.oga",
            "/usr/share/sounds/freedesktop/stereo/message-new-instant.oga",
            "/usr/share/sounds/freedesktop/stereo/dialog-information.oga",
        ],
    };

    let target_file = sound_paths.iter().find(|p| Path::new(p).exists());
    let Some(&chosen_sound) = target_file else {
        return;
    };

    // Try players in order of preference: pw-play, paplay, canberra-gtk-play, aplay
    let players: &[(&str, &[&str])] = &[
        ("pw-play", &[chosen_sound]),
        ("paplay", &[chosen_sound]),
        ("canberra-gtk-play", &["-f", chosen_sound]),
        ("aplay", &[chosen_sound]),
    ];

    for &(prog, args) in players {
        let bin = match path_override {
            Some(path_env) => match crate::tools::resolve_tool(prog, path_env) {
                Some(path) => path,
                None => continue,
            },
            None => PathBuf::from(prog),
        };
        let mut cmd = Command::new(&bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Reaped on a waiter thread: dropping the `Child` here would leak a
        // zombie for as long as the (long-lived) daemon process runs.
        if crate::spawn::spawn_and_reap(&mut cmd).is_ok() {
            break;
        }
    }
}

/// D-Bus Notification Service implementation of `org.freedesktop.Notifications`.
pub struct NotificationServer {
    pub state_path: Option<PathBuf>,
}

impl NotificationServer {
    #[must_use]
    pub fn new(state_path: Option<PathBuf>) -> Self {
        Self { state_path }
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationServer {
    /// Capabilities supported by the notification server.
    #[must_use]
    #[allow(clippy::unused_self)]
    pub fn get_capabilities(&self) -> Vec<String> {
        vec![
            "actions".to_string(),
            "body".to_string(),
            "body-markup".to_string(),
            "persistence".to_string(),
            "sound".to_string(),
        ]
    }

    /// Server information.
    #[must_use]
    #[allow(clippy::unused_self)]
    pub fn get_server_information(
        &self,
    ) -> (&'static str, &'static str, &'static str, &'static str) {
        ("flex-notify", "flex", env!("CARGO_PKG_VERSION"), "1.2")
    }

    /// Close notification by ID.
    ///
    /// # Errors
    /// Returns error if D-Bus signal emission fails.
    pub async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let mut state = load_state(self.state_path.as_deref());
        if let Some(item) = state.notifications.iter_mut().find(|n| n.id == id) {
            item.is_dismissed = true;
            let _ = save_state(&state, self.state_path.as_deref());
        }
        let _ = Self::notification_closed(&emitter, id, 3).await;
        Ok(())
    }

    /// Process and store incoming notification.
    #[allow(
        clippy::too_many_arguments,
        clippy::needless_pass_by_value,
        clippy::too_many_lines
    )]
    pub fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: std::collections::HashMap<String, zbus::zvariant::Value>,
        expire_timeout: i32,
    ) -> u32 {
        let _ = expire_timeout;
        let urgency = hints
            .get("urgency")
            .map_or(Urgency::Normal, parse_hint_urgency);
        let progress = hints
            .get("value")
            .or_else(|| hints.get("progress"))
            .and_then(parse_hint_progress);

        let image_path = hints
            .get("image-path")
            .or_else(|| hints.get("image_path"))
            .or_else(|| hints.get("image-data"))
            .and_then(|v| match v {
                zbus::zvariant::Value::Str(s) => {
                    let path_str = s.as_str().strip_prefix("file://").unwrap_or(s.as_str());
                    if !path_str.is_empty() && std::path::Path::new(path_str).is_file() {
                        Some(path_str.to_string())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .or_else(|| {
                let icon_str = app_icon.strip_prefix("file://").unwrap_or(&app_icon);
                if !icon_str.is_empty() && std::path::Path::new(icon_str).is_file() {
                    Some(icon_str.to_string())
                } else {
                    None
                }
            });

        let group = hints
            .get("group")
            .or_else(|| hints.get("category"))
            .or_else(|| hints.get("desktop-entry"))
            .and_then(|v| match v {
                zbus::zvariant::Value::Str(s) => Some(s.to_string()),
                _ => None,
            });

        let mut parsed_actions = Vec::new();
        for chunk in actions.chunks(2) {
            if chunk.len() == 2 {
                parsed_actions.push(NotificationAction {
                    id: chunk[0].clone(),
                    title: chunk[1].clone(),
                });
            }
        }

        let mut state = load_state(self.state_path.as_deref());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let target_id = if replaces_id > 0 {
            replaces_id
        } else {
            state.notifications.iter().map(|n| n.id).max().unwrap_or(0) + 1
        };

        let is_app_muted = state
            .muted_apps
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&app_name));

        if let Some(existing) = state.notifications.iter_mut().find(|n| n.id == target_id) {
            existing.app_name = app_name;
            existing.summary = summary;
            existing.body = body;
            existing.urgency = urgency;
            existing.timestamp = now;
            existing.progress = progress;
            if !app_icon.is_empty() {
                existing.app_icon = Some(app_icon);
            }
            if image_path.is_some() {
                existing.image_path = image_path;
            }
            if group.is_some() {
                existing.group = group;
            }
            if !parsed_actions.is_empty() {
                existing.actions = parsed_actions;
            }
            existing.is_dismissed = false;
        } else {
            let mut item = NotificationItem::new(target_id, app_name, summary, body, urgency);
            item.timestamp = now;
            item.progress = progress;
            if !app_icon.is_empty() {
                item.app_icon = Some(app_icon);
            }
            item.image_path = image_path;
            item.group = group;
            item.actions = parsed_actions;
            state.notifications.push(item);
        }

        let is_dnd = state.controls.dnd.is_active(now);
        let _ = save_state(&state, self.state_path.as_deref());

        let suppress_sound = hints
            .get("suppress-sound")
            .and_then(|v| match v {
                zbus::zvariant::Value::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        if !is_dnd && !is_app_muted && !suppress_sound {
            play_notification_sound(urgency, false);
            spawn_toast(target_id, self.state_path.as_deref());
        }

        target_id
    }

    /// Emitted when a notification is closed.
    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    /// Emitted when an action button is clicked on a notification.
    #[zbus(signal)]
    async fn action_invoked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

/// Format the header line of a notification toast card.
/// Total target width: `target_right_col` display columns (excluding right margin).
/// Left margin: 2 spaces. Icon width: 1 cell. Spacing: 2 spaces.
/// App name & summary separated by " · ".
/// Relative timestamp right-aligned to `target_right_col`.
#[must_use]
pub fn format_toast_header(
    icon: &str,
    app_name: &str,
    summary: &str,
    rel: &str,
    target_right_col: usize,
) -> String {
    use unicode_width::UnicodeWidthStr as _;

    let prefix = format!("  {icon}  ");
    let prefix_width = prefix.width();
    let rel_width = rel.width();
    let sep = " · ";
    let sep_width = sep.width();
    let app_width = app_name.width();

    let max_text_width = if target_right_col > prefix_width + rel_width + 1 {
        target_right_col - prefix_width - rel_width - 1
    } else {
        10
    };

    let summary_avail = if max_text_width > app_width + sep_width {
        max_text_width - app_width - sep_width
    } else {
        0
    };

    let truncated_summary = if summary_avail == 0 {
        String::new()
    } else if summary.width() > summary_avail {
        let mut s = String::new();
        let mut w = 0;
        for c in summary.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw + 1 > summary_avail {
                s.push('…');
                break;
            }
            s.push(c);
            w += cw;
        }
        s
    } else {
        summary.to_string()
    };

    let left_combined_width = if truncated_summary.is_empty() {
        app_width
    } else {
        app_width + sep_width + truncated_summary.width()
    };

    let fill_spaces = if target_right_col > prefix_width + left_combined_width + rel_width {
        target_right_col - prefix_width - left_combined_width - rel_width
    } else {
        1
    };

    let spaces_str = " ".repeat(fill_spaces);

    if truncated_summary.is_empty() {
        format!("{prefix}\x1b[1m{app_name}\x1b[0m{spaces_str}\x1b[2m{rel}\x1b[0m")
    } else {
        format!(
            "{prefix}\x1b[1m{app_name}\x1b[0m · {truncated_summary}{spaces_str}\x1b[2m{rel}\x1b[0m"
        )
    }
}

/// Format a modern boxed Wiremix notification toast card with rounded borders.
/// Total width: `width` columns (default 50).
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn format_toast_card(
    item: &NotificationItem,
    rel_time: &str,
    width: usize,
    _timeout_secs: u64,
) -> String {
    use unicode_width::UnicodeWidthStr as _;

    let target_width = width.max(36);
    let inner_width = target_width.saturating_sub(4); // 2 spaces padding + 2 border chars

    let (border_ansi, icon, icon_ansi) = match item.urgency {
        Urgency::Critical => ("\x1b[1;31m", "󰀦", "\x1b[1;31m"),
        Urgency::Normal => ("\x1b[90m", "󰂚", "\x1b[37m"),
        Urgency::Low => ("\x1b[2;37m", "󰂞", "\x1b[2;37m"),
    };

    let reset = "\x1b[0m";
    let bold = "\x1b[1m";
    let dim = "\x1b[2m";
    let yellow_bold = "\x1b[1;33m";
    let cyan = "\x1b[36m";

    let left_width = 3 + 1 + 1 + item.app_name.width() + 1; // "╭─ " + icon(1) + " " + app + " "
    let crit_badge = if item.urgency == Urgency::Critical {
        " [CRITICAL]"
    } else {
        ""
    };

    let right_head_raw = format!("{crit_badge} {rel_time} ─╮");
    let right_width = right_head_raw.width();

    let fill_len = target_width.saturating_sub(left_width + right_width);
    let fill_dashes = "─".repeat(fill_len);

    let top_line = format!(
        "{border_ansi}╭─ {reset}{icon_ansi}{icon}{reset} {bold}{}{reset} {border_ansi}{fill_dashes}{reset}{yellow_bold}{crit_badge}{reset} {dim}{rel_time}{reset} {border_ansi}─╮{reset}",
        item.app_name
    );

    let make_content_row = |content_spans: &str, content_len: usize| -> String {
        let pad_len = inner_width.saturating_sub(content_len);
        let pad = " ".repeat(pad_len);
        format!("{border_ansi}│{reset} {content_spans}{pad} {border_ansi}│{reset}")
    };

    let mut lines = Vec::new();
    lines.push(top_line);

    let summary_text = item.summary.replace('\n', " ");
    let mut sum_truncated = String::new();
    let mut sum_w = 0;
    for c in summary_text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if sum_w + cw > inner_width {
            break;
        }
        sum_truncated.push(c);
        sum_w += cw;
    }
    let summary_formatted = format!("{bold}{sum_truncated}{reset}");
    lines.push(make_content_row(&summary_formatted, sum_w));

    if !item.body.is_empty() {
        let body_clean = item.body.replace('\n', " ");
        let mut body_truncated = String::new();
        let mut body_w = 0;
        for c in body_clean.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if body_w + cw + 1 > inner_width {
                body_truncated.push('…');
                body_w += 1;
                break;
            }
            body_truncated.push(c);
            body_w += cw;
        }
        let body_formatted = format!("{dim}{body_truncated}{reset}");
        lines.push(make_content_row(&body_formatted, body_w));
    }

    if let Some(p) = item.progress {
        let pct = (p.clamp(0.0, 1.0) * 100.0) as usize;
        let bar_max = inner_width.saturating_sub(17);
        let fill = (bar_max * pct) / 100;
        let empty = bar_max.saturating_sub(fill);
        let filled_bar = "█".repeat(fill);
        let empty_bar = "░".repeat(empty);

        let prog_str =
            format!("Progress: [{cyan}{filled_bar}{reset}{dim}{empty_bar}{reset}] {pct:>3}%");
        let raw_prog = format!("Progress: [{filled_bar}{empty_bar}] {pct:>3}%");
        lines.push(make_content_row(&prog_str, raw_prog.width()));
    }

    if !item.actions.is_empty() {
        let mut action_spans = format!("{bold}Actions:{reset}");
        let mut raw_actions = String::from("Actions:");
        let mut act_w = 8;
        for (idx, action) in item.actions.iter().enumerate().take(3) {
            let num = idx + 1;
            let act_text = format!(" {yellow_bold}[{num}]{reset} {} ", action.title);
            let raw_text = format!(" [{num}] {} ", action.title);
            let act_len = raw_text.width();
            if act_w + act_len > inner_width {
                break;
            }
            action_spans.push_str(&act_text);
            raw_actions.push_str(&raw_text);
            act_w += act_len;
        }
        lines.push(make_content_row(&action_spans, raw_actions.width()));
    }

    let bot_dash = "─".repeat(target_width.saturating_sub(2));
    lines.push(format!("{border_ansi}╰{bot_dash}╯{reset}"));

    lines.join("\n")
}

/// Spawn a transient toast overlay for a newly-arrived notification.
///
/// Launches `kitty --class flex-notify-toast -o font_size=11 -o remember_window_size=no
/// -o initial_window_width=50c -o initial_window_height=6c -o window_padding_width=0 -o window_padding_height=0 -e flex-notify toast <id>` detached.
pub fn spawn_toast(id: u32, state_path: Option<&Path>) {
    spawn_toast_with_path(id, state_path, None);
}

/// [`spawn_toast`] with an explicit `PATH` override (test seam).
///
/// `None` inherits the ambient `PATH`; `Some` shadows it (including inside
/// the `sh -c` body, via an exported `PATH`) so tests run stub `sh`/`kitty`
/// scripts without touching the process env or opening windows.
pub fn spawn_toast_with_path(id: u32, state_path: Option<&Path>, path_override: Option<&str>) {
    let state_arg =
        state_path.map_or_else(String::new, |p| format!(" --state-file '{}'", p.display()));
    let cmd = format!(
        "kitty --class flex-notify-toast -o font_size=11 -o remember_window_size=no -o initial_window_width=50c -o initial_window_height=6c -o window_padding_width=0 -o window_padding_height=0 -e flex-notify{state_arg} toast {id}"
    );
    // Double-fork via `sh -c '… &'`: the grandchild is reparented to init so
    // no daemon FDs (including the zbus socket) are inherited.
    // Do NOT redirect kitty's stdio: kitty opens its own PTY for the child
    // (`-e flex-notify toast <id>`), so stdin/stdout of that child are the
    // PTY — not /dev/null.  Redirecting them here would break the ANSI render
    // (stdout→null = blank window) and the keypress poll (stdin→null = stty
    // fails + instant-exit for Normal or spin-forever for Critical).
    let sh_bin = match path_override {
        Some(path_env) => match crate::tools::resolve_tool("sh", path_env) {
            Some(path) => path,
            None => return,
        },
        None => PathBuf::from("sh"),
    };
    let mut toast = Command::new(&sh_bin);
    toast
        .args(["-c", &format!("{cmd} &")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path_env) = path_override {
        toast.env("PATH", path_env);
    }
    // Reaped on a waiter thread: the intermediate `sh` exits immediately and
    // would otherwise linger as a zombie under the long-lived daemon.
    let _ = crate::spawn::spawn_and_reap(&mut toast);
}

/// Run the D-Bus Notification daemon.
///
/// # Errors
/// Returns error if D-Bus connection or name acquisition fails.
pub async fn run_daemon(state_path: Option<PathBuf>, _replace: bool) -> anyhow::Result<()> {
    let server = NotificationServer::new(state_path);
    let _conn = zbus::connection::Builder::session()?
        .name("org.freedesktop.Notifications")?
        .serve_at("/org/freedesktop/Notifications", server)?
        .build()
        .await?;

    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_extractor_detects_otp_urls_and_colors() {
        let text = "Your GitHub verification code is 849201. Please visit https://github.com/login and use theme #7aa2f7.";
        let entities = extract_entities(text);

        assert_eq!(
            entities,
            vec![
                ExtractedEntity::Url("https://github.com/login".to_string()),
                ExtractedEntity::HexColor("#7aa2f7".to_string()),
                ExtractedEntity::OtpCode("849201".to_string()),
            ]
        );
    }

    #[test]
    fn relative_time_formatting() {
        assert_eq!(format_relative_time(1000, 1020), "Just Now");
        assert_eq!(format_relative_time(1000, 1120), "2m ago");
        assert_eq!(format_relative_time(1000, 4600), "1h ago");
        assert_eq!(format_relative_time(1000, 90000), "1d ago");
    }

    #[test]
    fn dnd_state_calculations() {
        let dnd = DndState::Timed {
            until: 2000,
            total_secs: 1000,
        };
        assert!(dnd.is_active(1500));
        assert!(!dnd.is_active(2500));
        assert_eq!(dnd.remaining_secs(1500), Some(500));
        assert_eq!(dnd.fraction_remaining(1500), Some(0.5));
    }

    #[test]
    fn state_save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("flex-notify-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-state.json");

        let mut state = NotifyState::default();
        state.notifications.push(NotificationItem::new(
            1,
            "Discord",
            "#dev-team",
            "Message body",
            Urgency::Normal,
        ));
        state.controls.dnd = DndState::Indefinite;

        save_state(&state, Some(&path)).expect("saved");
        let loaded = load_state(Some(&path));

        assert_eq!(loaded.notifications.len(), 1);
        assert_eq!(loaded.notifications[0].app_name, "Discord");
        assert_eq!(loaded.controls.dnd, DndState::Indefinite);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_hint_urgency_variants() {
        assert_eq!(
            parse_hint_urgency(&zbus::zvariant::Value::U8(0)),
            Urgency::Low
        );
        assert_eq!(
            parse_hint_urgency(&zbus::zvariant::Value::U8(1)),
            Urgency::Normal
        );
        assert_eq!(
            parse_hint_urgency(&zbus::zvariant::Value::U8(2)),
            Urgency::Critical
        );
        assert_eq!(
            parse_hint_urgency(&zbus::zvariant::Value::Str("critical".into())),
            Urgency::Critical
        );
        assert_eq!(
            parse_hint_urgency(&zbus::zvariant::Value::I32(2)),
            Urgency::Critical
        );
    }

    #[test]
    fn parse_hint_progress_percentage_and_fraction() {
        let p1 = parse_hint_progress(&zbus::zvariant::Value::U8(75));
        assert_eq!(p1, Some(0.75));

        let p2 = parse_hint_progress(&zbus::zvariant::Value::F64(0.42));
        assert_eq!(p2, Some(0.42));

        let p3 = parse_hint_progress(&zbus::zvariant::Value::I32(150));
        assert_eq!(p3, Some(1.0));
    }

    #[test]
    fn notification_server_capabilities_and_info() {
        let srv = NotificationServer::new(None);
        let caps = srv.get_capabilities();
        assert!(caps.contains(&"actions".to_string()));
        assert!(caps.contains(&"body".to_string()));
        assert!(caps.contains(&"sound".to_string()));

        let info = srv.get_server_information();
        assert_eq!(info.0, "flex-notify");
        assert_eq!(info.1, "flex");
    }

    #[test]
    fn notification_server_ingests_and_replaces_notifications() {
        let dir = std::env::temp_dir().join(format!("flex-notify-srv-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("srv-state.json");

        let srv = NotificationServer::new(Some(path.clone()));

        let mut hints = std::collections::HashMap::new();
        hints.insert("urgency".to_string(), zbus::zvariant::Value::U8(2));
        hints.insert("value".to_string(), zbus::zvariant::Value::U8(50));

        let id = srv.notify(
            "Cargo".to_string(),
            0,
            "cargo-icon".to_string(),
            "Build Finished".to_string(),
            "Compilation completed in 2.3s".to_string(),
            vec!["open".to_string(), "Open Artifacts".to_string()],
            hints,
            -1,
        );

        assert_eq!(id, 1);
        let s1 = load_state(Some(&path));
        assert_eq!(s1.notifications.len(), 1);
        assert_eq!(s1.notifications[0].app_name, "Cargo");
        assert_eq!(s1.notifications[0].urgency, Urgency::Critical);
        assert_eq!(s1.notifications[0].progress, Some(0.5));
        assert_eq!(s1.notifications[0].actions.len(), 1);
        assert_eq!(s1.notifications[0].actions[0].id, "open");

        let mut hints_update = std::collections::HashMap::new();
        hints_update.insert("value".to_string(), zbus::zvariant::Value::U8(100));
        let id2 = srv.notify(
            "Cargo".to_string(),
            1,
            "cargo-icon".to_string(),
            "Build Complete".to_string(),
            "All targets finished".to_string(),
            vec![],
            hints_update,
            -1,
        );

        assert_eq!(id2, 1);
        let s2 = load_state(Some(&path));
        assert_eq!(s2.notifications.len(), 1);
        assert_eq!(s2.notifications[0].summary, "Build Complete");
        assert_eq!(s2.notifications[0].progress, Some(1.0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_mime_sniffs_magic_bytes() {
        let dir = std::env::temp_dir().join(format!("flex-notify-img-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let fixture = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).expect("fixture");
            path
        };

        assert_eq!(
            image_mime(&fixture("a.bin", b"\x89PNG\r\n\x1a\nrest")),
            Some("image/png")
        );
        assert_eq!(
            image_mime(&fixture("b.bin", &[0xFF, 0xD8, 0xFF, 0xE0])),
            Some("image/jpeg")
        );
        assert_eq!(
            image_mime(&fixture("c.bin", b"GIF89a...")),
            Some("image/gif")
        );
        assert_eq!(
            image_mime(&fixture("d.bin", b"RIFF\x00\x00\x00\x00WEBP")),
            Some("image/webp")
        );
        assert_eq!(image_mime(&fixture("e.bin", b"BM....")), Some("image/bmp"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_mime_falls_back_to_extension_and_rejects_text() {
        let dir = std::env::temp_dir().join(format!("flex-notify-ext-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let fixture = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).expect("fixture");
            path
        };

        // SVG has no magic: extension-only.
        assert_eq!(
            image_mime(&fixture("v.svg", b"<svg></svg>")),
            Some("image/svg+xml")
        );
        // Readable file, image extension, unrecognized contents: extension wins.
        assert_eq!(
            image_mime(&fixture("w.png", b"not really a png")),
            Some("image/png")
        );
        // Plain text is never an image.
        assert_eq!(image_mime(&fixture("t.txt", b"hello")), None);
        assert_eq!(image_mime(&fixture("noext", b"hello")), None);
        // Missing files resolve to nothing (no target emitted).
        assert_eq!(image_mime(&dir.join("ghost.png")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notify_ingests_image_path_hint_for_existing_files() {
        let dir = std::env::temp_dir().join(format!("flex-notify-hint-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hint-state.json");
        let shot = dir.join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\npixels").expect("fixture");

        let srv = NotificationServer::new(Some(path.clone()));
        let mut hints = std::collections::HashMap::new();
        hints.insert(
            "image-path".to_string(),
            zbus::zvariant::Value::Str(format!("file://{}", shot.display()).into()),
        );
        srv.notify(
            "Shots".to_string(),
            0,
            String::new(),
            "Screenshot saved".to_string(),
            shot.display().to_string(),
            vec![],
            hints,
            -1,
        );
        let loaded = load_state(Some(&path));
        assert_eq!(
            loaded.notifications[0].image_path.as_deref(),
            Some(shot.to_str().expect("utf8 path"))
        );

        // Missing file: no image attached (the drawer's preview/target gate).
        let mut bad_hints = std::collections::HashMap::new();
        bad_hints.insert(
            "image-path".to_string(),
            zbus::zvariant::Value::Str("/nonexistent/ghost.png".into()),
        );
        srv.notify(
            "Shots".to_string(),
            0,
            String::new(),
            "Screenshot saved".to_string(),
            "gone".to_string(),
            vec![],
            bad_hints,
            -1,
        );
        let reloaded = load_state(Some(&path));
        assert_eq!(reloaded.notifications.len(), 2);
        assert_eq!(reloaded.notifications[1].image_path, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_image_file_refuses_missing_and_non_image() {
        assert!(!copy_image_file("/nonexistent/ghost.png", "image/png"));
        let dir = std::env::temp_dir().join(format!("flex-notify-ref-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let text = dir.join("note.txt");
        std::fs::write(&text, b"hello").expect("fixture");
        assert!(!copy_image_file(&text.to_string_lossy(), "image/png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn containing_dir_resolves_parent_and_guards_empty() {
        let dir = std::env::temp_dir().join(format!("flex-notify-dir-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sub = dir.join("sub");
        let _ = std::fs::create_dir_all(&sub);

        let dir_of = |p: &str| reveal_targets(p).map(|(_, d)| d);
        // Missing file resolves to its parent.
        let file = dir.join("report.txt");
        assert_eq!(dir_of(&file.to_string_lossy()), Some(dir.clone()));
        // file:// prefix is stripped before resolution.
        assert_eq!(
            dir_of(&format!("file://{}", file.display())),
            Some(dir.clone())
        );
        // Existing directory reveals itself.
        assert_eq!(dir_of(&sub.to_string_lossy()), Some(sub.clone()));

        // Silent no-op cases: no parent to reveal, empty input.
        assert_eq!(dir_of("note.txt"), None);
        assert_eq!(dir_of(""), None);
        assert_eq!(dir_of("   "), None);
        assert_eq!(dir_of("file://"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reveal_targets_splits_file_and_dir() {
        let dir = std::env::temp_dir().join(format!("flex-notify-rev-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let file = dir.join("report.txt");
        let (path, parent) = reveal_targets(&file.to_string_lossy()).expect("targets");
        assert_eq!(path, file);
        assert_eq!(parent, dir);

        let (dpath, revealed) = reveal_targets(&dir.to_string_lossy()).expect("dir targets");
        assert_eq!(dpath, dir);
        assert_eq!(revealed, dir);

        assert_eq!(reveal_targets("note.txt"), None);
        assert_eq!(reveal_targets(""), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_manager_prefers_stub_path_order() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("flex-notify-mgr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let stub = dir.join("dolphin");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path_env = dir.to_string_lossy().into_owned();

        assert_eq!(
            first_manager(&["dolphin", "nautilus"], &path_env),
            Some("dolphin")
        );
        assert_eq!(first_manager(&["nautilus", "nemo"], &path_env), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_matching_client_dynamic_passes_work_as_expected() {
        let clients = serde_json::json!([
            {
                "address": "0x1111",
                "class": "steam_app_2694490",
                "initialClass": "steam_app_2694490",
                "title": "Path of Exile 2",
                "focusHistoryID": 0,
                "workspace": { "id": 1 }
            },
            {
                "address": "0x2222",
                "class": "brave-browser",
                "initialClass": "brave-browser",
                "title": "GitHub - gom3az/flex",
                "focusHistoryID": 2,
                "workspace": { "id": 2 }
            },
            {
                "address": "0x3333",
                "class": "kitty",
                "initialClass": "kitty",
                "title": "agy - Antigravity Assistant",
                "focusHistoryID": 1,
                "workspace": { "id": 3 }
            }
        ]);
        let clients_arr = clients.as_array().unwrap();

        // 1. Exact match on class
        let res1 = find_matching_client(clients_arr, "brave-browser");
        assert_eq!(res1, Some(("0x2222", Some(2))));

        // 2. Substring match on title
        let res2 = find_matching_client(clients_arr, "path of exile");
        assert_eq!(res2, Some(("0x1111", Some(1))));

        // 3. Normalized matching (spaces/hyphens e.g. "brave browser" -> "brave-browser")
        let res3 = find_matching_client(clients_arr, "brave browser");
        assert_eq!(res3, Some(("0x2222", Some(2))));

        // 4. Token match for agent / terminal target
        let res4 = find_matching_client(clients_arr, "Antigravity Ready");
        assert_eq!(res4, Some(("0x3333", Some(3))));

        // 5. Unknown app returns None so open_application can trigger detached launch (setsid -f)
        let res5 = find_matching_client(clients_arr, "UnknownApp");
        assert_eq!(res5, None);
    }
}
