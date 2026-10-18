//! `net` provider: bandwidth consumption monitor, network interface status, and speedtest benchmark.
//!
//! Implements Tab 1 (`Bandwidth`) with interface totals plus per-process TCP attribution
//! ("Top Talkers") and the unattributed remainder, Tab 2 (`Interfaces`) with network
//! interface metadata and default route indicators, and Tab 3 (`Speedtest`) with
//! non-blocking network throughput & latency benchmarks.

use flex_core::{Menu, Row, RowId, Tab, Target};

use crate::exec::net::{
    format_bytes, format_speed, scan_interfaces, snapshot_bandwidth, BandwidthSnapshot,
    InterfaceInfo,
};
use crate::exec::speedtest::{self, SpeedtestPhase, SpeedtestSnapshot};
use crate::providers;

/// Provider name for the `ACTION:` line.
pub const PROVIDER: &str = "net";

pub const TAB_BANDWIDTH: &str = "Bandwidth";

pub const TAB_INTERFACES: &str = "Interfaces";

pub const TAB_SPEEDTEST: &str = "Speedtest";

pub const SPEEDTEST_RUN_ID: &str = "speedtest:run";

/// Wire-share percentage for a compact text gauge (0–100, rounded).
fn share_pct(share: f32) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let pct = (f64::from(share) * 100.0).round() as i64;
    pct.clamp(0, 100)
}

/// Build the `Bandwidth` tab: interface total, TCP-attributed processes,
/// and the unattributed remainder.
#[must_use]
pub fn bandwidth_tab() -> Tab {
    let snapshot = snapshot_bandwidth();
    let rows = build_bandwidth_rows(&snapshot);
    let mut tab = Tab::with_rows(TAB_BANDWIDTH, rows);
    tab.bare_rows = false;
    tab.filterable = true;
    tab.deletable = true;
    tab
}

