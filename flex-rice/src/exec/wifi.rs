//! `wifi` executor: the Wi-Fi picker flow (radio on/off, disconnect, connect).
//!
//! Port of the retired `flex-wifi.sh` wrapper: after the menu selects a
//! row, the executor validates the id (non-empty, no `/`, no newline —
//! `flex-wifi.sh:37`) and dispatches (`flex-wifi.sh:174-185`):
//!
//! - `on` / `off` → `nmcli radio wifi on|off` (quiet, `|| true`);
//! - `disconnect` → fresh interface discovery, `nmcli device disconnect`,
//!   then always `notify-send -a Wi-Fi Disconnected {ssid}` (the SSID is
//!   stripped from the `Disconnect from {ssid}` label, `:181`);
//! - `wifi` → fresh-state connect/disconnect for the SSID in the label
//!   (`do_connect`, `:95-170`): vanished interface → nothing; connected →
//!   disconnect + notify; open or stale label → connect (+ `Connected` or
//!   `Failed` notify); secure → saved-profile attempt first (connect from
//!   the stored profile, `:140-145`), then the `/dev/tty` password prompt
//!   (`:148-156`), then connect with the password;
//! - `noop` → exit `0` (the empty-scan placeholder).
//!
//! Snapshot convention (the [`exec::shot`](super::shot) template): [`plan`] builds the [`Step`]s
//! from already-decided inputs, [`describe`] renders one step as a single
//! line — unit tests pin the lines, and integration tests diff the
//! stub-`PATH` call logs against the same shapes. Wifi-specific describe
//! lines:
//!
//! - `nmcli radio wifi on` / `nmcli radio wifi off`
//! - `nmcli device disconnect <iface>`
//! - `nmcli device wifi connect <ssid> ifname <iface>`
//! - `nmcli device wifi connect <ssid> password <password> ifname <iface>`
//!   (the password renders verbatim)
//! - `<notify> -a Wi-Fi <Connected|Disconnected|Failed> <ssid>`
//!
//! Planning reads (`nmcli` discovery/list/profiles) run inside [`execute`]
//! to decide the [`PlannedAction`] and are therefore not [`Step`]s. The
//! conditional notifies (the wrapper's
//! `if connect; then …; else …` verbatim) ride on [`Step::Notify`] +
//! [`NotifyWhen`]: the open and secure plans list both the `Connected`
//! (success) and the `Failed` (failure) notifies, exactly one of which
//! runs; the disconnect and saved-profile plans notify unconditionally.
//!
//! Shared parsing (called, not duplicated):
//!
//! - fresh-state parsing via [`parse_nmcli_devices`](crate::providers::wifi::parse_nmcli_devices)
//!   and [`parse_nmcli_wifi`](crate::providers::wifi::parse_nmcli_wifi) —
//!   the same functions the picker scan paths use;
//! - saved-profile parsing via [`parse_saved_profiles`](crate::providers::wifi::parse_saved_profiles)
//!   (the wrapper's `is_saved` + `nmcli_unescape` verbatim: last-colon
//!   split, `802-11-wireless` only, names unescaped before compare);
//! - the [`NotifyWhen`] gating enum.
//!
//! Deliberately mirrored from the retired center port (same structure,
//! wifi-local copies — the convention every `exec` module follows):
//!
//! - `nmcli_cmd`, `ambient_path`, `resolve_tool`, `tool_quiet`,
//!   `tool_captured`, `set_echo`, and the `execute`/`execute_with` split;
//! - the `WifiConnectSecure` step shape (identical argv order).
//!
//! Deliberate departures from the wrapper (all tested):
//!
//! - No `ACTION:` unescape step: the wrapper's `wifi_unescape` undoes the
//!   wire escaping of the `ACTION:` line, but the in-process label is
//!   already raw, so the SSID compares identically with no transform.
//! - The saved-profile attempt runs inside `decide`: the attempt is both
//!   the probe (did the stored key work?) and the effect (it connects),
//!   exactly like the wrapper's order (`is_saved` → connect →
//!   notify-or-prompt). `execute_with` never repeats it: the success plan
//!   holds only the `Connected` notify, and the rejected-key plan holds
//!   the secure-connect sequence. Integration tests pin the full
//!   read-then-act sequences through [`execute`], so the single attempt is
//!   still asserted exactly once.
//! - The open arm notifies `Failed` on failure (the wrapper's `:130-134`
//!   `if/else` verbatim). Center's open arm notifies success-only — the two
//!   wrappers differ, and each port mirrors its own wrapper.
//! - User-visible stderr goes through the `err: &mut dyn Write` seam
//!   ([`execute_with_stdio`]) instead of writing the process stderr
//!   directly, so the parity tests byte-compare the prompt/status bytes
//!   without a live tty. Production [`execute`] passes the locked stderr.
//! - Exit-code normalisation: every wifi arm is quiet in the wrapper
//!   (`|| true` / `2>/dev/null`, and the prompt paths `exit 0`), so the
//!   port returns `Ok` even when a tool is missing or fails; only malformed
//!   and unknown ids error (through the shared runner's single
//!   `flex: error:` prefix).
//!
//! The three mandated prompt behaviours (`flex-wifi.sh:149-161`), all
//! implemented in [`read_password`] and tested without a live tty:
//!
//! 1. the prompt (`Password for {ssid}: `, no newline) goes to **stderr**
//!    (the past regression — a blank-looking hung popup — came from
//!    `read -p`'s stderr reaching `/dev/null`);
//! 2. the read falls back to stdin when `/dev/tty` is unavailable
//!    (`read -rs </dev/tty || read -rs` verbatim: the tty is read first,
//!    stdin only when there is no tty or the tty read itself fails);
//! 3. an empty answer prints `No password entered — not connecting to
//!    {ssid}.` instead of exiting silently.
//!
//! `stty -echo` / `stty echo` run with stdin wired to `/dev/tty` (the right
//! call given `unsafe_code = "deny"`: no `termios`); echo is restored on
//! every normal exit path. Residual risk: `panic = "abort"` means no guard
//! runs, so an abort mid-prompt leaves echo off (recovered with `reset`);
//! reads return `Option`/`String` and never panic.
//!
//! Env seams (empty values fall back, like the wrapper's `${VAR:-default}`):
//! `NMCLI`, `NOTIFY_SEND` (tool overrides), `FLEX_WIFI_PASSWORD` (skips the
//! prompt).
//!
//! [`NotifyWhen`]: crate::exec::NotifyWhen

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::exec::NotifyWhen;
use crate::providers::wifi;
use crate::spawn::RetryExec as _;
use crate::tools;

