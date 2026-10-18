//! `bt` executor: native Bluetooth command manager and `PipeWire` audio router.
//!
//! Manages `bluetoothctl`, `wpctl`, and `pactl` subprocess calls with transient-`ETXTBSY`
//! retry ([`spawn::RetryExec`]).
//!
//! Dispatches actions for:
//! - Power On / Off (`bluetoothctl power on|off`)
//! - Scan / Discovery On / Off (`bluetoothctl scan on|off`)
//! - Discoverable / Pairable (`bluetoothctl discoverable on|off`, `bluetoothctl pairable on|off`)
//! - Connect / Disconnect (`bluetoothctl connect <mac>`, `bluetoothctl disconnect <mac>`)
//! - Pair / Trust / Remove (`bluetoothctl pair <mac>`, `bluetoothctl trust <mac>`, `bluetoothctl remove <mac>`)
//! - Default Audio Sink selection via `wpctl set-default` / `pactl set-default-sink`
//! - Audio Profile switching (A2DP / HFP) via `pactl set-card-profile` / `wpctl set-profile`

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::exec::NotifyWhen;
use crate::providers::bt as provider;
use crate::spawn::RetryExec as _;
use crate::tools;

/// What [`execute`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteReport {
    /// The action ID that ran.
    pub action_id: String,
    /// Resolved detail (e.g. MAC address or target name).
    pub detail: Option<String>,
}

/// One executable Bluetooth step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `bluetoothctl power on`
    BtPowerOn { bt: String },
    /// `bluetoothctl power off`
    BtPowerOff { bt: String },
    /// `bluetoothctl scan on`
    BtScanOn { bt: String },
    /// `bluetoothctl scan off`
    BtScanOff { bt: String },
    /// `bluetoothctl discoverable on|off`
    BtDiscoverable { bt: String, on: bool },
    /// `bluetoothctl pairable on|off`
    BtPairable { bt: String, on: bool },
    /// `bluetoothctl connect <mac>`
    BtConnect { bt: String, mac: String },
    /// `bluetoothctl disconnect <mac>`
    BtDisconnect { bt: String, mac: String },
    /// `bluetoothctl pair <mac>`
    BtPair { bt: String, mac: String },
    /// `bluetoothctl trust <mac>`
    BtTrust { bt: String, mac: String },
    /// `bluetoothctl remove <mac>`
    BtRemove { bt: String, mac: String },
    /// `wpctl set-default <sink_id>`
    WpctlSetDefaultSink { wpctl: String, sink_id: String },
    /// `pactl set-default-sink <sink_name>`
    PactlSetDefaultSink { pactl: String, sink_name: String },
    /// `pactl set-card-profile <card> <profile>`
    PactlSetProfile {
        pactl: String,
        card: String,
        profile: String,
    },
    /// Desktop notification
    Notify {
        notify: String,
        summary: String,
        body: String,
        when: NotifyWhen,
    },
}

/// Render a single step into an argv string representation.
#[must_use]
pub fn describe(step: &Step) -> String {
    match step {
        Step::BtPowerOn { bt } => format!("{bt} power on"),
        Step::BtPowerOff { bt } => format!("{bt} power off"),
        Step::BtScanOn { bt } => format!("{bt} scan on"),
        Step::BtScanOff { bt } => format!("{bt} scan off"),
        Step::BtDiscoverable { bt, on } => {
            format!("{bt} discoverable {}", if *on { "on" } else { "off" })
        }
        Step::BtPairable { bt, on } => {
            format!("{bt} pairable {}", if *on { "on" } else { "off" })
        }
        Step::BtConnect { bt, mac } => format!("{bt} connect {mac}"),
        Step::BtDisconnect { bt, mac } => format!("{bt} disconnect {mac}"),
        Step::BtPair { bt, mac } => format!("{bt} pair {mac}"),
        Step::BtTrust { bt, mac } => format!("{bt} trust {mac}"),
        Step::BtRemove { bt, mac } => format!("{bt} remove {mac}"),
        Step::WpctlSetDefaultSink { wpctl, sink_id } => format!("{wpctl} set-default {sink_id}"),
        Step::PactlSetDefaultSink { pactl, sink_name } => {
            format!("{pactl} set-default-sink {sink_name}")
        }
        Step::PactlSetProfile {
            pactl,
            card,
            profile,
        } => format!("{pactl} set-card-profile {card} {profile}"),
        Step::Notify {
            notify,
            summary,
            body,
            ..
        } => format!("{notify} -a Bluetooth {summary} {body}"),
    }
}

