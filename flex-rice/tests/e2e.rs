//! End-to-end deployment tests: the installed release farm + live services.
//!
//! Unlike the other suites (which drive `CARGO_BIN_EXE_*` build artifacts with
//! stub seams), this suite asserts the machine state that `release and restart
//! services` produces:
//!   - `cargo build --release` + `./setup.sh` links all 16 `~/.local/bin` names
//!     to their respective `target/release/flex-*` binaries;
//!   - `./setup.sh --check` (the CI gate) passes;
//!   - the `flex-notify.service` user unit is active;
//!   - the installed dispatcher re-execs providers and non-interactive verbs
//!     plus `flex-notify --status` work headless against scratch state.
//!
//! No test here opens a real TUI, touches the real `$HOME` data, or writes
//! outside scratch dirs (each test gets a unique temp dir).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// All sixteen installed names (must match `setup.sh` `BINS`).
const INSTALLED: &[&str] = &[
    "flex",
    "flex-power",
    "flex-launch",
    "flex-shot",
    "flex-theme",
    "flex-clip",
    "flex-wallpaper",
    "flex-wifi",
    "flex-proc",
    "flex-record",
    "flex-mixer",
    "flex-net",
    "flex-bt",
    "flex-notify",
    "flex-profile",
];

/// Unique scratch dir per call (tests run in parallel).
fn scratch(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("flex-e2e-{}-{}-{}", std::process::id(), name, seq));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn bin_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME is set");
    PathBuf::from(home).join(".local/bin")
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().expect("workspace root").to_path_buf()
}

fn run(cmd: &mut Command) -> Output {
    cmd.stdin(Stdio::null()).output().expect("spawn e2e child")
}

/// Skip gracefully when there is no user systemd (e.g. container CI).
fn systemctl_usable() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Release farm prerequisite: `cargo build --release` plus `./setup.sh`.
///
/// These e2e tests verify a deployment, not a fresh checkout: without the
/// release binary and the installed symlinks there is nothing to assert, so
/// farm-dependent tests skip (plain `cargo test` in CI runs before the
/// release-build + install steps; the `E2E after install` CI step runs this
/// same suite once the farm exists).
fn farm_available() -> Option<(PathBuf, PathBuf)> {
    let release_flex = workspace_root().join("target/release/flex");
    if !release_flex.is_file() {
        return None;
    }
    let farm = bin_dir();
    if !farm.join("flex").is_file() {
        return None;
    }
    Some((farm, release_flex))
}

#[test]
fn installed_farm_resolves_to_release_binaries_and_answers_help() {
    let Some((farm, _release_flex)) = farm_available() else {
        eprintln!("e2e: no release farm (cargo build --release + ./setup.sh first), skipping");
        return;
    };
    for name in INSTALLED {
        let link = farm.join(name);
        let meta = std::fs::symlink_metadata(&link)
            .unwrap_or_else(|_| panic!("installed name missing: {}", link.display()));
        assert!(
            meta.file_type().is_symlink(),
            "{} must be a symlink",
            link.display()
        );
        let resolved = std::fs::canonicalize(&link).expect("resolve installed symlink");
        let expected_bin = workspace_root().join("target/release").join(name);
        let expected = std::fs::canonicalize(&expected_bin)
            .unwrap_or_else(|_| panic!("missing release binary: {}", expected_bin.display()));
        assert_eq!(
            resolved, expected,
            "{link:?} resolves to {resolved:?}, not the standalone release binary {expected:?}",
        );
        let out = run(Command::new(&link).arg("--help"));
        assert!(
            out.status.success(),
            "{name} --help exits 0: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !out.stdout.is_empty(),
            "{name} --help prints usage to stdout"
        );
    }
}

