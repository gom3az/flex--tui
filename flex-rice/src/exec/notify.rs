//! Notification center engine, persistence, entity extractors, and quick control probes.
//!
//! Provides safe, high-performance state management for `flex-notify`, including:
//! - In-flight progress updates and notification threading
//! - Regex-free / robust entity extraction (2FA/OTP codes, URLs, hex colors)
//! - Quick system toggles (DND presets, Night Light, Caffeine, Mic mute)
//! - MPRIS track info and playback commands
//! - Persistent runtime store at `$XDG_RUNTIME_DIR/flex-notify.json`

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::providers::launch;

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
    /// Absolute end of the sender's requested lifetime (`expire_timeout`).
    ///
    /// `None` means "keep until dismissed", flex's server default; critical
    /// alerts never expire.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// When the notification was dismissed, used only by archive retention.
    /// Entries archived before this field existed leave it unset.
    #[serde(default)]
    pub archived_at: Option<u64>,
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
            expires_at: None,
            archived_at: None,
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

    /// Classify this notification's sender label (see [`Sender`]).
    #[must_use]
    pub fn sender(&self) -> Sender {
        Sender::classify(&self.app_name, self.app_icon.as_deref())
    }

    /// Whether this notification belongs in the active feed at `now`.
    ///
    /// The single predicate behind the feed rows, the Clear All counter and
    /// the Waybar badge: a snooze hides the notification only until it
    /// elapses, and an elapsed sender timeout hides it for good.
    #[must_use]
    pub fn is_active(&self, now: u64) -> bool {
        if self.is_dismissed {
            return false;
        }
        if self.is_snoozed && self.snooze_until.is_none_or(|until| now < until) {
            return false;
        }
        self.expires_at.is_none_or(|at| now < at)
    }

    /// Whether this notification belongs in the archive: dismissed by the
    /// user, or retired by its sender's timeout.
    #[must_use]
    pub fn is_archived(&self, now: u64) -> bool {
        self.is_dismissed || self.expires_at.is_some_and(|at| now >= at)
    }

    /// Move this notification into the archive at `now`.
    ///
    /// Idempotent: the first dismissal time is what retention measures, so a
    /// later clear-all cannot extend an old entry's life.
    pub fn archive(&mut self, now: u64) {
        if !self.is_dismissed {
            self.is_dismissed = true;
            self.archived_at = Some(now);
        }
    }

    /// Bring an archived notification back to the feed.
    pub fn restore(&mut self) {
        self.is_dismissed = false;
        self.archived_at = None;
        // A restored sender-expired notification must actually come back.
        self.expires_at = None;
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

/// A user-facing failure from an Open action.
///
/// Returned instead of shown immediately so the caller can finish its own
/// bookkeeping first and then call [`report_failure`]. See [`report_failure`]
/// for why the feedback is a transient toast instead of a stored
/// notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Notification summary (`File not found`, …).
    pub summary: &'static str,
    /// Notification body (the path, or a short reason).
    pub detail: String,
}

impl Failure {
    /// Build a failure with a `String`-able detail.
    #[must_use]
    pub fn new(summary: &'static str, detail: impl Into<String>) -> Self {
        Self {
            summary,
            detail: detail.into(),
        }
    }
}

/// Open a file path using `xdg-open` detached via `setsid -f`.
///
/// A path captured from an older notification may be gone by the time the
/// user taps `Open File`; the detached `xdg-open` would then fail invisibly
/// (and the drawer exits right after, taking its stderr with it). Returns a
/// [`Failure`] for the caller to report once its state is saved.
#[must_use]
pub fn open_file(path_str: &str) -> Option<Failure> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    open_file_with(path_str, &path_env)
}

/// [`open_file`] with an explicit `PATH` (test seam).
fn open_file_with(path_str: &str, path_env: &str) -> Option<Failure> {
    let expanded = expand_home(path_str);
    if !Path::new(&expanded).exists() {
        return Some(Failure::new("File not found", expanded));
    }
    let (Some(setsid), Some(xdg_open)) = (
        crate::tools::resolve_tool("setsid", path_env),
        crate::tools::resolve_tool("xdg-open", path_env),
    ) else {
        return Some(Failure::new(
            "Cannot open file",
            format!("xdg-open is not installed: {expanded}"),
        ));
    };
    if crate::spawn::spawn_detached(&setsid, &xdg_open, &[expanded.as_str()]).is_err() {
        return Some(Failure::new("Cannot open file", expanded));
    }
    None
}

/// Report a failed action as a transient toast.
///
/// Deliberately *not* persisted: this is feedback about something the user
/// just did, not an incoming event. A stored copy would leave a feed entry
/// behind and bump the unread badge on every failed attempt. The `transient`
/// freedesktop hint tells the server to show it and drop it, so the daemon
/// renders the card straight from the payload.
///
/// Best-effort: without `notify-send` (or a running daemon) there is nothing
/// left to report with, so it stays silent.
pub fn report_failure(summary: &str, detail: &str) {
    let path_env = std::env::var("PATH").unwrap_or_default();
    report_failure_with(summary, detail, &path_env);
}

