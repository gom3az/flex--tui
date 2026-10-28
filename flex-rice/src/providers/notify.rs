//! Notification Center Drawer provider (`flex-notify`).
//!
//! Renders a native, search-free right-side drawer dashboard containing:
//! - Quick Controls shelf (DND toggle & Focus presets, Clear All)
//! - MPRIS media player card with progress bar
//! - Sticky critical system alerts
//! - In-flight download/upgrade progress trackers
//! - Expandable app thread stacks
//! - Smart entity extractors (1-click copy OTP, URLs, hex colors)
//! - Per-app mute rules and Focus/DND timers

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use flex_core::{Menu, Row, RowId, Tab, Target};

use crate::exec::notify::{
    self, DndState, ExtractedEntity, NotificationItem, NotifyState, Urgency,
};

/// Thread expansion state tracking.
fn expanded_threads() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Toggle expanded state for an app thread.
pub fn toggle_thread_expanded(app: &str) {
    let mut guard = match expanded_threads().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.contains(app) {
        guard.remove(app);
    } else {
        guard.insert(app.to_string());
    }
}

/// Check if an app thread is expanded.
#[must_use]
pub fn is_thread_expanded(app: &str) -> bool {
    let guard = match expanded_threads().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.contains(app)
}

/// The provider name matching `flex notify` and `flex-notify`.
pub const PROVIDER: &str = "notify";

/// Tab titles
pub const TAB_FEED: &str = "Notifications";
pub const TAB_HISTORY: &str = "History";

/// Row ids of the two non-notification cards.
///
/// Neither is an action: `Enter` on those rows opens their target dropdown,
/// so [`NotifyAction::parse`] deliberately knows nothing about them.
pub const ACTION_QUICK_CONTROLS: &str = "quick:controls";
pub const ACTION_MPRIS_TRACK: &str = "mpris:track";

/// Escape a payload field so the whole action id stays a single
/// whitespace-free `ACTION:` field.
///
/// Printable ASCII passes through — paths and URLs stay readable — except `%`
/// (the escape byte) and `:` (the field separator of multi-field actions).
/// Whitespace, control bytes and non-ASCII bytes are `%XX`-encoded, so the id
/// survives both the whitespace-delimited `ACTION:` protocol and logging.
fn encode_field(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if (0x21..=0x7E).contains(&byte) && byte != b'%' && byte != b':' {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Inverse of [`encode_field`]; `None` for malformed escapes.
fn decode_field(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Every action the notification drawer emits, and the single source of its
/// id format.
///
/// Producers and the dispatcher share this type: [`encode`](Self::encode) is
/// the only place ids are built and [`parse`](Self::parse) the only place they
/// are understood, so an action cannot be emitted in a shape nobody handles.
/// [`execute`] then matches the enum exhaustively — no `strip_prefix` chain,
/// no prefix collisions (`dismiss:` vs `dismiss_app:`) and no title-substring
/// fallbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyAction {
    /// Open the sender application; `icon` is the notification's hint.
    OpenApp {
        name: String,
        icon: Option<String>,
    },
    /// `Enter` on a notification row — same operation as [`Self::OpenApp`].
    OpenNotification(u32),
    OpenFile(String),
    OpenDir(String),
    CopyPath(String),
    CopyImage(String),
    CopyOtp(String),
    OpenUrl(String),
    CopyUrl(String),
    CopyHex(String),
    /// Copy the notification's summary and body.
    CopyBody(u32),
    CopyMedia {
        title: String,
        artist: String,
    },
    /// A D-Bus action button attached to the notification.
    DbusAction {
        notification: u32,
        action: String,
    },
    Dismiss(u32),
    DismissApp(String),
    MuteApp(String),
    UnmuteApp(String),
    PriorityApp(String),
    ToggleThread(String),
    Snooze {
        notification: u32,
        minutes: u32,
    },
    /// Wake a snoozed notification now.
    Unsnooze(u32),
    Restore(u32),
    /// Permanently drop one notification (History's Delete key).
    Purge(u32),
    ClearAll,
    /// Permanently drop every archived notification.
    ClearHistory,
    ToggleDnd,
    DndTimed {
        minutes: u32,
    },
    DndIndefinite,
    DndOff,
    PlayPause,
    NextTrack,
    PreviousTrack,
    Seek {
        seconds: i32,
    },
    FocusPlayer(String),
}

impl NotifyAction {
    /// Id emitted on the `ACTION:` line for this action.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn encode(&self) -> RowId {
        let id = match self {
            Self::OpenApp { name, icon } => format!(
                "open_app:{}:{}",
                encode_field(name),
                icon.as_deref().map_or_else(String::new, encode_field)
            ),
            Self::OpenNotification(id) => format!("notif:{id}"),
            Self::OpenFile(path) => format!("open_file:{}", encode_field(path)),
            Self::OpenDir(path) => format!("open_dir:{}", encode_field(path)),
            Self::CopyPath(path) => format!("copy_path:{}", encode_field(path)),
            Self::CopyImage(path) => format!("copy_image:{}", encode_field(path)),
            Self::CopyOtp(code) => format!("copy_otp:{}", encode_field(code)),
            Self::OpenUrl(url) => format!("open_url:{}", encode_field(url)),
            Self::CopyUrl(url) => format!("copy_url:{}", encode_field(url)),
            Self::CopyHex(hex) => format!("copy_hex:{}", encode_field(hex)),
            Self::CopyBody(id) => format!("copy_body:{id}"),
            Self::CopyMedia { title, artist } => {
                format!(
                    "copy_media:{}:{}",
                    encode_field(title),
                    encode_field(artist)
                )
            }
            Self::DbusAction {
                notification,
                action,
            } => format!("action:{notification}:{}", encode_field(action)),
            Self::Dismiss(id) => format!("dismiss:{id}"),
            Self::DismissApp(app) => format!("dismiss_app:{}", encode_field(app)),
            Self::MuteApp(app) => format!("mute_app:{}", encode_field(app)),
            Self::UnmuteApp(app) => format!("unmute_app:{}", encode_field(app)),
            Self::PriorityApp(app) => format!("priority_app:{}", encode_field(app)),
            Self::ToggleThread(app) => format!("toggle_thread:{}", encode_field(app)),
            Self::Snooze {
                notification,
                minutes,
            } => format!("snooze:{minutes}:{notification}"),
            Self::Unsnooze(id) => format!("unsnooze:{id}"),
            Self::Restore(id) => format!("restore:{id}"),
            Self::Purge(id) => format!("purge:{id}"),
            Self::ClearAll => String::from("clear_all"),
            Self::ClearHistory => String::from("clear_history"),
            Self::ToggleDnd => String::from("toggle_dnd"),
            Self::DndTimed { minutes } => format!("dnd:{minutes}m"),
            Self::DndIndefinite => String::from("dnd:indefinite"),
            Self::DndOff => String::from("dnd:off"),
            Self::PlayPause => String::from("play_pause"),
            Self::NextTrack => String::from("next_track"),
            Self::PreviousTrack => String::from("prev_track"),
            Self::Seek { seconds } => format!("seek:{seconds}"),
            Self::FocusPlayer(player) => format!("focus_player:{}", encode_field(player)),
        };
        RowId::new(id)
    }

    /// Decode an id produced by [`encode`](Self::encode).
    ///
    /// Row ids that double as actions (`notif:<id>`, `group:<app>`) are
    /// accepted as well; pure identity ids (`hist:<id>`, the shelf cards) and
    /// foreign ids return `None`.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn parse(raw: &str) -> Option<Self> {
        let (tag, payload) = raw.split_once(':').unwrap_or((raw, ""));
        Some(match tag {
            "open_app" => {
                let (name, icon) = payload.split_once(':')?;
                let icon = if icon.is_empty() {
                    None
                } else {
                    Some(decode_field(icon)?)
                };
                Self::OpenApp {
                    name: decode_field(name)?,
                    icon,
                }
            }
            "notif" => Self::OpenNotification(payload.parse().ok()?),
            "open_file" => Self::OpenFile(decode_field(payload)?),
            "open_dir" => Self::OpenDir(decode_field(payload)?),
            "copy_path" => Self::CopyPath(decode_field(payload)?),
            "copy_image" => Self::CopyImage(decode_field(payload)?),
            "copy_otp" => Self::CopyOtp(decode_field(payload)?),
            "open_url" => Self::OpenUrl(decode_field(payload)?),
            "copy_url" => Self::CopyUrl(decode_field(payload)?),
            "copy_hex" => Self::CopyHex(decode_field(payload)?),
            "copy_body" => Self::CopyBody(payload.parse().ok()?),
            "copy_media" => {
                let (title, artist) = payload.split_once(':')?;
                Self::CopyMedia {
                    title: decode_field(title)?,
                    artist: decode_field(artist)?,
                }
            }
            "action" => {
                let (id, action) = payload.split_once(':')?;
                Self::DbusAction {
                    notification: id.parse().ok()?,
                    action: decode_field(action)?,
                }
            }
            "dismiss" => Self::Dismiss(payload.parse().ok()?),
            "dismiss_app" => Self::DismissApp(decode_field(payload)?),
            "mute_app" => Self::MuteApp(decode_field(payload)?),
            "unmute_app" => Self::UnmuteApp(decode_field(payload)?),
            "priority_app" => Self::PriorityApp(decode_field(payload)?),
            "toggle_thread" | "group" => Self::ToggleThread(decode_field(payload)?),
            "snooze" => {
                let (minutes, id) = payload.split_once(':')?;
                Self::Snooze {
                    notification: id.parse().ok()?,
                    minutes: minutes.parse().ok()?,
                }
            }
            "restore" => Self::Restore(payload.parse().ok()?),
            "unsnooze" => Self::Unsnooze(payload.parse().ok()?),
            "purge" => Self::Purge(payload.parse().ok()?),
            "clear_all" => Self::ClearAll,
            "clear_history" => Self::ClearHistory,
            "toggle_dnd" => Self::ToggleDnd,
            "dnd" => match payload {
                "off" => Self::DndOff,
                "indefinite" => Self::DndIndefinite,
                _ => Self::DndTimed {
                    minutes: payload.strip_suffix('m')?.parse().ok()?,
                },
            },
            "play_pause" => Self::PlayPause,
            "next_track" => Self::NextTrack,
            "prev_track" => Self::PreviousTrack,
            "seek" => Self::Seek {
                seconds: payload.parse().ok()?,
            },
            "focus_player" => Self::FocusPlayer(decode_field(payload)?),
            _ => return None,
        })
    }

    /// Destructive action the Delete key performs on `row_id`.
    ///
    /// Row identity and action ids differ: a notification row is
    /// `notif:<id>` but Delete dismisses it, a thread row is `group:<app>` but
    /// Delete dismisses that app, a snoozed row (`unsnooze:<id>`) is dismissed
    /// rather than woken, a history row (`hist:<id>`) is purged for good, and
    /// the Quick Controls card clears everything clearable.
    #[must_use]
    pub fn for_delete(row_id: &str) -> Option<Self> {
        if row_id == ACTION_QUICK_CONTROLS {
            return Some(Self::ClearAll);
        }
        if let Some(id) = row_id.strip_prefix("hist:") {
            return id.parse().ok().map(Self::Purge);
        }
        match Self::parse(row_id)? {
            Self::OpenNotification(id) | Self::Unsnooze(id) => Some(Self::Dismiss(id)),
            Self::ToggleThread(app) => Some(Self::DismissApp(app)),
            _ => None,
        }
    }

    /// Dropdown target that emits this action.
    #[must_use]
    pub fn target(self, label: impl Into<String>) -> Target {
        Target::new(self.encode(), label)
    }
}

