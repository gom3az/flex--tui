//! `flex-bt` comprehensive unit and golden tests.
//!
//! Asserts Wiremix design compliance, provider row structures, dropdown targets,
//! audio sink & profile handling, adapter controls, tick refresh, and executor dispatch.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use flex_core::{render, Menu};
use flex_rice::exec::bt as exec_bt;
use flex_rice::providers::bt;

const SHOW_OUTPUT_ON: &str = r"Controller 00:1A:7D:DA:71:13 (public)
	Name: fedora-desktop
	Alias: fedora-desktop
	Class: 0x006c010c
	Powered: yes
	Powering: no
	Discoverable: no
	DiscoverableTimeout: 0x000000b4
	Pairable: yes
	PairableTimeout: 0x00000000
	Discovering: no
	Modalias: usb:v1D6Bp0246d0542
";

const SHOW_OUTPUT_OFF: &str = r"Controller 00:1A:7D:DA:71:13 (public)
	Name: fedora-desktop
	Powered: no
	Discoverable: no
	Pairable: no
	Discovering: no
";

const DEVICES_OUTPUT: &str = r"Device F4:4E:FC:21:40:48 WH-1000XM4
Device 00:1B:66:81:28:B4 MX Master 3S
Device 14:3F:A6:49:12:DF Keychron K2
";

const INFO_WH1000XM4: &str = r"Device F4:4E:FC:21:40:48 (public)
	Name: WH-1000XM4
	Alias: WH-1000XM4
	Class: 0x00240404
	Icon: audio-headset
	Paired: yes
	Bonded: yes
	Trusted: yes
	Blocked: no
	Connected: yes
	LegacyPairing: no
	UUID: Audio Sink                (0000110b-0000-1000-8000-00805f9b34fb)
	UUID: A/V Remote Control Target (0000110c-0000-1000-8000-00805f9b34fb)
	UUID: Advanced Audio Distribu.. (0000110d-0000-1000-8000-00805f9b34fb)
	UUID: Handsfree                 (0000111e-0000-1000-8000-00805f9b34fb)
	Battery Percentage: 0x55 (85)
	RSSI: -45 dBm
";

const INFO_MX_MASTER: &str = r"Device 00:1B:66:81:28:B4 (public)
	Name: MX Master 3S
	Alias: MX Master 3S
	Class: 0x00002580
	Icon: input-mouse
	Paired: no
	Connected: no
	RSSI: -64 dBm
";

const INFO_KEYCHRON: &str = r"Device 14:3F:A6:49:12:DF (public)
	Name: Keychron K2
	Alias: Keychron K2
	Class: 0x00002540
	Icon: input-keyboard
	Paired: yes
	Connected: no
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

fn mock_info(mac: &str) -> Option<String> {
    match mac {
        "F4:4E:FC:21:40:48" => Some(INFO_WH1000XM4.to_string()),
        "00:1B:66:81:28:B4" => Some(INFO_MX_MASTER.to_string()),
        "14:3F:A6:49:12:DF" => Some(INFO_KEYCHRON.to_string()),
        _ => None,
    }
}

fn draw(menu: &mut Menu, w: u16, h: u16) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("terminal created");
    terminal
        .draw(|frame| render::render(frame, menu))
        .expect("frame rendered");
    terminal.backend().buffer().clone()
}

fn buffer_line(buf: &Buffer, y: u16, w: u16) -> String {
    let mut out = String::new();
    for x in 0..w {
        let cell = buf.cell((x, y)).expect("cell exists");
        if !cell.skip {
            out.push_str(cell.symbol());
        }
    }
    out
}

#[test]
fn parse_controller_status() {
    let on = bt::parse_controller(SHOW_OUTPUT_ON).expect("on adapter");
    assert_eq!(on.mac, "00:1A:7D:DA:71:13");
    assert_eq!(on.name, "fedora-desktop");
    assert!(on.powered);
    assert!(!on.discoverable);
    assert!(on.pairable);
    assert!(!on.discovering);

    let off = bt::parse_controller(SHOW_OUTPUT_OFF).expect("off adapter");
    assert!(!off.powered);
}

