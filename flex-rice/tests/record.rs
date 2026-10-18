//! `flex-record start|status|stop`: the ported recording helpers.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn scratch(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("flex-record-{}-{name}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, body).expect("stub script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Stub `wf-recorder` (logs argv), `pgrep` (status), `notify-send` (logs
/// argv), and `kill`/`killall` (log argv).
fn install_stubs(dir: &Path, pgrep_status: u8) {
    write_exe(
        &dir.join("wf-recorder"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}/rec.log'\n",
            dir.display()
        ),
    );
    write_exe(
        &dir.join("pgrep"),
        &format!("#!/usr/bin/env bash\nexit {pgrep_status}\n"),
    );
    write_exe(
        &dir.join("notify-send"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> '{}/notify.log'\n",
            dir.display()
        ),
    );
    write_exe(
        &dir.join("kill"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> '{}/kill.log'\n",
            dir.display()
        ),
    );
    write_exe(
        &dir.join("killall"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" >> '{}/kill.log'\n",
            dir.display()
        ),
    );
}

fn run(dir: &Path, info: &Path, args: &[&str]) -> Output {
    let path_env = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_flex-record"))
        .args(args)
        .env("HOME", dir)
        .env("PATH", path_env)
        .env("FLEX_RECORD_INFO", info)
        .stdin(Stdio::null())
        .output()
        .expect("run flex-record")
}

#[test]
fn start_spawns_the_recorder_and_registers_it() {
    let dir = scratch("start");
    install_stubs(&dir, 1);
    let info = dir.join("recording.info");
    let file = dir.join("out.mp4");

    let output = run(
        &dir,
        &info,
        &["-a", "-g", "10,10 20x20", file.to_str().expect("utf8")],
    );
    assert!(
        output.status.success(),
        "start exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let rec = dir.join("rec.log");
    // Poll for the full argv, not just the file: `> rec.log` truncates at
    // open, so a reader can observe a partially-written log under load.
    let expected_count = 14;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let logged = loop {
        let logged = std::fs::read_to_string(&rec).unwrap_or_default();
        if logged.lines().count() >= expected_count || std::time::Instant::now() >= deadline {
            break logged;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(
        logged.lines().collect::<Vec<_>>(),
        vec![
            "-c",
            "av1_vaapi",
            "-r",
            "30",
            "-p",
            "b=5M",
            "-p",
            "maxrate=5M",
            "-g",
            "10,10 20x20",
            "--audio-backend=pipewire",
            "-a",
            "-f",
            file.to_str().expect("utf8"),
        ],
    );
    let registered = std::fs::read_to_string(&info).expect("recording.info");
    assert!(
        registered.ends_with(&format!("|{}\n", file.display())),
        "registry carries the file: {registered:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn start_maps_quality_and_fps_to_recorder_params() {
    let dir = scratch("quality");
    install_stubs(&dir, 1);
    let info = dir.join("recording.info");
    let file = dir.join("out.mp4");

    let output = run(
        &dir,
        &info,
        &[
            "--quality",
            "high",
            "--fps=60",
            "-g",
            "10,10 20x20",
            file.to_str().expect("utf8"),
        ],
    );
    assert!(
        output.status.success(),
        "start exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let rec = dir.join("rec.log");
    let expected_count = 12;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let logged = loop {
        let logged = std::fs::read_to_string(&rec).unwrap_or_default();
        if logged.lines().count() >= expected_count || std::time::Instant::now() >= deadline {
            break logged;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(
        logged.lines().collect::<Vec<_>>(),
        vec![
            "-c",
            "av1_vaapi",
            "-r",
            "60",
            "-p",
            "b=10M",
            "-p",
            "maxrate=10M",
            "-g",
            "10,10 20x20",
            "-f",
            file.to_str().expect("utf8"),
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn start_rejects_unknown_quality_and_bad_fps() {
    let dir = scratch("bad-opts");
    install_stubs(&dir, 1);
    let info = dir.join("recording.info");
    let file = dir.join("out.mp4");

    let output = run(
        &dir,
        &info,
        &["--quality", "ultra", file.to_str().expect("utf8")],
    );
    assert!(!output.status.success(), "unknown quality bails");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown quality"),
        "names the bad value: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = run(
        &dir,
        &info,
        &["--fps", "banana", file.to_str().expect("utf8")],
    );
    assert!(!output.status.success(), "bad fps bails");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn status_is_silent_and_exits_1_when_idle() {
    let dir = scratch("status-idle");
    install_stubs(&dir, 1);
    let info = dir.join("recording.info");
    let output = run(&dir, &info, &["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn status_prints_the_badge_when_running() {
    let dir = scratch("status-running");
    install_stubs(&dir, 0);
    let info = dir.join("recording.info");
    let output = run(&dir, &info, &["status"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("REC"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stop_interrupts_the_registered_pid() {
    let dir = scratch("stop-pid");
    install_stubs(&dir, 0);
    let info = dir.join("recording.info");
    std::fs::write(&info, "12345|/walls/out.mp4\n").expect("info");

    let output = run(&dir, &info, &["stop"]);
    assert!(output.status.success());
    assert!(!info.exists(), "registry removed");
    let kill = std::fs::read_to_string(dir.join("kill.log")).expect("kill log");
    assert_eq!(kill.lines().collect::<Vec<_>>(), vec!["-s", "INT", "12345"]);
    let notify = std::fs::read_to_string(dir.join("notify.log")).expect("notify log");
    assert!(notify.contains("/walls/out.mp4"), "saved path: {notify:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stop_without_a_registry_kills_all_recorders() {
    let dir = scratch("stop-all");
    install_stubs(&dir, 0);
    let info = dir.join("recording.info");
    let output = run(&dir, &info, &["stop"]);
    assert!(output.status.success());
    let kill = std::fs::read_to_string(dir.join("kill.log")).expect("kill log");
    assert_eq!(
        kill.lines().collect::<Vec<_>>(),
        vec!["-INT", "wf-recorder"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stop_without_a_registry_and_idle_does_nothing() {
    let dir = scratch("stop-idle");
    install_stubs(&dir, 1); // pgrep exits 1 -> no recorder running
    let info = dir.join("recording.info");
    let output = run(&dir, &info, &["stop"]);
    assert!(output.status.success());
    assert!(!dir.join("kill.log").exists(), "no kill attempted");
    assert!(!dir.join("notify.log").exists(), "no notification sent");
    let _ = std::fs::remove_dir_all(&dir);
}