/// Current unix timestamp in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// OPT-8 entity cache keyed by `(id, timestamp, content hash)`: per-tick row
/// rebuilds reuse the extracted entities, and a replacement that reuses an id
/// within the same second still re-scans because the hash moved.
type EntityCache = Mutex<HashMap<(u32, u64, u64), Vec<ExtractedEntity>>>;
fn entity_cache() -> &'static EntityCache {
    static CACHE: OnceLock<EntityCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cached [`notify::extract_entities`] for one notification.
fn cached_entities(item: &NotificationItem) -> Vec<ExtractedEntity> {
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};

    let mut hasher = DefaultHasher::new();
    item.summary.hash(&mut hasher);
    item.body.hash(&mut hasher);
    let key = (item.id, item.timestamp, hasher.finish());
    if let Ok(guard) = entity_cache().lock() {
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }
    let entities = notify::extract_entities(&format!("{} {}", item.summary, item.body));
    if let Ok(mut guard) = entity_cache().lock() {
        if guard.len() > 1024 {
            guard.clear();
        }
        guard.insert(key, entities.clone());
    }
    entities
}

/// OPT-8 relative-time cache: recomputed only on minute rollover.
/// Keyed by `(timestamp, age_minutes)` so a cached entry is exact for its
/// whole minute of age; diffs under two minutes bypass the cache so the
/// `Just Now` → `1m ago` transition stays exact.
fn rel_time_cache() -> &'static Mutex<HashMap<(u64, u64), String>> {
    static CACHE: OnceLock<Mutex<HashMap<(u64, u64), String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cached [`notify::format_relative_time`].
fn cached_rel_time(timestamp: u64, now: u64) -> String {
    let age = now.saturating_sub(timestamp);
    if age < 120 {
        return notify::format_relative_time(timestamp, now);
    }
    let key = (timestamp, age / 60);
    if let Ok(guard) = rel_time_cache().lock() {
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }
    let value = notify::format_relative_time(timestamp, now);
    if let Ok(mut guard) = rel_time_cache().lock() {
        if guard.len() > 2048 {
            guard.clear();
        }
        guard.insert(key, value.clone());
    }
    value
}

/// Append a `Copy Image` target when `path` is a readable image file (any
/// `image/*` by magic bytes, extension fallback). Missing files and
/// non-images add nothing, so text paths keep their current targets.
fn push_copy_image_target(targets: &mut Vec<Target>, path: &str) {
    if notify::image_mime(std::path::Path::new(path)).is_some() {
        let file_name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        targets.push(
            NotifyAction::CopyImage(path.to_string()).target(format!("Copy Image ({file_name})")),
        );
    }
}