/// Convert one [`BandwidthSnapshot`] into wiremix `Row` objects.
///
/// Public so integration tests can pin the row shapes against fabricated
/// snapshots without touching the network (live `ss` only runs inside
/// [`snapshot_bandwidth`](crate::exec::net::snapshot_bandwidth)).
///
/// Row 0 is the interface total (same sampler as waybar, always agrees with
/// the bar); process rows carry per-connection TCP rates with a wifi-style
/// wire-share text gauge in `meta` (`46% [████░░░░]`); the trailing
/// `unattributed` row holds whatever the sockets don't explain (UDP/QUIC,
/// churned or short-lived connections, kernel traffic).
/// Process rows keep the `proc:<pid>` id so kill/signal actions still work;
/// header and remainder ids are inert no-ops in `exec::net::execute`.
///
/// Compact by construction: no row sets `volume`/`config`/`detail`, so every
/// row renders 1-line like the wifi picker (see `NodeMetrics::for_row`).
#[must_use]
pub fn build_bandwidth_rows(snapshot: &BandwidthSnapshot) -> Vec<Row> {
    use std::fmt::Write as _;

    let mut rows = Vec::with_capacity(snapshot.talkers.len() + 2);
    if let Some((iface, rx_rate, tx_rate)) = snapshot.iface.as_ref() {
        let meta = format!(
            "⬇ {}  ⬆ {}  (interface total)",
            format_speed(*rx_rate),
            format_speed(*tx_rate)
        );
        rows.push(Row::with_meta(
            RowId::new(format!("iface:{iface}")),
            (*iface).clone(),
            meta,
        ));
    }
    rows.extend(snapshot.talkers.iter().map(|process| {
        let id = RowId::new(format!("proc:{}", process.pid));
        let mut label = String::with_capacity(process.comm.len() + 1 + 10);
        label.push_str(&process.comm);
        label.push(' ');
        let _ = write!(label, "{}", process.pid);
        let meta = format!(
            "{:>3}% {}  ⬇ {}  ⬆ {}",
            share_pct(process.share),
            super::wifi::bar(share_pct(process.share), 100, 8),
            format_speed(process.rx_rate),
            format_speed(process.tx_rate)
        );
        let mut row = Row::with_meta(id, label, meta);
        row.confirmable = true;
        row
    }));
    let (unattributed_rx, unattributed_tx) = snapshot.unattributed;
    if snapshot.iface.is_some() && (unattributed_rx > 0.0 || unattributed_tx > 0.0) {
        let total = snapshot
            .iface
            .as_ref()
            .map_or(0.0, |(_, rx_rate, tx_rate)| rx_rate + tx_rate);
        #[allow(clippy::cast_possible_truncation)]
        let share = if total > 0.0 {
            ((unattributed_rx + unattributed_tx) / total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        #[allow(clippy::cast_possible_truncation)]
        let pct = (share * 100.0).round() as i64;
        let row = Row::with_meta(
            RowId::new(providers::NOOP_ID),
            "unattributed (UDP · short-lived · kernel)",
            format!(
                "{pct:>3}% {}  ⬇ {}  ⬆ {}",
                super::wifi::bar(pct, 100, 8),
                format_speed(unattributed_rx),
                format_speed(unattributed_tx)
            ),
        );
        rows.push(row);
    }
    if rows.is_empty() {
        return vec![providers::empty_row("— no active network processes —")];
    }
    rows
}

/// Build the `Interfaces` tab: lists network interfaces and addresses.
#[must_use]
pub fn interfaces_tab() -> Tab {
    let ifaces = scan_interfaces();
    let rows = build_interface_rows(&ifaces);
    let mut tab = Tab::with_rows(TAB_INTERFACES, rows);
    tab.bare_rows = false;
    tab.filterable = true;
    tab.deletable = false;
    tab
}

/// Convert interface records into wiremix `Row` objects.
fn build_interface_rows(ifaces: &[InterfaceInfo]) -> Vec<Row> {
    if ifaces.is_empty() {
        return vec![providers::empty_row("— no network interfaces —")];
    }

    ifaces
        .iter()
        .map(|iface| {
            let id = RowId::new(format!("iface:{}", iface.name));
            let label = format!("{} ({})", iface.name, iface.operstate);
            let ip_part = iface.ip_cidr.as_deref().unwrap_or("no carrier");
            let meta = format!(
                "{ip_part}  ·  ⬇ {}  ⬆ {}",
                format_bytes(iface.rx_bytes),
                format_bytes(iface.tx_bytes)
            );

            let mut row = Row::with_meta(id, label, meta);
            row.is_default = iface.is_default;

            let mut targets = Vec::new();
            if let Some(ref ip) = iface.ip_cidr {
                let clean_ip = ip.split('/').next().unwrap_or(ip);
                targets.push(Target::new(
                    RowId::new(format!("copy:{clean_ip}")),
                    format!("Copy IPv4 ({clean_ip})"),
                ));
            }
            if let Some(ref gw) = iface.gateway {
                targets.push(Target::new(
                    RowId::new(format!("copy:{gw}")),
                    format!("Copy Gateway ({gw})"),
                ));
            }
            if iface.name.starts_with("wl") {
                targets.push(Target::new(RowId::new("wifi"), "Open Wi-Fi Picker"));
            }

            row.targets = targets;
            row
        })
        .collect()
}

/// Build the `Speedtest` tab: interactive throughput & latency benchmark.
#[must_use]
pub fn speedtest_tab() -> Tab {
    let snapshot = speedtest::get_snapshot();
    let rows = build_speedtest_rows(&snapshot);
    let mut tab = Tab::with_rows(TAB_SPEEDTEST, rows);
    tab.bare_rows = false;
    tab.filterable = false;
    tab.deletable = false;
    tab
}

/// Convert speedtest state snapshot into wiremix `Row` objects.
#[must_use]
pub fn build_speedtest_rows(snapshot: &SpeedtestSnapshot) -> Vec<Row> {
    let run_meta = match snapshot.phase {
        SpeedtestPhase::Idle => "[Enter] Start",
        SpeedtestPhase::TestingPing => "Probing Ping...",
        SpeedtestPhase::TestingDownload => "Testing Download...",
        SpeedtestPhase::TestingUpload => "Testing Upload...",
        SpeedtestPhase::Complete => "[Enter] Retest",
        SpeedtestPhase::Failed => "[Enter] Retry",
    };

    let run_row = Row::with_meta(RowId::new(SPEEDTEST_RUN_ID), "Run Full Speedtest", run_meta);

    let ping_row = Row::with_meta(
        RowId::new("speedtest:ping"),
        "Latency / Ping",
        snapshot.ping_display(),
    );

    let download_row = Row::with_meta(
        RowId::new("speedtest:download"),
        "Download Speed",
        snapshot.download_display(),
    );

    let upload_row = Row::with_meta(
        RowId::new("speedtest:upload"),
        "Upload Speed",
        snapshot.upload_display(),
    );

    let server_row = Row::with_meta(
        RowId::new("speedtest:server"),
        "Target Server",
        snapshot.target_server.clone(),
    );

    vec![run_row, ping_row, download_row, upload_row, server_row]
}

/// Build the complete interactive `flex-net` menu.
#[must_use]
pub fn net_menu() -> Menu {
    let mut tabs = vec![bandwidth_tab(), interfaces_tab(), speedtest_tab()];
    // Pids and scan snapshots come and go: searchable, but nothing stable
    // worth learning.
    for tab in &mut tabs {
        tab.learnable = false;
    }
    providers::menu(PROVIDER, tabs)
}

/// Per-tick refresh for `flex-net`.
///
/// OPT-6: skips unless a `net` tab is active (throttling to 3 s lives in
/// [`providers::tick_hook`]). OPT-9: reuses row allocations via
/// [`providers::sync_rows_in_place`] with a single focus-restore pass.
pub fn refresh(menu: &mut Menu) {
    let active = menu.app.active_tab().is_some_and(|tab| {
        tab.name == TAB_BANDWIDTH || tab.name == TAB_INTERFACES || tab.name == TAB_SPEEDTEST
    });
    if !active {
        return;
    }
    let previous = menu.app.focused_row().map(|row| row.id.clone());

    if let Some(tab) = menu
        .app
        .tabs
        .iter_mut()
        .find(|tab| tab.name == TAB_BANDWIDTH)
    {
        let snapshot = snapshot_bandwidth();
        let fresh = build_bandwidth_rows(&snapshot);
        super::sync_rows_in_place(&mut tab.rows, fresh);
    }

    if let Some(tab) = menu
        .app
        .tabs
        .iter_mut()
        .find(|tab| tab.name == TAB_INTERFACES)
    {
        let ifaces = scan_interfaces();
        let fresh = build_interface_rows(&ifaces);
        super::sync_rows_in_place(&mut tab.rows, fresh);
    }

    if let Some(tab) = menu
        .app
        .tabs
        .iter_mut()
        .find(|tab| tab.name == TAB_SPEEDTEST)
    {
        let snapshot = speedtest::get_snapshot();
        let fresh = build_speedtest_rows(&snapshot);
        super::sync_rows_in_place(&mut tab.rows, fresh);
    }

    super::restore_focus(menu, previous);
}