/// Render an entire plan as snapshot lines.
#[must_use]
pub fn describe_plan(steps: &[Step]) -> Vec<String> {
    steps.iter().map(describe).collect()
}

/// Lookup `BLUETOOTHCTL` program override, else `bluetoothctl`.
#[must_use]
pub fn bluetoothctl_cmd() -> String {
    std::env::var("BLUETOOTHCTL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("bluetoothctl"))
}

/// Lookup `WPCTL` program override, else `wpctl`.
#[must_use]
pub fn wpctl_cmd() -> String {
    std::env::var("WPCTL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("wpctl"))
}

/// Lookup `PACTL` program override, else `pactl`.
#[must_use]
pub fn pactl_cmd() -> String {
    std::env::var("PACTL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("pactl"))
}

/// Lookup `NOTIFY_SEND` program override, else `notify-send`.
#[must_use]
pub fn notify_cmd() -> String {
    std::env::var("NOTIFY_SEND")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from("notify-send"))
}

fn ambient_path() -> String {
    tools::ambient_path()
}

fn resolve_tool(name: &str, path_env: &str) -> Option<PathBuf> {
    tools::resolve_tool(name, path_env)
}

fn tool_quiet(path_env: &str, name: &str, args: &[&str]) -> bool {
    let Some(bin) = resolve_tool(name, path_env) else {
        return false;
    };
    Command::new(&bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status_retrying()
        .is_ok_and(|status| status.success())
}

fn tool_captured(path_env: &str, name: &str, args: &[&str]) -> Option<String> {
    let bin = resolve_tool(name, path_env)?;
    let output = Command::new(&bin)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output_retrying()
        .ok()?;
    Some(tools::decode_stdout(output.stdout))
}

#[allow(dead_code)]
#[must_use]
pub fn execute_step(step: &Step, path_env: &str) -> bool {
    match step {
        Step::BtPowerOn { bt } => tool_quiet(path_env, bt, &["power", "on"]),
        Step::BtPowerOff { bt } => tool_quiet(path_env, bt, &["power", "off"]),
        Step::BtScanOn { bt } => tool_quiet(path_env, bt, &["scan", "on"]),
        Step::BtScanOff { bt } => tool_quiet(path_env, bt, &["scan", "off"]),
        Step::BtDiscoverable { bt, on } => tool_quiet(
            path_env,
            bt,
            &["discoverable", if *on { "on" } else { "off" }],
        ),
        Step::BtPairable { bt, on } => {
            tool_quiet(path_env, bt, &["pairable", if *on { "on" } else { "off" }])
        }
        Step::BtConnect { bt, mac } => tool_quiet(path_env, bt, &["connect", mac]),
        Step::BtDisconnect { bt, mac } => tool_quiet(path_env, bt, &["disconnect", mac]),
        Step::BtPair { bt, mac } => tool_quiet(path_env, bt, &["pair", mac]),
        Step::BtTrust { bt, mac } => tool_quiet(path_env, bt, &["trust", mac]),
        Step::BtRemove { bt, mac } => tool_quiet(path_env, bt, &["remove", mac]),
        Step::WpctlSetDefaultSink { wpctl, sink_id } => {
            tool_quiet(path_env, wpctl, &["set-default", sink_id])
        }
        Step::PactlSetDefaultSink { pactl, sink_name } => {
            tool_quiet(path_env, pactl, &["set-default-sink", sink_name])
        }
        Step::PactlSetProfile {
            pactl,
            card,
            profile,
        } => tool_quiet(path_env, pactl, &["set-card-profile", card, profile]),
        Step::Notify {
            notify,
            summary,
            body,
            ..
        } => tool_quiet(path_env, notify, &["-a", "Bluetooth", summary, body]),
    }
}

/// Turn Bluetooth controller power on.
///
/// # Errors
///
/// Returns an error if the power on action fails.
pub fn power_on(path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["power", "on"]);
    Ok(())
}

/// Turn Bluetooth controller power off.
///
/// # Errors
///
/// Returns an error if the power off action fails.
pub fn power_off(path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["power", "off"]);
    Ok(())
}

/// Start Bluetooth scan.
///
/// # Errors
///
/// Returns an error if initiating scan fails.
pub fn scan_on(path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["scan", "on"]);
    Ok(())
}