/// Push the targets that depend on exactly one notification: extracted
/// entities, the sender's application, its D-Bus action buttons and a hint
/// image.
///
/// Shared by the standalone row and the thread header, which used to carry two
/// copies of this block — and drifted: a fix in one was easy to miss in the
/// other.
fn push_content_targets(
    targets: &mut Vec<Target>,
    item: &NotificationItem,
    sender: &notify::Sender,
    entities: &[ExtractedEntity],
) {
    for entity in entities {
        match entity {
            ExtractedEntity::OtpCode(code) => targets.push(
                NotifyAction::CopyOtp(code.clone()).target(format!("Copy OTP Code ({code})")),
            ),
            ExtractedEntity::Url(url) => {
                targets
                    .push(NotifyAction::OpenUrl(url.clone()).target(format!("Open Link ({url})")));
                targets
                    .push(NotifyAction::CopyUrl(url.clone()).target(format!("Copy Link ({url})")));
            }
            ExtractedEntity::FilePath(path) => {
                let file_name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path);
                targets.push(
                    NotifyAction::OpenFile(path.clone()).target(format!("Open File ({file_name})")),
                );
                targets.push(NotifyAction::OpenDir(path.clone()).target("Open Containing Folder"));
                targets.push(
                    NotifyAction::CopyPath(path.clone()).target(format!("Copy Path ({file_name})")),
                );
                push_copy_image_target(targets, path);
            }
            ExtractedEntity::HexColor(hex) => targets
                .push(NotifyAction::CopyHex(hex.clone()).target(format!("Copy Color ({hex})"))),
        }
    }

    // Only when the sender actually resolves to an application (see
    // [`notify::Sender`]): `notify-send`, `e2e` and other bare labels have
    // nothing to open.
    let app_name = item.app_name.trim();
    if sender.is_openable() {
        targets.push(
            NotifyAction::OpenApp {
                name: app_name.to_string(),
                icon: item.app_icon.clone(),
            }
            .target(format!("Open App ({app_name})")),
        );
    }

    for action in &item.actions {
        // `default` is the spec's body click, which Enter already performs;
        // listing it as a button would duplicate the row's own affordance.
        if action.id == "default" {
            continue;
        }
        targets.push(
            NotifyAction::DbusAction {
                notification: item.id,
                action: action.id.clone(),
            }
            .target(format!("Action: {}", action.title)),
        );
    }

    // The `image-path` file gets the same Copy Image treatment as body paths.
    if let Some(img) = item.image_path.as_deref() {
        push_copy_image_target(targets, img);
    }
}

/// Targets that edit the app as a whole: mute (with the promised one-hour
/// lifetime) or its undo, plus priority ordering. Shared by standalone rows and
/// thread headers so both offer the same controls.
fn push_app_targets(targets: &mut Vec<Target>, app_name: &str, state: &NotifyState, now: u64) {
    if state.is_muted(app_name, now) {
        targets.push(
            NotifyAction::UnmuteApp(app_name.to_string()).target(format!("Unmute {app_name}")),
        );
    } else {
        targets.push(
            NotifyAction::MuteApp(app_name.to_string())
                .target(format!("Silence {app_name} for 1 Hour")),
        );
    }

    let priority_label = if state.is_priority(app_name, now) {
        format!("Remove Priority for {app_name}")
    } else {
        format!("Toggle Priority for {app_name}")
    };
    targets.push(NotifyAction::PriorityApp(app_name.to_string()).target(priority_label));
}

/// Header meta shared by every notification row: relative time plus any active
/// per-app rule, so "why is this quiet?" and "why is this first?" are visible.
fn row_meta(item: &NotificationItem, state: &NotifyState, now: u64) -> String {
    let mut meta = cached_rel_time(item.timestamp, now);
    if item.urgency == Urgency::Critical {
        meta.push_str(" · Critical");
    }
    if state.is_muted(&item.app_name, now) {
        meta.push_str(" · Muted");
    }
    if state.is_priority(&item.app_name, now) {
        meta.push_str(" · Priority");
    }
    meta
}

/// Helper to construct a notification Row with body text preview, graphic image preview, and actions.
fn notification_row_from(item: &NotificationItem, state: &NotifyState, now: u64) -> Row {
    let sender = item.sender();
    let entities = if sender.body_is_content() {
        cached_entities(item)
    } else {
        Vec::new()
    };

    let mut targets = Vec::new();
    push_content_targets(&mut targets, item, &sender, &entities);

    // Management targets
    targets.push(NotifyAction::Dismiss(item.id).target("Dismiss Notification"));
    targets.push(
        NotifyAction::DismissApp(item.app_name.clone())
            .target(format!("Dismiss All from {}", item.app_name)),
    );
    if !item.body.is_empty() {
        targets.push(NotifyAction::CopyBody(item.id).target("Copy Message Text"));
    }
    targets.push(
        NotifyAction::Snooze {
            notification: item.id,
            minutes: 15,
        }
        .target("Snooze 15 Minutes"),
    );
    targets.push(
        NotifyAction::Snooze {
            notification: item.id,
            minutes: 60,
        }
        .target("Snooze 1 Hour"),
    );
    push_app_targets(&mut targets, &item.app_name, state, now);

    let (label, sublabel) = (item.app_name.clone(), Some(item.summary.clone()));
    let meta = row_meta(item, state, now);

    let mut row = Row::with_targets(
        NotifyAction::OpenNotification(item.id).encode(),
        label,
        targets,
        0,
    );
    row.sublabel = sublabel;
    row.meta = Some(meta);
    row.is_default = item.is_pinned || item.urgency == Urgency::Critical;
    row.hide_target_in_header = true;

    if !item.body.is_empty() {
        let clean_body = item.body.replace('\n', " ");
        row.detail = Some(clean_body);
    }

    if let Some(img) = &item.image_path {
        if std::path::Path::new(img).is_file() {
            row.preview_image = Some(img.clone());
        }
    }

    if let Some(prog) = item.progress {
        row.volume = Some(prog.clamp(0.0, 1.0));
    }

    row
}

/// Helper to construct a grouped thread parent row showing the latest notification's preview,
/// with an expansion indicator (`▶` / `▼`) and thread management targets.
fn grouped_parent_row_from(
    app_name: &str,
    items: &[&NotificationItem],
    state: &NotifyState,
    now: u64,
    expanded: bool,
) -> Row {
    let latest = items[0];
    // Grouped by `app_name`, so the newest member's icon hint classifies the
    // whole thread (icons only matter for names that match no desktop `Name`).
    let sender = notify::Sender::classify(app_name, latest.app_icon.as_deref());
    let entities = if sender.body_is_content() {
        cached_entities(latest)
    } else {
        Vec::new()
    };

    let mut targets = Vec::new();

    // 1. Thread expansion target first
    let toggle_title = if expanded {
        String::from("Collapse Thread")
    } else {
        format!("Expand Thread ({} earlier)", items.len() - 1)
    };
    targets.push(NotifyAction::ToggleThread(app_name.to_string()).target(toggle_title));

    // 2. Content targets from the latest notification
    push_content_targets(&mut targets, latest, &sender, &entities);

    // 3. Thread and notification management targets
    targets.push(NotifyAction::Dismiss(latest.id).target("Dismiss Latest Notification"));
    targets.push(
        NotifyAction::DismissApp(app_name.to_string())
            .target(format!("Dismiss All ({})", items.len())),
    );
    if !latest.body.is_empty() {
        targets.push(NotifyAction::CopyBody(latest.id).target("Copy Latest Message Text"));
    }
    targets.push(
        NotifyAction::Snooze {
            notification: latest.id,
            minutes: 15,
        }
        .target("Snooze 15 Minutes"),
    );
    targets.push(
        NotifyAction::Snooze {
            notification: latest.id,
            minutes: 60,
        }
        .target("Snooze 1 Hour"),
    );
    push_app_targets(&mut targets, app_name, state, now);

    let glyph = if expanded { '▼' } else { '▶' };
    let label = format!("{glyph} 󰙯 {app_name} ({})", items.len());
    let sublabel = Some(latest.summary.clone());
    let meta = row_meta(latest, state, now);

    let mut row = Row::with_targets(
        RowId::new(format!("group:{}", encode_field(app_name))),
        label,
        targets,
        0,
    );
    row.sublabel = sublabel;
    row.meta = Some(meta);
    row.is_default = latest.is_pinned || latest.urgency == Urgency::Critical;
    row.hide_target_in_header = true;

    if !latest.body.is_empty() {
        let clean_body = latest.body.replace('\n', " ");
        row.detail = Some(clean_body);
    }

    if let Some(img) = &latest.image_path {
        if std::path::Path::new(img).is_file() {
            row.preview_image = Some(img.clone());
        }
    }

    if let Some(prog) = latest.progress {
        row.volume = Some(prog.clamp(0.0, 1.0));
    }

    row
}

