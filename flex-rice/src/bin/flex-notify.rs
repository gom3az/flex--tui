//! `flex-notify` binary entrypoint.
//!
//! Dual mode operation:
//! - With `-m` / `--menu` (or `flex notify`): launches the interactive
//!   right-side drawer TUI inside the terminal popup.
//! - Default / without `-m`: outputs JSON status for Waybar polling with
//!   unread badge counts, active DND state, and tooltip previews.

#![deny(unsafe_code)]
#![deny(clippy::all)]
#![deny(clippy::pedantic)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use flex_core::backend::EXIT_CANCELLED;
use flex_core::Outcome;
use flex_rice::exec::notify::{self, DndState, Urgency};
use flex_rice::providers::notify as provider;
use flex_rice::runner::{self, GlobalStyle, Provider};

#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)]
#[command(
    name = "flex-notify",
    version,
    about = "Native Right-Side Notification Center Drawer and Waybar Status Module"
)]
struct Cli {
    /// Global presentation flags.
    #[command(flatten)]
    style: GlobalStyle,

    /// Open the interactive Notification Center Drawer TUI
    #[arg(short = 'm', long = "menu")]
    menu: bool,

    /// Run the background D-Bus Notification service (org.freedesktop.Notifications)
    #[arg(short = 'd', long = "daemon")]
    daemon: bool,

    /// Clear all active non-critical notifications
    #[arg(long = "clear-all")]
    clear_all: bool,

    /// Toggle Do Not Disturb mode
    #[arg(long = "toggle-dnd")]
    toggle_dnd: bool,

    /// Print the selected `ACTION:` line without executing it.
    #[arg(long)]
    print_action: bool,

    /// Output JSON status for Waybar polling (default when not in popup)
    #[arg(long = "status")]
    status: bool,

    /// State file override (for testing)
    #[arg(long = "state-file", hide = true)]
    state_file: Option<PathBuf>,

    /// Direct notification operations
    #[command(subcommand)]
    op: Option<NotifyOp>,
}

#[derive(Debug, Subcommand)]
enum NotifyOp {
    /// Run the background D-Bus notification daemon
    Daemon {
        /// Replace existing notification daemon on D-Bus
        #[arg(long)]
        replace: bool,
    },
    /// Post a new notification into the notification feed
    Send {
        /// Notification summary / title
        summary: String,

        /// Notification body
        #[arg(default_value = "")]
        body: String,

        /// Application name
        #[arg(short = 'a', long = "app", default_value = "System")]
        app: String,

        /// Urgency level (low, normal, critical)
        #[arg(short = 'u', long = "urgency", default_value = "normal")]
        urgency: String,

        /// In-flight progress fraction (0.0 - 1.0)
        #[arg(long = "progress")]
        progress: Option<f32>,

        /// Path to attached preview image
        #[arg(short = 'i', long = "image")]
        image: Option<String>,

        /// Group / Category identifier
        #[arg(short = 'g', long = "group")]
        group: Option<String>,
    },
    /// Clear all non-critical notifications
    ClearAll,
    /// Toggle Do Not Disturb mode
    ToggleDnd,
    /// Render a transient toast overlay for a notification and auto-dismiss
    Toast {
        /// Notification ID to render
        id: u32,
    },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Render a 2-line toast card and auto-dismiss after a timeout.
///
/// - Normal/Low: 5 s auto-dismiss; [SUPER+N] opens the notification center.
/// - Critical:   stays until dismiss or timeout; [SUPER+N] opens the notification center.
///
/// The window is rendered directly to stdout without ratatui so the kitty
/// instance stays tiny. Hyprland positions it via the `flex-notify-toast`
/// window rule.
fn run_toast(id: u32, state_path: Option<&std::path::Path>) {
    use std::io::Write as _;

    let state = notify::load_state(state_path);
    let now = now_secs();

    let item = state.notifications.iter().find(|n| n.id == id);
    let Some(item) = item else {
        return;
    };

    let is_critical = item.urgency == Urgency::Critical;
    let timeout_secs: u64 = if is_critical { 7 } else { 4 };

    let rel = notify::format_relative_time(item.timestamp, now);

    let width = crossterm::terminal::size().map_or(50, |(cols, _)| (cols as usize).clamp(36, 120));

    let card = notify::format_toast_card(item, &rel, width, timeout_secs);
    for (idx, line) in card.lines().enumerate() {
        print!("\x1b[{};1H{line}\x1b[K", idx + 1);
    }
    let _ = std::io::stdout().flush();

    std::thread::sleep(std::time::Duration::from_secs(timeout_secs));

    print!("\x1b[?25h");
    let _ = std::io::stdout().flush();
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        runner::fail(&err);
    }
}