/// A validated wifi action id (`flex-wifi.sh:174-185` arms; the SSID travels
/// in the label, never in the whitespace-split id token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiAction {
    /// `on`: `nmcli radio wifi on`.
    On,
    /// `off`: `nmcli radio wifi off`.
    Off,
    /// `disconnect`: drop the active connection (the SSID is stripped from
    /// the `Disconnect from {ssid}` label).
    Disconnect,
    /// `wifi`: fresh-state connect/disconnect for the SSID in the label.
    Connect,
    /// `noop`: the empty-scan placeholder — exit `0`, never resolve or act.
    Noop,
    /// Well-formed but unsupported: `select` reports `unknown action` (like
    /// the wrapper's `*)` arm).
    Unknown(String),
}

impl WifiAction {
    /// Validate a menu action id, mirroring the wrapper's id check
    /// (non-empty, no `/`, no newline — `flex-wifi.sh:37`). `None` is the
    /// wrapper's `bad id` exit; `noop` is the empty-scan placeholder.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        if id.is_empty() || id.contains('/') || id.contains('\n') {
            return None;
        }
        match id {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            "disconnect" => Some(Self::Disconnect),
            "wifi" => Some(Self::Connect),
            "noop" => Some(Self::Noop),
            _ => Some(Self::Unknown(id.to_string())),
        }
    }
}

/// One wifi step: a tool spawn (pure data, no process started).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `nmcli radio wifi on` (quiet, `|| true`).
    NmcliRadioOn {
        /// `NMCLI` program.
        nmcli: String,
    },
    /// `nmcli radio wifi off` (quiet, `|| true`).
    NmcliRadioOff {
        /// `NMCLI` program.
        nmcli: String,
    },
    /// `nmcli device disconnect <iface>` (quiet, `|| true`).
    NmcliDisconnect {
        /// `NMCLI` program.
        nmcli: String,
        /// Wi-Fi interface.
        iface: String,
    },
    /// `nmcli device wifi connect <ssid> ifname <iface>` (gates its
    /// notifies).
    NmcliConnect {
        /// `NMCLI` program.
        nmcli: String,
        /// Target SSID (one argument, so spaces/colons survive).
        ssid: String,
        /// Wi-Fi interface.
        iface: String,
    },
    /// `nmcli device wifi connect <ssid> password <password> ifname
    /// <iface>` (gates its notifies).
    NmcliConnectSecure {
        /// `NMCLI` program.
        nmcli: String,
        /// Target SSID (one argument).
        ssid: String,
        /// Wi-Fi password (one argument; renders verbatim in [`describe`]).
        password: String,
        /// Wi-Fi interface.
        iface: String,
    },
    /// `<notify> -a Wi-Fi <summary> <body>` (quiet, `|| true`).
    Notify {
        /// `NOTIFY_SEND` program.
        notify: String,
        /// `Connected` / `Disconnected` / `Failed` (the wrapper's summaries).
        summary: String,
        /// SSID named in the notification.
        body: String,
        /// Which outcome runs it (see [`NotifyWhen`]).
        when: NotifyWhen,
    },
}

/// A fully-decided wifi action: [`execute`] performs the planning reads
/// (`nmcli` discovery/list/profiles, the saved-profile attempt, the
/// password prompt) and [`execute_with`] runs the [`plan`] steps with no
/// further input (the `execute`/`execute_with` split from the pilot
/// template).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedAction {
    /// Nothing to do (`noop`, vanished interface, empty password): no
    /// steps, exit `0`.
    None,
    /// Turn the radio on.
    RadioOn {
        /// `NMCLI` program.
        nmcli: String,
    },
    /// Turn the radio off.
    RadioOff {
        /// `NMCLI` program.
        nmcli: String,
    },
    /// Disconnect from the current network, then always notify (covers both
    /// the explicit `disconnect` arm and selecting the connected network).
    Disconnect {
        /// `NMCLI` program.
        nmcli: String,
        /// Wi-Fi interface.
        iface: String,
        /// SSID named in the notify.
        ssid: String,
        /// `NOTIFY_SEND` program.
        notify: String,
    },
    /// Connect to an open (or stale-label) network: `Connected` notify on
    /// success, `Failed` on failure (the wrapper's `if/else` verbatim).
    WifiConnectOpen {
        /// `NMCLI` program.
        nmcli: String,
        /// Target SSID.
        ssid: String,
        /// Wi-Fi interface.
        iface: String,
        /// `NOTIFY_SEND` program.
        notify: String,
    },
    /// Connect with a password: `Connected` notify on success, `Failed` on
    /// failure (the wrapper's `if/else` verbatim).
    WifiConnectSecure {
        /// `NMCLI` program.
        nmcli: String,
        /// Target SSID.
        ssid: String,
        /// Wi-Fi password.
        password: String,
        /// Wi-Fi interface.
        iface: String,
        /// `NOTIFY_SEND` program.
        notify: String,
    },
    /// The saved-profile attempt already connected (it ran inside `decide`,
    /// where the wrapper runs it): only the `Connected` notify remains.
    WifiProfileConnected {
        /// SSID named in the notify.
        ssid: String,
        /// `NOTIFY_SEND` program.
        notify: String,
    },
}