/// Helper to construct an indented child row for expanded thread items.
fn child_notification_row_from(
    item: &NotificationItem,
    state: &NotifyState,
    now: u64,
    is_last: bool,
) -> Row {
    let mut row = notification_row_from(item, state, now);
    let prefix = if is_last { "  └─ " } else { "  ├─ " };
    let summary_clean = item.summary.trim();
    let body_clean = item.body.replace('\n', " ");
    let body_clean = body_clean.trim();

    row.label = if body_clean.is_empty() || summary_clean == body_clean {
        format!("{prefix}{summary_clean}")
    } else if summary_clean.is_empty() {
        format!("{prefix}{body_clean}")
    } else {
        format!("{prefix}{summary_clean}: {body_clean}")
    };
    row.sublabel = None;
    row.detail = None;
    row.compact = true;
    row
}

/// Rows for notifications hidden by a live snooze.
///
/// A snooze is a reminder, not a deletion: without these rows the entry
/// disappears from both tabs until its deadline, with no way to wake it early.
/// Enter on the row wakes it (the row id is the `Unsnooze` action), and the
/// meta carries the remaining time.
fn push_snoozed_rows(rows: &mut Vec<Row>, state: &NotifyState, now: u64) {
    let mut snoozed: Vec<&NotificationItem> = state
        .notifications
        .iter()
        .filter(|n| !n.is_dismissed && n.is_snoozed_at(now))
        .collect();
    snoozed.sort_by_key(|n| n.snooze_until.unwrap_or(u64::MAX));

    for item in snoozed {
        let remaining = item
            .snooze_until
            .map_or(0, |until| until.saturating_sub(now));
        let mut row = notification_row_from(item, state, now);
        row.id = NotifyAction::Unsnooze(item.id).encode();
        row.label = format!("󰒲 {}", item.app_name);
        row.meta = Some(format!("Snoozed · {}m left", remaining.div_ceil(60)));
        // Re-snoozing a sleeping row makes no sense; waking it does.
        row.targets
            .retain(|t| !t.id.as_str().starts_with("snooze:"));
        row.targets
            .insert(0, NotifyAction::Unsnooze(item.id).target("Wake Now"));
        rows.push(row);
    }
}

/// Construct the `[Feed]` tab.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn feed_tab_from(state: &NotifyState, now: u64) -> Tab {
    let mut rows = Vec::new();

    // 1. Top Quick Controls Shelf
    let dnd_label = if state.controls.dnd.is_active(now) {
        if let Some(rem) = state.controls.dnd.remaining_secs(now) {
            format!("DND: On ({}m)", rem / 60)
        } else {
            String::from("DND: On")
        }
    } else {
        String::from("DND: Off")
    };

    let active_count = state
        .notifications
        .iter()
        .filter(|n| n.is_active(now))
        .count();
    let archived_count = state
        .notifications
        .iter()
        .filter(|n| n.is_archived(now))
        .count();

    let mut quick_targets = vec![
        NotifyAction::ToggleDnd.target(format!("Toggle {dnd_label}")),
        NotifyAction::DndTimed { minutes: 25 }.target("Pomodoro Sprint (25 Mins)"),
        NotifyAction::DndTimed { minutes: 60 }.target("Deep Work (1 Hour)"),
        NotifyAction::DndTimed { minutes: 120 }.target("Focus Session (2 Hours)"),
        NotifyAction::DndIndefinite.target("Silence Until Turned Off"),
        NotifyAction::DndOff.target("Turn DND Off"),
        NotifyAction::ClearAll.target(format!("Clear All ({active_count} Active)")),
    ];
    if archived_count > 0 {
        quick_targets
            .push(NotifyAction::ClearHistory.target(format!("Clear History ({archived_count})")));
    }
    let mut quick_row = Row::with_targets(
        RowId::new(ACTION_QUICK_CONTROLS),
        "Quick Controls",
        quick_targets,
        0,
    );
    quick_row.sublabel = Some(format!("󰂛 {dnd_label}  •  󰅖 Clear ({active_count})"));
    quick_row.meta = Some(format!("{active_count} Active"));
    if let Some(frac) = state.controls.dnd.fraction_remaining(now) {
        quick_row.volume = Some(frac);
    }
    quick_row.hide_target_in_header = true;
    rows.push(quick_row);

    // 2. MPRIS Media Player Card (if present)
    if let Some(mpris) = &state.controls.mpris {
        let title_label = format!("󰝚 {}", mpris.title);
        let sublabel = if mpris.artist.is_empty() || mpris.artist == "Unknown Artist" {
            None
        } else {
            Some(mpris.artist.clone())
        };

        let time_meta = if mpris.is_live || mpris.length_secs == 0 {
            format!(
                "{} · Live · {}",
                notify::format_duration(mpris.position_secs),
                mpris.player
            )
        } else {
            format!(
                "{} / {} · {}",
                notify::format_duration(mpris.position_secs),
                notify::format_duration(mpris.length_secs),
                mpris.player
            )
        };

        let volume_frac = if !mpris.is_live && mpris.length_secs > 0 {
            #[allow(clippy::cast_precision_loss)]
            Some(((mpris.position_secs as f32) / (mpris.length_secs as f32)).clamp(0.0, 1.0))
        } else {
            None
        };

        let mpris_targets = vec![
            NotifyAction::PlayPause.target(if mpris.is_playing {
                "Pause Playback"
            } else {
                "Resume Playback"
            }),
            NotifyAction::NextTrack.target("Next Track"),
            NotifyAction::PreviousTrack.target("Previous Track"),
            NotifyAction::Seek { seconds: 10 }.target("Seek Forward +10s"),
            NotifyAction::Seek { seconds: -10 }.target("Seek Backward -10s"),
            NotifyAction::FocusPlayer(mpris.player.clone())
                .target(format!("Focus {}", mpris.player)),
            NotifyAction::CopyMedia {
                title: mpris.title.clone(),
                artist: mpris.artist.clone(),
            }
            .target("Copy Media Info"),
        ];

        let mut mpris_row = Row::with_targets(
            RowId::new(ACTION_MPRIS_TRACK),
            title_label,
            mpris_targets,
            0,
        );
        mpris_row.sublabel = sublabel;
        mpris_row.meta = Some(time_meta);
        mpris_row.volume = volume_frac;
        mpris_row.hide_target_in_header = true;
        rows.push(mpris_row);
    }

    // 3. Notification Feed Rows
    let active_notifs: Vec<&NotificationItem> = state
        .notifications
        .iter()
        .filter(|n| n.is_active(now))
        .collect();

    // 3a. Sticky Critical Alerts at Top (most recent first)
    let (mut critical_notifs, normal_notifs): (Vec<&NotificationItem>, Vec<&NotificationItem>) =
        active_notifs
            .into_iter()
            .partition(|n| n.urgency == Urgency::Critical);

    critical_notifs.sort_by_key(|n| std::cmp::Reverse(n.timestamp));

    for item in critical_notifs {
        rows.push(notification_row_from(item, state, now));
    }

    // 3b. Grouped Non-Critical Active Stream by Application
    let mut app_groups: std::collections::BTreeMap<&str, Vec<&NotificationItem>> =
        std::collections::BTreeMap::new();
    for item in normal_notifs {
        app_groups
            .entry(item.app_name.as_str())
            .or_default()
            .push(item);
    }

    let mut sorted_apps: Vec<(&str, Vec<&NotificationItem>)> = app_groups.into_iter().collect();
    // Priority apps float above ordinary ones; recency orders each partition.
    sorted_apps.sort_by_key(|(app, items)| {
        (
            !state.is_priority(app, now),
            std::cmp::Reverse(items.iter().map(|n| n.timestamp).max().unwrap_or(0)),
        )
    });

    for (app_name, mut items) in sorted_apps {
        items.sort_by_key(|n| std::cmp::Reverse(n.timestamp));
        if items.len() > 1 {
            let expanded = is_thread_expanded(app_name);
            rows.push(grouped_parent_row_from(
                app_name, &items, state, now, expanded,
            ));

            if expanded {
                let older_count = items.len() - 1;
                for (i, item) in items[1..].iter().enumerate() {
                    let is_last = i + 1 == older_count;
                    rows.push(child_notification_row_from(item, state, now, is_last));
                }
            }
        } else if let Some(item) = items.first() {
            rows.push(notification_row_from(item, state, now));
        }
    }

    push_snoozed_rows(&mut rows, state, now);

    if rows.is_empty() {
        rows.push(Row::new(
            RowId::new("empty:notif"),
            "No Active Notifications",
        ));
    }

    let mut tab = Tab::with_rows(TAB_FEED, rows);
    tab.bare_rows = false;
    tab.filterable = false; // Search does not belong in notification center
    tab.deletable = true;
    tab
}