#[test]
fn parse_devices_and_properties() {
    let devs = bt::parse_devices(DEVICES_OUTPUT);
    assert_eq!(devs.len(), 3);
    assert_eq!(devs[0].0, "F4:4E:FC:21:40:48");
    assert_eq!(devs[0].1, "WH-1000XM4");

    assert_eq!(bt::parse_battery(INFO_WH1000XM4), Some(85));
    assert_eq!(bt::parse_rssi(INFO_MX_MASTER), Some(-64));
    assert!(bt::is_audio_device(INFO_WH1000XM4));
    assert!(!bt::is_audio_device(INFO_MX_MASTER));
    assert!(bt::is_default_audio_sink(
        "F4:4E:FC:21:40:48",
        "WH-1000XM4",
        Some(WPCTL_STATUS)
    ));
}

#[test]
fn devices_tab_exact_wiremix_contract() {
    let adapter = bt::parse_controller(SHOW_OUTPUT_ON);
    let tab = bt::devices_tab_from(
        adapter.as_ref(),
        Some(DEVICES_OUTPUT),
        &mock_info,
        Some(WPCTL_STATUS),
    );

    assert_eq!(tab.name, bt::TAB_DEVICES);
    assert!(!tab.bare_rows);
    assert!(!tab.filterable);
    assert_eq!(tab.rows.len(), 4);

    assert_eq!(tab.rows[0].id.as_str(), bt::POWER_OFF_ID);
    assert_eq!(tab.rows[0].label, "Turn Bluetooth Off");
    assert_eq!(tab.rows[0].meta.as_deref(), Some("bluetoothctl power off"));

    let r1 = &tab.rows[1];
    assert_eq!(r1.label, "WH-1000XM4 F4:4E:FC:21:40:48");
    assert_eq!(
        r1.meta.as_deref(),
        Some(" 85% [███████░] Connected · Audio")
    );
    assert!(r1.is_default);
    assert_eq!(
        r1.volume, None,
        "battery is a text gauge, row stays compact"
    );
    assert_eq!(r1.targets.len(), 6);
    assert_eq!(r1.targets[0].title, "Connect");
    assert_eq!(r1.targets[1].title, "Disconnect");
    assert_eq!(r1.targets[2].title, "Set as Default Audio Sink");
    assert!(r1.targets[2].is_default);
    assert_eq!(r1.targets[3].title, "Switch to High Quality (A2DP)");
    assert_eq!(r1.targets[4].title, "Switch to Headset Mode (HFP)");
    assert_eq!(r1.targets[5].title, "Unpair (Forget Device)");

    let r2 = &tab.rows[2];
    assert_eq!(r2.label, "MX Master 3S 00:1B:66:81:28:B4");
    assert_eq!(r2.meta.as_deref(), Some("Available (-64 dBm)"));
    assert!(!r2.is_default);
    assert_eq!(r2.volume, None);

    let r3 = &tab.rows[3];
    assert_eq!(r3.label, "Keychron K2 14:3F:A6:49:12:DF");
    assert_eq!(r3.meta.as_deref(), Some("Paired"));
    assert!(!r3.is_default);
    assert_eq!(r3.volume, None);
}

#[test]
fn adapters_tab_contract() {
    let adapter = bt::parse_controller(SHOW_OUTPUT_ON);
    let tab = bt::adapters_tab_from(adapter.as_ref());

    assert_eq!(tab.name, bt::TAB_ADAPTERS);
    assert_eq!(tab.rows.len(), 4);
    assert_eq!(
        tab.rows[0].label,
        "Controller fedora-desktop (00:1A:7D:DA:71:13)"
    );
    assert_eq!(
        tab.rows[0].meta.as_deref(),
        Some("Powered: yes · Discovering: no")
    );
    assert_eq!(tab.rows[1].label, "Discoverable: off");
    assert_eq!(tab.rows[2].label, "Pairable: on");
    assert_eq!(tab.rows[3].label, "Scan / Discovery: idle");
}