fn run_waybar_status(state_path: Option<&Path>) {
    let mut state = notify::load_state(state_path);
    notify::probe_quick_controls(&mut state.controls);
    let now = now_secs();

    let active: Vec<_> = state
        .notifications
        .iter()
        .filter(|n| {
            !n.is_dismissed && (!n.is_snoozed || n.snooze_until.is_some_and(|until| now >= until))
        })
        .collect();

    let unread_count = active.len();
    let is_dnd = state.controls.dnd.is_active(now);
    let has_critical = active.iter().any(|n| n.urgency == Urgency::Critical);

    let (text, class) = if is_dnd {
        if let Some(rem) = state.controls.dnd.remaining_secs(now) {
            (format!(" 󰂛 {}m", rem / 60), "dnd")
        } else {
            (String::from(" 󰂛 DND"), "dnd")
        }
    } else if has_critical {
        (format!(" 󰀦 {unread_count}"), "critical")
    } else if unread_count > 0 {
        (format!(" 󰂚 {unread_count}"), "has-unread")
    } else {
        (String::from(" 󰂚 0"), "empty")
    };

    let tooltip = if unread_count == 0 {
        if let Some(mpris) = &state.controls.mpris {
            let status_icon = if mpris.is_playing {
                "Playing"
            } else {
                "Paused"
            };
            format!(
                "No unread notifications\n\n󰝚 {}: {} — {}\nStatus: {status_icon} ({})\nClick to open Notification Center",
                mpris.player,
                mpris.title,
                mpris.artist,
                notify::format_duration(mpris.position_secs)
            )
        } else if is_dnd {
            String::from("Do Not Disturb Active\nNo new notifications")
        } else {
            String::from("No unread notifications\nClick to open Notification Center")
        }
    } else {
        let mut lines = vec![format!("{unread_count} unread notification(s):")];

        let mut app_map: std::collections::BTreeMap<&str, Vec<&notify::NotificationItem>> =
            std::collections::BTreeMap::new();
        for item in &active {
            app_map
                .entry(item.app_name.as_str())
                .or_default()
                .push(item);
        }

        let mut sorted_apps: Vec<(&str, Vec<&notify::NotificationItem>)> =
            app_map.into_iter().collect();
        sorted_apps.sort_by_key(|(_, items)| {
            std::cmp::Reverse(items.iter().map(|n| n.timestamp).max().unwrap_or(0))
        });

        for (app, mut items) in sorted_apps {
            items.sort_by_key(|n| std::cmp::Reverse(n.timestamp));
            lines.push(format!("\n󰙯 {app} ({}):", items.len()));
            for item in items.iter().take(3) {
                let rel = notify::format_relative_time(item.timestamp, now);
                if item.body.is_empty() {
                    lines.push(format!("  • ({rel}) {}", item.summary));
                } else {
                    lines.push(format!("  • ({rel}) {}: {}", item.summary, item.body));
                }
            }
            if items.len() > 3 {
                lines.push(format!("    ... and {} more", items.len() - 3));
            }
        }

        if let Some(mpris) = &state.controls.mpris {
            lines.push(format!(
                "\n󰝚 Now Playing: {} — {} ({})",
                mpris.title, mpris.artist, mpris.player
            ));
        }
        lines.join("\n")
    };

    let json = serde_json::json!({
        "text": text,
        "tooltip": tooltip,
        "class": class,
        "count": unread_count,
    });

    println!("{json}");
}