/// [`report_failure`] with an explicit `PATH` (test seam).
fn report_failure_with(summary: &str, detail: &str, path_env: &str) {
    let Some(bin) = crate::tools::resolve_tool("notify-send", path_env) else {
        return;
    };
    let _ = Command::new(&bin)
        .args([
            "-a",
            "flex-notify",
            "-h",
            "boolean:transient:true",
            summary,
            detail,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
/// file manager. Failures (nothing to reveal, a vanished folder, no manager)
/// come back as a [`Failure`] for the caller to report.
#[must_use]
pub fn open_dir(path_str: &str) -> Option<Failure> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    open_dir_with_path(path_str, &path_env)
}

/// [`open_dir`] with an explicit `PATH` (test seam for manager lookup).
fn open_dir_with_path(path_str: &str, path_env: &str) -> Option<Failure> {
    let Some((file, dir)) = reveal_targets(path_str) else {
        let label = path_str.trim();
        if label.is_empty() {
            return None;
        }
        return Some(Failure::new("Folder not found", label));
    };
    let dir_str = dir.to_string_lossy().into_owned();
    if !dir.is_dir() {
        return Some(Failure::new("Folder not found", dir_str));
    }
    let Some(setsid) = crate::tools::resolve_tool("setsid", path_env) else {
        return Some(Failure::new("Cannot open folder", dir_str));
    };
    if file.is_file() {
        let file_str = file.to_string_lossy();
        if let Some(mgr) = first_manager(SELECT_MANAGERS, path_env) {
            let _ = crate::spawn::spawn_detached(&setsid, Path::new(mgr), &["--select", &file_str]);
            return None;
        }
    }
    if let Some(mgr) = first_manager(BROWSE_MANAGERS, path_env) {
        let _ = crate::spawn::spawn_detached(&setsid, Path::new(mgr), &[dir_str.as_str()]);
        return None;
    }
    let Some(xdg_open) = crate::tools::resolve_tool("xdg-open", path_env) else {
        return Some(Failure::new("Cannot open folder", dir_str));
    };
    if crate::spawn::spawn_detached(&setsid, &xdg_open, &[dir_str.as_str()]).is_err() {
        return Some(Failure::new("Cannot open folder", dir_str));
    }
    None
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

/// Flex-owned sender labels: `app_name`s flex itself emits that stand for
/// something other than a desktop application.
///
/// `clip` posts as `Cliphist` (no `cliphist` binary exists — the state lives in
/// the flex clip popup) and flex's own feedback posts as `flex-notify`. Both
/// are recognised before desktop resolution, so a coincidentally named
/// `.desktop` file can never hijack them.
fn flex_sender(clean: &str) -> Option<Sender> {
    match clean {
        "flex-notify" => Some(Sender::Feedback),
        "cliphist" => Some(Sender::FlexPopup {
            class: crate::popup::WIDE_CLASS,
            provider: &["clip"],
        }),
        _ => None,
    }
}

/// Open (or toggle) the popup running `flex <provider…>`.
///
/// Goes through [`crate::popup::toggle_detached`] so an already-open popup of
/// the same variant closes instead of stacking, exactly like the keybinds, and
/// a newly spawned popup lands in its own session: the drawer runs this action
/// inside its kitty pty and exits right after, so a same-group child would be
/// killed by the pty hangup before its window appears. `kind` is
/// [`crate::terminal::detect`] in production and pinned to kitty in tests;
/// `path_env` is the stub-`PATH` seam.
fn open_flex_popup(
    class: &str,
    provider: &[&str],
    kind: crate::terminal::TerminalKind,
    path_env: &str,
) -> bool {
    let Some(dispatcher) = flex_dispatcher(path_env) else {
        log_notify_trace("[FLEX_POPUP] cannot locate the flex dispatcher");
        return false;
    };
    let mut argv = Vec::with_capacity(provider.len() + 1);
    argv.push(dispatcher.to_string_lossy().into_owned());
    argv.extend(provider.iter().map(|arg| String::from(*arg)));
    crate::popup::toggle_detached(kind, class, &argv, Some(path_env)).is_ok()
}

/// Absolute path to the `flex` dispatcher.
///
/// The drawer inherits its launcher's `PATH` — waybar's, which has no
/// `~/.local/bin` — so a bare `flex` in the popup command dies with "command
/// not found" and kitty closes the window immediately. Prefer the sibling of
/// the running binary (the whole farm is installed in one directory, and the
/// dispatcher resolves its providers the same way), then `PATH`.
fn flex_dispatcher(path_env: &str) -> Option<PathBuf> {
    sibling_binary("flex", path_env)
}

/// Absolute path to an installed farm binary called `name`.
///
/// The drawer inherits its launcher's `PATH` — waybar's, which has no
/// `~/.local/bin` — so bare names fail there. Prefer the sibling of the
/// running binary (the whole farm is installed in one directory), then `PATH`.
fn sibling_binary(name: &str, path_env: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(name);
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    crate::tools::resolve_tool(name, path_env)
}

/// What a notification's `app_name` names.
///
/// `app_name` is a free-form display label, not an identifier: CLI posts
/// default to `notify-send`, flex's own providers send `Wi-Fi` / `Cliphist`,
/// test harnesses send `e2e`, and scripts send their own file names. Actions
/// that need an application (`Open App`) are therefore derived from what the
/// label resolves to, never from a list of labels to exclude.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    /// An installed desktop application.
    Application(launch::DesktopEntry),
    /// A flex-owned label that stands for one of flex's popups.
    FlexPopup {
        /// Popup window class — the toggle key, see [`crate::popup`].
        class: &'static str,
        /// `flex` provider argv the popup runs.
        provider: &'static [&'static str],
    },
    /// Flex's own feedback channel: the body is the action that just failed.
    Feedback,
    /// No application behind the label: nothing to open or launch.
    Other,
}

impl Sender {
    /// Classify `app_name`, using the `app_icon` hint to link display names
    /// that match no desktop `Name` (KDE's `Crash Reporting System` is only
    /// connected to its entry by `Icon=tools-report-bug`).
    #[must_use]
    pub fn classify(app_name: &str, app_icon: Option<&str>) -> Self {
        let key = (app_name.trim().to_string(), app_icon.map(str::to_string));
        if let Ok(cache) = sender_cache().lock() {
            if let Some(hit) = cache.get(&key) {
                return hit.clone();
            }
        }
        let sender = Self::classify_in(&launch::app_dirs(), app_name, app_icon);
        if let Ok(mut cache) = sender_cache().lock() {
            // The feed rebuilds every tick; entries are bounded by the number
            // of distinct senders, but stay defensive like the other caches.
            if cache.len() >= 512 {
                cache.clear();
            }
            cache.insert(key, sender.clone());
        }
        sender
    }

    /// [`classify`](Self::classify) over explicit desktop dirs (tests).
    ///
    /// Uncached: the memo is keyed by sender label alone, so fixture
    /// directories would collide with the real scan.
    #[must_use]
    pub fn classify_in(app_dirs: &[PathBuf], app_name: &str, app_icon: Option<&str>) -> Self {
        let clean = app_name.trim().to_lowercase();
        if clean.is_empty() {
            return Self::Other;
        }
        if let Some(sender) = flex_sender(&clean) {
            return sender;
        }
        launch::find_app_in(app_dirs, app_name.trim(), app_icon)
            .map_or(Self::Other, Self::Application)
    }

    /// Whether `Open App` has a real target.
    #[must_use]
    pub fn is_openable(&self) -> bool {
        matches!(self, Self::Application(_) | Self::FlexPopup { .. })
    }

    /// Whether the body is user content worth extracting (paths, links, OTPs).
    ///
    /// [`Feedback`](Self::Feedback) bodies carry the path that just failed to
    /// open, so extracting them would re-offer the same broken targets.
    #[must_use]
    pub fn body_is_content(&self) -> bool {
        !matches!(self, Self::Feedback)
    }

    /// Short name for trace logs.
    fn label(&self) -> &'static str {
        match self {
            Self::Application(_) => "application",
            Self::FlexPopup { .. } => "flex-popup",
            Self::Feedback => "feedback",
            Self::Other => "other",
        }
    }
}

