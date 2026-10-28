//! Integration and golden tests for `flex-notify` provider and binary.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use flex_core::Menu;
use flex_rice::exec::notify::{
    self, DndState, ExtractedEntity, MprisTrack, NotificationItem, NotifyState, Urgency,
};
use flex_rice::providers::notify as provider;

fn draw(menu: &mut Menu, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal creates");
    terminal
        .draw(|f| flex_core::render::render(f, menu))
        .expect("renders cleanly");
    terminal.backend().buffer().clone()
}

fn buffer_line(buf: &Buffer, y: u16, width: u16) -> String {
    let mut s = String::new();
    for x in 0..width {
        s.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
    }
    s
}

fn mock_state() -> NotifyState {
    let mut state = NotifyState::default();

    // Controls
    state.controls.dnd = DndState::Timed {
        until: 2500,
        total_secs: 1500,
    };
    state.controls.mpris = Some(MprisTrack {
        player: "spotify".to_string(),
        title: "Get Lucky".to_string(),
        artist: "Daft Punk ft. Pharrell Williams".to_string(),
        album: Some("Random Access Memories".to_string()),
        position_secs: 134,
        length_secs: 248,
        is_playing: true,
        is_live: false,
        art_url: None,
    });

    // 1. Critical low battery alert
    let mut n1 = NotificationItem::new(
        1,
        "System",
        "Low Battery Warning (8%)",
        "Connect AC charger immediately",
        Urgency::Critical,
    );
    n1.timestamp = 990;
    state.notifications.push(n1);

    // 2. In-flight download notification
    let mut n2 = NotificationItem::new(
        2,
        "PackageKit",
        "System Upgrade (mesa, linux-firmware)",
        "Downloading packages: 14.2 MB/s",
        Urgency::Normal,
    );
    n2.timestamp = 980;
    n2.progress = Some(0.64);
    state.notifications.push(n2);

    // 3. Discord message with URL and OTP
    let mut n3 = NotificationItem::new(
        3,
        "Discord",
        "#dev-team",
        "Alex: Check PR #142 at https://github.com/gom3az/flex and use 2FA code 849201",
        Urgency::Normal,
    );
    n3.timestamp = 880;
    state.notifications.push(n3);

    // 4. Dismissed item in history
    let mut n4 =
        NotificationItem::new(4, "Slack", "#general", "Standup in 5 minutes", Urgency::Low);
    n4.timestamp = 500;
    n4.is_dismissed = true;
    state.notifications.push(n4);

    state
}

#[test]
fn entity_extractor_robustness() {
    let text = "Your verification code is 492018. Visit https://auth.example.com/verify or check color #ff5555.";
    let entities = notify::extract_entities(text);

    assert!(entities
        .iter()
        .any(|e| matches!(e, ExtractedEntity::OtpCode(c) if c == "492018")));
    assert!(entities
        .iter()
        .any(|e| matches!(e, ExtractedEntity::Url(u) if u == "https://auth.example.com/verify")));
    assert!(entities
        .iter()
        .any(|e| matches!(e, ExtractedEntity::HexColor(h) if h == "#ff5555")));
}

#[test]
fn feed_tab_contract_and_wiremix_rows() {
    let state = mock_state();
    let tab = provider::feed_tab_from(&state, 1000);

    assert_eq!(tab.name, provider::TAB_FEED);
    assert!(!tab.bare_rows);
    assert!(
        !tab.filterable,
        "Notification Center must not have a search bar"
    );
    assert_eq!(tab.rows.len(), 5);

    assert_eq!(tab.rows[0].id.as_str(), provider::ACTION_QUICK_CONTROLS);
    assert_eq!(tab.rows[0].label, "Quick Controls");

    assert_eq!(tab.rows[1].id.as_str(), provider::ACTION_MPRIS_TRACK);
    assert!(tab.rows[1].label.contains("Get Lucky"));
    assert!(tab.rows[1].volume.is_some());

    let r2 = &tab.rows[2];
    assert_eq!(r2.id.as_str(), "notif:1");
    assert!(
        r2.is_default,
        "Critical alerts must have the default marker ◇"
    );
    assert_eq!(r2.label, "System");
    assert_eq!(r2.sublabel.as_deref(), Some("Low Battery Warning (8%)"));

    let r3 = &tab.rows[3];
    assert_eq!(r3.id.as_str(), "notif:2");
    assert_eq!(r3.volume, Some(0.64));

    let r4 = &tab.rows[4];
    assert_eq!(r4.id.as_str(), "notif:3");
    assert!(r4.targets.iter().any(|t| t.title.contains("849201")));
    assert!(r4
        .targets
        .iter()
        .any(|t| t.title.contains("https://github.com")));
}

