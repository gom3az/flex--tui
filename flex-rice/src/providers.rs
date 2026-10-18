//! Providers: each parses one subprocess's stdout once into `Vec<Row>`.
//!
//! This is the rice-specific half of flex: every provider reads this
//! machine's tools and `$HOME` conventions, and every provider's action is
//! executed by the matching `exec` module, never here.

#[path = "providers/theme_.rs"]
pub mod theme_;

pub mod bt;
pub mod clip;
pub mod launch;
pub mod net;
pub mod notify;
pub mod power;
pub mod proc;
pub mod profile;
pub mod rec_opt;
pub mod shot;
pub mod wallpaper;
pub mod wifi;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use flex_core::{Menu, Row, RowId, Tab, TickHook};

use crate::spawn::RetryExec as _;

/// No-op row id (the executor exits 0, no effect).
///
/// Defined once here because every provider's empty state ends up as a
/// `noop` row (B-026).
pub const NOOP_ID: &str = "noop";

/// Placeholder row for a provider whose scan found nothing.
///
/// Selecting it yields the `noop` id, which every executor treats as a
/// no-op: the menu is never blank, and `Enter` on the placeholder
/// cannot act on a row that does not exist (B-026).
#[must_use]
pub fn empty_row(label: &str) -> Row {
    Row::new(RowId::new(NOOP_ID), label)
}

/// Read one snapshot: fixture file when `$file_env` is set (tests), else
/// run `cmd` with `args` once. `None` on any failure — missing command,
/// failing exit, unreadable fixture — so callers degrade to offline or
/// empty rows. A set-but-empty seam forces `None` (offline-path tests).
/// Child stderr is discarded (bash `2>/dev/null` parity).
///
/// Shared by the `wifi` and `bt` providers (same seam contract, different
/// `$WIFI_*`/`$BT_*` variables), so the rule lives in exactly one place.
pub(crate) fn snapshot(file_env: &str, cmd: &str, args: &[&str]) -> Option<String> {
    if let Ok(path) = std::env::var(file_env) {
        if path.is_empty() {
            return None;
        }
        return std::fs::read_to_string(path).ok();
    }
    let output = std::process::Command::new(cmd)
        .args(args)
        .output_retrying()
        .ok()?;
    if output.status.success() {
        Some(crate::tools::decode_stdout(output.stdout))
    } else {
        None
    }
}

/// Per-tick refresh for the providers whose rows go stale while the menu is
/// open: `wifi` swaps in a background scan once it finishes, and `proc`
/// re-sweeps `/proc` keeping the cursor on the same pid.
///
/// Installed by [`menu`]; filter, focus and scroll survive all refreshes.
///
/// OPT-6: the 1 s engine tick stays for gauges/clock only. `proc`/`net`
/// refresh at most every 3 s, `bt` every 5 s, `notify` every 2 s, and only
/// when one of that provider's tabs is active — idle on an unrelated tab
/// performs no spawns or disk reads. `wifi` stays event-driven (its
/// background scan wakes the UI via `wake_ui` when finished).
pub fn tick_hook(menu: &mut Menu) {
    if menu.provider == wifi::PROVIDER {
        wifi::refresh_scan(menu);
    }
    if menu.provider == proc::PROVIDER
        && active_tab_is(menu, &[proc::TAB_NAME])
        && throttle_due("proc", Duration::from_secs(3))
    {
        proc::refresh(menu);
    }
    if menu.provider == net::PROVIDER
        && active_tab_is(
            menu,
            &[net::TAB_BANDWIDTH, net::TAB_INTERFACES, net::TAB_SPEEDTEST],
        )
        && throttle_due("net", Duration::from_secs(3))
    {
        net::refresh(menu);
    }
    if menu.provider == bt::PROVIDER
        && active_tab_is(menu, &[bt::TAB_DEVICES, bt::TAB_ADAPTERS])
        && throttle_due("bt", Duration::from_secs(5))
    {
        bt::refresh(menu);
    }
    if menu.provider == notify::PROVIDER
        && active_tab_is(menu, &[notify::TAB_FEED, notify::TAB_HISTORY])
        && throttle_due("notify", Duration::from_secs(2))
    {
        notify::refresh(menu);
    }
}

/// [`Menu::new`] with [`tick_hook`] installed.
///
/// Use this instead of `Menu::new` anywhere in this crate: the engine has no
/// knowledge of which providers need refreshing, so a menu built without the
/// hook would silently stop updating gauges and Wi-Fi scans.
#[must_use]
pub fn menu(provider: impl Into<String>, tabs: Vec<Tab>) -> Menu {
    let hook: TickHook = tick_hook;
    Menu::new(provider, tabs).on_tick(hook)
}

/// Whether the active tab is one of `names` (OPT-6 active-tab gate).
fn active_tab_is(menu: &Menu, names: &[&str]) -> bool {
    menu.app
        .active_tab()
        .is_some_and(|tab| names.contains(&tab.name.as_str()))
}