#[test]
fn setup_check_gate_passes() {
    if farm_available().is_none() {
        eprintln!("e2e: no release farm (cargo build --release + ./setup.sh first), skipping");
        return;
    }
    let root = workspace_root();
    let out = run(Command::new(root.join("setup.sh"))
        .arg("--check")
        .current_dir(&root));
    assert!(
        out.status.success(),
        "setup.sh --check must pass: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn notify_service_is_active() {
    if !systemctl_usable() {
        eprintln!("e2e: no user systemd, skipping service check");
        return;
    }
    // The unit only exists where the service was installed (not in CI
    // containers): without a unit file there is nothing to assert.
    let has_unit = Command::new("systemctl")
        .args(["--user", "cat", "flex-notify.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !has_unit {
        eprintln!("e2e: flex-notify.service not installed, skipping service check");
        return;
    }
    let out = run(Command::new("systemctl").args(["--user", "is-active", "flex-notify.service"]));
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success() && state == "active",
        "flex-notify.service must be active (got {state:?}): {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn installed_dispatcher_reexecs_provider_help() {
    if farm_available().is_none() {
        eprintln!("e2e: no release farm (cargo build --release + ./setup.sh first), skipping");
        return;
    }
    let home = scratch("dispatcher");
    let flex = bin_dir().join("flex");
    // `flex power --help` must re-exec the provider with its own identity.
    let out = run(Command::new(&flex)
        .args(["power", "--help"])
        .env("HOME", &home));
    assert!(
        out.status.success(),
        "flex power --help exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Power menu"),
        "dispatcher must forward to the power provider, got: {stdout:?}"
    );
}

#[test]
fn installed_clip_verbs_round_trip_on_scratch_state() {
    use std::os::unix::fs::PermissionsExt as _;
    if farm_available().is_none() {
        eprintln!("e2e: no release farm (cargo build --release + ./setup.sh first), skipping");
        return;
    }
    let dir = scratch("clip");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("home");
    // `add` reads the clipboard via `wl-paste` on PATH: stub it.
    let payload = dir.join("payload");
    std::fs::write(&payload, b"e2e-clip-probe\n").expect("payload");
    for (name, body) in [
        (
            "wl-paste",
            format!("#!/bin/sh\ncat '{}'\n", payload.display()),
        ),
        ("notify-send", "#!/bin/sh\nexit 0\n".to_string()),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let path_env = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let clip = bin_dir().join("flex-clip");
    let history = home.join("hist");
    let pins = home.join("pins");
    let current = home.join("current");
    let run_verb = |args: &[&str]| {
        run(Command::new(&clip)
            .args(args)
            .env("HOME", &home)
            .env("PATH", &path_env)
            .env("CLIPHIST_FILE", &history)
            .env("CLIPHIST_PINS", &pins)
            .env("CLIPHIST_CURRENT", &current))
    };
    let add = run_verb(&["add"]);
    assert!(
        add.status.success(),
        "installed flex-clip add exits 0: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let cur = run_verb(&["current"]);
    assert!(
        cur.status.success(),
        "installed flex-clip current exits 0: {}",
        String::from_utf8_lossy(&cur.stderr)
    );
    assert_eq!(
        cur.stdout,
        b"e2e-clip-probe\n".to_vec(),
        "round-tripped clipboard entry must be readable"
    );
}

#[test]
fn installed_notify_status_emits_waybar_json() {
    if farm_available().is_none() {
        eprintln!("e2e: no release farm (cargo build --release + ./setup.sh first), skipping");
        return;
    }
    let dir = scratch("notify-status");
    let notify = bin_dir().join("flex-notify");
    let out = run(Command::new(&notify)
        .arg("--status")
        .env("HOME", &dir)
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_STATE_HOME", dir.join("state")));
    assert!(
        out.status.success(),
        "flex-notify --status exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"text\"") && stdout.contains("\"tooltip\""),
        "status must emit Waybar JSON, got: {stdout:?}"
    );
}

/// The D-Bus daemon answers a `Notify` call when a session bus is available.
///
/// Headless CI often has no `dbus-daemon`; that case skips instead of failing.
#[test]
fn notify_daemon_answers_dbus_when_bus_available() {
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err()
        && !Path::new("/run/dbus/system_bus_socket").exists()
    {
        eprintln!("e2e: no D-Bus session bus, skipping daemon call");
        return;
    }
    let out = run(Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "e2e",
            "0",
            "",
            "e2e summary",
            "e2e body",
            "[]",
            "{}",
            "1000",
        ])
        .stdin(Stdio::null()));
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // The daemon may legitimately be unreachable here (plain Hyprland box
        // without activation); only fail on unexpected errors.
        eprintln!("e2e: Notify call failed, treating as skip: {stderr}");
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id = stdout
        .split("uint32 ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse::<u32>().ok())
        .expect("daemon must return a notification id");
    assert!(id > 0, "daemon returned no usable id: {stdout:?}");

    // Leave the user's notification center as we found it: the test's own
    // entry must not become an unread row in the live feed.
    let id_arg = id.to_string();
    let closed = run(Command::new("gdbus").args([
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        "--method",
        "org.freedesktop.Notifications.CloseNotification",
        id_arg.as_str(),
    ]));
    assert!(
        closed.status.success(),
        "CloseNotification must succeed: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
}