/// Memo for [`Sender::classify`]: desktop entries do not change while the
/// drawer is open, and every miss scans the whole application directory.
type SenderCache = Mutex<HashMap<(String, Option<String>), Sender>>;

/// Global memo backing [`Sender::classify`].
fn sender_cache() -> &'static SenderCache {
    static CACHE: OnceLock<SenderCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Open or focus a desktop application and switch workspace.
pub fn open_application(app_name: &str) {
    open_application_with(app_name, None);
}

/// [`open_application`] plus the notification's `app_icon` hint.
///
/// Notification `app_name`s are application *display names*, not binaries:
/// KDE's crash reporter sends `"Crash Reporting System"`, which has no
/// executable and no desktop `Name` — only `Icon=tools-report-bug`. The icon
/// hint lets [`launch::find_app`] resolve such names to a real `.desktop`
/// entry instead of silently spawning a nonexistent command.
pub fn open_application_with(app_name: &str, app_icon: Option<&str>) {
    let path_env = std::env::var("PATH").unwrap_or_default();
    open_application_seeded(app_name, app_icon, &launch::app_dirs(), &path_env);
}

/// [`open_application_with`] with explicit desktop dirs and `PATH` (test
/// seams: no ambient scan, no real spawn).
fn open_application_seeded(
    app_name: &str,
    app_icon: Option<&str>,
    app_dirs: &[PathBuf],
    path_env: &str,
) {
    let clean = app_name.trim().to_lowercase();
    let sender = Sender::classify_in(app_dirs, app_name, app_icon);
    log_notify_trace(&format!(
        "[OPEN_APP] app_name='{app_name}', clean='{clean}', icon={app_icon:?}, sender={}",
        sender.label()
    ));

    match sender {
        // A flex-owned label: toggle the popup that owns the same state (the
        // same machinery a keybind uses) instead of spawning a nonexistent
        // binary named after the label.
        Sender::FlexPopup { class, provider } => {
            log_notify_trace(&format!(
                "[FLEX_POPUP] '{clean}' -> flex {} (class='{class}')",
                provider.join(" ")
            ));
            if !open_flex_popup(class, provider, crate::terminal::detect(), path_env) {
                log_notify_trace(&format!(
                    "[FLEX_POPUP_FAILED] could not open flex {provider:?}"
                ));
            }
        }
        // A real application: focus its window when one is already open,
        // otherwise launch the desktop entry's `Exec` (field codes stripped,
        // `Terminal=true` handled by the shared launch executor).
        Sender::Application(entry) => {
            // `hyprctl` is resolved against `path_env` (not the ambient
            // `PATH`) so the stub-`PATH` seam never dispatches focus commands
            // at live clients.
            let hyprctl = crate::tools::resolve_tool("hyprctl", path_env);
            if let Some(hyprctl) = hyprctl.as_deref() {
                // Unset any active or global fullscreen lock across clients so
                // window focus/launch is visible.
                unset_fullscreen(hyprctl);
                if focus_existing_client(hyprctl, &clean) {
                    return;
                }
            }
            log_notify_trace(&format!(
                "[DESKTOP_RESOLVE] '{clean}' -> {} (Exec='{}')",
                entry.id, entry.exec
            ));
            if !launch_desktop_entry(&entry, path_env) {
                log_notify_trace(&format!(
                    "[DESKTOP_RESOLVE_FAILED] could not launch {}",
                    entry.id
                ));
            }
        }
        // Feedback and ordinary labels name no application; there is nothing
        // to open, and guessing (spawning `notify-send`, focusing a terminal)
        // would act on the wrong thing.
        Sender::Feedback | Sender::Other => {
            log_notify_trace(&format!(
                "[OPEN_APP] no application behind '{clean}'; nothing to open"
            ));
        }
    }
}

/// Launch a resolved desktop entry through [`crate::exec::launch`], reusing
/// its field-code stripping and `Terminal=true` handling. `path_env` is the
/// stub-`PATH` seam (`setsid` is resolved there).
fn launch_desktop_entry(entry: &launch::DesktopEntry, path_env: &str) -> bool {
    let stripped = launch::strip_field_codes(&entry.exec);
    let mut tokens = stripped.split_whitespace().map(str::to_string);
    let Some(program) = tokens.next() else {
        return false;
    };
    let args: Vec<String> = tokens.collect();
    // Read `$TERMINAL` only for `Terminal=true` entries, like the launch
    // executor does (a plain GUI launch must never consult it).
    let terminal = entry.terminal.then(crate::terminal::detect);
    crate::exec::launch::execute_with(&entry.id, &program, &args, terminal, Some(path_env)).is_ok()
}