/// Build the [`Step`]s for a decided [`PlannedAction`] (no process started).
#[must_use]
pub fn plan(planned: &PlannedAction) -> Vec<Step> {
    match planned {
        PlannedAction::None => Vec::new(),
        PlannedAction::RadioOn { nmcli } => vec![Step::NmcliRadioOn {
            nmcli: nmcli.clone(),
        }],
        PlannedAction::RadioOff { nmcli } => vec![Step::NmcliRadioOff {
            nmcli: nmcli.clone(),
        }],
        PlannedAction::Disconnect {
            nmcli,
            iface,
            ssid,
            notify,
        } => vec![
            Step::NmcliDisconnect {
                nmcli: nmcli.clone(),
                iface: iface.clone(),
            },
            Step::Notify {
                notify: notify.clone(),
                summary: String::from("Disconnected"),
                body: ssid.clone(),
                when: NotifyWhen::Always,
            },
        ],
        PlannedAction::WifiConnectOpen {
            nmcli,
            ssid,
            iface,
            notify,
        } => vec![
            Step::NmcliConnect {
                nmcli: nmcli.clone(),
                ssid: ssid.clone(),
                iface: iface.clone(),
            },
            Step::Notify {
                notify: notify.clone(),
                summary: String::from("Connected"),
                body: ssid.clone(),
                when: NotifyWhen::Success,
            },
            Step::Notify {
                notify: notify.clone(),
                summary: String::from("Failed"),
                body: ssid.clone(),
                when: NotifyWhen::Failure,
            },
        ],
        PlannedAction::WifiConnectSecure {
            nmcli,
            ssid,
            password,
            iface,
            notify,
        } => vec![
            Step::NmcliConnectSecure {
                nmcli: nmcli.clone(),
                ssid: ssid.clone(),
                password: password.clone(),
                iface: iface.clone(),
            },
            Step::Notify {
                notify: notify.clone(),
                summary: String::from("Connected"),
                body: ssid.clone(),
                when: NotifyWhen::Success,
            },
            Step::Notify {
                notify: notify.clone(),
                summary: String::from("Failed"),
                body: ssid.clone(),
                when: NotifyWhen::Failure,
            },
        ],
        PlannedAction::WifiProfileConnected { ssid, notify } => vec![Step::Notify {
            notify: notify.clone(),
            summary: String::from("Connected"),
            body: ssid.clone(),
            when: NotifyWhen::Always,
        }],
    }
}

/// Render one [`Step`] as a single snapshot line: `argv` joined by spaces.
///
/// This is the template snapshot convention: unit tests pin these lines per
/// id (pure, no spawn), and the integration tests diff the stub-`PATH` call
/// logs against the same shapes.
#[must_use]
pub fn describe(step: &Step) -> String {
    match step {
        Step::NmcliRadioOn { nmcli } => format!("{nmcli} radio wifi on"),
        Step::NmcliRadioOff { nmcli } => format!("{nmcli} radio wifi off"),
        Step::NmcliDisconnect { nmcli, iface } => format!("{nmcli} device disconnect {iface}"),
        Step::NmcliConnect { nmcli, ssid, iface } => {
            format!("{nmcli} device wifi connect {ssid} ifname {iface}")
        }
        Step::NmcliConnectSecure {
            nmcli,
            ssid,
            password,
            iface,
        } => format!("{nmcli} device wifi connect {ssid} password {password} ifname {iface}"),
        Step::Notify {
            notify,
            summary,
            body,
            ..
        } => format!("{notify} -a Wi-Fi {summary} {body}"),
    }
}

/// Render a whole [`plan`] as snapshot lines (see [`describe`]).
#[must_use]
pub fn describe_plan(steps: &[Step]) -> Vec<String> {
    steps.iter().map(describe).collect()
}

/// `NMCLI` override, else `nmcli` (empty values fall back, like the
/// wrapper's `${NMCLI:-nmcli}`).
#[must_use]
pub fn nmcli_cmd() -> String {
    if let Ok(cmd) = std::env::var("NMCLI") {
        if !cmd.is_empty() {
            return cmd;
        }
    }
    String::from("nmcli")
}

/// `NOTIFY_SEND` override, else `notify-send` (empty values fall back, like
/// the wrapper's `${NOTIFY_SEND:-notify-send}`).
#[must_use]
pub fn notify_cmd() -> String {
    if let Ok(cmd) = std::env::var("NOTIFY_SEND") {
        if !cmd.is_empty() {
            return cmd;
        }
    }
    String::from("notify-send")
}

/// The ambient `PATH`, empty when unset (tool resolution then fails cleanly
/// instead of inheriting a surprising default).
fn ambient_path() -> String {
    tools::ambient_path()
}

/// Resolve `name` against `path_env` (`:`-separated, shell-style).
///
/// Returns the first entry naming an existing file, so stub-`PATH` tests can
/// shadow the real tools without touching the process env. An absolute
/// `name` (the usual override shape) resolves to itself, exactly like the
/// wrapper's `"$nmcli_cmd" …` direct invocation.
fn resolve_tool(name: &str, path_env: &str) -> Option<PathBuf> {
    tools::resolve_tool(name, path_env)
}