/// Construct the `[History]` tab.
#[must_use]
pub fn history_tab_from(state: &NotifyState, now: u64) -> Tab {
    let mut rows = Vec::new();

    let mut dismissed_items: Vec<&NotificationItem> = state
        .notifications
        .iter()
        .filter(|n| n.is_archived(now))
        .collect();
    dismissed_items.sort_by_key(|n| std::cmp::Reverse(n.timestamp));

    for item in dismissed_items.iter().take(30) {
        let rel_time = cached_rel_time(item.timestamp, now);
        let mut targets = vec![
            NotifyAction::Restore(item.id).target("Restore Notification"),
            NotifyAction::CopyBody(item.id).target("Copy Message Text"),
        ];
        if let Some(img) = item.image_path.as_deref() {
            push_copy_image_target(&mut targets, img);
        }

        let mut row = Row::with_targets(
            RowId::new(format!("hist:{}", item.id)),
            item.app_name.clone(),
            targets,
            0,
        );
        row.sublabel = Some(item.summary.clone());
        let archive_reason = if item.is_dismissed {
            "Dismissed"
        } else {
            "Expired"
        };
        row.meta = Some(format!("{rel_time} · {archive_reason}"));
        row.offline = true;
        row.hide_target_in_header = true;
        if !item.body.is_empty() {
            let clean_body = item.body.replace('\n', " ");
            row.detail = Some(clean_body);
        }
        if let Some(img) = &item.image_path {
            if std::path::Path::new(img).is_file() {
                row.preview_image = Some(img.clone());
            }
        }
        rows.push(row);
    }

    if rows.is_empty() {
        rows.push(Row::new(RowId::new("empty:history"), "History is Empty"));
    }

    let mut tab = Tab::with_rows(TAB_HISTORY, rows);
    tab.bare_rows = false;
    tab.filterable = false;
    tab.deletable = true;
    tab
}

/// Build the 2-tab Notification Center menu (`Notifications`, `History`).
#[must_use]
pub fn menu_from(state: &NotifyState, now: u64) -> Menu {
    let tabs = vec![feed_tab_from(state, now), history_tab_from(state, now)];
    crate::menu(PROVIDER, tabs)
}

/// Construct menu using live disk state and live hardware/MPRIS probes.
#[must_use]
pub fn menu() -> Menu {
    let mut state = notify::load_state(None);
    notify::probe_quick_controls(&mut state.controls);
    menu_from(&state, now_secs())
}

/// In-place tick hook for background updates.
///
/// OPT-6: throttled to 2 s in [`crate::providers::tick_hook`] so the disk
/// read plus `playerctl`/`wpctl` probes do not run every second. OPT-9:
/// reuses row allocations in place (`mem::replace` + diff) and keeps
/// focus/scroll stable per tab.
pub fn refresh(menu: &mut Menu) {
    let mut state = notify::load_state(None);
    notify::probe_quick_controls(&mut state.controls);
    let now = now_secs();

    let focused_id = menu.app.focused_row().map(|row| row.id.clone());
    let new_menu = menu_from(&state, now);
    for (i, new_tab) in new_menu.app.tabs.into_iter().enumerate() {
        if let Some(tab) = menu.app.tabs.get_mut(i) {
            crate::providers::sync_rows_in_place(&mut tab.rows, new_tab.rows);
            tab.state.focus = tab.state.focus.min(tab.rows.len().saturating_sub(1));
        }
    }
    crate::providers::restore_focus(menu, focused_id);
}

/// Report on execution outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteReport {
    pub action_id: String,
    pub detail: Option<String>,
    pub should_close: bool,
}

/// Helper to copy text to system clipboard via `wl-copy` or `xclip` detached.
pub fn copy_to_clipboard(text: &str) {
    let mut wl = std::process::Command::new("wl-copy");
    wl.arg(text)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if crate::spawn::spawn_and_reap(&mut wl).is_ok() {
        return;
    }

    if let Ok(mut child) = std::process::Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write as _;
            let _ = stdin.write_all(text.as_bytes());
        }
        crate::spawn::reap_detached(child);
    }
}