#[test]
fn feed_tab_groups_multi_notification_apps() {
    let mut state = mock_state();
    // Add a second Discord notification to trigger group header stack
    let mut n5 = NotificationItem::new(
        5,
        "Discord",
        "#general",
        "Lunch time everyone!",
        Urgency::Normal,
    );
    n5.timestamp = 900;
    state.notifications.push(n5);

    let tab = provider::feed_tab_from(&state, 1000);
    assert_eq!(tab.rows.len(), 5);

    let group_row = tab
        .rows
        .iter()
        .find(|r| r.id.as_str() == "group:Discord")
        .expect("group header found");
    assert!(group_row.label.contains("▶ 󰙯 Discord (2)"));
    assert_eq!(group_row.sublabel.as_deref(), Some("#general"));
    assert!(group_row
        .targets
        .iter()
        .any(|t| t.title.contains("Expand Thread (1 earlier)")));
    assert!(group_row
        .targets
        .iter()
        .any(|t| t.title.contains("Dismiss All (2)")));

    provider::toggle_thread_expanded("Discord");
    let expanded_tab = provider::feed_tab_from(&state, 1000);
    assert_eq!(expanded_tab.rows.len(), 6);

    let expanded_row = expanded_tab
        .rows
        .iter()
        .find(|r| r.id.as_str() == "group:Discord")
        .expect("group header found");
    assert!(expanded_row.label.contains("▼ 󰙯 Discord (2)"));

    let child_row = &expanded_tab.rows[5];
    assert!(child_row.label.contains("└─ #dev-team"));

    provider::toggle_thread_expanded("Discord");
}