/// Fixed `nmcli` argv shapes (OPT-11): `&'static` consts, no per-call `Vec<String>`.
const DEVICE_TYPE_ARGS: &[&str] = &["-t", "-f", "DEVICE,TYPE", "device"];
/// Fixed `nmcli connection show` argv shape.
const CONNECTION_SHOW_ARGS: &[&str] = &["-t", "-f", "NAME,TYPE", "connection", "show"];
/// Fixed `nmcli radio wifi` prefix; the trailing `on`/`off` appends.
const RADIO_WIFI_ARGS: &[&str] = &["radio", "wifi"];
/// Fixed `nmcli device wifi connect` head (`ssid`/`password`/`ifname` append).
const CONNECT_HEAD_ARGS: &[&str] = &["device", "wifi", "connect"];

/// Run one tool with `args` (quiet): stdio nulled, failures swallowed —
/// the wrapper's `… 2>/dev/null || true`. Returns whether it exited `0`; a
/// missing tool or failed spawn counts as failure, never an error.
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

/// Capture one tool's stdout (query): stderr nulled, stdin nulled, like the
/// wrapper's `$(… 2>/dev/null || true)` — stdout is captured even on a
/// non-zero exit. `None` only when the tool is missing or the spawn fails.
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

/// First Wi-Fi interface from `nmcli -t -f DEVICE,TYPE device` (the
/// wrapper's `wifi_iface` verbatim: the first `*:wifi` device).
/// `None` when the interface vanished since the snapshot.
fn wifi_iface(nmcli: &str, path_env: &str) -> Option<String> {
    let out = tool_captured(path_env, nmcli, DEVICE_TYPE_ARGS)?;
    wifi::parse_nmcli_devices(&out)
}

/// `nmcli … wifi list` output for `iface` (`None` on any failure — the
/// caller treats a missing scan as a stale label, i.e. the open arm).
/// `--rescan no` reads `NetworkManager`'s cache: the picker just scanned
/// (its background rescan), and triggering another scan would add ~3 s
/// between Enter and the connect (`flex-wifi.sh:114-117`).
fn wifi_list(nmcli: &str, path_env: &str, iface: &str) -> Option<String> {
    let args = [
        "-t",
        "-f",
        "IN-USE,SSID,SIGNAL,SECURITY",
        "device",
        "wifi",
        "list",
        "ifname",
        iface,
        "--rescan",
        "no",
    ];
    tool_captured(path_env, nmcli, &args)
}

/// Whether a saved Wi-Fi profile exists for `ssid` (the wrapper's
/// `is_saved` verbatim, via the same [`wifi::parse_saved_profiles`] the
/// picker rows use: split against the last colon, `802-11-wireless` only,
/// names unescaped before compare — `flex-wifi.sh:75-84`).
fn is_saved(nmcli: &str, path_env: &str, ssid: &str) -> bool {
    tool_captured(path_env, nmcli, CONNECTION_SHOW_ARGS).is_some_and(|out| {
        wifi::parse_saved_profiles(&out)
            .iter()
            .any(|saved| saved == ssid)
    })
}

/// Connect using the stored profile / no credentials (the wrapper's
/// `connect_without_password`: open networks and the saved-profile
/// attempt share it — `flex-wifi.sh:87-89`).
fn connect_without_password(nmcli: &str, path_env: &str, ssid: &str, iface: &str) -> bool {
    let args = [
        CONNECT_HEAD_ARGS[0],
        CONNECT_HEAD_ARGS[1],
        CONNECT_HEAD_ARGS[2],
        ssid,
        "ifname",
        iface,
    ];
    tool_quiet(path_env, nmcli, &args)
}

/// Post-TUI status line on `err` (the wrapper's `status`: stderr is the
/// popup's terminal, and flex has already left the alternate screen, so
/// this is what the user sees while a connect runs — `flex-wifi.sh:65`).
fn status(err: &mut dyn std::io::Write, line: &str) {
    let _ = writeln!(err, "{line}");
}

/// One prompt read: `Some` line (trailing CR/LF stripped; EOF reads as
/// empty) or `None` when the read itself failed (the wrapper's `||`
/// trigger for the stdin fallback).
fn read_pw_line(reader: &mut dyn BufRead) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(_) => {
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Some(line)
        }
        Err(_) => None,
    }
}

/// `stty -echo` / `stty echo` with stdin on `/dev/tty` (best-effort: every
/// failure is swallowed — the prompt still works, just visibly).
fn set_echo(path_env: &str, on: bool) {
    let Ok(tty) = File::open("/dev/tty") else {
        return;
    };
    let Some(stty) = resolve_tool("stty", path_env) else {
        return;
    };
    let arg = if on { "echo" } else { "-echo" };
    let _ = Command::new(&stty)
        .arg(arg)
        .stdin(tty)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status_retrying();
}

/// Secure-wifi password: `FLEX_WIFI_PASSWORD` when set and non-empty, else
/// the `/dev/tty` prompt with the three mandated behaviours
/// (`flex-wifi.sh:149-161` — see the module docs). Live sources: the real
/// `/dev/tty` (falling back to the real stdin) and the locked stderr.
#[must_use]
pub fn read_password(ssid: &str, path_env: &str, err: &mut dyn std::io::Write) -> String {
    let tty_file = File::open("/dev/tty").ok();
    let mut tty_reader = tty_file.map(BufReader::new);
    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let tty = tty_reader.as_mut().map(|reader| reader as &mut dyn BufRead);
    read_password_with(ssid, path_env, tty, &mut stdin_lock, err)
}