/// Stop Bluetooth scan.
///
/// # Errors
///
/// Returns an error if stopping scan fails.
pub fn scan_off(path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["scan", "off"]);
    Ok(())
}

/// Set Bluetooth controller discoverable status.
///
/// # Errors
///
/// Returns an error if updating discoverable status fails.
pub fn set_discoverable(on: bool, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["discoverable", if on { "on" } else { "off" }]);
    Ok(())
}

/// Set Bluetooth controller pairable status.
///
/// # Errors
///
/// Returns an error if updating pairable status fails.
pub fn set_pairable(on: bool, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["pairable", if on { "on" } else { "off" }]);
    Ok(())
}

/// Connect to a Bluetooth device.
///
/// # Errors
///
/// Returns an error if connecting fails.
pub fn connect(mac: &str, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let notify = notify_cmd();
    let success = tool_quiet(path, &bt, &["connect", mac]);
    if success {
        let _ = tool_quiet(path, &notify, &["-a", "Bluetooth", "Connected", mac]);
    } else {
        let _ = tool_quiet(
            path,
            &notify,
            &["-a", "Bluetooth", "Connection Failed", mac],
        );
    }
    Ok(())
}

/// Disconnect from a Bluetooth device.
///
/// # Errors
///
/// Returns an error if disconnecting fails.
pub fn disconnect(mac: &str, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let notify = notify_cmd();
    let _ = tool_quiet(path, &bt, &["disconnect", mac]);
    let _ = tool_quiet(path, &notify, &["-a", "Bluetooth", "Disconnected", mac]);
    Ok(())
}

/// Pair and trust a Bluetooth device.
///
/// # Errors
///
/// Returns an error if pairing fails.
pub fn pair(mac: &str, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["pair", mac]);
    let _ = tool_quiet(path, &bt, &["trust", mac]);
    Ok(())
}

/// Remove / unpair a Bluetooth device.
///
/// # Errors
///
/// Returns an error if removing fails.
pub fn remove(mac: &str, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let notify = notify_cmd();
    let _ = tool_quiet(path, &bt, &["remove", mac]);
    let _ = tool_quiet(path, &notify, &["-a", "Bluetooth", "Removed Device", mac]);
    Ok(())
}

/// Trust a Bluetooth device.
///
/// # Errors
///
/// Returns an error if trusting fails.
pub fn trust(mac: &str, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let bt = bluetoothctl_cmd();
    let _ = tool_quiet(path, &bt, &["trust", mac]);
    Ok(())
}