/// Execute a selected action or target.
///
/// # Errors
/// Returns error if state file cannot be updated.
#[allow(clippy::too_many_lines)]
pub fn execute(
    action_id: &str,
    target_title: &str,
    state_path: Option<&std::path::Path>,
) -> anyhow::Result<ExecuteReport> {
    notify::log_notify_trace(&format!(
        "[EXECUTE] action_id='{action_id}', title='{target_title}'"
    ));
    let mut state = notify::load_state(state_path);
    let now = now_secs();
    let mut should_close = false;
    // Collected during the action, shown once the state is settled: the
    // feedback is a transient toast (nothing persisted), so it cannot affect
    // the store either way, but a user should only see it after the attempt.
    let mut failure: Option<notify::Failure> = None;

    // The media card's row id doubles as its play/pause action; it is a row,
    // not a dropdown target, so it stays outside the `encode`/`parse` pair.
    let parsed = if action_id == ACTION_MPRIS_TRACK {
        Some(NotifyAction::PlayPause)
    } else {
        NotifyAction::parse(action_id)
    };

    if let Some(action) = parsed {
        match action {
            NotifyAction::OpenNotification(id) => {
                // Enter on a notification is the spec's body click: a sender
                // that registered a `default` action expects to hear about it,
                // and opening its app instead would silently drop the intent.
                let has_default = state
                    .notifications
                    .iter()
                    .find(|n| n.id == id)
                    .is_some_and(|n| n.actions.iter().any(|a| a.id == "default"));
                if has_default && notify::invoke_dbus_action(id, "default") {
                    // The daemon archived the entry; our snapshot is stale.
                    state = notify::load_state(state_path);
                } else if let Some(n) = state.notifications.iter().find(|n| n.id == id) {
                    notify::open_application_with(&n.app_name, n.app_icon.as_deref());
                }
                should_close = true;
            }
            NotifyAction::OpenApp { name, icon } => {
                notify::open_application_with(&name, icon.as_deref());
                should_close = true;
            }
            NotifyAction::OpenFile(path) => {
                failure = notify::open_file(&path);
                should_close = true;
            }
            NotifyAction::OpenDir(path) => {
                failure = notify::open_dir(&path);
                should_close = true;
            }
            NotifyAction::OpenUrl(url) => {
                notify::open_url(&url);
                should_close = true;
            }
            NotifyAction::CopyBody(id) => {
                if let Some(n) = state.notifications.iter().find(|n| n.id == id) {
                    let text = if n.body.is_empty() {
                        n.summary.clone()
                    } else {
                        format!("{}\n{}", n.summary, n.body)
                    };
                    copy_to_clipboard(&text);
                }
            }
            NotifyAction::CopyOtp(code) => copy_to_clipboard(&code),
            NotifyAction::CopyUrl(url) => copy_to_clipboard(&url),
            NotifyAction::CopyPath(path) => copy_to_clipboard(&path),
            NotifyAction::CopyHex(hex) => copy_to_clipboard(&hex),
            NotifyAction::CopyMedia { title, artist } => {
                copy_to_clipboard(&format!("{title} - {artist}"));
            }
            NotifyAction::CopyImage(path) => {
                // Image bytes with their MIME type (not the path text). Like
                // the other `copy_*` targets this stays open for repeated
                // copies; a file lost since listing time reports instead of
                // failing silently.
                let mime = notify::image_mime(std::path::Path::new(&path)).unwrap_or("image/png");
                if !notify::copy_image_file(&path, mime) {
                    failure = Some(notify::Failure::new("Image not found", &path));
                }
            }
            NotifyAction::Dismiss(id) => {
                if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                    n.archive(now);
                }
            }
            NotifyAction::DismissApp(app) => {
                for n in &mut state.notifications {
                    if n.app_name == app {
                        n.archive(now);
                    }
                }
            }
            NotifyAction::ToggleThread(app) => toggle_thread_expanded(&app),
            NotifyAction::MuteApp(app) => state.mute_app(&app, now + 60 * 60),
            NotifyAction::UnmuteApp(app) => state.unmute_app(&app),
            NotifyAction::PriorityApp(app) => state.toggle_priority(&app),
            NotifyAction::Restore(id) => {
                if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                    n.restore();
                }
            }
            NotifyAction::Snooze {
                notification,
                minutes,
            } => {
                if let Some(n) = state
                    .notifications
                    .iter_mut()
                    .find(|n| n.id == notification)
                {
                    n.snooze(minutes, now);
                }
            }
            NotifyAction::Unsnooze(id) => {
                if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                    n.unsnooze();
                }
            }
            NotifyAction::DbusAction {
                notification,
                action,
            } => {
                // The signal has to come from the daemon (the name owner), so
                // deliver the click there. Without a daemon — or if it rejects
                // a stale button — acknowledge locally so the drawer still
                // closes instead of lying about a live action.
                if notify::invoke_dbus_action(notification, &action) {
                    state = notify::load_state(state_path);
                } else if let Some(n) = state
                    .notifications
                    .iter_mut()
                    .find(|n| n.id == notification)
                {
                    n.archive(now);
                }
                should_close = true;
            }
            NotifyAction::ClearAll => {
                for n in &mut state.notifications {
                    if !n.is_pinned && n.urgency != Urgency::Critical {
                        n.archive(now);
                    }
                }
            }
            NotifyAction::Purge(id) => state.purge(id),
            NotifyAction::ClearHistory => state.clear_history(now),
            NotifyAction::DndTimed { minutes } => {
                let secs = u64::from(minutes) * 60;
                state.controls.dnd = DndState::Timed {
                    until: now + secs,
                    total_secs: secs,
                };
            }
            NotifyAction::DndOff => state.controls.dnd = DndState::Off,
            NotifyAction::DndIndefinite => state.controls.dnd = DndState::Indefinite,
            NotifyAction::ToggleDnd => {
                state.controls.dnd = if state.controls.dnd.is_active(now) {
                    DndState::Off
                } else {
                    DndState::Indefinite
                };
            }
            NotifyAction::PlayPause => {
                let _ = std::process::Command::new("playerctl")
                    .arg("play-pause")
                    .status();
            }
            NotifyAction::NextTrack => {
                let _ = std::process::Command::new("playerctl").arg("next").status();
            }
            NotifyAction::PreviousTrack => {
                let _ = std::process::Command::new("playerctl")
                    .arg("previous")
                    .status();
            }
            NotifyAction::Seek { seconds } => {
                let magnitude = seconds.unsigned_abs();
                let sign = if seconds >= 0 { '+' } else { '-' };
                let _ = std::process::Command::new("playerctl")
                    .args(["position", &format!("{magnitude}{sign}")])
                    .status();
            }
            NotifyAction::FocusPlayer(player) => {
                notify::open_application(&player);
                should_close = true;
            }
        }
    }

    notify::save_state(&state, state_path)?;

    // After the save, never before: this store is a full overwrite, so any
    // state written by the daemon before this point would be lost. The
    // feedback toast is transient (nothing is stored), but reporting only
    // once the action and the bookkeeping are done keeps the two orderings
    // independent.
    if let Some(failure) = failure {
        notify::report_failure(failure.summary, &failure.detail);
    }

    Ok(ExecuteReport {
        action_id: action_id.to_string(),
        detail: Some(target_title.to_string()),
        should_close,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_tab_builds_expected_wiremix_rows_and_targets() {
        let mut state = NotifyState::default();
        state.notifications.push(NotificationItem::new(
            1,
            "Discord",
            "#dev-team",
            "Hey please check PR #142 at https://github.com/gom3az/flex with code 849201",
            Urgency::Normal,
        ));
        state.notifications.push(NotificationItem::new(
            2,
            "System",
            "Low Battery Warning",
            "Plug in AC adapter soon",
            Urgency::Critical,
        ));

        let tab = feed_tab_from(&state, 1000);
        assert_eq!(tab.name, TAB_FEED);
        assert!(!tab.bare_rows);
        assert!(!tab.filterable, "No search prompt in notification drawer");

        assert_eq!(tab.rows[0].id.as_str(), ACTION_QUICK_CONTROLS);
        assert_eq!(tab.rows[0].label, "Quick Controls");

        let r1 = &tab.rows[1];
        assert_eq!(r1.label, "System");
        assert_eq!(r1.sublabel.as_deref(), Some("Low Battery Warning"));
        assert!(r1.is_default);

        let r2 = &tab.rows[2];
        assert_eq!(r2.label, "Discord");
        assert_eq!(r2.sublabel.as_deref(), Some("#dev-team"));
        assert!(r2.targets.iter().any(|t| t.title.contains("849201")));
        assert!(r2
            .targets
            .iter()
            .any(|t| t.title.contains("https://github.com")));
    }

    #[test]
    fn senders_without_an_app_get_no_open_app_target() {
        let mut state = NotifyState::default();
        state.notifications.push(NotificationItem::new(
            9092,
            "notify-send",
            "Build finished",
            "3 warnings",
            Urgency::Normal,
        ));
        state.notifications.push(NotificationItem::new(
            9093,
            "e2e",
            "e2e summary",
            "e2e body",
            Urgency::Normal,
        ));

        let tab = feed_tab_from(&state, 1000);
        for id in ["notif:9092", "notif:9093"] {
            let row = tab
                .rows
                .iter()
                .find(|r| r.id.as_str() == id)
                .expect("notif row");
            let ids: Vec<&str> = row.targets.iter().map(|t| t.id.as_str()).collect();
            assert!(
                !ids.iter().any(|t| t.starts_with("open_app:")),
                "{id} has no application to open: {ids:?}"
            );
        }
    }

    #[test]
    fn flex_feedback_notifications_have_no_open_app_target() {
        let mut state = NotifyState::default();
        state.notifications.push(NotificationItem::new(
            9091,
            "flex-notify",
            "File not found",
            "/tmp/gone.png",
            Urgency::Normal,
        ));

        let tab = feed_tab_from(&state, 1000);
        let row = tab
            .rows
            .iter()
            .find(|r| r.id.as_str() == "notif:9091")
            .expect("notif row");
        let ids: Vec<&str> = row.targets.iter().map(|t| t.id.as_str()).collect();
        assert!(
            !ids.iter().any(|id| id.starts_with("open_app:")),
            "flex's own feedback is not an application: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.starts_with("open_file:")
                || id.starts_with("open_dir:")
                || id.starts_with("copy_path:")),
            "the failed path must not offer open targets that loop back: {ids:?}"
        );
    }

    #[test]
    fn feed_row_offers_copy_image_for_image_paths_only() {
        let dir = std::env::temp_dir().join(format!("flex-notify-imgrow-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let shot = dir.join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\npixels").expect("fixture");
        let note = dir.join("note.txt");
        std::fs::write(&note, b"hello").expect("fixture");

        let mut state = NotifyState::default();
        // Ids 101+: the entity cache is process-global keyed by
        // `(id, timestamp)`, so fixture ids must not collide with other
        // tests stamping the same second.
        state.notifications.push(NotificationItem::new(
            101,
            "Shots",
            "Screenshot saved",
            format!("{} {}", shot.display(), note.display()),
            Urgency::Normal,
        ));
        let tab = feed_tab_from(&state, 1000);
        let row = tab
            .rows
            .iter()
            .find(|r| r.id.as_str() == "notif:101")
            .expect("notif row");
        let ids: Vec<&str> = row.targets.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ids.iter()
                .any(|id| id.starts_with("copy_image:") && id.ends_with("shot.png")),
            "image path gets a Copy Image target: {ids:?}"
        );
        assert!(
            !ids.iter()
                .any(|id| id.starts_with("copy_image:") && id.ends_with("note.txt")),
            "text path keeps text-only targets: {ids:?}"
        );
        assert!(
            ids.iter().any(|id| id.starts_with("copy_path:")),
            "existing Copy Path target stays: {ids:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feed_row_offers_copy_image_for_hint_images() {
        let dir = std::env::temp_dir().join(format!("flex-notify-hintrow-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let shot = dir.join("hint.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\npixels").expect("fixture");

        let mut state = NotifyState::default();
        state.notifications.push(
            NotificationItem::new(102, "Shots", "Screenshot saved", "", Urgency::Normal)
                .with_image(shot.to_string_lossy().into_owned()),
        );
        let tab = feed_tab_from(&state, 1000);
        let row = tab
            .rows
            .iter()
            .find(|r| r.id.as_str() == "notif:102")
            .expect("notif row");
        assert!(
            row.targets
                .iter()
                .any(|t| t.id.as_str().starts_with("copy_image:")),
            "hint image gets a Copy Image target"
        );
        assert!(
            row.preview_image.is_some(),
            "graphic preview still attached"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn execute_copy_image_reports_without_closing() {
        let dir = std::env::temp_dir().join(format!("flex-notify-imgexec-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");

        let mut state = NotifyState::default();
        state.notifications.push(NotificationItem::new(
            103,
            "Shots",
            "Screenshot saved",
            "Text",
            Urgency::Normal,
        ));
        notify::save_state(&state, Some(&path)).unwrap();

        // Missing file: reports the miss instead of erroring; drawer stays open.
        let report = execute(
            "copy_image:/nonexistent/ghost.png",
            "Copy Image (ghost.png)",
            Some(&path),
        )
        .unwrap();
        assert_eq!(report.action_id, "copy_image:/nonexistent/ghost.png");
        assert!(!report.should_close);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn execution_clears_non_critical_notifications() {
        let dir = std::env::temp_dir().join(format!("flex-notify-exec-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");

        let mut state = NotifyState::default();
        state.notifications.push(NotificationItem::new(
            1,
            "Discord",
            "Chat",
            "Text",
            Urgency::Normal,
        ));
        state.notifications.push(NotificationItem::new(
            2,
            "System",
            "Battery",
            "Text",
            Urgency::Critical,
        ));
        notify::save_state(&state, Some(&path)).unwrap();

        execute(
            NotifyAction::ClearAll.encode().as_str(),
            "Clear All",
            Some(&path),
        )
        .unwrap();

        let updated = notify::load_state(Some(&path));
        assert!(updated.notifications[0].is_dismissed);
        assert!(
            !updated.notifications[1].is_dismissed,
            "Critical notifications protected from Clear All"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn action_ids_round_trip_through_the_codec() {
        let actions = vec![
            NotifyAction::OpenApp {
                name: "Crash Reporting System".to_string(),
                icon: Some("dialog-error".to_string()),
            },
            NotifyAction::OpenApp {
                name: "Screenshot".to_string(),
                icon: None,
            },
            NotifyAction::OpenNotification(17),
            NotifyAction::OpenFile("/home/u/My Documents/a b.png".to_string()),
            NotifyAction::OpenDir("/home/u/My Documents".to_string()),
            NotifyAction::CopyPath("/tmp/100%/a:b.txt".to_string()),
            NotifyAction::CopyImage("/tmp/ümlaut.png".to_string()),
            NotifyAction::CopyOtp("042 991".to_string()),
            NotifyAction::OpenUrl("https://example.com/a?b=c:d".to_string()),
            NotifyAction::CopyUrl("https://example.com/a b".to_string()),
            NotifyAction::CopyHex("#ff00aa".to_string()),
            NotifyAction::CopyBody(3),
            NotifyAction::CopyMedia {
                title: "Get Lucky: Remix".to_string(),
                artist: "Daft Punk".to_string(),
            },
            NotifyAction::DbusAction {
                notification: 9,
                action: "default: open".to_string(),
            },
            NotifyAction::Dismiss(1),
            NotifyAction::DismissApp("Discord (Canary)".to_string()),
            NotifyAction::MuteApp("System".to_string()),
            NotifyAction::UnmuteApp("System".to_string()),
            NotifyAction::PriorityApp("System".to_string()),
            NotifyAction::ToggleThread("Dev Team".to_string()),
            NotifyAction::Snooze {
                notification: 4,
                minutes: 15,
            },
            NotifyAction::Restore(11),
            NotifyAction::Unsnooze(13),
            NotifyAction::Purge(12),
            NotifyAction::ClearAll,
            NotifyAction::ClearHistory,
            NotifyAction::ToggleDnd,
            NotifyAction::DndTimed { minutes: 120 },
            NotifyAction::DndIndefinite,
            NotifyAction::DndOff,
            NotifyAction::PlayPause,
            NotifyAction::NextTrack,
            NotifyAction::PreviousTrack,
            NotifyAction::Seek { seconds: -10 },
            NotifyAction::FocusPlayer("spotify".to_string()),
        ];

        for action in actions {
            let encoded = action.encode();
            assert!(
                !encoded.as_str().chars().any(char::is_whitespace),
                "{} must stay a single ACTION: field",
                encoded.as_str()
            );
            assert_eq!(
                NotifyAction::parse(encoded.as_str()),
                Some(action.clone()),
                "round trip for {}",
                encoded.as_str()
            );
        }
    }

    #[test]
    fn history_archives_sender_expired_notifications() {
        let mut state = NotifyState::default();
        let mut expired = NotificationItem::new(5, "App", "short lived", "", Urgency::Normal);
        expired.expires_at = Some(500);
        state.notifications.push(expired);

        let feed = feed_tab_from(&state, 600);
        assert!(
            !feed.rows.iter().any(|r| r.id.as_str() == "notif:5"),
            "an elapsed sender timeout leaves the feed"
        );

        let history = history_tab_from(&state, 600);
        let row = history
            .rows
            .iter()
            .find(|r| r.id.as_str() == "hist:5")
            .expect("expired notification is archived");
        assert!(
            row.meta.as_deref().is_some_and(|m| m.ends_with("Expired")),
            "the archive explains why it left the feed: {:?}",
            row.meta
        );
    }

    #[test]
    fn clear_all_count_uses_the_same_active_predicate_as_the_feed() {
        let mut state = NotifyState::default();
        state
            .notifications
            .push(NotificationItem::new(1, "A", "fresh", "", Urgency::Normal));

        let mut sleeping = NotificationItem::new(2, "B", "sleeping", "", Urgency::Normal);
        sleeping.is_snoozed = true;
        sleeping.snooze_until = Some(200);
        state.notifications.push(sleeping);

        let mut awake = NotificationItem::new(3, "C", "woke up", "", Urgency::Normal);
        awake.is_snoozed = true;
        awake.snooze_until = Some(50);
        state.notifications.push(awake);

        let tab = feed_tab_from(&state, 100);

        // The feed hides only the live snooze...
        assert!(tab.rows.iter().any(|r| r.id.as_str() == "notif:1"));
        assert!(
            !tab.rows.iter().any(|r| r.id.as_str() == "notif:2"),
            "a live snooze stays out of the feed"
        );
        assert!(
            tab.rows.iter().any(|r| r.id.as_str() == "notif:3"),
            "an elapsed snooze returns to the feed"
        );

        // ...and the counter agrees with it instead of counting the snooze as
        // inactive.
        assert_eq!(tab.rows[0].meta.as_deref(), Some("2 Active"));
        assert!(tab.rows[0]
            .targets
            .iter()
            .any(|t| t.title.contains("Clear All (2 Active)")));
    }

    #[test]
    fn delete_key_maps_each_row_kind_to_its_destructive_action() {
        assert_eq!(
            NotifyAction::for_delete("notif:7"),
            Some(NotifyAction::Dismiss(7))
        );
        assert_eq!(
            NotifyAction::for_delete("group:Discord"),
            Some(NotifyAction::DismissApp("Discord".to_string()))
        );
        assert_eq!(
            NotifyAction::for_delete("hist:9"),
            Some(NotifyAction::Purge(9)),
            "history rows are purged, not re-dismissed"
        );
        assert_eq!(
            NotifyAction::for_delete("unsnooze:5"),
            Some(NotifyAction::Dismiss(5)),
            "Delete on a sleeping row dismisses it, it does not wake it"
        );
        assert_eq!(
            NotifyAction::for_delete(ACTION_QUICK_CONTROLS),
            Some(NotifyAction::ClearAll)
        );
        assert_eq!(NotifyAction::for_delete("mpris:track"), None);
        assert_eq!(NotifyAction::for_delete("hist:not-a-number"), None);
    }

    #[test]
    fn priority_apps_float_first_and_mute_offers_an_undo() {
        let mut older = NotificationItem::new(1, "Discord", "older", "", Urgency::Normal);
        older.timestamp = 100;
        let mut newer = NotificationItem::new(2, "Slack", "newer", "", Urgency::Normal);
        newer.timestamp = 200;

        let mut state = NotifyState {
            notifications: vec![older, newer],
            priority_apps: vec![notify::AppRule {
                app: "Discord".to_string(),
                until: None,
            }],
            ..NotifyState::default()
        };
        state.mute_app("Slack", 1_000 + 3_600);

        let tab = feed_tab_from(&state, 500);
        assert_eq!(tab.rows[0].id.as_str(), ACTION_QUICK_CONTROLS);
        assert_eq!(
            tab.rows[1].id.as_str(),
            "notif:1",
            "the priority app floats above the newer one"
        );
        assert!(tab.rows[1]
            .meta
            .as_deref()
            .is_some_and(|m| m.ends_with("Priority")));
        assert!(tab.rows[1]
            .targets
            .iter()
            .any(|t| t.title == "Remove Priority for Discord"));

        assert_eq!(tab.rows[2].id.as_str(), "notif:2");
        assert!(
            tab.rows[2]
                .meta
                .as_deref()
                .is_some_and(|m| m.ends_with("Muted")),
            "a muted app still shows in the feed, marked as quiet"
        );
        assert!(tab.rows[2]
            .targets
            .iter()
            .any(|t| t.id.as_str().starts_with("unmute_app:")));

        // No archive yet: Clear History is absent from the shelf.
        assert!(!tab.rows[0]
            .targets
            .iter()
            .any(|t| t.id.as_str() == "clear_history"));
    }

    #[test]
    fn clear_history_target_appears_only_for_archived_rows() {
        let mut state = NotifyState::default();
        let mut dismissed = NotificationItem::new(1, "A", "gone", "", Urgency::Normal);
        dismissed.is_dismissed = true;
        state.notifications.push(dismissed);

        let tab = feed_tab_from(&state, 100);
        let target = tab.rows[0]
            .targets
            .iter()
            .find(|t| t.id.as_str() == "clear_history")
            .expect("archived rows offer Clear History");
        assert_eq!(target.title, "Clear History (1)");
    }

    #[test]
    fn snoozed_rows_stay_visible_with_their_remaining_time() {
        let mut sleeping = NotificationItem::new(1, "Slack", "standup in 10", "", Urgency::Normal);
        sleeping.timestamp = 100;
        sleeping.snooze(15, 1_000);
        let mut awake = NotificationItem::new(2, "Discord", "ping", "", Urgency::Normal);
        awake.timestamp = 100;

        let state = NotifyState {
            notifications: vec![sleeping, awake],
            ..NotifyState::default()
        };
        let tab = feed_tab_from(&state, 1_000);

        assert_eq!(tab.rows[0].id.as_str(), ACTION_QUICK_CONTROLS);
        assert_eq!(
            tab.rows[1].id.as_str(),
            "notif:2",
            "a live snooze leaves the active stream"
        );
        let snoozed = &tab.rows[2];
        assert_eq!(snoozed.id.as_str(), "unsnooze:1");
        assert_eq!(snoozed.meta.as_deref(), Some("Snoozed · 15m left"));
        assert!(snoozed.targets.iter().any(|t| t.title == "Wake Now"));
        assert!(
            !snoozed
                .targets
                .iter()
                .any(|t| t.id.as_str().starts_with("snooze:")),
            "a sleeping row offers no second snooze"
        );
    }

    #[test]
    fn the_default_action_is_the_body_click_not_a_button() {
        let mut item = NotificationItem::new(1, "Firefox", "Done", "", Urgency::Normal);
        item.actions = vec![
            notify::NotificationAction {
                id: "default".to_string(),
                title: "Open".to_string(),
            },
            notify::NotificationAction {
                id: "reply".to_string(),
                title: "Reply".to_string(),
            },
        ];
        let state = NotifyState {
            notifications: vec![item],
            ..NotifyState::default()
        };
        let tab = feed_tab_from(&state, 100);
        let row = &tab.rows[1];
        assert!(row
            .targets
            .iter()
            .any(|t| t.id.as_str() == "action:1:reply"));
        assert!(
            !row.targets
                .iter()
                .any(|t| t.id.as_str() == "action:1:default"),
            "Enter already performs the default action"
        );
    }

    #[test]
    fn non_action_row_ids_do_not_parse() {
        for raw in [
            "",
            "hist:3",
            "empty:notif",
            "quick:controls",
            "mpris:track",
            "dismiss:quick:controls",
            "dismiss:group:Discord",
            "open_app:only-a-name",
            "copy_otp:%ZZ",
        ] {
            assert_eq!(NotifyAction::parse(raw), None, "{raw} is not an action");
        }
    }
}
