//! Bluetooth provider (`flex bt` / `flex-bt`): native Bluetooth manager.
//!
//! Provides two wiremix-compliant tabs:
//! - Tab 1: `[Devices]`
//!   - Top row: Bluetooth power toggle (`Turn Bluetooth Off / On`).
//!   - Device rows: `<name> <mac>`, marker `◇` on connected audio sink, meta
//!     `85% [███████░] Connected · Audio` or `Available (-64 dBm)` —
//!     battery renders as a wifi-style text gauge in `meta`, never as
//!     `Row::volume`, so every row stays 1-line compact like the wifi
//!     picker.
//!   - Dropdown targets: Connect, Disconnect, Set as Default Audio Sink,
//!     Switch to High Quality (A2DP), Switch to Headset Mode (HFP), Unpair (Forget Device).
//! - Tab 2: `[Adapters]`
//!   - Controller status, Discoverable, Pairable, and Scanning controls.
//!
//! Periodic refreshes are handled by [`refresh`] via [`providers::tick_hook`].

use std::path::Path;

use flex_core::{Menu, Row, RowId, Tab, Target};

use crate::providers::{self};

/// Provider name for the `ACTION:` line.
pub const PROVIDER: &str = "bt";

/// Tab 1 title: Devices.
pub const TAB_DEVICES: &str = "Devices";

/// Tab 2 title: Adapters.
pub const TAB_ADAPTERS: &str = "Adapters";

/// Action ID: Turn Bluetooth power on.
pub const POWER_ON_ID: &str = "power:on";

/// Action ID: Turn Bluetooth power off.
pub const POWER_OFF_ID: &str = "power:off";

/// Snapshot seam for `bluetoothctl show` output.
pub const BT_SHOW_FILE_ENV: &str = "BT_SHOW_FILE";

/// Snapshot seam for `bluetoothctl devices` output.
pub const BT_DEVICES_FILE_ENV: &str = "BT_DEVICES_FILE";

/// Snapshot seam directory: `$DIR/{mac}` holds `bluetoothctl info {mac}` output.
pub const BT_INFO_DIR_ENV: &str = "BT_INFO_DIR";

/// Snapshot seam for `wpctl status` or default audio sink probe.
pub const BT_DEFAULT_SINK_FILE_ENV: &str = "BT_DEFAULT_SINK_FILE";

/// Information about a Bluetooth adapter / controller parsed from `bluetoothctl show`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct BtAdapterInfo {
    /// Controller MAC address.
    pub mac: String,
    /// Controller display name / alias.
    pub name: String,
    /// Whether the controller is powered on.
    pub powered: bool,
    /// Whether the controller is discoverable.
    pub discoverable: bool,
    /// Whether the controller is pairable.
    pub pairable: bool,
    /// Whether discovery / scanning is actively running.
    pub discovering: bool,
}

/// Information about a Bluetooth remote device.
#[derive(Debug, Clone, PartialEq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct BtDeviceInfo {
    /// Device MAC address.
    pub mac: String,
    /// Device name / alias.
    pub name: String,
    /// Whether the device is connected.
    pub connected: bool,
    /// Whether the device is paired.
    pub paired: bool,
    /// Whether the device is trusted.
    pub trusted: bool,
    /// Battery level percentage (0..=100), if known.
    pub battery: Option<u8>,
    /// Signal strength RSSI (dBm), if known.
    pub rssi: Option<i32>,
    /// Whether the device is an audio sink/source.
    pub is_audio: bool,
    /// Whether this device is the default audio sink.
    pub is_default_sink: bool,
}