/// [`read_password`] with injectable sources: `tty` is the `/dev/tty` line
/// source when present (`None` exercises the stdin fallback with piped
/// stdin and no live tty), `stdin` feeds the fallback path, and `err`
/// captures every user-visible byte (prompt, post-read newline, the
/// empty-answer line). Tests drive this directly (no TUI/pty); production
/// goes through [`read_password`].
#[must_use]
pub fn read_password_with(
    ssid: &str,
    path_env: &str,
    tty: Option<&mut dyn BufRead>,
    stdin: &mut dyn BufRead,
    err: &mut dyn std::io::Write,
) -> String {
    if let Ok(stored) = std::env::var("FLEX_WIFI_PASSWORD") {
        if !stored.is_empty() {
            return stored;
        }
    }
    // The prompt is printed HERE, not via `read -p`: `read -p` writes to
    // stderr, and an early cut sent that stderr to /dev/null, so the popup
    // sat blank and looked hung while it waited for a password.
    let _ = write!(err, "Password for {ssid}: ");
    let _ = err.flush();
    // `stty` needs the live tty: skip the dance entirely on the
    // stdin-fallback path (a pipe has no echo to silence).
    let live = tty.is_some();
    if live {
        set_echo(path_env, false);
    }
    let password = if let Some(reader) = tty {
        read_pw_line(reader).unwrap_or_else(|| read_pw_line(stdin).unwrap_or_default())
    } else {
        read_pw_line(stdin).unwrap_or_default()
    };
    if live {
        set_echo(path_env, true);
    }
    let _ = writeln!(err);
    if password.is_empty() {
        // Never fail silently: an empty answer (or the blind Enter that a
        // missing prompt invites) must say so instead of closing the popup
        // with nothing done.
        let _ = writeln!(err, "No password entered — not connecting to {ssid}.");
    }
    password
}

/// What [`execute`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteReport {
    /// The action id that ran.
    pub action_id: String,
    /// Resolved detail (the SSID for connect/disconnect flows; `None` for
    /// effect-free rows like `on`/`off`/`noop`).
    pub detail: Option<String>,
}

/// Strip the `Disconnect from ` label prefix for the notify body (the
/// wrapper's `${label#Disconnect from }` — `flex-wifi.sh:181`).
fn disconnect_ssid(label: &str) -> String {
    label
        .strip_prefix("Disconnect from ")
        .unwrap_or(label)
        .to_string()
}

/// Run the selected wifi action: validate the id, short-circuit `noop`,
/// take fresh `nmcli` state where the wrapper does, and run the [`plan`]
/// steps. `label` is the in-process row label (already raw — no wire
/// unescape, see the module docs). `path_env` shadows the ambient `PATH`
/// when `Some` (the stub seam tests use); `None` inherits it. User-visible
/// output goes to the locked process stderr.
///
/// # Errors
///
/// When the id is malformed (`bad id`, like the wrapper) or well-formed
/// but unsupported (`unknown action`, like the wrapper's `*)` arm).
/// Messages carry no `flex:` prefix; the runner reports them. Every effect
/// arm is quiet (the wrapper's `|| true`), so tool failures never error.
pub fn execute(action_id: &str, label: &str, path_env: Option<&str>) -> Result<ExecuteReport> {
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    let tty_file = File::open("/dev/tty").ok();
    let mut tty_reader = tty_file.map(BufReader::new);
    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let tty = tty_reader.as_mut().map(|reader| reader as &mut dyn BufRead);
    execute_with_stdio(action_id, label, path_env, tty, &mut stdin_lock, &mut err)
}

/// [`execute`] with injectable stdio: `tty` is the `/dev/tty` line source
/// for the password prompt (`None` exercises the stdin fallback with piped
/// stdin and no live tty), `stdin` feeds that fallback, and `err` captures
/// every user-visible byte (prompt, progress, empty-answer line) so the
/// parity tests byte-compare stderr without a pty. Production goes through
/// [`execute`].
///
/// # Errors
///
/// When the id is malformed or unknown (see [`execute`]).
pub fn execute_with_stdio(
    action_id: &str,
    label: &str,
    path_env: Option<&str>,
    tty: Option<&mut dyn BufRead>,
    stdin: &mut dyn BufRead,
    err: &mut dyn std::io::Write,
) -> Result<ExecuteReport> {
    let Some(action) = WifiAction::parse(action_id) else {
        anyhow::bail!("wifi: bad id '{action_id}'");
    };
    let path_env = path_env.map_or_else(ambient_path, str::to_string);
    let planned = decide(&action, label, &path_env, tty, stdin, err)?;
    Ok(execute_with(action_id, &planned, Some(&path_env)))
}

/// Decide half of [`execute`]: validate, take fresh tool state, prompt
/// where the wrapper prompts (no effect started, except the
/// saved-profile attempt — see the module docs).
fn decide(
    action: &WifiAction,
    label: &str,
    path_env: &str,
    tty: Option<&mut dyn BufRead>,
    stdin: &mut dyn BufRead,
    err: &mut dyn std::io::Write,
) -> Result<PlannedAction> {
    match action {
        WifiAction::Noop => Ok(PlannedAction::None),
        WifiAction::On => Ok(PlannedAction::RadioOn { nmcli: nmcli_cmd() }),
        WifiAction::Off => Ok(PlannedAction::RadioOff { nmcli: nmcli_cmd() }),
        WifiAction::Disconnect => {
            let nmcli = nmcli_cmd();
            let notify = notify_cmd();
            let Some(iface) = wifi_iface(&nmcli, path_env) else {
                return Ok(PlannedAction::None);
            };
            Ok(PlannedAction::Disconnect {
                nmcli,
                iface,
                ssid: disconnect_ssid(label),
                notify,
            })
        }
        WifiAction::Connect => Ok(decide_connect(label, path_env, tty, stdin, err)),
        WifiAction::Unknown(id) => {
            anyhow::bail!("wifi: unknown action: {id}");
        }
    }
}