/// Query `hyprctl clients -j` and hand the parsed client list to `visit`.
fn with_clients(hyprctl: &Path, visit: impl FnOnce(&[serde_json::Value])) {
    let Ok(output) = Command::new(hyprctl)
        .args(["clients", "-j"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Ok(json_str) = String::from_utf8(output.stdout) else {
        return;
    };
    let Ok(clients) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) else {
        return;
    };
    visit(&clients);
}

/// Unset any active or global fullscreen lock so window focus/launch is
/// visible.
fn unset_fullscreen(hyprctl: &Path) {
    with_clients(hyprctl, |clients| {
        for client in clients {
            let fs = client["fullscreen"].as_i64().unwrap_or(0);
            let fs_client = client["fullscreenClient"].as_i64().unwrap_or(0);
            if fs == 0 && fs_client == 0 {
                continue;
            }
            let Some(addr) = client["address"].as_str() else {
                continue;
            };
            log_notify_trace(&format!(
                "[FULLSCREEN] Unsetting fullscreen for client address='{addr}'"
            ));
            let lua_cmd = format!(
                "hl.dsp.window.fullscreen({{ action = \"unset\", window = \"address:{addr}\" }})"
            );
            let _ = Command::new(hyprctl)
                .args(["dispatch", &lua_cmd])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    });
}

/// Focus an already-open client window matching `clean`; `false` when no
/// client matches (the caller launches the app instead).
fn focus_existing_client(hyprctl: &Path, clean: &str) -> bool {
    let mut focused = false;
    with_clients(hyprctl, |clients| {
        let Some((address, ws_id)) = find_matching_client(clients, clean) else {
            return;
        };
        let address_owned = address.to_string();
        log_notify_trace(&format!(
            "[FOCUS_DISPATCH_SYNC] Dispatching window focus for address='{address_owned}', ws={ws_id:?}"
        ));
        let lua_win = format!("hl.dsp.focus({{ window = \"address:{address_owned}\" }})");
        let _ = Command::new(hyprctl)
            .args(["dispatch", &lua_win])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(50));
        focused = true;
    });
    focused
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

/// A per-app preference with an optional lifetime.
///
/// Mutes are promised for one hour and expire on their own; priority is
/// indefinite until toggled off. Storing `until` instead of a bare name is
/// what makes "Silence … for 1 Hour" honest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRule {
    pub app: String,
    /// Unix timestamp the rule stops applying; `None` = until removed.
    #[serde(default)]
    pub until: Option<u64>,
}

impl AppRule {
    #[must_use]
    pub fn is_active(&self, now: u64) -> bool {
        self.until.is_none_or(|until| now < until)
    }
}

/// Accept both the pre-rule store (`"muted_apps": ["Discord"]`) and the typed
/// one, so an upgrade never resets the whole notification state.
fn deserialize_app_rules<'de, D>(deserializer: D) -> Result<Vec<AppRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Name(String),
        Rule(AppRule),
    }

    Ok(Vec::<Entry>::deserialize(deserializer)?
        .into_iter()
        .map(|entry| match entry {
            Entry::Name(app) => AppRule { app, until: None },
            Entry::Rule(rule) => rule,
        })
        .collect())
}

/// How long a dismissed or expired notification stays in the store.
///
/// History only renders the newest 30; the retained window keeps that archive
/// useful across a session restart without letting the JSON grow forever.
pub const ARCHIVE_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

/// Complete runtime state stored in `$XDG_RUNTIME_DIR/flex-notify.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NotifyState {
    pub notifications: Vec<NotificationItem>,
    pub controls: QuickControls,
    #[serde(default, deserialize_with = "deserialize_app_rules")]
    pub muted_apps: Vec<AppRule>,
    #[serde(default, deserialize_with = "deserialize_app_rules")]
    pub priority_apps: Vec<AppRule>,
}

impl NotifyState {
    /// Silence `app` (case-insensitively) until `until`.
    pub fn mute_app(&mut self, app: &str, until: u64) {
        self.muted_apps
            .retain(|rule| !rule.app.eq_ignore_ascii_case(app));
        self.muted_apps.push(AppRule {
            app: app.to_string(),
            until: Some(until),
        });
    }

    /// Remove any mute rule for `app`, active or stale.
    pub fn unmute_app(&mut self, app: &str) {
        self.muted_apps
            .retain(|rule| !rule.app.eq_ignore_ascii_case(app));
    }

    #[must_use]
    pub fn is_muted(&self, app: &str, now: u64) -> bool {
        self.muted_apps
            .iter()
            .any(|rule| rule.app.eq_ignore_ascii_case(app) && rule.is_active(now))
    }

    /// Flip the app between priority and ordinary feed ordering.
    pub fn toggle_priority(&mut self, app: &str) {
        if let Some(pos) = self
            .priority_apps
            .iter()
            .position(|rule| rule.app.eq_ignore_ascii_case(app))
        {
            self.priority_apps.remove(pos);
        } else {
            self.priority_apps.push(AppRule {
                app: app.to_string(),
                until: None,
            });
        }
    }

    #[must_use]
    pub fn is_priority(&self, app: &str, now: u64) -> bool {
        self.priority_apps
            .iter()
            .any(|rule| rule.app.eq_ignore_ascii_case(app) && rule.is_active(now))
    }

    /// Permanently drop one notification (History's Delete key).
    pub fn purge(&mut self, id: u32) {
        self.notifications.retain(|item| item.id != id);
    }

    /// Permanently drop the archive (dismissed or sender-expired entries).
    pub fn clear_history(&mut self, now: u64) {
        self.notifications.retain(|item| !item.is_archived(now));
    }

    /// Drop rules that no longer apply and archive entries past retention.
    fn settle(&mut self, now: u64) {
        self.muted_apps.retain(|rule| rule.is_active(now));
        self.priority_apps.retain(|rule| rule.is_active(now));
        self.notifications.retain(|item| {
            // Legacy archive entries have no `archived_at`; keep them until an
            // explicit Clear History instead of guessing from their age.
            item.archived_at
                .or(item.expires_at)
                .is_none_or(|archived_at| now.saturating_sub(archived_at) <= ARCHIVE_RETENTION_SECS)
        });
    }
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
        if let Ok(mut state) = serde_json::from_str::<NotifyState>(&content) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            state.settle(now);
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
    /// `PATH` used to spawn toast overlays; `None` inherits the daemon's.
    /// Tests pin it to a stub farm so no window actually opens.
    toast_path_env: Option<String>,
}

impl NotificationServer {
    #[must_use]
    pub fn new(state_path: Option<PathBuf>) -> Self {
        Self {
            state_path,
            toast_path_env: None,
        }
    }

