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

/// Action IDs for quick controls
pub const ACTION_QUICK_CONTROLS: &str = "quick:controls";
pub const ACTION_MPRIS_TRACK: &str = "mpris:track";
pub const ACTION_CLEAR_ALL: &str = "action:clear_all";
pub const ACTION_TOGGLE_DND: &str = "action:toggle_dnd";

/// Current unix timestamp in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// OPT-8 entity cache keyed by `(notification id, timestamp)`: notification
/// text is immutable for a given `(id, timestamp)`, so per-tick row rebuilds
/// reuse the extracted entities instead of re-scanning strings.
type EntityCache = Mutex<HashMap<(u32, u64), Vec<ExtractedEntity>>>;
fn entity_cache() -> &'static EntityCache {
    static CACHE: OnceLock<EntityCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cached [`notify::extract_entities`] for one notification.
fn cached_entities(item: &NotificationItem) -> Vec<ExtractedEntity> {
    let key = (item.id, item.timestamp);
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

/// OPT-8 byte-level pre-check for the three system-app names that never get
/// an "Open App" target. Returns true for `system`/`packagekit`/`wiremix`
/// in any ASCII case without allocating.
fn is_system_app_name(name: &str) -> bool {
    if name.len() > "packagekit".len() {
        return false;
    }
    name.eq_ignore_ascii_case("system")
        || name.eq_ignore_ascii_case("packagekit")
        || name.eq_ignore_ascii_case("wiremix")
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
        targets.push(Target::new(
            RowId::new(format!("copy_image:{path}")),
            format!("Copy Image ({file_name})"),
        ));
    }
}