/// Parse `bluetoothctl show` output into [`BtAdapterInfo`].
#[must_use]
pub fn parse_controller(text: &str) -> Option<BtAdapterInfo> {
    let mut info = BtAdapterInfo::default();
    let mut found = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Controller ") {
            if let Some(mac) = rest.split_whitespace().next() {
                info.mac = mac.to_string();
                found = true;
            }
        } else if let Some(name) = trimmed.strip_prefix("Name: ") {
            info.name = name.trim().to_string();
        } else if let Some(alias) = trimmed.strip_prefix("Alias: ") {
            if info.name.is_empty() {
                info.name = alias.trim().to_string();
            }
        } else if let Some(powered) = trimmed.strip_prefix("Powered: ") {
            info.powered = powered.trim() == "yes";
        } else if let Some(disc) = trimmed.strip_prefix("Discoverable: ") {
            info.discoverable = disc.trim() == "yes";
        } else if let Some(pair) = trimmed.strip_prefix("Pairable: ") {
            info.pairable = pair.trim() == "yes";
        } else if let Some(discovering) = trimmed.strip_prefix("Discovering: ") {
            info.discovering = discovering.trim() == "yes";
        }
    }

    if found || !info.mac.is_empty() || info.powered {
        Some(info)
    } else {
        None
    }
}

/// Parse `bluetoothctl devices` into a list of `(mac, name)` tuples.
#[must_use]
pub fn parse_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Device ")?;
            let (mac, name) = rest.split_once(' ')?;
            if mac.is_empty() {
                return None;
            }
            Some((mac.trim().to_string(), name.trim().to_string()))
        })
        .collect()
}

/// Parse battery percentage from `bluetoothctl info` output.
///
/// Handles `Battery Percentage: 0x55 (85)`, `Battery Percentage: 85%`, `Battery Percentage: 85`.
#[must_use]
pub fn parse_battery(info_out: &str) -> Option<u8> {
    for line in info_out.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Battery Percentage:") {
            let rest = rest.trim();
            if let (Some(start), Some(end)) = (rest.find('('), rest.find(')')) {
                if start < end {
                    if let Ok(val) = rest[start + 1..end].trim().parse::<u8>() {
                        return Some(val.min(100));
                    }
                }
            }
            let cleaned = rest.trim_end_matches('%').trim();
            if let Ok(val) = cleaned.parse::<u8>() {
                return Some(val.min(100));
            }
        }
    }
    None
}

/// Parse RSSI signal strength from `bluetoothctl info` output.
///
/// Handles `RSSI: -64` or `RSSI: -64 dBm`.
#[must_use]
pub fn parse_rssi(info_out: &str) -> Option<i32> {
    for line in info_out.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("RSSI:") {
            let cleaned = rest.trim().trim_end_matches("dBm").trim();
            if let Ok(val) = cleaned.parse::<i32>() {
                return Some(val);
            }
        }
    }
    None
}

/// Check whether the device info indicates an audio device.
///
/// OPT-8: byte-level `starts_with`/substring pre-check runs before the
/// `to_lowercase` allocation, so non-audio `info` outputs skip the fold.
#[must_use]
pub fn is_audio_device(info_out: &str) -> bool {
    if !contains_audio_token_ascii(info_out) {
        return false;
    }
    let lower = info_out.to_lowercase();
    lower.contains("audio sink")
        || lower.contains("audio source")
        || lower.contains("advanced audio distribu")
        || lower.contains("handsfree")
        || lower.contains("headset")
        || lower.contains("a/v remote control")
        || lower.contains("icon: audio-")
}