fn handle_op(op: NotifyOp, state_path: Option<&Path>) -> anyhow::Result<()> {
    match op {
        NotifyOp::Daemon { replace } => {
            zbus::block_on(notify::run_daemon(
                state_path.map(Path::to_path_buf),
                replace,
            ))?;
            Ok(())
        }
        NotifyOp::Send {
            summary,
            body,
            app,
            urgency,
            progress,
            image,
            group,
        } => {
            let urg = Urgency::parse(&urgency);
            let mut item = notify::NotificationItem::new(0, app, summary, body, urg);
            item.progress = progress;
            item.image_path = image;
            item.group = group;
            let id = notify::post_notification(item, state_path)?;
            println!("Notification {id} posted");
            Ok(())
        }
        NotifyOp::ClearAll => {
            let mut state = notify::load_state(state_path);
            for n in &mut state.notifications {
                if !n.is_pinned && n.urgency != Urgency::Critical {
                    n.is_dismissed = true;
                }
            }
            notify::save_state(&state, state_path)?;
            println!("Cleared active notifications");
            Ok(())
        }
        NotifyOp::ToggleDnd => {
            let mut state = notify::load_state(state_path);
            let now = now_secs();
            state.controls.dnd = if state.controls.dnd.is_active(now) {
                DndState::Off
            } else {
                DndState::Indefinite
            };
            notify::save_state(&state, state_path)?;
            println!("DND toggled");
            Ok(())
        }
        NotifyOp::Toast { id } => {
            run_toast(id, state_path);
            Ok(())
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let state_path = cli.state_file.as_deref();
    let style = cli.style.options();

    if cli.daemon {
        return zbus::block_on(notify::run_daemon(cli.state_file, false));
    }

    if let Some(op) = cli.op {
        return handle_op(op, state_path);
    }

    if cli.clear_all {
        let mut state = notify::load_state(state_path);
        for n in &mut state.notifications {
            if !n.is_pinned && n.urgency != Urgency::Critical {
                n.is_dismissed = true;
            }
        }
        notify::save_state(&state, state_path)?;
        println!("Cleared active notifications");
        return Ok(());
    }

    if cli.toggle_dnd {
        let mut state = notify::load_state(state_path);
        let now = now_secs();
        state.controls.dnd = if state.controls.dnd.is_active(now) {
            DndState::Off
        } else {
            DndState::Indefinite
        };
        notify::save_state(&state, state_path)?;
        println!("DND toggled");
        return Ok(());
    }

    if cli.menu {
        runner::popup_guard(Provider::Notify)?;

        if cli.print_action {
            return runner::run_select(Provider::Notify, style).await;
        }

        loop {
            let menu = runner::build_menu(Provider::Notify, style)?;
            match flex_core::run::run_capture(menu).await? {
                Outcome::Chosen {
                    action_id, label, ..
                }
                | Outcome::Toggle {
                    action_id, label, ..
                } => {
                    if let Ok(report) = provider::execute(&action_id, &label, state_path) {
                        if report.should_close {
                            std::process::exit(0);
                        }
                    }
                }
                Outcome::Target { target, title, .. } => {
                    if let Ok(report) = provider::execute(&target, &title, state_path) {
                        if report.should_close {
                            std::process::exit(0);
                        }
                    }
                }
                Outcome::Delete { action_id, .. } => {
                    let action = if action_id.starts_with("notif:") {
                        format!("dismiss:{}", action_id.trim_start_matches("notif:"))
                    } else if action_id == provider::ACTION_QUICK_CONTROLS
                        || action_id == "quick:controls"
                        || action_id == "quick_controls"
                        || action_id == "clear_all"
                        || action_id == provider::ACTION_CLEAR_ALL
                    {
                        provider::ACTION_CLEAR_ALL.to_string()
                    } else {
                        format!("dismiss:{action_id}")
                    };
                    let _ = provider::execute(&action, "Dismiss", state_path);
                }
                Outcome::Quit { code } => {
                    std::process::exit(code);
                }
                Outcome::Cancelled => {
                    std::process::exit(EXIT_CANCELLED);
                }
            }
        }
    }
    run_waybar_status(state_path);
    Ok(())
}