/// Per-provider last-refresh instants for OPT-6 throttling.
fn last_ticks() -> &'static Mutex<HashMap<&'static str, Instant>> {
    static LAST: OnceLock<Mutex<HashMap<&'static str, Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `key` may refresh now; records the instant on success.
///
/// First call for a key always succeeds so the first tick after opening a
/// tab is fresh. Poisoned locks degrade to "due" (a refresh is always safe).
fn throttle_due(key: &'static str, interval: Duration) -> bool {
    let now = Instant::now();
    let Ok(mut guard) = last_ticks().lock() else {
        return true;
    };
    match guard.get(key) {
        Some(last) if now.duration_since(*last) < interval => false,
        _ => {
            guard.insert(key, now);
            true
        }
    }
}

/// Single focus-restore pass (OPT-9): move focus to the row with `previous`
/// id in the filtered view, then clamp. No-op when `previous` is `None`.
pub(crate) fn restore_focus(menu: &mut Menu, previous: Option<RowId>) {
    let Some(id) = previous else {
        menu.app.clamp_focus();
        return;
    };
    let position = menu.app.active_tab().and_then(|tab| {
        menu.app
            .visible_rows()
            .iter()
            .position(|&index| tab.rows.get(index).is_some_and(|row| row.id == id))
    });
    if let Some(position) = position {
        if let Some(state) = menu.app.active_tab_mut().map(|tab| &mut tab.state) {
            state.focus = position;
        }
    }
    menu.app.clamp_focus();
}

/// In-place row sync (OPT-9): when `current` and `fresh` share the same ids
/// in order, rewrite only changed fields so existing `Row`/`String`
/// allocations (and stable ids/targets) survive the tick; otherwise fall back
/// to wholesale replace via `mem::replace`. Row count/order changes (new/dead
/// pids) take the fallback path.
///
/// Every semantic field is covered (`PartialEq` minus the lazy `text_cache`,
/// which refreshes itself on read): a same-shape tick is indistinguishable
/// from a replace, so providers may toggle flags like `hide_target_in_header`
/// or `confirmable` between ticks without an id change.
pub(crate) fn sync_rows_in_place(current: &mut Vec<Row>, fresh: Vec<Row>) {
    let same_shape = current.len() == fresh.len()
        && current
            .iter()
            .zip(fresh.iter())
            .all(|(old, new)| old.id == new.id);
    if !same_shape {
        *current = fresh;
        return;
    }
    for (old, new) in current.iter_mut().zip(fresh) {
        if old.label != new.label {
            old.label = new.label;
        }
        if old.meta != new.meta {
            old.meta = new.meta;
        }
        if old.confirmable != new.confirmable {
            old.confirmable = new.confirmable;
        }
        if old.volume != new.volume {
            old.volume = new.volume;
        }
        if old.muted != new.muted {
            old.muted = new.muted;
        }
        if old.peaks != new.peaks {
            old.peaks = new.peaks;
        }
        if old.config != new.config {
            old.config = new.config;
        }
        if old.offline != new.offline {
            old.offline = new.offline;
        }
        if old.is_default != new.is_default {
            old.is_default = new.is_default;
        }
        if old.sublabel != new.sublabel {
            old.sublabel = new.sublabel;
        }
        if old.detail != new.detail {
            old.detail = new.detail;
        }
        if old.targets != new.targets {
            old.targets = new.targets;
        }
        if old.target_index != new.target_index {
            old.target_index = new.target_index;
        }
        if old.preview_image != new.preview_image {
            old.preview_image = new.preview_image;
        }
        if old.hide_target_in_header != new.hide_target_in_header {
            old.hide_target_in_header = new.hide_target_in_header;
        }
        if old.compact != new.compact {
            old.compact = new.compact;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same-shape ticks must propagate every semantic flag, not just the
    /// originally-covered subset: a provider that toggles e.g.
    /// `hide_target_in_header` between ticks without an id change must see
    /// it on screen (previously it stayed stale until an id reorder forced
    /// the replace fallback).
    #[test]
    fn sync_rows_in_place_propagates_every_flag() {
        let mut current = vec![
            Row::new(RowId::new("a"), "a"),
            Row::new(RowId::new("b"), "b"),
        ];
        let mut fresh_a = Row::with_meta(RowId::new("a"), "a2", "m");
        fresh_a.confirmable = true;
        fresh_a.muted = true;
        fresh_a.peaks = Some(flex_core::RowPeaks::Mono(0.5));
        fresh_a.config = Some(String::from("cfg"));
        fresh_a.preview_image = Some(String::from("/tmp/x.png"));
        fresh_a.hide_target_in_header = true;
        fresh_a.compact = true;
        let fresh = vec![fresh_a, Row::new(RowId::new("b"), "b")];

        super::sync_rows_in_place(&mut current, fresh.clone());

        assert_eq!(current, fresh, "same-shape sync must equal a replace");
    }

    /// Shape changes still take the wholesale-replace path.
    #[test]
    fn sync_rows_in_place_replaces_on_shape_change() {
        let mut current = vec![Row::new(RowId::new("a"), "a")];
        let fresh = vec![
            Row::new(RowId::new("a"), "a"),
            Row::new(RowId::new("b"), "b"),
        ];
        super::sync_rows_in_place(&mut current, fresh.clone());
        assert_eq!(current, fresh);
    }
}