/// Case-insensitive ASCII pre-filter for [`is_audio_device`]: true when the
/// text could contain one of the audio markers, without allocating.
fn contains_audio_token_ascii(text: &str) -> bool {
    const TOKENS: &[&[u8]] = &[
        b"audio", b"hands", b"headset", b"a/v", b"icon:", b"hfp", b"a2dp",
    ];
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    for token in TOKENS {
        if bytes.len() < token.len() {
            continue;
        }
        // Sliding ASCII case-insensitive search.
        for window in bytes.windows(token.len()) {
            let mut hit = true;
            for (a, b) in window.iter().zip(token.iter()) {
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

/// Check if the MAC or name matches the active default audio sink in `PipeWire` / `PulseAudio` status.
#[must_use]
pub fn is_default_audio_sink(mac: &str, name: &str, default_sink_status: Option<&str>) -> bool {
    let Some(status) = default_sink_status else {
        return false;
    };
    // One fold per device (was: `replace` + two `to_lowercase` + one more
    // for the name). `replace` after folding is equivalent for ASCII MACs.
    let mac_colon = mac.to_lowercase();
    let mac_underscore_lower = mac_colon.replace(':', "_");
    let name_lower = name.to_lowercase();

    for line in status.lines() {
        // Byte-level default-line pre-check before any allocation: most
        // status lines are not default lines, so they skip the fold.
        let is_default_line = line.contains('*')
            || contains_ascii_insensitive(line, "default sink:")
            || contains_ascii_insensitive(line, "default audio sink");
        if !is_default_line {
            continue;
        }
        // Case-insensitive substring search without folding the whole line
        // (was: `line.to_lowercase()` per status line per device).
        if contains_ascii_insensitive(line, &mac_colon)
            || contains_ascii_insensitive(line, &mac_underscore_lower)
            || (!name_lower.is_empty() && contains_ascii_insensitive(line, &name_lower))
        {
            return true;
        }
    }
    false
}

/// ASCII case-insensitive substring search with no allocation.
fn contains_ascii_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay = haystack.as_bytes();
    let ndl = needle.as_bytes();
    if ndl.len() > hay.len() {
        return false;
    }
    // Fast path: first-byte filter before the full window compare.
    let first = ndl[0].to_ascii_lowercase();
    for window in hay.windows(ndl.len()) {
        if window[0].to_ascii_lowercase() != first {
            continue;
        }
        let mut hit = true;
        for (a, b) in window.iter().zip(ndl.iter()) {
            if !a.eq_ignore_ascii_case(b) {
                hit = false;
                break;
            }
        }
        if hit {
            return true;
        }
    }
    false
}

/// Construct the meta string for a device row, wifi-compact style.
///
/// Connected rows with a known battery lead with a text gauge
/// (`85% [███████░]`); rows without a battery keep the plain status text.
/// Compact by construction: the gauge lives in `meta`, never in
/// `Row::volume`, so the row renders 1-line.
#[must_use]
pub fn build_device_meta(
    connected: bool,
    battery: Option<u8>,
    is_audio: bool,
    rssi: Option<i32>,
    paired: bool,
) -> String {
    if connected {
        match (battery, is_audio) {
            (Some(b), true) => format!(
                "{b:>3}% {} Connected · Audio",
                super::wifi::bar(i64::from(b), 100, 8)
            ),
            (Some(b), false) => {
                format!(
                    "{b:>3}% {} Connected",
                    super::wifi::bar(i64::from(b), 100, 8)
                )
            }
            (None, true) => String::from("Connected · Audio"),
            (None, false) => String::from("Connected"),
        }
    } else if let Some(r) = rssi {
        format!("Available ({r} dBm)")
    } else if paired {
        String::from("Paired")
    } else {
        String::from("Available")
    }
}

/// Build a wiremix [`Row`] for a Bluetooth device.
#[must_use]
pub fn build_device_row(
    mac: &str,
    name: &str,
    info_out: Option<&str>,
    default_sink_status: Option<&str>,
) -> Row {
    let info = info_out.unwrap_or_default();
    let connected = info.contains("Connected: yes");
    let paired = info.contains("Paired: yes");
    let battery = parse_battery(info);
    let rssi = parse_rssi(info);
    let is_audio = is_audio_device(info);
    let is_default = connected && is_audio && is_default_audio_sink(mac, name, default_sink_status);

    let label = if name.is_empty() {
        mac.to_string()
    } else {
        format!("{name} {mac}")
    };

    let meta = build_device_meta(connected, battery, is_audio, rssi, paired);
    let id = RowId::new(format!("device:{mac}"));
    let mut row = Row::with_meta(id, label, meta);
    row.is_default = is_default;
    // Rows with targets render the current target (Connect/Disconnect) in
    // the header right column instead of `meta`; hide it so the battery
    // gauge + status in `meta` stays visible on the 1-line compact row
    // (the dropdown still lists every target).
    row.hide_target_in_header = true;

    let mut sink_target = Target::new(
        RowId::new(format!("sink:{mac}")),
        "Set as Default Audio Sink",
    );
    if is_default {
        sink_target.is_default = true;
    }

    row.targets = vec![
        Target::new(RowId::new(format!("connect:{mac}")), "Connect"),
        Target::new(RowId::new(format!("disconnect:{mac}")), "Disconnect"),
        sink_target,
        Target::new(
            RowId::new(format!("profile:a2dp:{mac}")),
            "Switch to High Quality (A2DP)",
        ),
        Target::new(
            RowId::new(format!("profile:hfp:{mac}")),
            "Switch to Headset Mode (HFP)",
        ),
        Target::new(
            RowId::new(format!("unpair:{mac}")),
            "Unpair (Forget Device)",
        ),
    ];

    row.target_index = usize::from(connected);
    row
}

/// Build Tab 1 (`[Devices]`) from snapshot data.
#[must_use]
pub fn devices_tab_from(
    adapter: Option<&BtAdapterInfo>,
    devices_out: Option<&str>,
    info_fn: &dyn Fn(&str) -> Option<String>,
    sink_status: Option<&str>,
) -> Tab {
    let mut rows = Vec::new();

    let powered = adapter.is_some_and(|a| a.powered);
    if let Some(a) = adapter {
        if a.powered {
            rows.push(Row::with_meta(
                RowId::new(POWER_OFF_ID),
                "Turn Bluetooth Off",
                "bluetoothctl power off",
            ));
        } else {
            rows.push(Row::with_meta(
                RowId::new(POWER_ON_ID),
                "Turn Bluetooth On",
                "bluetoothctl power on",
            ));
        }
    } else {
        // Missing controller / offline
        let mut tab = Tab::with_rows(
            TAB_DEVICES,
            vec![Row::offline_placeholder(
                RowId::new(providers::NOOP_ID),
                flex_core::strings::OFFLINE_LABEL,
            )],
        );
        tab.bare_rows = false;
        tab.filterable = false;
        tab.deletable = false;
        return tab;
    }

    if powered {
        if let Some(text) = devices_out {
            let devs = parse_devices(text);
            if devs.is_empty() {
                rows.push(Row::new(
                    RowId::new(providers::NOOP_ID),
                    "(No Bluetooth devices)",
                ));
            } else {
                for (mac, name) in &devs {
                    let info = info_fn(mac);
                    rows.push(build_device_row(mac, name, info.as_deref(), sink_status));
                }
            }
        } else {
            rows.push(Row::new(
                RowId::new(providers::NOOP_ID),
                "(No Bluetooth devices)",
            ));
        }
    }

    let mut tab = Tab::with_rows(TAB_DEVICES, rows);
    tab.bare_rows = false;
    tab.filterable = false;
    tab.deletable = false;
    tab
}

/// Build Tab 2 (`[Adapters]`) from adapter snapshot data.
#[must_use]
pub fn adapters_tab_from(adapter: Option<&BtAdapterInfo>) -> Tab {
    let Some(a) = adapter else {
        let mut tab = Tab::with_rows(
            TAB_ADAPTERS,
            vec![Row::offline_placeholder(
                RowId::new(providers::NOOP_ID),
                flex_core::strings::OFFLINE_LABEL,
            )],
        );
        tab.bare_rows = false;
        tab.filterable = false;
        tab.deletable = false;
        return tab;
    };

    let controller_label = if a.name.is_empty() {
        format!("Controller {}", a.mac)
    } else {
        format!("Controller {} ({})", a.name, a.mac)
    };

    let controller_meta = format!(
        "Powered: {} · Discovering: {}",
        if a.powered { "yes" } else { "no" },
        if a.discovering { "yes" } else { "no" }
    );

    let mut controller_row = Row::with_meta(
        RowId::new(format!("adapter:info:{}", a.mac)),
        controller_label,
        controller_meta,
    );
    controller_row.targets = vec![
        Target::new(RowId::new("adapter:power:on"), "Power On"),
        Target::new(RowId::new("adapter:power:off"), "Power Off"),
        Target::new(RowId::new("adapter:scan:on"), "Start Scan"),
        Target::new(RowId::new("adapter:scan:off"), "Stop Scan"),
    ];

    let disc_val = if a.discoverable { "on" } else { "off" };
    let mut disc_row = Row::with_meta(
        RowId::new(format!(
            "adapter:discoverable:{}",
            if a.discoverable { "off" } else { "on" }
        )),
        format!("Discoverable: {disc_val}"),
        format!("bluetoothctl discoverable {disc_val}"),
    );
    disc_row.targets = vec![
        Target::new(RowId::new("adapter:discoverable:on"), "Enable Discoverable"),
        Target::new(
            RowId::new("adapter:discoverable:off"),
            "Disable Discoverable",
        ),
    ];

    let pair_val = if a.pairable { "on" } else { "off" };
    let mut pair_row = Row::with_meta(
        RowId::new(format!(
            "adapter:pairable:{}",
            if a.pairable { "off" } else { "on" }
        )),
        format!("Pairable: {pair_val}"),
        format!("bluetoothctl pairable {pair_val}"),
    );
    pair_row.targets = vec![
        Target::new(RowId::new("adapter:pairable:on"), "Enable Pairable"),
        Target::new(RowId::new("adapter:pairable:off"), "Disable Pairable"),
    ];

    let scan_val = if a.discovering { "active" } else { "idle" };
    let mut scan_row = Row::with_meta(
        RowId::new(format!(
            "adapter:scan:{}",
            if a.discovering { "off" } else { "on" }
        )),
        format!("Scan / Discovery: {scan_val}"),
        format!(
            "bluetoothctl scan {}",
            if a.discovering { "off" } else { "on" }
        ),
    );
    scan_row.targets = vec![
        Target::new(RowId::new("adapter:scan:on"), "Start Scanning"),
        Target::new(RowId::new("adapter:scan:off"), "Stop Scanning"),
    ];

    let rows = vec![controller_row, disc_row, pair_row, scan_row];
    let mut tab = Tab::with_rows(TAB_ADAPTERS, rows);
    tab.bare_rows = false;
    tab.filterable = false;
    tab.deletable = false;
    tab
}

/// Helper to read `bluetoothctl info {mac}` snapshot or live.
fn bt_info_snapshot(mac: &str) -> Option<String> {
    if let Ok(dir) = std::env::var(BT_INFO_DIR_ENV) {
        if dir.is_empty() {
            return None;
        }
        return std::fs::read_to_string(Path::new(&dir).join(mac)).ok();
    }
    super::snapshot(BT_INFO_DIR_ENV, "bluetoothctl", &["info", mac])
}

/// Helper to read default audio sink status snapshot or live from wpctl/pactl.
fn bt_default_sink_snapshot() -> Option<String> {
    if let Ok(file) = std::env::var(BT_DEFAULT_SINK_FILE_ENV) {
        if file.is_empty() {
            return None;
        }
        return std::fs::read_to_string(file).ok();
    }
    // Live probe: try wpctl status first, then pactl info
    if let Some(out) = super::snapshot("", "wpctl", &["status"]) {
        return Some(out);
    }
    super::snapshot("", "pactl", &["info"])
}

/// Build the `Devices` tab live.
#[must_use]
pub fn devices_tab() -> Tab {
    let show_out = super::snapshot(BT_SHOW_FILE_ENV, "bluetoothctl", &["show"]);
    let adapter = show_out.as_deref().and_then(parse_controller);
    let devices_out = super::snapshot(BT_DEVICES_FILE_ENV, "bluetoothctl", &["devices"]);
    let sink_status = bt_default_sink_snapshot();

    devices_tab_from(
        adapter.as_ref(),
        devices_out.as_deref(),
        &bt_info_snapshot,
        sink_status.as_deref(),
    )
}

/// Build the `Adapters` tab live.
#[must_use]
pub fn adapters_tab() -> Tab {
    let show_out = super::snapshot(BT_SHOW_FILE_ENV, "bluetoothctl", &["show"]);
    let adapter = show_out.as_deref().and_then(parse_controller);
    adapters_tab_from(adapter.as_ref())
}

/// Build the full interactive Bluetooth menu.
#[must_use]
pub fn bt_menu() -> Menu {
    let mut tabs = vec![devices_tab(), adapters_tab()];
    // Device rows are scan snapshots (MACs come and go): searchable, but
    // nothing stable worth learning.
    for tab in &mut tabs {
        tab.learnable = false;
    }
    providers::menu(PROVIDER, tabs)
}

/// Per-tick refresh for `flex-bt`: re-probes controller and device states.
///
/// OPT-6: skips unless a `bt` tab is active (throttled to 5 s in
/// [`providers::tick_hook`], so idle ticks cost no `bluetoothctl`/`wpctl`
/// spawns). OPT-9: reuses row allocations in place with a single
/// focus-restore pass.
pub fn refresh(menu: &mut Menu) {
    let active = menu
        .app
        .active_tab()
        .is_some_and(|tab| tab.name == TAB_DEVICES || tab.name == TAB_ADAPTERS);
    if !active {
        return;
    }
    let previous = menu.app.focused_row().map(|row| row.id.clone());

    let show_out = super::snapshot(BT_SHOW_FILE_ENV, "bluetoothctl", &["show"]);
    let adapter = show_out.as_deref().and_then(parse_controller);
    let devices_out = super::snapshot(BT_DEVICES_FILE_ENV, "bluetoothctl", &["devices"]);
    let sink_status = bt_default_sink_snapshot();

    if let Some(tab) = menu.app.tabs.iter_mut().find(|tab| tab.name == TAB_DEVICES) {
        let fresh = devices_tab_from(
            adapter.as_ref(),
            devices_out.as_deref(),
            &bt_info_snapshot,
            sink_status.as_deref(),
        );
        super::sync_rows_in_place(&mut tab.rows, fresh.rows);
    }

    if let Some(tab) = menu
        .app
        .tabs
        .iter_mut()
        .find(|tab| tab.name == TAB_ADAPTERS)
    {
        let fresh = adapters_tab_from(adapter.as_ref());
        super::sync_rows_in_place(&mut tab.rows, fresh.rows);
    }

    super::restore_focus(menu, previous);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW_POWERED: &str = r"Controller 00:1A:7D:DA:71:13 (public)
	Name: fedora-host
	Alias: fedora-host
	Class: 0x006c010c
	Powered: yes
	Powering: no
	Discoverable: no
	DiscoverableTimeout: 0x000000b4
	Pairable: yes
	PairableTimeout: 0x00000000
	Discovering: no
";

    const SHOW_UNPOWERED: &str = r"Controller 00:1A:7D:DA:71:13 (public)
	Name: fedora-host
	Powered: no
	Discoverable: no
	Pairable: no
	Discovering: no
";

    const DEVICES_TXT: &str = r"Device F4:4E:FC:21:40:48 WH-1000XM4
Device 00:1B:66:81:28:B4 MX Master 3S
";

    const INFO_HEADPHONES: &str = r"Device F4:4E:FC:21:40:48 (public)
	Name: WH-1000XM4
	Alias: WH-1000XM4
	Class: 0x00240404
	Icon: audio-headset
	Paired: yes
	Trusted: yes
	Connected: yes
	UUID: Audio Sink                (0000110b-0000-1000-8000-00805f9b34fb)
	Battery Percentage: 0x55 (85)
";

    const INFO_MOUSE: &str = r"Device 00:1B:66:81:28:B4 (public)
	Name: MX Master 3S
	Alias: MX Master 3S
	Class: 0x00002580
	Icon: input-mouse
	Paired: no
	Connected: no
	RSSI: -64 dBm
";

    const WPCTL_STATUS: &str = r"Audio
 ├─ Devices:
 │      40. Built-in Audio                      [alsa]
 │      45. WH-1000XM4                          [bluez5]
 │
 ├─ Sinks:
 │      41. Built-in Audio Analog Stereo        [vol: 0.74]
 │  *   46. WH-1000XM4                          [vol: 0.85]
";

    #[test]
    fn parse_controller_extracts_all_fields() {
        let info = parse_controller(SHOW_POWERED).expect("adapter parsed");
        assert_eq!(info.mac, "00:1A:7D:DA:71:13");
        assert_eq!(info.name, "fedora-host");
        assert!(info.powered);
        assert!(!info.discoverable);
        assert!(info.pairable);
        assert!(!info.discovering);
    }

    #[test]
    fn parse_devices_extracts_mac_and_name() {
        let devs = parse_devices(DEVICES_TXT);
        assert_eq!(
            devs,
            vec![
                ("F4:4E:FC:21:40:48".to_string(), "WH-1000XM4".to_string()),
                ("00:1B:66:81:28:B4".to_string(), "MX Master 3S".to_string()),
            ]
        );
    }

    #[test]
    fn battery_and_rssi_parsing() {
        assert_eq!(parse_battery(INFO_HEADPHONES), Some(85));
        assert_eq!(parse_rssi(INFO_MOUSE), Some(-64));
    }

    #[test]
    fn audio_sink_detection_and_default_marking() {
        assert!(is_audio_device(INFO_HEADPHONES));
        assert!(!is_audio_device(INFO_MOUSE));
        assert!(is_default_audio_sink(
            "F4:4E:FC:21:40:48",
            "WH-1000XM4",
            Some(WPCTL_STATUS)
        ));
    }

    #[test]
    fn devices_tab_builds_expected_wiremix_rows() {
        let adapter = parse_controller(SHOW_POWERED);
        let info_map = |mac: &str| match mac {
            "F4:4E:FC:21:40:48" => Some(INFO_HEADPHONES.to_string()),
            "00:1B:66:81:28:B4" => Some(INFO_MOUSE.to_string()),
            _ => None,
        };

        let tab = devices_tab_from(
            adapter.as_ref(),
            Some(DEVICES_TXT),
            &info_map,
            Some(WPCTL_STATUS),
        );

        assert_eq!(tab.name, TAB_DEVICES);
        assert_eq!(tab.rows.len(), 3);

        assert_eq!(tab.rows[0].id.as_str(), POWER_OFF_ID);
        assert_eq!(tab.rows[0].label, "Turn Bluetooth Off");

        assert_eq!(tab.rows[1].label, "WH-1000XM4 F4:4E:FC:21:40:48");
        assert_eq!(
            tab.rows[1].meta.as_deref(),
            Some(" 85% [███████░] Connected · Audio")
        );
        assert!(tab.rows[1].is_default);
        assert_eq!(
            tab.rows[1].volume, None,
            "battery is a text gauge, row stays compact"
        );
        for row in &tab.rows {
            assert_eq!(row.volume, None, "no volume bars: {}", row.label);
        }
        assert_eq!(tab.rows[1].targets.len(), 6);
        assert_eq!(tab.rows[1].targets[0].title, "Connect");
        assert_eq!(tab.rows[1].targets[1].title, "Disconnect");
        assert_eq!(tab.rows[1].targets[2].title, "Set as Default Audio Sink");
        assert!(tab.rows[1].targets[2].is_default);

        assert_eq!(tab.rows[2].label, "MX Master 3S 00:1B:66:81:28:B4");
        assert_eq!(tab.rows[2].meta.as_deref(), Some("Available (-64 dBm)"));
        assert!(!tab.rows[2].is_default);
        assert_eq!(tab.rows[2].volume, None);
    }

    #[test]
    fn unpowered_adapter_renders_turn_on_row() {
        let adapter = parse_controller(SHOW_UNPOWERED);
        let tab = devices_tab_from(adapter.as_ref(), Some(DEVICES_TXT), &|_| None, None);
        assert_eq!(tab.rows.len(), 1);
        assert_eq!(tab.rows[0].id.as_str(), POWER_ON_ID);
        assert_eq!(tab.rows[0].label, "Turn Bluetooth On");
    }

    #[test]
    fn adapters_tab_builds_controller_and_controls() {
        let adapter = parse_controller(SHOW_POWERED);
        let tab = adapters_tab_from(adapter.as_ref());
        assert_eq!(tab.name, TAB_ADAPTERS);
        assert_eq!(tab.rows.len(), 4);
        assert_eq!(
            tab.rows[0].label,
            "Controller fedora-host (00:1A:7D:DA:71:13)"
        );
        assert_eq!(tab.rows[1].label, "Discoverable: off");
        assert_eq!(tab.rows[2].label, "Pairable: on");
        assert_eq!(tab.rows[3].label, "Scan / Discovery: idle");
    }
}