/// Set default audio sink for a Bluetooth device via `wpctl` or `pactl`.
///
/// # Errors
///
/// Returns an error if setting default audio sink fails.
pub fn set_default_audio_sink(mac: &str, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let wpctl = wpctl_cmd();
    let pactl = pactl_cmd();
    let notify = notify_cmd();

    let mac_underscore = mac.replace(':', "_");
    let mac_colon = mac.to_lowercase();
    let mac_underscore_lower = mac_underscore.to_lowercase();

    // 1. Try finding sink ID via wpctl status
    if let Some(status) = tool_captured(path, &wpctl, &["status"]) {
        let mut in_sinks = false;
        for line in status.lines() {
            let trimmed = line.trim();
            if trimmed.contains("Sinks:") {
                in_sinks = true;
                continue;
            }
            if in_sinks && (trimmed.contains("Sources:") || trimmed.contains("Filters:")) {
                break;
            }
            if in_sinks {
                let line_lower = line.to_lowercase();
                if line_lower.contains(&mac_colon) || line_lower.contains(&mac_underscore_lower) {
                    // Extract ID e.g. "46. WH-1000XM4" or "│   46. WH-1000XM4"
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    for part in parts {
                        let candidate = part.trim_end_matches('.');
                        if let Ok(id) = candidate.parse::<u32>() {
                            let _ = tool_quiet(path, &wpctl, &["set-default", &id.to_string()]);
                            let _ = tool_quiet(
                                path,
                                &notify,
                                &["-a", "Bluetooth", "Default Audio Sink Set", mac],
                            );
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    // 2. Fallback to pactl set-default-sink
    let sink_name = format!("bluez_output.{mac_underscore}.1");
    let _ = tool_quiet(path, &pactl, &["set-default-sink", &sink_name]);
    let _ = tool_quiet(
        path,
        &notify,
        &["-a", "Bluetooth", "Default Audio Sink Set", mac],
    );
    Ok(())
}

/// Set audio profile for a Bluetooth audio card (e.g. `a2dp-sink` or `headset-head-unit`).
///
/// # Errors
///
/// Returns an error if setting the audio card profile fails.
pub fn set_audio_profile(mac: &str, profile: &str, path_override: Option<&str>) -> Result<()> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);
    let pactl = pactl_cmd();
    let notify = notify_cmd();

    let mac_underscore = mac.replace(':', "_");
    let card_name = format!("bluez_card.{mac_underscore}");

    let profile_name = match profile {
        "a2dp" | "a2dp-sink" | "a2dp_sink" => "a2dp-sink",
        "hfp" | "headset-head-unit" | "headset_head_unit" => "headset-head-unit",
        other => other,
    };

    let _ = tool_quiet(
        path,
        &pactl,
        &["set-card-profile", &card_name, profile_name],
    );

    let summary = if profile_name == "a2dp-sink" {
        "Audio Profile: High Quality (A2DP)"
    } else {
        "Audio Profile: Headset Mode (HFP)"
    };
    let _ = tool_quiet(path, &notify, &["-a", "Bluetooth", summary, mac]);
    Ok(())
}

/// Execute a Bluetooth action by ID.
///
/// Supported action ID shapes:
/// - `power:on` / `on` / `adapter:power:on`
/// - `power:off` / `off` / `adapter:power:off`
/// - `scan:on` / `adapter:scan:on`
/// - `scan:off` / `adapter:scan:off`
/// - `adapter:scan:toggle`
/// - `discoverable:on` / `adapter:discoverable:on`
/// - `discoverable:off` / `adapter:discoverable:off`
/// - `pairable:on` / `adapter:pairable:on`
/// - `pairable:off` / `adapter:pairable:off`
/// - `connect:<mac>`
/// - `disconnect:<mac>`
/// - `pair:<mac>`
/// - `trust:<mac>`
/// - `unpair:<mac>` / `remove:<mac>`
/// - `sink:<mac>`
/// - `profile:a2dp:<mac>`
/// - `profile:hfp:<mac>`
/// - `device:<mac>` (connect/disconnect toggle)
/// - `noop`
///
/// # Errors
///
/// When an action ID is malformed or unhandled.
#[allow(clippy::too_many_lines)]
pub fn execute(action_id: &str, label: &str, path_override: Option<&str>) -> Result<ExecuteReport> {
    let ambient = ambient_path();
    let path = path_override.unwrap_or(&ambient);

    if action_id == "noop" {
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: None,
        });
    }

    if action_id == "power:on" || action_id == "on" || action_id == "adapter:power:on" {
        power_on(Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("power:on")),
        });
    }

    if action_id == "power:off" || action_id == "off" || action_id == "adapter:power:off" {
        power_off(Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("power:off")),
        });
    }

    if action_id == "scan:on" || action_id == "adapter:scan:on" {
        scan_on(Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("scan:on")),
        });
    }

    if action_id == "scan:off" || action_id == "adapter:scan:off" {
        scan_off(Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("scan:off")),
        });
    }

    if action_id == "discoverable:on" || action_id == "adapter:discoverable:on" {
        set_discoverable(true, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("discoverable:on")),
        });
    }

    if action_id == "discoverable:off" || action_id == "adapter:discoverable:off" {
        set_discoverable(false, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("discoverable:off")),
        });
    }

    if action_id == "pairable:on" || action_id == "adapter:pairable:on" {
        set_pairable(true, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("pairable:on")),
        });
    }

    if action_id == "pairable:off" || action_id == "adapter:pairable:off" {
        set_pairable(false, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(String::from("pairable:off")),
        });
    }

    if let Some(mac) = action_id.strip_prefix("connect:") {
        connect(mac, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id.strip_prefix("disconnect:") {
        disconnect(mac, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id.strip_prefix("pair:") {
        pair(mac, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id.strip_prefix("trust:") {
        trust(mac, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id
        .strip_prefix("unpair:")
        .or_else(|| action_id.strip_prefix("remove:"))
    {
        remove(mac, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id.strip_prefix("sink:") {
        set_default_audio_sink(mac, Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id.strip_prefix("profile:a2dp:") {
        set_audio_profile(mac, "a2dp", Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id.strip_prefix("profile:hfp:") {
        set_audio_profile(mac, "hfp", Some(path))?;
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    if let Some(mac) = action_id.strip_prefix("device:") {
        let bt = bluetoothctl_cmd();
        let is_connected = tool_captured(path, &bt, &["info", mac])
            .is_some_and(|info| info.contains("Connected: yes"));
        if is_connected {
            disconnect(mac, Some(path))?;
        } else {
            connect(mac, Some(path))?;
        }
        return Ok(ExecuteReport {
            action_id: action_id.to_string(),
            detail: Some(mac.to_string()),
        });
    }

    anyhow::bail!("bt: unknown action id '{action_id}' (label: '{label}')");
}

/// Sample Bluetooth state and format Waybar JSON (`text`/`tooltip`/`class`).
///
/// Headless poller for `custom/bt` (`flex-bt --status`, 5 s `exec`): reuses the
/// provider parsers and `$BT_*` snapshot seams, so fixture-driven tests pin the
/// exact payload Waybar renders. Degrades to `off` JSON when the controller is
/// missing or powered off — never an error, never non-JSON.
#[must_use]
pub fn status_json_from(
    show_out: Option<&str>,
    devices_out: Option<&str>,
    info_fn: &dyn Fn(&str) -> Option<String>,
) -> String {
    let adapter = show_out.and_then(provider::parse_controller);
    let Some(info) = adapter else {
        return serde_json::json!({
            "text": "󰂲 off",
            "tooltip": "No Bluetooth adapter\nClick to open manager",
            "class": "off",
        })
        .to_string();
    };
    if !info.powered {
        return serde_json::json!({
            "text": "󰂲 off",
            "tooltip": "Bluetooth off\nClick to open manager",
            "class": "off",
        })
        .to_string();
    }

    let controller_label = if info.name.is_empty() {
        info.mac.clone()
    } else {
        format!("{} ({})", info.name, info.mac)
    };
    let mut tooltip = format!("Controller {controller_label}");
    let mut connected: Vec<String> = Vec::new();

    if let Some(text) = devices_out {
        let devices = provider::parse_devices(text);
        if devices.is_empty() {
            tooltip.push_str("\n(No Bluetooth devices)");
        }
        for (mac, name) in &devices {
            let detail = info_fn(mac);
            let detail_str = detail.as_deref().unwrap_or_default();
            let is_connected = detail_str.contains("Connected: yes");
            let battery = provider::parse_battery(detail_str);
            let paired = detail_str.contains("Paired: yes");
            let rssi = provider::parse_rssi(detail_str);
            let is_audio = provider::is_audio_device(detail_str);
            let meta = provider::build_device_meta(is_connected, battery, is_audio, rssi, paired);
            let display = if name.is_empty() { mac } else { name };
            let _ = write!(tooltip, "\n• {display} ({mac}): {meta}");
            if is_connected {
                connected.push(display.clone());
            }
        }
    } else {
        tooltip.push_str("\n(No Bluetooth devices)");
    }
    tooltip.push_str("\nClick to open manager");

    // Text carries identity only (no battery %): the pill's pulseaudio half
    // already shows a percentage (volume), and the battery lives in the tooltip.
    let (text, class) = match connected.as_slice() {
        [] => (String::from("󰂯 on"), String::from("on")),
        [name] => (format!("󰂯 {name}"), String::from("connected")),
        many => (
            format!("󰂯 {} connected", many.len()),
            String::from("connected"),
        ),
    };

    serde_json::json!({
        "text": text,
        "tooltip": tooltip,
        "class": class,
    })
    .to_string()
}

/// Live Waybar poller: snapshot `show`/`devices`/`info` (fixture seams first,
/// live `bluetoothctl` otherwise) and render [`status_json_from`].
#[must_use]
pub fn status_json() -> String {
    let show_out =
        crate::providers::snapshot(provider::BT_SHOW_FILE_ENV, "bluetoothctl", &["show"]);
    let devices_out =
        crate::providers::snapshot(provider::BT_DEVICES_FILE_ENV, "bluetoothctl", &["devices"]);
    let ambient = ambient_path();
    status_json_from(show_out.as_deref(), devices_out.as_deref(), &|mac: &str| {
        if let Ok(dir) = std::env::var(provider::BT_INFO_DIR_ENV) {
            if dir.is_empty() {
                return None;
            }
            if let Ok(body) = std::fs::read_to_string(std::path::Path::new(&dir).join(mac)) {
                return Some(body);
            }
            return None;
        }
        let bt = bluetoothctl_cmd();
        tool_captured(&ambient, &bt, &["info", mac])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_descriptions() {
        assert_eq!(
            describe(&Step::BtPowerOn {
                bt: "bluetoothctl".to_string()
            }),
            "bluetoothctl power on"
        );
        assert_eq!(
            describe(&Step::BtPowerOff {
                bt: "bluetoothctl".to_string()
            }),
            "bluetoothctl power off"
        );
        assert_eq!(
            describe(&Step::BtConnect {
                bt: "bluetoothctl".to_string(),
                mac: "AA:BB:CC:DD:EE:FF".to_string()
            }),
            "bluetoothctl connect AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(
            describe(&Step::BtDisconnect {
                bt: "bluetoothctl".to_string(),
                mac: "AA:BB:CC:DD:EE:FF".to_string()
            }),
            "bluetoothctl disconnect AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(
            describe(&Step::WpctlSetDefaultSink {
                wpctl: "wpctl".to_string(),
                sink_id: "46".to_string()
            }),
            "wpctl set-default 46"
        );
        assert_eq!(
            describe(&Step::PactlSetProfile {
                pactl: "pactl".to_string(),
                card: "bluez_card.AA_BB_CC_DD_EE_FF".to_string(),
                profile: "a2dp-sink".to_string(),
            }),
            "pactl set-card-profile bluez_card.AA_BB_CC_DD_EE_FF a2dp-sink"
        );
    }
}