/// Helper to construct a notification Row with body text preview, graphic image preview, and actions.
#[allow(clippy::too_many_lines)]
fn notification_row_from(item: &NotificationItem, now: u64, _is_child: bool) -> Row {
    let rel_time = cached_rel_time(item.timestamp, now);
    let entities = cached_entities(item);

    let mut targets = Vec::new();

    // 1. Extracted entity targets first
    for entity in &entities {
        match entity {
            ExtractedEntity::OtpCode(code) => {
                targets.push(Target::new(
                    RowId::new(format!("copy_otp:{code}")),
                    format!("Copy OTP Code ({code})"),
                ));
            }
            ExtractedEntity::Url(url) => {
                targets.push(Target::new(
                    RowId::new(format!("open_url:{url}")),
                    format!("Open Link ({url})"),
                ));
                targets.push(Target::new(
                    RowId::new(format!("copy_url:{url}")),
                    format!("Copy Link ({url})"),
                ));
            }
            ExtractedEntity::FilePath(path) => {
                let file_name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path);
                targets.push(Target::new(
                    RowId::new(format!("open_file:{path}")),
                    format!("Open File ({file_name})"),
                ));
                targets.push(Target::new(
                    RowId::new(format!("open_dir:{path}")),
                    "Open Containing Folder".to_string(),
                ));
                targets.push(Target::new(
                    RowId::new(format!("copy_path:{path}")),
                    format!("Copy Path ({file_name})"),
                ));
                push_copy_image_target(&mut targets, path);
            }
            ExtractedEntity::HexColor(hex) => {
                targets.push(Target::new(
                    RowId::new(format!("copy_hex:{hex}")),
                    format!("Copy Color ({hex})"),
                ));
            }
        }
    }

    // 2. Open desktop application if non-system app
    // OPT-8: byte-level length + first-byte pre-check before the
    // `to_lowercase` allocation; system-app names are short ASCII.
    let clean_app = item.app_name.trim();
    let is_system_app = clean_app.is_empty() || is_system_app_name(clean_app);
    if !is_system_app {
        targets.push(Target::new(
            RowId::new(format!("open_app:{clean_app}")),
            format!("Open App ({clean_app})"),
        ));
    }

    // 3. Attached D-Bus action buttons
    for action in &item.actions {
        targets.push(Target::new(
            RowId::new(format!("action:{}:{}", item.id, action.id)),
            format!("Action: {}", action.title),
        ));
    }

    // 3b. Image-hint copy target (before the row takes `targets`): the
    // `image-path` file gets the same Copy Image treatment as body paths.
    if let Some(img) = item.image_path.as_deref() {
        push_copy_image_target(&mut targets, img);
    }

    // 3. Primary management targets
    targets.push(Target::new(
        RowId::new(format!("dismiss:{}", item.id)),
        "Dismiss Notification",
    ));
    targets.push(Target::new(
        RowId::new(format!("dismiss_app:{}", item.app_name)),
        format!("Dismiss All from {}", item.app_name),
    ));
    if !item.body.is_empty() {
        targets.push(Target::new(
            RowId::new(format!("copy_body:{}", item.id)),
            "Copy Message Text",
        ));
    }
    targets.push(Target::new(
        RowId::new(format!("snooze_15:{}", item.id)),
        "Snooze 15 Minutes",
    ));
    targets.push(Target::new(
        RowId::new(format!("snooze_60:{}", item.id)),
        "Snooze 1 Hour",
    ));
    targets.push(Target::new(
        RowId::new(format!("mute_app:{}", item.app_name)),
        format!("Silence {} for 1 Hour", item.app_name),
    ));

    let (label, sublabel) = (item.app_name.clone(), Some(item.summary.clone()));

    let meta = if item.urgency == Urgency::Critical {
        format!("{rel_time} · Critical")
    } else {
        rel_time
    };

    let mut row = Row::with_targets(RowId::new(format!("notif:{}", item.id)), label, targets, 0);
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
#[allow(clippy::too_many_lines)]
fn grouped_parent_row_from(
    app_name: &str,
    items: &[&NotificationItem],
    now: u64,
    expanded: bool,
) -> Row {
    let latest = items[0];
    let rel_time = cached_rel_time(latest.timestamp, now);
    let entities = cached_entities(latest);

    let mut targets = Vec::new();

    // 1. Thread expansion target first
    let toggle_title = if expanded {
        String::from("Collapse Thread")
    } else {
        format!("Expand Thread ({} earlier)", items.len() - 1)
    };
    targets.push(Target::new(
        RowId::new(format!("toggle_thread:{app_name}")),
        toggle_title,
    ));

    // 2. Extracted entity targets from the latest notification
    for entity in &entities {
        match entity {
            ExtractedEntity::OtpCode(code) => {
                targets.push(Target::new(
                    RowId::new(format!("copy_otp:{code}")),
                    format!("Copy OTP Code ({code})"),
                ));
            }
            ExtractedEntity::Url(url) => {
                targets.push(Target::new(
                    RowId::new(format!("open_url:{url}")),
                    format!("Open Link ({url})"),
                ));
                targets.push(Target::new(
                    RowId::new(format!("copy_url:{url}")),
                    format!("Copy Link ({url})"),
                ));
            }
            ExtractedEntity::FilePath(path) => {
                let file_name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path);
                targets.push(Target::new(
                    RowId::new(format!("open_file:{path}")),
                    format!("Open File ({file_name})"),
                ));
                targets.push(Target::new(
                    RowId::new(format!("open_dir:{path}")),
                    "Open Containing Folder".to_string(),
                ));
                targets.push(Target::new(
                    RowId::new(format!("copy_path:{path}")),
                    format!("Copy Path ({file_name})"),
                ));
                push_copy_image_target(&mut targets, path);
            }
            ExtractedEntity::HexColor(hex) => {
                targets.push(Target::new(
                    RowId::new(format!("copy_hex:{hex}")),
                    format!("Copy Color ({hex})"),
                ));
            }
        }
    }

    // 3. Open desktop application (OPT-8 byte-level pre-check, see above).
    let clean_app = app_name.trim();
    if !clean_app.is_empty() && !is_system_app_name(clean_app) {
        targets.push(Target::new(
            RowId::new(format!("open_app:{clean_app}")),
            format!("Open App ({clean_app})"),
        ));
    }

    // 4. Attached D-Bus action buttons for the latest item
    for action in &latest.actions {
        targets.push(Target::new(
            RowId::new(format!("action:{}:{}", latest.id, action.id)),
            format!("Action: {}", action.title),
        ));
    }

    // 4b. Image-hint copy target (before the row takes `targets`).
    if let Some(img) = latest.image_path.as_deref() {
        push_copy_image_target(&mut targets, img);
    }

    // 5. Thread and notification management targets
    targets.push(Target::new(
        RowId::new(format!("dismiss:{}", latest.id)),
        "Dismiss Latest Notification",
    ));
    targets.push(Target::new(
        RowId::new(format!("dismiss_app:{app_name}")),
        format!("Dismiss All ({})", items.len()),
    ));
    if !latest.body.is_empty() {
        targets.push(Target::new(
            RowId::new(format!("copy_body:{}", latest.id)),
            "Copy Latest Message Text",
        ));
    }
    targets.push(Target::new(
        RowId::new(format!("snooze_15:{}", latest.id)),
        "Snooze 15 Minutes",
    ));
    targets.push(Target::new(
        RowId::new(format!("snooze_60:{}", latest.id)),
        "Snooze 1 Hour",
    ));
    targets.push(Target::new(
        RowId::new(format!("mute_app:{app_name}")),
        format!("Silence {app_name} for 1 Hour"),
    ));
    targets.push(Target::new(
        RowId::new(format!("priority_app:{app_name}")),
        format!("Toggle Priority for {app_name}"),
    ));

    let glyph = if expanded { '▼' } else { '▶' };
    let label = format!("{glyph} 󰙯 {app_name} ({})", items.len());
    let sublabel = Some(latest.summary.clone());

    let meta = if latest.urgency == Urgency::Critical {
        format!("{rel_time} · Critical")
    } else {
        rel_time
    };

    let mut row = Row::with_targets(RowId::new(format!("group:{app_name}")), label, targets, 0);
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
fn child_notification_row_from(item: &NotificationItem, now: u64, is_last: bool) -> Row {
    let mut row = notification_row_from(item, now, true);
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
        .filter(|n| !n.is_dismissed && !n.is_snoozed)
        .count();

    let quick_targets = vec![
        Target::new(RowId::new("toggle_dnd"), format!("Toggle {dnd_label}")),
        Target::new(RowId::new("dnd:25m"), "Pomodoro Sprint (25 Mins)"),
        Target::new(RowId::new("dnd:60m"), "Deep Work (1 Hour)"),
        Target::new(RowId::new("dnd:120m"), "Focus Session (2 Hours)"),
        Target::new(RowId::new("dnd:indefinite"), "Silence Until Turned Off"),
        Target::new(RowId::new("dnd:off"), "Turn DND Off"),
        Target::new(
            RowId::new("clear_all"),
            format!("Clear All ({active_count} Active)"),
        ),
    ];
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
            Target::new(
                RowId::new("play_pause"),
                if mpris.is_playing {
                    "Pause Playback"
                } else {
                    "Resume Playback"
                },
            ),
            Target::new(RowId::new("next_track"), "Next Track"),
            Target::new(RowId::new("prev_track"), "Previous Track"),
            Target::new(RowId::new("seek_forward_10"), "Seek Forward +10s"),
            Target::new(RowId::new("seek_backward_10"), "Seek Backward -10s"),
            Target::new(
                RowId::new(format!("focus_player:{}", mpris.player)),
                format!("Focus {}", mpris.player),
            ),
            Target::new(
                RowId::new(format!("copy_media:{} - {}", mpris.title, mpris.artist)),
                "Copy Media Info",
            ),
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
        .filter(|n| {
            !n.is_dismissed && (!n.is_snoozed || n.snooze_until.is_some_and(|until| now >= until))
        })
        .collect();

    // 3a. Sticky Critical Alerts at Top (most recent first)
    let (mut critical_notifs, normal_notifs): (Vec<&NotificationItem>, Vec<&NotificationItem>) =
        active_notifs
            .into_iter()
            .partition(|n| n.urgency == Urgency::Critical);

    critical_notifs.sort_by_key(|n| std::cmp::Reverse(n.timestamp));

    for item in critical_notifs {
        rows.push(notification_row_from(item, now, false));
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
    sorted_apps.sort_by_key(|(_, items)| {
        std::cmp::Reverse(items.iter().map(|n| n.timestamp).max().unwrap_or(0))
    });

    for (app_name, mut items) in sorted_apps {
        items.sort_by_key(|n| std::cmp::Reverse(n.timestamp));
        if items.len() > 1 {
            let expanded = is_thread_expanded(app_name);
            rows.push(grouped_parent_row_from(app_name, &items, now, expanded));

            if expanded {
                let older_count = items.len() - 1;
                for (i, item) in items[1..].iter().enumerate() {
                    let is_last = i + 1 == older_count;
                    rows.push(child_notification_row_from(item, now, is_last));
                }
            }
        } else if let Some(item) = items.first() {
            rows.push(notification_row_from(item, now, false));
        }
    }

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
        .filter(|n| n.is_dismissed)
        .collect();
    dismissed_items.sort_by_key(|n| std::cmp::Reverse(n.timestamp));

    for item in dismissed_items.iter().take(30) {
        let rel_time = cached_rel_time(item.timestamp, now);
        let mut targets = vec![
            Target::new(
                RowId::new(format!("restore:{}", item.id)),
                "Restore Notification",
            ),
            Target::new(
                RowId::new(format!("copy_body:{}", item.id)),
                "Copy Message Text",
            ),
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
        row.meta = Some(format!("{rel_time} · Dismissed"));
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

    if let Some(id_str) = action_id.strip_prefix("notif:") {
        if let Ok(id) = id_str.parse::<u32>() {
            if let Some(n) = state.notifications.iter().find(|n| n.id == id) {
                notify::open_application(&n.app_name);
                should_close = true;
            }
        }
    } else if action_id == ACTION_CLEAR_ALL
        || action_id == "clear_all"
        || action_id == "dismiss:clear_all"
        || action_id == ACTION_QUICK_CONTROLS
        || action_id == "quick_controls"
        || action_id == "dismiss:quick_controls"
        || action_id == "dismiss:quick:controls"
        || target_title.contains("Clear All")
    {
        for n in &mut state.notifications {
            if !n.is_pinned && n.urgency != Urgency::Critical {
                n.is_dismissed = true;
            }
        }
    } else if action_id == "play_pause"
        || action_id == ACTION_MPRIS_TRACK
        || target_title.contains("Playback")
    {
        let _ = std::process::Command::new("playerctl")
            .arg("play-pause")
            .status();
    } else if action_id == "next_track" || target_title.contains("Next Track") {
        let _ = std::process::Command::new("playerctl").arg("next").status();
    } else if action_id == "prev_track" || target_title.contains("Previous Track") {
        let _ = std::process::Command::new("playerctl")
            .arg("previous")
            .status();
    } else if action_id == "seek_forward_10" || target_title.contains("Forward +10s") {
        let _ = std::process::Command::new("playerctl")
            .args(["position", "10+"])
            .status();
    } else if action_id == "seek_backward_10" || target_title.contains("Backward -10s") {
        let _ = std::process::Command::new("playerctl")
            .args(["position", "10-"])
            .status();
    } else if let Some(player) = action_id.strip_prefix("focus_player:") {
        notify::open_application(player);
        should_close = true;
    } else if let Some(info) = action_id.strip_prefix("copy_media:") {
        copy_to_clipboard(info);
    } else if let Some(id_str) = action_id.strip_prefix("dismiss:") {
        if let Ok(id) = id_str.parse::<u32>() {
            if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                n.is_dismissed = true;
            }
        }
    } else if let Some(app) = action_id
        .strip_prefix("toggle_thread:")
        .or_else(|| action_id.strip_prefix("channel:"))
    {
        toggle_thread_expanded(app);
    } else if let Some(app) = action_id
        .strip_prefix("dismiss_app:")
        .or_else(|| action_id.strip_prefix("dismiss:group:"))
    {
        for n in &mut state.notifications {
            if n.app_name == app {
                n.is_dismissed = true;
            }
        }
    } else if let Some(app) = action_id.strip_prefix("group:") {
        toggle_thread_expanded(app);
    } else if let Some(app) = action_id.strip_prefix("mute_app:") {
        if !state.muted_apps.iter().any(|a| a.eq_ignore_ascii_case(app)) {
            state.muted_apps.push(app.to_string());
        }
    } else if let Some(app) = action_id.strip_prefix("priority_app:") {
        if let Some(pos) = state
            .priority_apps
            .iter()
            .position(|a| a.eq_ignore_ascii_case(app))
        {
            state.priority_apps.remove(pos);
        } else {
            state.priority_apps.push(app.to_string());
        }
    } else if let Some(id_str) = action_id.strip_prefix("restore:") {
        if let Ok(id) = id_str.parse::<u32>() {
            if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                n.is_dismissed = false;
            }
        }
    } else if let Some(id_str) = action_id.strip_prefix("copy_body:") {
        if let Ok(id) = id_str.parse::<u32>() {
            if let Some(n) = state.notifications.iter().find(|n| n.id == id) {
                let text = if n.body.is_empty() {
                    n.summary.clone()
                } else {
                    format!("{}\n{}", n.summary, n.body)
                };
                copy_to_clipboard(&text);
            }
        }
    } else if let Some(id_str) = action_id.strip_prefix("snooze_15:") {
        if let Ok(id) = id_str.parse::<u32>() {
            if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                n.is_snoozed = true;
                n.snooze_until = Some(now + 15 * 60);
            }
        }
    } else if let Some(id_str) = action_id.strip_prefix("snooze_60:") {
        if let Ok(id) = id_str.parse::<u32>() {
            if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                n.is_snoozed = true;
                n.snooze_until = Some(now + 60 * 60);
            }
        }
    } else if let Some(rest) = action_id.strip_prefix("action:") {
        if let Some((id_str, _action_key)) = rest.split_once(':') {
            if let Ok(id) = id_str.parse::<u32>() {
                if let Some(n) = state.notifications.iter_mut().find(|n| n.id == id) {
                    n.is_dismissed = true;
                }
            }
        }
        should_close = true;
    } else if let Some(code) = action_id.strip_prefix("copy_otp:") {
        copy_to_clipboard(code);
    } else if let Some(url) = action_id.strip_prefix("open_url:") {
        notify::open_url(url);
        should_close = true;
    } else if let Some(url) = action_id.strip_prefix("copy_url:") {
        copy_to_clipboard(url);
    } else if let Some(path) = action_id.strip_prefix("open_file:") {
        notify::open_file(path);
        should_close = true;
    } else if let Some(path) = action_id.strip_prefix("open_dir:") {
        notify::open_dir(path);
        should_close = true;
    } else if let Some(path) = action_id.strip_prefix("copy_path:") {
        copy_to_clipboard(path);
    } else if let Some(path) = action_id.strip_prefix("copy_image:") {
        // Image bytes with their MIME type (not the path text). Like the
        // other `copy_*` targets this stays open for repeated copies; a
        // file lost since listing time reports instead of failing silent.
        let mime = notify::image_mime(std::path::Path::new(path)).unwrap_or("image/png");
        if !notify::copy_image_file(path, mime) {
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "flex-notify", "Image not found", path])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    } else if let Some(app) = action_id.strip_prefix("open_app:") {
        notify::open_application(app);
        should_close = true;
    } else if let Some(hex) = action_id.strip_prefix("copy_hex:") {
        copy_to_clipboard(hex);
    } else if action_id == "dnd:25m" || target_title.contains("Pomodoro") {
        state.controls.dnd = DndState::Timed {
            until: now + 25 * 60,
            total_secs: 25 * 60,
        };
    } else if action_id == "dnd:60m" || target_title.contains("Deep Work") {
        state.controls.dnd = DndState::Timed {
            until: now + 60 * 60,
            total_secs: 60 * 60,
        };
    } else if action_id == "dnd:120m" || target_title.contains("Focus Session") {
        state.controls.dnd = DndState::Timed {
            until: now + 120 * 60,
            total_secs: 120 * 60,
        };
    } else if action_id == "dnd:off" || target_title.contains("DND Off") {
        state.controls.dnd = DndState::Off;
    } else if action_id == "dnd:indefinite" {
        state.controls.dnd = DndState::Indefinite;
    } else if action_id == "toggle_dnd" || action_id == ACTION_TOGGLE_DND {
        state.controls.dnd = if state.controls.dnd.is_active(now) {
            DndState::Off
        } else {
            DndState::Indefinite
        };
    }

    notify::save_state(&state, state_path)?;

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

        execute(ACTION_CLEAR_ALL, "Clear All", Some(&path)).unwrap();

        let updated = notify::load_state(Some(&path));
        assert!(updated.notifications[0].is_dismissed);
        assert!(
            !updated.notifications[1].is_dismissed,
            "Critical notifications protected from Clear All"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