/// Decide the `wifi` flow on fresh `nmcli` state (the wrapper's
/// `do_connect` verbatim: vanished interface → nothing; connected →
/// disconnect; open or stale label → connect; secure → saved-profile
/// attempt, then password, then connect).
fn decide_connect(
    ssid: &str,
    path_env: &str,
    tty: Option<&mut dyn BufRead>,
    stdin: &mut dyn BufRead,
    err: &mut dyn std::io::Write,
) -> PlannedAction {
    let nmcli = nmcli_cmd();
    let notify = notify_cmd();
    let Some(iface) = wifi_iface(&nmcli, path_env) else {
        return PlannedAction::None;
    };
    let list = wifi_list(&nmcli, path_env, &iface).unwrap_or_default();
    let found = wifi::parse_nmcli_wifi(&list)
        .into_iter()
        .find(|net| net.ssid == ssid);
    match found {
        Some(net) if net.connected => PlannedAction::Disconnect {
            nmcli,
            iface,
            ssid: ssid.to_string(),
            notify,
        },
        Some(net) if net.security.is_empty() || net.security == "--" => {
            // Unknown (stale label) also lands here via the `None` arm
            // below: treated as open, like an empty `$sec` in bash.
            status(err, &format!("Connecting to {ssid}…"));
            PlannedAction::WifiConnectOpen {
                nmcli,
                ssid: ssid.to_string(),
                iface,
                notify,
            }
        }
        Some(_) => {
            // A saved profile means NetworkManager already holds the
            // credentials: asking for the password again is wrong, so
            // connect from the profile first and only ask when that fails
            // (e.g. the network's key changed).
            if is_saved(&nmcli, path_env, ssid) {
                status(err, &format!("Connecting to {ssid}…"));
                if connect_without_password(&nmcli, path_env, ssid, &iface) {
                    return PlannedAction::WifiProfileConnected {
                        ssid: ssid.to_string(),
                        notify,
                    };
                }
                status(
                    err,
                    &format!("Saved credentials for {ssid} were rejected — enter the password."),
                );
            }
            let password = read_password_with(ssid, path_env, tty, stdin, err);
            if password.is_empty() {
                return PlannedAction::None;
            }
            status(err, &format!("Connecting to {ssid}…"));
            PlannedAction::WifiConnectSecure {
                nmcli,
                ssid: ssid.to_string(),
                password,
                iface,
                notify,
            }
        }
        None => {
            status(err, &format!("Connecting to {ssid}…"));
            PlannedAction::WifiConnectOpen {
                nmcli,
                ssid: ssid.to_string(),
                iface,
                notify,
            }
        }
    }
}

/// Effect half of [`execute`], with the decided [`PlannedAction`] given (no
/// env reads, no resolve, no prompt): the shape integration tests drive
/// with a stub `PATH` and scratch `HOME`. Infallible: every wifi arm is
/// quiet (the wrapper's `|| true`), so missing tools and failed spawns are
/// swallowed, never errors.
#[must_use]
pub fn execute_with(
    action_id: &str,
    planned: &PlannedAction,
    path_env: Option<&str>,
) -> ExecuteReport {
    let path_env = path_env.map_or_else(ambient_path, str::to_string);
    // The previous gated-tool outcome: only the `nmcli connect` steps set
    // it from a real exit status (quiet steps never fail, like `|| true`).
    let mut last_ok = true;
    for step in plan(planned) {
        run_step(&path_env, step, &mut last_ok);
    }
    ExecuteReport {
        action_id: action_id.to_string(),
        detail: report_detail(planned),
    }
}

/// Run one [`Step`] (the [`execute_with`] interpreter body). `last_ok`
/// carries the previous gated-tool outcome for [`NotifyWhen`].
fn run_step(path_env: &str, step: Step, last_ok: &mut bool) {
    match step {
        Step::NmcliRadioOn { nmcli } => {
            let args = [RADIO_WIFI_ARGS[0], RADIO_WIFI_ARGS[1], "on"];
            tool_quiet(path_env, &nmcli, &args);
        }
        Step::NmcliRadioOff { nmcli } => {
            let args = [RADIO_WIFI_ARGS[0], RADIO_WIFI_ARGS[1], "off"];
            tool_quiet(path_env, &nmcli, &args);
        }
        Step::NmcliDisconnect { nmcli, iface } => {
            let args = ["device", "disconnect", iface.as_str()];
            tool_quiet(path_env, &nmcli, &args);
        }
        Step::NmcliConnect { nmcli, ssid, iface } => {
            let args = [
                CONNECT_HEAD_ARGS[0],
                CONNECT_HEAD_ARGS[1],
                CONNECT_HEAD_ARGS[2],
                ssid.as_str(),
                "ifname",
                iface.as_str(),
            ];
            *last_ok = tool_quiet(path_env, &nmcli, &args);
        }
        Step::NmcliConnectSecure {
            nmcli,
            ssid,
            password,
            iface,
        } => {
            let args = [
                CONNECT_HEAD_ARGS[0],
                CONNECT_HEAD_ARGS[1],
                CONNECT_HEAD_ARGS[2],
                ssid.as_str(),
                "password",
                password.as_str(),
                "ifname",
                iface.as_str(),
            ];
            *last_ok = tool_quiet(path_env, &nmcli, &args);
        }
        Step::Notify {
            notify,
            summary,
            body,
            when,
        } => {
            let run = match when {
                NotifyWhen::Always => true,
                NotifyWhen::Success => *last_ok,
                NotifyWhen::Failure => !*last_ok,
            };
            if run {
                let args = ["-a", "Wi-Fi", summary.as_str(), body.as_str()];
                tool_quiet(path_env, &notify, &args);
            }
        }
    }
}