    /// [`new`] with an explicit `PATH` for toast spawning (test seam).
    #[must_use]
    pub fn with_toast_path(state_path: Option<PathBuf>, path_env: impl Into<String>) -> Self {
        Self {
            state_path,
            toast_path_env: Some(path_env.into()),
        }
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
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            item.archive(now);
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

        // `transient` (freedesktop hint): show it, never keep it. flex's own
        // failure feedback ("File not found", …) posts this way, so a failed
        // tap cannot leave a feed entry behind or bump the unread badge.
        let transient = matches!(
            hints.get("transient"),
            Some(zbus::zvariant::Value::Bool(true))
        );
        if transient {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            if !load_state(self.state_path.as_deref())
                .controls
                .dnd
                .is_active(now)
            {
                spawn_transient_toast_with_path(&summary, &body, self.toast_path_env.as_deref());
            }
            return 0;
        }

        let mut state = load_state(self.state_path.as_deref());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        // freedesktop `expire_timeout`: -1 asks for the server default (flex
        // keeps the entry until dismissed, because this is a notification
        // center), 0 means never, and a positive value retires the entry after
        // the sender's own lifetime. Critical alerts are never expired.
        let expires_at = (expire_timeout > 0 && urgency != Urgency::Critical)
            .then(|| now + u64::from(expire_timeout.unsigned_abs()).div_ceil(1000));

        let target_id = if replaces_id > 0 {
            replaces_id
        } else {
            state.notifications.iter().map(|n| n.id).max().unwrap_or(0) + 1
        };

        let is_app_muted = state.is_muted(&app_name, now);

        if let Some(existing) = state.notifications.iter_mut().find(|n| n.id == target_id) {
            existing.app_name = app_name;
            existing.summary = summary;
            existing.body = body;
            existing.urgency = urgency;
            existing.timestamp = now;
            existing.progress = progress;
            existing.expires_at = expires_at;
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
            existing.archived_at = None;
        } else {
            let mut item = NotificationItem::new(target_id, app_name, summary, body, urgency);
            item.timestamp = now;
            item.progress = progress;
            item.expires_at = expires_at;
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
            spawn_toast_with_path(
                target_id,
                self.state_path.as_deref(),
                self.toast_path_env.as_deref(),
            );
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

/// Auto-dismiss delay used when a sender asked for the server default.
pub const TOAST_DEFAULT_SECS: u64 = 4;
/// Critical alerts stay on screen a little longer than ordinary toasts.
pub const TOAST_CRITICAL_SECS: u64 = 7;
/// Upper bound for a sender-requested toast lifetime: a banner must never
/// pin itself to the screen for minutes because of a stray timeout.
pub const TOAST_MAX_SECS: u64 = 30;

/// Auto-dismiss delay for a toast card at `now`.
///
/// A positive `expire_timeout` is honored (clamped to 1..=[`TOAST_MAX_SECS`])
/// relative to [`NotificationItem::expires_at`]; otherwise the server default
/// applies.
#[must_use]
pub fn toast_timeout_secs(item: &NotificationItem, now: u64) -> u64 {
    if let Some(at) = item.expires_at {
        return at.saturating_sub(now).clamp(1, TOAST_MAX_SECS);
    }
    if item.urgency == Urgency::Critical {
        TOAST_CRITICAL_SECS
    } else {
        TOAST_DEFAULT_SECS
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

/// Kitty flags shared by every toast window (stored and transient).
const TOAST_KITTY_ARGS: &[&str] = &[
    "--class",
    "flex-notify-toast",
    "-o",
    "font_size=11",
    "-o",
    "remember_window_size=no",
    "-o",
    "initial_window_width=50c",
    "-o",
    "initial_window_height=6c",
    "-o",
    "window_padding_width=0",
    "-o",
    "window_padding_height=0",
];

/// The `kitty …` command prefix for a toast window.
fn toast_kitty_cmd() -> String {
    format!("kitty {}", TOAST_KITTY_ARGS.join(" "))
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
    let cmd = format!("{} -e flex-notify{state_arg} toast {id}", toast_kitty_cmd());
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

/// Spawn a toast card from text, without storing it.
///
/// [`spawn_toast`] renders a stored notification by id; feedback that must not
/// be persisted (the freedesktop `transient` hint — e.g. flex's own
/// "File not found") has no id to look up, so the text travels in argv and
/// nothing is written to the state store.
fn spawn_transient_toast_with_path(summary: &str, body: &str, path_override: Option<&str>) {
    let ambient = std::env::var("PATH").unwrap_or_default();
    let path_env = path_override.unwrap_or(&ambient);
    let Some(bin) = sibling_binary("flex-notify", path_env) else {
        return;
    };
    let sh_bin = match path_override {
        Some(path_env) => match crate::tools::resolve_tool("sh", path_env) {
            Some(path) => path,
            None => return,
        },
        None => PathBuf::from("sh"),
    };
    // The text arrives as positional parameters, never spliced into the
    // command string: a summary/body containing quotes, `$` or `;` must stay
    // inert. Same `sh -c '… &'` double-fork as [`spawn_toast_with_path`], so
    // the daemon's FDs are not inherited.
    let script = format!("{} -e \"$0\" toast-text \"$1\" \"$2\" &", toast_kitty_cmd());
    let mut toast = Command::new(&sh_bin);
    toast
        .args(["-c", &script, &bin.to_string_lossy(), summary, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path_env) = path_override {
        toast.env("PATH", path_env);
    }
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
        let (stubs, _log) = toast_dir("srv-stubs");

        let srv = NotificationServer::with_toast_path(Some(path.clone()), stubs.to_string_lossy());

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
        let _ = std::fs::remove_dir_all(&stubs);
    }

    #[test]
    fn transient_notifications_are_toasted_but_never_stored() {
        let (stubs, kitty_log) = toast_dir("srv-transient");
        let path = stubs.join("state.json");
        let srv = NotificationServer::with_toast_path(Some(path.clone()), stubs.to_string_lossy());

        let mut hints = std::collections::HashMap::new();
        hints.insert("transient".to_string(), zbus::zvariant::Value::Bool(true));

        let id = srv.notify(
            "flex-notify".to_string(),
            0,
            String::new(),
            "File not found".to_string(),
            "/tmp/gone.png".to_string(),
            vec![],
            hints,
            -1,
        );

        // No id, no feed entry, no unread badge — just the card on screen.
        assert_eq!(id, 0);
        assert_eq!(
            load_state(Some(&path)).notifications.first(),
            None,
            "the failed open must not add a feed entry"
        );
        let logged = read_log_soon(&kitty_log);
        assert!(
            logged.contains("File not found") && logged.contains("/tmp/gone.png"),
            "the toast still renders the payload: {logged:?}"
        );
        let _ = std::fs::remove_dir_all(&stubs);
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
    fn lifecycle_predicate_handles_snooze_and_sender_expiry() {
        let now = 1_000;
        let mut item = NotificationItem::new(1, "App", "summary", "body", Urgency::Normal);
        assert!(item.is_active(now));
        assert!(!item.is_archived(now));

        item.is_snoozed = true;
        item.snooze_until = Some(now + 60);
        assert!(!item.is_active(now), "a live snooze hides the item");
        assert!(!item.is_archived(now), "a snooze is not an archive");
        assert!(item.is_active(now + 60), "an elapsed snooze wakes up");

        item.is_snoozed = false;
        item.snooze_until = None;
        item.expires_at = Some(now + 10);
        assert!(item.is_active(now));
        assert!(!item.is_archived(now));
        assert!(
            !item.is_active(now + 10),
            "elapsed sender timeout retires it"
        );
        assert!(item.is_archived(now + 10));

        item.is_dismissed = true;
        assert!(!item.is_active(now));
        assert!(item.is_archived(now));
    }

    #[test]
    fn app_rules_expire_and_mute_is_undoable() {
        let mut state = NotifyState::default();
        state.mute_app("Discord", 1_000 + 3_600);

        assert!(
            state.is_muted("discord", 1_000),
            "rules match case-insensitively"
        );
        assert!(
            !state.is_muted("discord", 1_000 + 3_600),
            "the promised hour actually ends"
        );

        state.mute_app("DISCORD", 2_000);
        assert_eq!(
            state.muted_apps.len(),
            1,
            "a refresh replaces, never stacks"
        );

        state.unmute_app("discord");
        assert!(!state.is_muted("Discord", 1_500));

        state.toggle_priority("Slack");
        assert!(state.is_priority("slack", 0));
        state.toggle_priority("SLACK");
        assert!(!state.is_priority("Slack", 0));
    }

    #[test]
    fn legacy_string_rules_still_deserialize() {
        let mut value = serde_json::to_value(NotifyState::default()).expect("state to json");
        value["muted_apps"] = serde_json::json!(["Discord"]);
        value["priority_apps"] = serde_json::json!(["System"]);

        let state: NotifyState = serde_json::from_value(value).expect("legacy store loads");
        assert_eq!(
            state.muted_apps,
            vec![AppRule {
                app: "Discord".to_string(),
                until: None,
            }]
        );
        assert_eq!(
            state.priority_apps,
            vec![AppRule {
                app: "System".to_string(),
                until: None,
            }]
        );
    }

    #[test]
    fn load_prunes_expired_rules_and_ancient_archives() {
        let dir =
            std::env::temp_dir().join(format!("flex-notify-retention-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("retention.json");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let mut ancient = NotificationItem::new(1, "A", "ancient", "", Urgency::Normal);
        ancient.archive(now - ARCHIVE_RETENTION_SECS - 10);

        let mut still_active = NotificationItem::new(2, "B", "active", "", Urgency::Normal);
        still_active.timestamp = now - ARCHIVE_RETENTION_SECS - 10;

        let state = NotifyState {
            notifications: vec![ancient, still_active],
            muted_apps: vec![AppRule {
                app: "Expired".to_string(),
                until: Some(now - 1),
            }],
            priority_apps: vec![AppRule {
                app: "Kept".to_string(),
                until: Some(now + 3_600),
            }],
            ..NotifyState::default()
        };
        save_state(&state, Some(&path)).expect("saved");

        let loaded = load_state(Some(&path));
        assert!(
            loaded.notifications.iter().any(|n| n.id == 2),
            "active notifications survive retention"
        );
        assert!(
            !loaded.notifications.iter().any(|n| n.id == 1),
            "archived entries past retention are dropped"
        );
        assert_eq!(loaded.muted_apps.first(), None, "expired rules are pruned");
        assert_eq!(loaded.priority_apps.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toast_timeout_follows_the_sender_deadline_within_bounds() {
        let now = 100;
        let mut item = NotificationItem::new(1, "App", "summary", "body", Urgency::Normal);
        assert_eq!(toast_timeout_secs(&item, now), TOAST_DEFAULT_SECS);

        item.urgency = Urgency::Critical;
        assert_eq!(toast_timeout_secs(&item, now), TOAST_CRITICAL_SECS);

        item.urgency = Urgency::Normal;
        item.expires_at = Some(now + 2);
        assert_eq!(toast_timeout_secs(&item, now), 2);
        item.expires_at = Some(now + 999);
        assert_eq!(toast_timeout_secs(&item, now), TOAST_MAX_SECS);
        item.expires_at = Some(now);
        assert_eq!(toast_timeout_secs(&item, now), 1);
        item.expires_at = Some(now - 50);
        assert_eq!(toast_timeout_secs(&item, now), 1);
    }

    #[test]
    fn notify_honors_expire_timeout_but_never_for_critical() {
        let dir = std::env::temp_dir().join(format!("flex-notify-expiry-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("expiry.json");
        let (stubs, _log) = toast_dir("expiry-stubs");
        let srv = NotificationServer::with_toast_path(Some(path.clone()), stubs.to_string_lossy());

        let id = srv.notify(
            "App".to_string(),
            0,
            String::new(),
            "Short lived".to_string(),
            String::new(),
            vec![],
            std::collections::HashMap::new(),
            1500,
        );
        let item = load_state(Some(&path))
            .notifications
            .into_iter()
            .find(|n| n.id == id)
            .expect("stored");
        let lifetime = item.expires_at.expect("expiry stored") - item.timestamp;
        assert!(
            (1..=2).contains(&lifetime),
            "1.5s is rounded up to whole seconds, got {lifetime}"
        );

        // Replacing with the server default clears the old deadline.
        srv.notify(
            "App".to_string(),
            id,
            String::new(),
            "Now default".to_string(),
            String::new(),
            vec![],
            std::collections::HashMap::new(),
            -1,
        );
        let replaced = load_state(Some(&path))
            .notifications
            .into_iter()
            .find(|n| n.id == id)
            .expect("replaced");
        assert_eq!(replaced.expires_at, None);

        // Critical alerts ignore even an explicit short timeout.
        let mut hints = std::collections::HashMap::new();
        hints.insert("urgency".to_string(), zbus::zvariant::Value::U8(2));
        let critical = srv.notify(
            "App".to_string(),
            0,
            String::new(),
            "Critical".to_string(),
            String::new(),
            vec![],
            hints,
            1000,
        );
        let critical = load_state(Some(&path))
            .notifications
            .into_iter()
            .find(|n| n.id == critical)
            .expect("critical stored");
        assert_eq!(critical.expires_at, None);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&stubs);
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
    fn open_application_launches_resolved_desktop_entry() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("flex-notify-openapp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");

        let apps = dir.join("applications");
        std::fs::create_dir_all(&apps).expect("apps dir");
        std::fs::write(
            apps.join("org.example.viewer.desktop"),
            "[Desktop Entry]\nName=Example Viewer\nExec=example-viewer --open\nIcon=tools-report-bug\nTerminal=false\n",
        )
        .expect("desktop entry");

        let log = dir.join("spawn.log");
        let stub = dir.join("setsid");
        std::fs::write(
            &stub,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", log.display()),
        )
        .expect("setsid stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path_env = dir.to_string_lossy().into_owned();

        // The KDE crash-reporter shape: a display name with no executable,
        // linked to its desktop entry only by the `app_icon` hint.
        open_application_seeded(
            "Crash Reporting System",
            Some("tools-report-bug"),
            &[apps],
            &path_env,
        );

        let spawned = std::fs::read_to_string(&log).expect("setsid stub ran");
        assert!(spawned.contains("example-viewer"), "spawned: {spawned:?}");
        assert!(spawned.contains("--open"), "spawned: {spawned:?}");
        assert!(
            !spawned.contains("crash reporting system"),
            "the display name must not be spawned as a binary: {spawned:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sender_classification_drives_openability() {
        let dir = std::env::temp_dir().join(format!("flex-notify-sender-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let apps = dir.join("applications");
        std::fs::create_dir_all(&apps).expect("apps dir");
        std::fs::write(
            apps.join("org.example.viewer.desktop"),
            "[Desktop Entry]\nName=Example Viewer\nExec=example-viewer --open\nIcon=tools-report-bug\nTerminal=false\n",
        )
        .expect("desktop entry");
        let dirs = vec![apps];

        // Real applications resolve by display name or by the icon hint.
        assert!(matches!(
            Sender::classify_in(&dirs, "Example Viewer", None),
            Sender::Application(_)
        ));
        assert!(matches!(
            Sender::classify_in(&dirs, "Crash Reporting System", Some("tools-report-bug")),
            Sender::Application(_)
        ));
        // flex-owned labels are not applications: `Cliphist` owns a popup and
        // `flex-notify` is flex's feedback channel.
        assert!(matches!(
            Sender::classify_in(&dirs, "Cliphist", None),
            Sender::FlexPopup { .. }
        ));
        assert_eq!(
            Sender::classify_in(&dirs, "flex-notify", None),
            Sender::Feedback
        );

        // Labels with no application behind them are `Other`, never openable.
        for label in ["notify-send", "e2e", "wiremix", "System", "   "] {
            let sender = Sender::classify_in(&dirs, label, None);
            assert_eq!(sender, Sender::Other, "{label:?}");
            assert!(!sender.is_openable(), "{label:?}");
        }

        // Only feedback bodies are not user content (they are failed paths).
        assert!(!Sender::Feedback.body_is_content());
        assert!(Sender::Other.body_is_content());
        assert!(Sender::classify_in(&dirs, "Example Viewer", None).body_is_content());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_senders_launch_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("flex-notify-noapp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");

        // Any spawn attempt would resolve these stubs and log argv.
        let log = dir.join("spawn.log");
        for name in ["setsid", "kitty", "hyprctl", "notify-send"] {
            let stub = dir.join(name);
            std::fs::write(
                &stub,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> '{}'\n",
                    log.display()
                ),
            )
            .expect("stub");
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let path_env = dir.to_string_lossy().into_owned();

        for label in ["notify-send", "e2e", "wiremix", "System"] {
            open_application_seeded(label, None, &[], &path_env);
        }

        assert!(
            !log.exists(),
            "a label with no application behind it must not spawn anything: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_application_routes_flex_owned_labels_to_their_popup() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("flex-notify-flexrel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");

        let chmod = |path: &std::path::Path| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        };
        // `pgrep` says nothing is open, so the toggle spawns; `setsid` logs its
        // argv and execs the terminal stub, which logs its own argv.
        let pgrep = dir.join("pgrep");
        std::fs::write(&pgrep, "#!/bin/sh\nexit 1\n").expect("pgrep stub");
        chmod(&pgrep);
        let kitty_log = dir.join("kitty.log");
        let kitty = dir.join("kitty");
        std::fs::write(
            &kitty,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" > '{}'\n",
                kitty_log.display()
            ),
        )
        .expect("kitty stub");
        chmod(&kitty);
        let setsid_log = dir.join("setsid.log");
        let setsid = dir.join("setsid");
        std::fs::write(
            &setsid,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nkitty=\"$2\"\nshift 2\nexec \"$kitty\" \"$@\"\n",
                setsid_log.display()
            ),
        )
        .expect("setsid stub");
        chmod(&setsid);
        // The popup must call the dispatcher by absolute path: the drawer's
        // `PATH` (inherited from waybar) has no `~/.local/bin`.
        let flex_stub = dir.join("flex");
        std::fs::write(&flex_stub, "#!/bin/sh\nexit 0\n").expect("flex stub");
        chmod(&flex_stub);
        let path_env = dir.to_string_lossy().into_owned();

        // `Cliphist` is flex's own label for the clipboard store: no binary,
        // no desktop entry — the tap must open the `flex clip` popup.
        open_application_seeded("Cliphist", None, &[], &path_env);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let spawned = loop {
            if let Ok(body) = std::fs::read_to_string(&kitty_log) {
                break body;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "kitty stub never ran: {}",
                kitty_log.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(
            spawned.contains("flex-menu-wide"),
            "wide clip popup class: {spawned:?}"
        );
        assert!(
            spawned.contains("POPUP_KITTY=1"),
            "popup marker env: {spawned:?}"
        );
        assert!(
            spawned.lines().any(|line| line == "clip"),
            "runs `flex clip`: {spawned:?}"
        );
        assert!(
            spawned
                .lines()
                .any(|line| line.ends_with("/flex") && line.starts_with('/')),
            "absolute dispatcher path, not a bare `flex`: {spawned:?}"
        );
        assert!(
            !spawned.lines().any(|line| line == "flex"),
            "the popup must not rely on `flex` being on `PATH`: {spawned:?}"
        );
        // Session-detached: the drawer's pty closes right after the action, so
        // the popup must be spawned through `setsid -f` or it dies of SIGHUP.
        let detached = std::fs::read_to_string(&setsid_log).expect("setsid stub ran");
        assert!(
            detached.lines().next() == Some("-f"),
            "session-detached spawn: {detached:?}"
        );
        assert!(
            !detached.contains("cliphist"),
            "must not spawn the nonexistent `cliphist` binary: {detached:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write an executable stub script (`chmod 755`).
    fn write_stub(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, body).expect("stub");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Scratch dir with logging stubs for the toast chain (`sh`, `kitty`,
    /// `flex-notify`), plus the log where the `kitty` stub records its argv.
    fn toast_dir(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("flex-notify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let log = dir.join("kitty.log");
        write_stub(
            &dir.join("kitty"),
            &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", log.display()),
        );
        // Real `sh` semantics (the script backgrounds kitty), but every bare
        // command inside it resolves through the stub `PATH`.
        write_stub(&dir.join("sh"), "#!/bin/sh\nexec /bin/sh \"$@\"\n");
        // Exists only so the toast binary resolves; the stub kitty never runs it.
        write_stub(&dir.join("flex-notify"), "#!/bin/sh\nexit 0\n");
        (dir, log)
    }

    /// Scratch dir with a logging `notify-send` stub, plus its log path.
    fn notifier_dir(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("flex-notify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let log = dir.join("notify.log");
        write_stub(
            &dir.join("notify-send"),
            &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", log.display()),
        );
        (dir, log)
    }

    /// Read a stub log, waiting briefly: the toast chain runs `sh -c '… &'`,
    /// so the `kitty` stub may still be starting when the spawn returns.
    fn read_log_soon(log: &Path) -> String {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(log) {
                if !text.trim().is_empty() {
                    return text;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("stub never logged: {}", log.display());
    }

    #[test]
    fn open_file_reports_missing_paths() {
        let dir = std::env::temp_dir().join(format!(
            "flex-notify-openfile-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path_env = dir.to_string_lossy().into_owned();

        let failure = open_file_with("/nonexistent/flex-ghost.png", &path_env).expect("reported");
        assert_eq!(failure.summary, "File not found");
        assert_eq!(failure.detail, "/nonexistent/flex-ghost.png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_file_launches_existing_paths_without_feedback() {
        let dir = std::env::temp_dir().join(format!(
            "flex-notify-openfile-exists-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let file = dir.join("kept.txt");
        std::fs::write(&file, b"hello").expect("fixture");
        let xdg_log = dir.join("xdg.log");
        write_stub(
            &dir.join("xdg-open"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
                xdg_log.display()
            ),
        );
        // `setsid -f <xdg-open> <file>`: run the stub directly.
        write_stub(
            &dir.join("setsid"),
            "#!/bin/sh\nbin=\"$2\"\nshift 2\nexec \"$bin\" \"$@\"\n",
        );
        let path_env = dir.to_string_lossy().into_owned();

        assert_eq!(
            open_file_with(&file.to_string_lossy(), &path_env),
            None,
            "an existing file reports nothing"
        );
        let opened = std::fs::read_to_string(&xdg_log).expect("xdg-open stub ran");
        assert_eq!(opened.trim(), file.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_dir_reports_missing_folders_and_ignores_empty_input() {
        let dir = std::env::temp_dir().join(format!(
            "flex-notify-opendir-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path_env = dir.to_string_lossy().into_owned();

        // Deleted parent folder: say so instead of doing nothing.
        let failure =
            open_dir_with_path("/nonexistent/flex-dir/file.png", &path_env).expect("reported");
        assert_eq!(failure.summary, "Folder not found");
        assert_eq!(failure.detail, "/nonexistent/flex-dir");

        // Nothing to point at (empty label): stay silent.
        assert_eq!(open_dir_with_path("   ", &path_env), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_failure_posts_transiently_so_nothing_is_stored() {
        let (dir, log) = notifier_dir("report");
        let path_env = dir.to_string_lossy().into_owned();

        report_failure_with("File not found", "/tmp/gone.png", &path_env);

        let logged = read_log_soon(&log);
        let args: Vec<&str> = logged.lines().collect();
        assert_eq!(
            args,
            [
                "-a",
                "flex-notify",
                "-h",
                "boolean:transient:true",
                "File not found",
                "/tmp/gone.png"
            ],
            "the transient hint keeps feedback out of the feed and the badge"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_toast_renders_from_argv_only() {
        let (dir, log) = toast_dir("transient");
        let path_env = dir.to_string_lossy().into_owned();
        let body = "/tmp/gone; rm -rf $HOME";

        spawn_transient_toast_with_path("File not found", body, Some(&path_env));

        let logged = read_log_soon(&log);
        let args: Vec<&str> = logged.lines().collect();
        assert_eq!(args.first(), Some(&"--class"));
        assert_eq!(args.get(1), Some(&"flex-notify-toast"));
        // The card text is argv, not a state lookup, and the body arrives
        // whole: shell metacharacters in a path stay inert.
        assert_eq!(
            &args[args.len() - 3..],
            ["toast-text", "File not found", body],
            "full argv: {args:?}"
        );
        let toast_bin = dir.join("flex-notify").to_string_lossy().into_owned();
        assert!(
            args.contains(&toast_bin.as_str()),
            "the toast binary resolves by absolute path, not PATH lookup: {args:?}"
        );
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