#[test]
fn notification_preview_carries_body_and_image() {
    let dir = std::env::temp_dir().join(format!("flex-notify-img-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let img_path = dir.join("screenshot.png");
    let _ = std::fs::write(&img_path, "dummy image content");

    let mut state = NotifyState::default();
    let mut notif = NotificationItem::new(
        10,
        "Flameshot",
        "Screenshot Captured",
        "Saved to /tmp/screenshot.png",
        Urgency::Normal,
    );
    notif.image_path = Some(img_path.to_str().unwrap().to_string());
    state.notifications.push(notif);

    let tab = provider::feed_tab_from(&state, 1000);
    let notif_row = tab
        .rows
        .iter()
        .find(|r| r.id.as_str() == "notif:10")
        .expect("row found");

    // Text preview in detail line
    assert_eq!(
        notif_row.detail.as_deref(),
        Some("Saved to /tmp/screenshot.png")
    );
    // Image preview in preview_image
    assert_eq!(
        notif_row.preview_image.as_deref(),
        Some(img_path.to_str().unwrap())
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quick_controls_carries_focus_presets_and_timer_progress() {
    let state = mock_state();
    let tab = provider::feed_tab_from(&state, 1000);
    let quick = &tab.rows[0];

    assert_eq!(quick.id.as_str(), provider::ACTION_QUICK_CONTROLS);
    assert!(
        quick.volume.is_some(),
        "Timed DND progress bar renders on Quick Controls"
    );
    let target_ids: Vec<&str> = quick.targets.iter().map(|t| t.id.as_str()).collect();
    assert!(target_ids.contains(&"dnd:25m"));
    assert!(target_ids.contains(&"dnd:60m"));
    assert!(target_ids.contains(&"dnd:120m"));
    assert!(target_ids.contains(&"dnd:indefinite"));
    assert!(target_ids.contains(&"dnd:off"));
}

#[test]
fn history_tab_renders_dismissed_archive() {
    let state = mock_state();
    let tab = provider::history_tab_from(&state, 1000);

    assert_eq!(tab.name, provider::TAB_HISTORY);
    assert!(!tab.filterable);
    assert_eq!(tab.rows.len(), 1);
    assert_eq!(tab.rows[0].label, "Slack");
    assert_eq!(tab.rows[0].sublabel.as_deref(), Some("#general"));
    assert!(tab.rows[0].offline);
}

#[test]
fn golden_80x24_rendering_drawer_wiremix_detail() {
    let state = mock_state();
    let mut menu = provider::menu_from(&state, 1000);
    assert_eq!(menu.app.tabs.len(), 2);

    let buf = draw(&mut menu, 80, 24);

    // Tab bar on the bottom line (y = 23)
    let tab_bar = buffer_line(&buf, 23, 80);
    assert!(tab_bar.contains("[Notifications]"));
    assert!(tab_bar.contains("History"));
    assert!(!tab_bar.contains("Channels"));
    assert!(!tab_bar.contains("Focus/DND"));

    // Entry 0 header (y = 1): Quick Controls Shelf
    let e0_header = buffer_line(&buf, 1, 80);
    assert!(e0_header.contains("░"));
    assert!(e0_header.contains("Quick Controls"));

    // Entry 1 header (y = 6): MPRIS Media Card
    let e1_header = buffer_line(&buf, 6, 80);
    assert!(e1_header.contains("Get Lucky"));

    // Entry 1 detail (y = 8): MPRIS volume/progress bar
    let e1_detail = buffer_line(&buf, 8, 80);
    assert!(e1_detail.contains("━"));

    // Entry 2 header (y = 11): Critical Alert with marker ◇
    let e2_header = buffer_line(&buf, 11, 80);
    assert!(e2_header.contains("◇"));
    assert!(e2_header.contains("System"));

    let e2_sub = buffer_line(&buf, 12, 80);
    assert!(e2_sub.contains("Low Battery Warning"));
}

#[test]
fn executor_dismiss_and_dnd_dispatch() {
    let dir = std::env::temp_dir().join(format!("flex-notify-test-exec-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("state.json");

    let state = mock_state();
    notify::save_state(&state, Some(&path)).expect("saved");

    // Dismiss single item (notif 3: Discord)
    let rep = provider::execute("dismiss:3", "Dismiss", Some(&path)).expect("exec");
    assert_eq!(rep.action_id, "dismiss:3");
    let s1 = notify::load_state(Some(&path));
    assert!(
        s1.notifications
            .iter()
            .find(|n| n.id == 3)
            .unwrap()
            .is_dismissed
    );

    // Clear all (non-critical)
    let _ = provider::execute(
        provider::NotifyAction::ClearAll.encode().as_str(),
        "Clear All",
        Some(&path),
    )
    .expect("exec");
    let s2 = notify::load_state(Some(&path));
    assert!(
        s2.notifications
            .iter()
            .find(|n| n.id == 2)
            .unwrap()
            .is_dismissed
    );
    // Critical alert protected
    assert!(
        !s2.notifications
            .iter()
            .find(|n| n.id == 1)
            .unwrap()
            .is_dismissed
    );

    // The Quick Controls *row* has no action of its own: Enter on it opens
    // the shelf, it does not clear anything. Only the shelf's Clear All
    // target does.
    let _ =
        provider::execute(provider::ACTION_QUICK_CONTROLS, "Dismiss", Some(&path)).expect("exec");
    let s3 = notify::load_state(Some(&path));
    assert!(
        !s3.notifications
            .iter()
            .find(|n| n.id == 1)
            .unwrap()
            .is_dismissed,
        "the Quick Controls row must not clear the critical alert"
    );

    // Group dismiss (the delete path maps a thread row to this action)
    notify::save_state(&state, Some(&path)).expect("saved again");
    let group = provider::NotifyAction::parse("group:Discord").expect("group row parses");
    assert_eq!(
        group,
        provider::NotifyAction::ToggleThread("Discord".to_string())
    );
    let _ = provider::execute("dismiss_app:Discord", "Dismiss", Some(&path)).expect("exec");
    let s4 = notify::load_state(Some(&path));
    assert!(
        s4.notifications
            .iter()
            .find(|n| n.id == 3)
            .unwrap()
            .is_dismissed
    );

    // Set Pomodoro 25m DND
    let _ = provider::execute("dnd:25m", "Pomodoro 25m", Some(&path)).expect("exec");
    let s3 = notify::load_state(Some(&path));
    assert!(matches!(
        s3.controls.dnd,
        DndState::Timed {
            total_secs: 1500,
            ..
        }
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn executor_app_rules_and_archive_disposal() {
    let dir = std::env::temp_dir().join(format!("flex-notify-test-rules-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("state.json");

    notify::save_state(&mock_state(), Some(&path)).expect("saved");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    // "Silence for 1 Hour" is a real, expiring rule.
    provider::execute("mute_app:Discord", "Silence", Some(&path)).expect("exec");
    let muted = notify::load_state(Some(&path));
    assert!(muted.is_muted("Discord", now));
    assert!(muted.is_muted("discord", now + 3_500));
    assert!(!muted.is_muted("Discord", now + 3_700));

    provider::execute("unmute_app:Discord", "Unmute", Some(&path)).expect("exec");
    assert!(
        !notify::load_state(Some(&path)).is_muted("Discord", now),
        "Unmute removes the rule before its hour is up"
    );

    // Priority toggles both ways.
    provider::execute("priority_app:Discord", "Priority", Some(&path)).expect("exec");
    assert!(notify::load_state(Some(&path)).is_priority("discord", now));
    provider::execute("priority_app:Discord", "Priority", Some(&path)).expect("exec");
    assert!(!notify::load_state(Some(&path)).is_priority("Discord", now));

    // Purge removes one archived row without touching the rest.
    provider::execute("purge:3", "Delete", Some(&path)).expect("exec");
    let purged = notify::load_state(Some(&path));
    assert!(!purged.notifications.iter().any(|n| n.id == 3));
    assert!(purged.notifications.iter().any(|n| n.id == 2));

    // Clear History removes archived rows only.
    provider::execute("clear_history", "Clear History", Some(&path)).expect("exec");
    let cleared = notify::load_state(Some(&path));
    assert!(
        !cleared.notifications.iter().any(|n| n.id == 4),
        "dismissed rows are purged"
    );
    assert!(
        cleared.notifications.iter().any(|n| n.id == 1),
        "active and critical rows survive"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn executor_snoozes_then_wakes_a_notification() {
    let dir = std::env::temp_dir().join(format!("flex-notify-test-snooze-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("state.json");

    let mut item = NotificationItem::new(7, "Slack", "standup", "in 10", Urgency::Normal);
    item.timestamp = 100;
    let mut state = NotifyState::default();
    state.notifications.push(item);
    notify::save_state(&state, Some(&path)).expect("saved");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    provider::execute("snooze:15:7", "Snooze 15 Minutes", Some(&path)).expect("exec");
    let snoozed = notify::load_state(Some(&path));
    assert!(snoozed.notifications[0].is_snoozed_at(now));
    assert!(!snoozed.notifications[0].is_active(now));
    let feed = provider::feed_tab_from(&snoozed, now);
    assert!(
        feed.rows.iter().any(|r| r.id.as_str() == "unsnooze:7"),
        "a sleeping row stays reachable from the drawer"
    );

    provider::execute("unsnooze:7", "Wake Now", Some(&path)).expect("exec");
    let awake = notify::load_state(Some(&path));
    assert!(!awake.notifications[0].is_snoozed);
    assert!(awake.notifications[0].is_active(now));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mpris_parser_handles_twitch_livestreams_and_youtube() {
    let twitch_line = "brave;;Playing;;Caliathlol;;CoC Comet Oracle - Grand Expeditions;;;;14798847;;9223372036854775807;;file:///tmp/.org.chromium.Chromium.Ir95Tl";
    let twitch = notify::parse_mpris_line(twitch_line).expect("parses twitch");
    assert_eq!(twitch.player, "brave");
    assert_eq!(twitch.artist, "Caliathlol");
    assert_eq!(twitch.title, "CoC Comet Oracle - Grand Expeditions");
    assert!(twitch.is_playing);
    assert!(twitch.is_live);
    assert_eq!(twitch.position_secs, 14);
    assert_eq!(twitch.length_secs, 0);
    assert_eq!(
        twitch.art_url.as_deref(),
        Some("/tmp/.org.chromium.Chromium.Ir95Tl")
    );

    let spotify_line = "spotify;;Paused;;Daft Punk;;Get Lucky;;RAM;;134000000;;248000000;;";
    let spotify = notify::parse_mpris_line(spotify_line).expect("parses spotify");
    assert_eq!(spotify.player, "spotify");
    assert_eq!(spotify.artist, "Daft Punk");
    assert!(!spotify.is_playing);
    assert!(!spotify.is_live);
    assert_eq!(spotify.position_secs, 134);
    assert_eq!(spotify.length_secs, 248);
}

#[test]
fn sound_playback_safety_with_dnd_and_urgency() {
    // Suppressed when DND is active
    notify::play_notification_sound(Urgency::Critical, true);
    notify::play_notification_sound(Urgency::Normal, true);
    notify::play_notification_sound(Urgency::Low, true);

    // Safe execution (non-blocking, tolerates missing sound players or headless test env)
    notify::play_notification_sound(Urgency::Normal, false);
    notify::play_notification_sound(Urgency::Critical, false);
}

/// Number of zombie (`Z`) children of this test process right now.
fn own_zombie_children() -> usize {
    let me = std::process::id();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    dir.filter_map(|entry| {
        let entry = entry.ok()?;
        entry.file_name().to_str()?.parse::<u32>().ok()?;
        let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
        // `comm` may contain spaces/parens: state follows the last `)`.
        let after_comm = stat.rsplit(')').next()?;
        let mut fields = after_comm.split_whitespace();
        let state = fields.next()?;
        let ppid: u32 = fields.nth(1)?.parse().ok()?;
        (state == "Z" && ppid == me).then_some(())
    })
    .count()
}

/// The daemon's fire-and-forget spawns (sound player, toast `sh`) must be
/// reaped, not left as zombies: without the waiter thread each notification
/// leaks one `pw-play` + one `sh` defunct pair under the daemon for its whole
/// lifetime. Reaping is async, so poll back to baseline with a timeout.
#[test]
fn daemon_spawns_leave_no_zombies() {
    use std::os::unix::fs::PermissionsExt as _;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("flex-notify-reap-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    for name in ["pw-play", "kitty"] {
        let stub = dir.join(name);
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // Seam-scoped `PATH`: the stub dir shadows the ambient `PATH` (so `sh`
    // still resolves, while `pw-play`/`kitty` hit the exit-0 stubs). Nothing
    // touches the process environment, so concurrent tests are unaffected.
    let stub_path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let baseline = own_zombie_children();
    // Sound path needs a real sound file (hardcoded freedesktop paths); skip
    // that leg where the fixtures are absent (e.g. CI containers) — the toast
    // leg below still exercises the reap path unconditionally.
    let sound_file_present = [
        "/usr/share/sounds/freedesktop/stereo/message.oga",
        "/usr/share/sounds/freedesktop/stereo/message-new-instant.oga",
        "/usr/share/sounds/freedesktop/stereo/dialog-information.oga",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).exists());
    if sound_file_present {
        notify::play_notification_sound_with_path(Urgency::Normal, false, Some(&stub_path));
    } else {
        eprintln!("no freedesktop sound file; skipping sound leg");
    }
    // `kitty` is stubbed above, so no window opens; the intermediate `sh`
    // still runs and must be reaped.
    notify::spawn_toast_with_path(4242, None, Some(&stub_path));

    for _ in 0..200 {
        if own_zombie_children() <= baseline {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = std::fs::remove_dir_all(&dir);
    panic!("zombie count never returned to baseline {baseline} (fire-and-forget spawn leaked)");
}

#[test]
fn notify_daemon_dbus_server_contract() {
    let dir = std::env::temp_dir().join(format!("flex-notify-dbus-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("dbus-state.json");

    let server = notify::NotificationServer::new(Some(path.clone()));
    let caps = server.get_capabilities();
    assert!(caps.contains(&"actions".to_string()));
    assert!(caps.contains(&"body".to_string()));
    assert!(caps.contains(&"sound".to_string()));

    let (name, vendor, ver, spec) = server.get_server_information();
    assert_eq!(name, "flex-notify");
    assert_eq!(vendor, "flex");
    assert_eq!(ver, env!("CARGO_PKG_VERSION"));
    assert_eq!(spec, "1.2");

    let mut hints = std::collections::HashMap::new();
    hints.insert("urgency".to_string(), zbus::zvariant::Value::U8(1));
    let notif_id = server.notify(
        "Firefox".to_string(),
        0,
        "firefox".to_string(),
        "Download Finished".to_string(),
        "ISO downloaded".to_string(),
        vec!["open".to_string(), "Open".to_string()],
        hints,
        5000,
    );
    assert_eq!(notif_id, 1);

    let state = notify::load_state(Some(&path));
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications[0].app_name, "Firefox");
    assert_eq!(state.notifications[0].summary, "Download Finished");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn toast_header_alignment_and_truncation() {
    // Standard header formatting with 2-space left margin and right-aligned timestamp
    let header = notify::format_toast_header("󰂚", "Discord", "#dev-team", "10s", 48);
    assert!(header.starts_with("  󰂚  \x1b[1mDiscord\x1b[0m · #dev-team"));
    assert!(header.ends_with("\x1b[2m10s\x1b[0m"));

    let long_summary = "A".repeat(100);
    let header_truncated = notify::format_toast_header("󰂚", "System", &long_summary, "2m", 48);
    assert!(header_truncated.contains('…'));
    assert!(header_truncated.ends_with("\x1b[2m2m\x1b[0m"));
}

#[test]
fn entity_extractor_file_path_and_url_expansion() {
    let text = "Check file /tmp/build.log and visit www.github.com/gom3az/flex";
    let entities = notify::extract_entities(text);
    assert!(entities
        .iter()
        .any(|e| matches!(e, ExtractedEntity::FilePath(p) if p == "/tmp/build.log")));
    assert!(entities.iter().any(
        |e| matches!(e, ExtractedEntity::Url(u) if u == "https://www.github.com/gom3az/flex")
    ));
}

#[test]
fn toast_boxed_card_rendering() {
    use flex_rice::exec::notify::{NotificationAction, NotificationItem, Urgency};
    use unicode_width::UnicodeWidthStr as _;

    let mut item = NotificationItem::new(
        101,
        "System Alert",
        "Disk Space Warning",
        "Root partition capacity reached 98%",
        Urgency::Critical,
    );
    item.progress = Some(0.98);
    item.actions = vec![
        NotificationAction {
            id: "clean".into(),
            title: "Clean Up".into(),
        },
        NotificationAction {
            id: "ignore".into(),
            title: "Ignore".into(),
        },
    ];

    let card = notify::format_toast_card(&item, "now", 50, 0);

    assert!(card.contains("╭─"));
    assert!(card.contains("─╮"));
    assert!(card.contains("╰"));
    assert!(card.contains("╯"));

    assert!(card.contains("󰀦"));
    assert!(card.contains("[CRITICAL]"));

    let strip_ansi = |s: &str| -> String {
        let mut out = String::new();
        let mut in_esc = false;
        for c in s.chars() {
            if c == '\x1b' {
                in_esc = true;
            } else if in_esc {
                if c == 'm' {
                    in_esc = false;
                }
            } else {
                out.push(c);
            }
        }
        out
    };

    for (idx, line) in card.lines().enumerate() {
        let clean = strip_ansi(line);
        assert_eq!(
            clean.width(),
            50,
            "Line {} display width mismatch: expected 50, got {} for '{}'",
            idx + 1,
            clean.width(),
            clean
        );
    }
}