/// Report detail for a decided action (the SSID the row carried; `None`
/// for effect-free rows like `on`/`off`/`noop`).
fn report_detail(planned: &PlannedAction) -> Option<String> {
    match planned {
        PlannedAction::None | PlannedAction::RadioOn { .. } | PlannedAction::RadioOff { .. } => {
            None
        }
        PlannedAction::Disconnect { ssid, .. }
        | PlannedAction::WifiConnectOpen { ssid, .. }
        | PlannedAction::WifiConnectSecure { ssid, .. }
        | PlannedAction::WifiProfileConnected { ssid, .. } => Some(ssid.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Serialize the tests that mutate `FLEX_WIFI_PASSWORD`: the process env
    /// is global, so parallel set/remove races produced a real flake
    /// (a stored-password test reading the fallback's removed value).
    fn password_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        match LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// `read_password_with` sources without a live tty: `None` tty plus a
    /// piped-stdin `Cursor` (the stdin-fallback path, byte for byte).
    fn no_tty(stdin: &[u8]) -> (Option<&mut dyn BufRead>, Cursor<Vec<u8>>) {
        (None, Cursor::new(stdin.to_vec()))
    }

    /// A `/dev/tty` source whose read always fails (the `||` trigger for
    /// the stdin fallback).
    struct FailingTty;

    impl BufRead for FailingTty {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(std::io::Error::other("no tty"))
        }
        fn consume(&mut self, _amount: usize) {}
    }

    impl std::io::Read for FailingTty {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no tty"))
        }
    }

    #[test]
    fn ids_validate_like_the_wrapper_check() {
        assert_eq!(WifiAction::parse("on"), Some(WifiAction::On));
        assert_eq!(WifiAction::parse("off"), Some(WifiAction::Off));
        assert_eq!(
            WifiAction::parse("disconnect"),
            Some(WifiAction::Disconnect)
        );
        assert_eq!(WifiAction::parse("wifi"), Some(WifiAction::Connect));
        assert_eq!(WifiAction::parse("noop"), Some(WifiAction::Noop));
        // Well-formed but unsupported: parses, `select` rejects later.
        assert_eq!(
            WifiAction::parse("format"),
            Some(WifiAction::Unknown(String::from("format"))),
        );
    }

    #[test]
    fn ids_reject_the_wrapper_bad_id_set() {
        for bad in ["", "a/b", "a\nb", "/lead", "trail/", "mid\ndle"] {
            assert_eq!(WifiAction::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn plan_snapshot_radio_arms_are_single_lines() {
        assert_eq!(
            describe_plan(&plan(&PlannedAction::RadioOn {
                nmcli: String::from("nmcli"),
            })),
            vec!["nmcli radio wifi on"],
        );
        assert_eq!(
            describe_plan(&plan(&PlannedAction::RadioOff {
                nmcli: String::from("nmcli"),
            })),
            vec!["nmcli radio wifi off"],
        );
    }

    #[test]
    fn plan_snapshot_disconnect_always_notifies() {
        assert_eq!(
            describe_plan(&plan(&PlannedAction::Disconnect {
                nmcli: String::from("nmcli"),
                iface: String::from("wlan0"),
                ssid: String::from("HomeNet"),
                notify: String::from("notify-send"),
            })),
            vec![
                "nmcli device disconnect wlan0",
                "notify-send -a Wi-Fi Disconnected HomeNet",
            ],
        );
    }

    #[test]
    fn plan_snapshot_open_lists_connect_plus_both_notifies() {
        // The wifi wrapper's `:130-134` has the `else`: exactly one notify runs.
        assert_eq!(
            describe_plan(&plan(&PlannedAction::WifiConnectOpen {
                nmcli: String::from("nmcli"),
                ssid: String::from("Coffee Shop"),
                iface: String::from("wlan0"),
                notify: String::from("notify-send"),
            })),
            vec![
                "nmcli device wifi connect Coffee Shop ifname wlan0",
                "notify-send -a Wi-Fi Connected Coffee Shop",
                "notify-send -a Wi-Fi Failed Coffee Shop",
            ],
        );
    }

    #[test]
    fn plan_snapshot_secure_lists_both_notifies() {
        // Exactly one runs (the wrapper's `if/else` via `NotifyWhen`).
        assert_eq!(
            describe_plan(&plan(&PlannedAction::WifiConnectSecure {
                nmcli: String::from("nmcli"),
                ssid: String::from("MyNet"),
                password: String::from("s3cret"),
                iface: String::from("wlan0"),
                notify: String::from("notify-send"),
            })),
            vec![
                "nmcli device wifi connect MyNet password s3cret ifname wlan0",
                "notify-send -a Wi-Fi Connected MyNet",
                "notify-send -a Wi-Fi Failed MyNet",
            ],
        );
    }

    #[test]
    fn plan_snapshot_saved_success_is_only_the_connected_notify() {
        // The profile attempt already ran inside `decide`; the plan holds
        // just the unconditional notify.
        assert_eq!(
            describe_plan(&plan(&PlannedAction::WifiProfileConnected {
                ssid: String::from("Corp:Net"),
                notify: String::from("notify-send"),
            })),
            vec!["notify-send -a Wi-Fi Connected Corp:Net"],
        );
    }

    #[test]
    fn plan_snapshot_none_is_empty() {
        assert_eq!(plan(&PlannedAction::None), [] as [Step; 0]);
    }

    #[test]
    fn tool_cmds_prefer_overrides_and_fall_back_empty() {
        let saved_nmcli = std::env::var("NMCLI").ok();
        let saved_notify = std::env::var("NOTIFY_SEND").ok();
        std::env::set_var("NMCLI", "/tmp/stub-nmcli");
        std::env::set_var("NOTIFY_SEND", "");
        assert_eq!(nmcli_cmd(), "/tmp/stub-nmcli");
        assert_eq!(notify_cmd(), "notify-send");
        match saved_nmcli {
            Some(value) => std::env::set_var("NMCLI", value),
            None => std::env::remove_var("NMCLI"),
        }
        match saved_notify {
            Some(value) => std::env::set_var("NOTIFY_SEND", value),
            None => std::env::remove_var("NOTIFY_SEND"),
        }
    }

    #[test]
    fn disconnect_ssid_strips_the_bash_prefix() {
        assert_eq!(
            disconnect_ssid("Disconnect from HomeNet"),
            "HomeNet",
            "the wrapper strips the Disconnect prefix"
        );
        assert_eq!(
            disconnect_ssid("HomeNet"),
            "HomeNet",
            "no prefix strips nothing"
        );
    }

    #[test]
    fn stored_password_skips_the_prompt_entirely() {
        let _guard = password_env_lock();
        let saved = std::env::var("FLEX_WIFI_PASSWORD").ok();
        std::env::set_var("FLEX_WIFI_PASSWORD", "s3cret");
        let (tty, mut stdin) = no_tty(b"ignored\n");
        let mut err = Vec::new();
        let password = read_password_with("MyNet", "", tty, &mut stdin, &mut err);
        assert_eq!(password, "s3cret");
        assert_eq!(err, b"", "no prompt bytes when the seam answers: {err:?}");
        match saved {
            Some(value) => std::env::set_var("FLEX_WIFI_PASSWORD", value),
            None => std::env::remove_var("FLEX_WIFI_PASSWORD"),
        }
    }

    #[test]
    fn prompt_goes_to_stderr_and_stdin_fallback_answers() {
        let _guard = password_env_lock();
        let saved = std::env::var("FLEX_WIFI_PASSWORD").ok();
        std::env::remove_var("FLEX_WIFI_PASSWORD");
        // No tty: the piped-stdin fallback answers (mandate 2).
        let (tty, mut stdin) = no_tty(b"s3cret\n");
        let mut err = Vec::new();
        let password = read_password_with("Corp:Net", "", tty, &mut stdin, &mut err);
        assert_eq!(password, "s3cret");
        assert_eq!(
            String::from_utf8_lossy(&err),
            "Password for Corp:Net: \n",
            "the prompt reaches stderr (mandate 1), then the post-read newline"
        );
        match saved {
            Some(value) => std::env::set_var("FLEX_WIFI_PASSWORD", value),
            None => std::env::remove_var("FLEX_WIFI_PASSWORD"),
        }
    }

    #[test]
    fn empty_answer_prints_the_explicit_line() {
        let _guard = password_env_lock();
        let saved = std::env::var("FLEX_WIFI_PASSWORD").ok();
        std::env::remove_var("FLEX_WIFI_PASSWORD");
        // EOF on piped stdin reads as empty (mandate 3).
        let (tty, mut stdin) = no_tty(b"");
        let mut err = Vec::new();
        let password = read_password_with("Corp:Net", "", tty, &mut stdin, &mut err);
        assert_eq!(password, "");
        assert_eq!(
            String::from_utf8_lossy(&err),
            "Password for Corp:Net: \nNo password entered — not connecting to Corp:Net.\n",
        );
        match saved {
            Some(value) => std::env::set_var("FLEX_WIFI_PASSWORD", value),
            None => std::env::remove_var("FLEX_WIFI_PASSWORD"),
        }
    }

    #[test]
    fn tty_is_read_first_and_stdin_only_on_tty_failure() {
        let _guard = password_env_lock();
        let saved = std::env::var("FLEX_WIFI_PASSWORD").ok();
        std::env::remove_var("FLEX_WIFI_PASSWORD");
        // A live tty answers: stdin stays untouched (the wrapper's
        // `read -rs </dev/tty` succeeds, so the `|| read -rs` never runs).
        let mut tty = Cursor::new(b"tty-secret\n".to_vec());
        let mut stdin = Cursor::new(b"stdin-secret\n".to_vec());
        let mut err = Vec::new();
        let password = read_password_with(
            "MyNet",
            "",
            Some(&mut tty as &mut dyn BufRead),
            &mut stdin,
            &mut err,
        );
        assert_eq!(password, "tty-secret");
        assert_eq!(
            stdin.position(),
            0,
            "stdin is never consumed when the tty answers"
        );
        // A failing tty falls back to stdin (the `||` arm verbatim).
        let mut failing = FailingTty;
        let mut stdin = Cursor::new(b"fallback-secret\n".to_vec());
        let mut err = Vec::new();
        let password = read_password_with(
            "MyNet",
            "",
            Some(&mut failing as &mut dyn BufRead),
            &mut stdin,
            &mut err,
        );
        assert_eq!(password, "fallback-secret");
        match saved {
            Some(value) => std::env::set_var("FLEX_WIFI_PASSWORD", value),
            None => std::env::remove_var("FLEX_WIFI_PASSWORD"),
        }
    }
}