#[test]
fn golden_80x24_rendering_wiremix_detail() {
    let adapter = bt::parse_controller(SHOW_OUTPUT_ON);
    let devices_tab = bt::devices_tab_from(
        adapter.as_ref(),
        Some(DEVICES_OUTPUT),
        &mock_info,
        Some(WPCTL_STATUS),
    );
    let adapters_tab = bt::adapters_tab_from(adapter.as_ref());

    let mut menu = flex_rice::menu(bt::PROVIDER, vec![devices_tab, adapters_tab]);

    let buf = draw(&mut menu, 80, 24);

    // Frame assertions:
    // Tab bar is on the bottom line (y = 23)
    let tab_bar = buffer_line(&buf, 23, 80);
    assert!(tab_bar.contains("[Devices]"));
    assert!(tab_bar.contains("Adapters"));

    // No filter line (filterable = false): entry 0 header is at y = 1
    let e0_header = buffer_line(&buf, 1, 80);
    assert!(e0_header.contains("░"));
    assert!(e0_header.contains("Turn Bluetooth Off"));

    // Compact pitch (1-line rows + 1 spacing): entry 1 header is at y = 3.
    // `hide_target_in_header` shows the battery/status meta (not the
    // Connect/Disconnect target, which lives in the dropdown).
    let e1_header = buffer_line(&buf, 3, 80);
    assert!(e1_header.contains("◇"));
    assert!(e1_header.contains("WH-1000XM4"));

    // Compact battery gauge lives in the header meta, not a detail line.
    assert!(e1_header.contains("85%"));
    assert!(e1_header.contains("█"));
    let mut all = String::new();
    for y in 0..24 {
        all.push_str(&buffer_line(&buf, y, 80));
    }
    assert!(
        !all.contains("━"),
        "no volume-bar detail lines in compact mode"
    );

    menu.app.switch_tab(1);
    let buf_adapters = draw(&mut menu, 80, 24);
    let tab_bar_2 = buffer_line(&buf_adapters, 23, 80);
    assert!(tab_bar_2.contains("Devices"));
    assert!(tab_bar_2.contains("[Adapters]"));
}

#[test]
fn tick_refresh_preserves_focus_and_updates_rows() {
    let adapter = bt::parse_controller(SHOW_OUTPUT_ON);
    let devices_tab = bt::devices_tab_from(
        adapter.as_ref(),
        Some(DEVICES_OUTPUT),
        &mock_info,
        Some(WPCTL_STATUS),
    );
    let adapters_tab = bt::adapters_tab_from(adapter.as_ref());

    let mut menu = flex_rice::menu(bt::PROVIDER, vec![devices_tab, adapters_tab]);

    menu.app.active_tab_mut().unwrap().state.focus = 1;
    assert_eq!(
        menu.app.focused_row().map(|r| r.label.as_str()),
        Some("WH-1000XM4 F4:4E:FC:21:40:48")
    );

    bt::refresh(&mut menu);

    let active_tab = menu.app.active_tab().unwrap();
    assert!(active_tab.state.focus < active_tab.rows.len());
}

#[test]
fn executor_dispatch_matrix() {
    assert_eq!(
        exec_bt::execute("noop", "", Some("")).unwrap(),
        exec_bt::ExecuteReport {
            action_id: "noop".to_string(),
            detail: None,
        }
    );

    assert_eq!(
        exec_bt::execute("power:on", "", Some("")).unwrap(),
        exec_bt::ExecuteReport {
            action_id: "power:on".to_string(),
            detail: Some("power:on".to_string()),
        }
    );

    assert_eq!(
        exec_bt::execute("connect:F4:4E:FC:21:40:48", "", Some("")).unwrap(),
        exec_bt::ExecuteReport {
            action_id: "connect:F4:4E:FC:21:40:48".to_string(),
            detail: Some("F4:4E:FC:21:40:48".to_string()),
        }
    );

    assert_eq!(
        exec_bt::execute("sink:F4:4E:FC:21:40:48", "", Some("")).unwrap(),
        exec_bt::ExecuteReport {
            action_id: "sink:F4:4E:FC:21:40:48".to_string(),
            detail: Some("F4:4E:FC:21:40:48".to_string()),
        }
    );

    assert_eq!(
        exec_bt::execute("profile:a2dp:F4:4E:FC:21:40:48", "", Some("")).unwrap(),
        exec_bt::ExecuteReport {
            action_id: "profile:a2dp:F4:4E:FC:21:40:48".to_string(),
            detail: Some("F4:4E:FC:21:40:48".to_string()),
        }
    );
}
