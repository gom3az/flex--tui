//! `flex shot` cutover tests (M3): provider rows, key-seq replays, shot
//! golden, `FLEX_TEST` determinism, and wrapper pipeline stubs.
//!
//! Row-set parity is against the deleted
//! `rofi/.config/rofi/scripts/screenshot.sh` (see `src/providers/shot.rs`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use flex_core::keys::{handle_key, KeyOutcome, EXIT_CANCELLED};
use flex_core::{backend, run, width, Menu};
use flex_rice::providers::shot;

fn shot_menu() -> Menu {
    flex_rice::menu(shot::PROVIDER, vec![shot::shot_tab()])
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn down() -> KeyEvent {
    press(KeyCode::Down)
}

// --- Provider rows ----------------------------------------------------------

#[test]
fn row_set_matches_bash_cap_rows_exactly() {
    let tab = shot::shot_tab();
    assert_eq!(tab.name, shot::TAB_NAME);
    assert!(tab.bare_rows, "capture uses bare rows");
    assert!(!tab.deletable, "capture rows are non-deletable");
    let rows: Vec<(&str, &str, Option<&str>)> = tab
        .rows
        .iter()
        .map(|row| (row.id.as_str(), row.label.as_str(), row.meta.as_deref()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("area-shot", "Area Screenshot", Some("PNG")),
            ("full-shot", "Full Screenshot", Some("PNG")),
            ("win-shot", "Window Screenshot", Some("PNG")),
            ("area-rec", "Area Recording", Some("MP4")),
            ("full-rec", "Full Recording", Some("MP4")),
        ]
    );
}

#[test]
fn default_focus_is_row_zero() {
    let menu = shot_menu();
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
}

// --- Key-seq replays ----------------------------------------------------------

#[test]
fn keyseq_down_down_enter_selects_window_shot() {
    let mut menu = shot_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[down(), down(), press(KeyCode::Enter)], base);
    assert_eq!(outcome, KeyOutcome::Select);
    let row = menu.app.focused_row().expect("focused row");
    assert_eq!(row.id.as_str(), "win-shot");
    assert_eq!(row.label, "Window Screenshot");
    // The wrapper's ACTION: line for this selection:
    // `ACTION: shot win-shot   Window Screenshot` (exit 0).
    assert_eq!(menu.provider, "shot");
}

#[test]
fn keyseq_down_to_area_recording() {
    // No filter in shot mode: use Down x3 to reach area-rec (index 3).
    let mut menu = shot_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[
            press(KeyCode::Down),
            press(KeyCode::Down),
            press(KeyCode::Down),
            press(KeyCode::Enter),
        ],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Select);
    let row = menu.app.focused_row().expect("focused row");
    assert_eq!(row.id.as_str(), "area-rec");
}

#[test]
fn keyseq_esc_cancels_with_no_action() {
    let mut menu = shot_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Esc)], base);
    assert_eq!(outcome, KeyOutcome::Quit(EXIT_CANCELLED));
}

#[test]
fn delete_never_fires_on_capture_rows() {
    let mut menu = shot_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[press(KeyCode::Delete), press(KeyCode::Delete)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Consumed, "Delete is dead on shot");
    assert!(
        !menu.app.active_state().expect("state").confirm_pending,
        "no confirm arms on non-deletable tabs"
    );
}

// --- Shot golden --------------------------------------------------------------

/// Concatenate non-skip symbols of row `y` (exact cell content, no ANSI).
fn row_text(buf: &ratatui::buffer::Buffer, y: u16, w: u16) -> String {
    let mut out = String::new();
    for x in 0..w {
        let cell = buf.cell((x, y)).expect("cell in frame");
        if !cell.skip {
            out.push_str(cell.symbol());
        }
    }
    out
}

#[test]
fn shot_default_view_golden_at_80x24() {
    let mut menu = shot_menu();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    terminal
        .draw(|frame| flex_core::render::render(frame, &mut menu))
        .expect("render frame");
    let buf = terminal.backend().buffer().clone();
    // Every row is exactly the frame width in display cells.
    for y in 0..24 {
        let mut x = 0_u16;
        let mut total = 0_usize;
        while x < 80 {
            let cell = buf.cell((x, y)).expect("cell in frame");
            assert!(!cell.skip, "dangling skip cell at ({x}, {y})");
            let symbol_w = width::str_width(cell.symbol());
            total += symbol_w;
            x += u16::try_from(symbol_w.max(1)).expect("row width fits u16");
        }
        assert_eq!(total, 80, "row {y} must be exactly 80 cells");
    }
    // Bare mode: list rows start below the reserved `•••` indicator line
    // (`LIST_INDICATOR_ROWS`), no tab bar, no filter, no meta.
    let first_y = flex_core::render::LIST_INDICATOR_ROWS / 2;
    assert!(
        row_text(&buf, first_y, 80).starts_with('░'),
        "row 0 is the first list row"
    );
    let mut all = String::new();
    for y in 0..24 {
        all.push_str(&row_text(&buf, y, 80));
    }
    assert!(!all.contains('›'), "no filter line");
    assert!(!all.contains("PNG"), "bare rows hide meta");
    assert!(all.contains("Area Screenshot"), "area shot row renders");
}

// --- FLEX_TEST seed extension -----------------------------------------------------

#[test]
fn flex_test_replays_are_deterministic_across_bases() {
    assert_ne!(backend::FLEX_TEST_SEED, 0);
    let script = [down(), down(), press(KeyCode::Enter)];
    let run_script = |menu: &mut Menu| {
        let base = run::test_base();
        let mut outcomes = Vec::new();
        for (index, key) in script.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let step = index as u32;
            outcomes.push(handle_key(menu, *key, base + run::FLEX_TEST_STEP * step));
        }
        outcomes
    };
    let mut first = shot_menu();
    let mut second = shot_menu();
    assert_eq!(run_script(&mut first), run_script(&mut second));
    assert_eq!(
        first.app.focused_row().expect("row").id,
        second.app.focused_row().expect("row").id
    );
}

fn stub_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("flex-shot-test-{}-{name}", std::process::id()))
}

// --- Executor (`flex-rice/src/exec/shot.rs`) ----------------------------------
//
// The detached worker (`flex-shot --capture ID FILE REC`) runs the capture
// synchronously with a stub `PATH`: no TUI, no pty, no live tools. The parent
// half (`execute_with`) is covered at the library level with stubbed
// `setsid`/`pgrep`/`pkill`; the popup guard itself lives in the shared
// runner (covered by `popup.rs`), with one binary-level re-exec probe below.

use std::os::unix::fs::PermissionsExt as _;

/// Write an executable stub script.
fn write_exe(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("stub script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Stub `PATH` for the worker: every capture tool logs `name args` to
/// `calls.log`; `grim` and the recording helper also touch their last arg
/// (the output file) so the later `wl-copy < file` redirect succeeds.
struct WorkerStubs {
    dir: std::path::PathBuf,
    log: std::path::PathBuf,
    path_env: String,
}

fn install_worker_stubs(
    name: &str,
    slurp_exit: i32,
    geometry: &str,
    hypr_json: &str,
) -> WorkerStubs {
    let dir = stub_dir(&format!("exec-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stub dir");
    let log = dir.join("calls.log");
    let logged = log.display().to_string();
    write_exe(
        &dir.join("slurp"),
        &format!(
            "#!/usr/bin/env bash\necho \"slurp $@\" >> \"{logged}\"\nprintf '%s' \"{geometry}\"\nexit {slurp_exit}\n"
        ),
    );
    std::fs::write(dir.join("hypr.json"), hypr_json).expect("hypr json");
    write_exe(
        &dir.join("hyprctl"),
        &format!(
            "#!/usr/bin/env bash\necho \"hyprctl $@\" >> \"{logged}\"\ncat \"{}/hypr.json\"\n",
            dir.display()
        ),
    );
    for tool in ["grim", "rec"] {
        write_exe(
            &dir.join(tool),
            &format!(
                "#!/usr/bin/env bash\necho \"{tool} $@\" >> \"{logged}\"\ntouch \"${{@: -1}}\"\n"
            ),
        );
    }
    write_exe(
        &dir.join("wl-copy"),
        &format!(
            "#!/usr/bin/env bash\nbytes=$(wc -c | tr -d ' ')\necho \"wl-copy <$bytes bytes>\" >> \"{logged}\"\n"
        ),
    );
    write_exe(
        &dir.join("notify-send"),
        &format!("#!/usr/bin/env bash\necho \"notify-send $@\" >> \"{logged}\"\n"),
    );
    let path_env = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    WorkerStubs { dir, log, path_env }
}

/// Run the detached worker the way the parent spawns it (minus `setsid`):
/// `flex-shot --capture ID FILE REC` with a stub `PATH`. Recording settings
/// come from the environment (as the parent exports them); the ambient
/// `FLEX_REC_*` are cleared so the matrix is hermetic unless `extra_env`
/// overrides them.
fn run_capture_worker(
    id: &str,
    file: &std::path::Path,
    rec: &std::path::Path,
    stubs: &WorkerStubs,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_flex-shot"));
    command
        .arg("--capture")
        .arg(id)
        .arg(file)
        .arg(rec)
        .env("PATH", &stubs.path_env)
        .env("POPUP_KITTY", "1")
        .env_remove("FLEX_REC_AUDIO")
        .env_remove("FLEX_REC_QUALITY")
        .env_remove("FLEX_REC_FPS")
        .stdin(std::process::Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run capture worker")
}

fn call_lines(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .expect("call log")
        .lines()
        .map(|line| line.trim().to_string())
        .collect()
}

/// Seven ids, one pipeline each: the worker must spawn exactly the wrapper's
/// tool sequence (see `exec::shot::describe` for the snapshot convention).
/// The retired `*-rec-audio` aliases stay executable (audio forced on).
#[test]
fn worker_matrix_runs_the_wrapper_pipeline_per_id() {
    let geometry = "11,22 33x44";
    let hypr_json = "{\"at\":[11,22],\"size\":[33,44]}";
    let ids = [
        "area-shot",
        "full-shot",
        "win-shot",
        "area-rec",
        "area-rec-audio",
        "full-rec",
        "full-rec-audio",
    ];
    for id in ids {
        let stubs = install_worker_stubs(id, 0, geometry, hypr_json);
        let ext = if id.contains("rec") { "mp4" } else { "png" };
        let file = stubs.dir.join(format!("out.{ext}"));
        let rec = stubs.dir.join("rec");
        let output = run_capture_worker(id, &file, &rec, &stubs, &[]);
        assert!(
            output.status.success(),
            "{id}: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(file.is_file(), "{id}: grim/rec stages the output file");
        let lines = call_lines(&stubs.log);
        let painted = file.display().to_string();
        let shot_tail = vec![
            format!("grim -g {geometry} {painted}"),
            String::from("wl-copy <0 bytes>"),
            format!("notify-send -h string:image-path:{painted} Screenshot saved {painted}"),
        ];
        // The `grim`/`rec` stubs log their fixed tool word plus `$@`, so the
        // expected lines name the stub (`rec …`), not the helper path.
        let expected: Vec<String> = match id {
            "area-shot" => {
                let mut rows = vec![String::from("slurp")];
                rows.extend(shot_tail);
                rows
            }
            "full-shot" => {
                let mut rows = vec![format!("grim {painted}")];
                rows.extend(shot_tail.into_iter().skip(1));
                rows
            }
            "win-shot" => {
                let mut rows = vec![String::from("hyprctl -j activewindow")];
                rows.extend(shot_tail);
                rows
            }
            "area-rec" | "area-rec-audio" => vec![
                String::from("slurp"),
                format!("rec -a -g {geometry} {painted}"),
            ],
            "full-rec" | "full-rec-audio" => vec![format!("rec -a {painted}")],
            other => panic!("unexpected id {other}"),
        };
        let _ = std::fs::remove_dir_all(&stubs.dir);
        assert_eq!(lines, expected, "{id}: worker tool sequence");
    }
}

/// Off-default recording settings ride the `RECORDING_START` seam as
/// `--quality`/`--fps` flags (default recordings keep the legacy shape).
#[test]
fn worker_passes_off_default_recording_options_to_the_helper() {
    let geometry = "11,22 33x44";
    let stubs = install_worker_stubs("rec-opts", 0, geometry, "{}");
    let file = stubs.dir.join("out.mp4");
    let rec = stubs.dir.join("rec");
    let output = run_capture_worker(
        "area-rec",
        &file,
        &rec,
        &stubs,
        &[
            ("FLEX_REC_AUDIO", "0"),
            ("FLEX_REC_QUALITY", "high"),
            ("FLEX_REC_FPS", "60"),
        ],
    );
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let painted = file.display().to_string();
    assert_eq!(
        call_lines(&stubs.log),
        vec![
            String::from("slurp"),
            format!("rec --quality high --fps 60 -g {geometry} {painted}"),
        ]
    );
    let _ = std::fs::remove_dir_all(&stubs.dir);
}

/// `slurp` cancel (non-zero exit, no geometry): exit 130 quietly, before
/// `grim` — the deliberate fix over the wrapper's `set +e` fallthrough.
#[test]
fn worker_slurp_cancel_exits_130_quietly() {
    let stubs = install_worker_stubs("cancel", 1, "11,22 33x44", "{}");
    let file = stubs.dir.join("out.png");
    let rec = stubs.dir.join("rec");
    let output = run_capture_worker("area-shot", &file, &rec, &stubs, &[]);
    assert_eq!(output.status.code(), Some(130), "cancel exits 130");
    assert_eq!(output.stdout, b"", "no stdout on cancel");
    assert_eq!(output.stderr, b"", "no diagnostics on cancel");
    assert_eq!(call_lines(&stubs.log), vec![String::from("slurp")]);
    assert!(!file.exists(), "nothing is staged on cancel");
    let _ = std::fs::remove_dir_all(&stubs.dir);
}

/// Unknown ids exit 1 with exactly one `flex: error:` prefix (the runner
/// owns the prefix; executor errors carry none of their own).
#[test]
fn worker_rejects_unknown_ids_with_a_single_prefix() {
    let stubs = install_worker_stubs("bad-id", 0, "11,22 33x44", "{}");
    let file = stubs.dir.join("out.png");
    let rec = stubs.dir.join("rec");
    let output = run_capture_worker("bogus", &file, &rec, &stubs, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(stderr, "flex: error: shot: unknown id 'bogus'\n");
    let _ = std::fs::remove_dir_all(&stubs.dir);
}

/// Parent half at the library level: detach the worker via `setsid -f`,
/// then close the exact popup class — all against stub tools.
#[test]
fn parent_detaches_the_worker_then_closes_the_popup() {
    use flex_rice::exec::shot::{execute_with, ShotId};

    let dir = stub_dir("exec-parent");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stub dir");
    let setsid_log = dir.join("setsid.log");
    let pkill_log = dir.join("pkill.log");
    write_exe(
        &dir.join("setsid"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}'\n",
            setsid_log.display()
        ),
    );
    write_exe(&dir.join("pgrep"), "#!/usr/bin/env bash\nexit 1\n");
    write_exe(
        &dir.join("pkill"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}'\n",
            pkill_log.display()
        ),
    );
    let path_env = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let file = dir.join("Screenshot-ts.png");
    let rec = dir.join("rec.sh");
    let report = execute_with(ShotId::AreaShot, &file, &rec, Some(&path_env))
        .expect("execute with stub PATH");
    assert_eq!(report.filepath, file);
    // The detach is spawned, not waited on: poll for the stub log.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let argv: Vec<String> = loop {
        if let Ok(body) = std::fs::read_to_string(&setsid_log) {
            if body.lines().count() == 6 {
                break body.lines().map(str::to_string).collect();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "setsid spawn never logged"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(
        argv.len(),
        6,
        "setsid -f <exe> --capture ID FILE REC: {argv:?}"
    );
    assert_eq!(argv[0], "-f");
    assert_eq!(
        argv.get(2..4),
        Some(&[String::from("--capture"), String::from("area-shot")][..]),
        "worker subcommand and id follow the exe"
    );
    assert_eq!(
        argv.get(4..),
        Some(&[file.display().to_string(), rec.display().to_string()][..]),
        "worker gets the staged file and the recording helper"
    );
    // `call_lines` trims, which would eat the load-bearing trailing space
    // in the pkill pattern — read this log raw.
    let pkill_argv: Vec<String> = std::fs::read_to_string(&pkill_log)
        .expect("pkill log")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        pkill_argv,
        vec![String::from("-f"), String::from("flex-menu ")],
        "pkill probes the exact popup class with the trailing space"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Outside a popup the binary re-execs into the `menu` popup (the shared
/// runner guard); the kitty spawn is asserted against a stub `PATH`.
#[test]
fn binary_outside_a_popup_reexecs_into_the_menu_popup() {
    let dir = stub_dir("exec-guard");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stub dir");
    let kitty_log = dir.join("kitty.log");
    write_exe(&dir.join("pgrep"), "#!/usr/bin/env bash\nexit 1\n");
    write_exe(
        &dir.join("kitty"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}.tmp'\nmv '{}.tmp' '{}'\n",
            kitty_log.display(),
            kitty_log.display(),
            kitty_log.display(),
        ),
    );
    let path_env = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("scratch HOME");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_flex-shot"))
        .env("PATH", &path_env)
        .env("HOME", &home)
        .env_remove("POPUP_KITTY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-shot outside a popup");
    assert!(
        output.status.success(),
        "toggle exits 0: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let logged = loop {
        if let Ok(body) = std::fs::read_to_string(&kitty_log) {
            break body.lines().map(str::to_string).collect::<Vec<_>>();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "kitty spawn never logged"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(
        logged.contains(&String::from("--class"))
            && logged.contains(&String::from("flex-menu"))
            && logged.contains(&String::from("POPUP_KITTY=1")),
        "re-exec uses the menu popup template: {logged:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Worker-side popup wait: with a popup reported open, `grim` must not run
/// until `pgrep` reports it gone — otherwise a fullscreen shot photographs
/// the flex TUI itself.
#[test]
fn worker_waits_for_popup_gone_before_capture() {
    let dir = stub_dir("exec-popup-wait");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stub dir");
    let log = dir.join("calls.log");
    let logged = log.display().to_string();
    let counted = dir.join("pgrep.count");
    let counted_str = counted.display().to_string();
    // Popup "open" for the first three probes, gone after: the worker polls
    // through them (3 x 50 ms) before touching the screen.
    write_exe(
        &dir.join("pgrep"),
        &format!(
            "#!/usr/bin/env bash\nn=$(cat \"{counted_str}\" 2>/dev/null || echo 0)\necho $((n + 1)) > \"{counted_str}\"\n[ \"$n\" -lt 3 ]\n"
        ),
    );
    write_exe(
        &dir.join("grim"),
        &format!("#!/usr/bin/env bash\necho \"grim $@\" >> \"{logged}\"\ntouch \"${{@: -1}}\"\n"),
    );
    write_exe(
        &dir.join("wl-copy"),
        &format!(
            "#!/usr/bin/env bash\nbytes=$(wc -c | tr -d ' ')\necho \"wl-copy <$bytes bytes>\" >> \"{logged}\"\n"
        ),
    );
    write_exe(
        &dir.join("notify-send"),
        &format!("#!/usr/bin/env bash\necho \"notify-send $@\" >> \"{logged}\"\n"),
    );
    let stubs = WorkerStubs {
        dir,
        log,
        path_env: format!(
            "{}:{}",
            stub_dir("exec-popup-wait").display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    };
    let file = stubs.dir.join("out.png");
    let rec = stubs.dir.join("rec");
    let output = run_capture_worker("full-shot", &file, &rec, &stubs, &[]);
    assert!(
        output.status.success(),
        "full-shot: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probes: usize = std::fs::read_to_string(&counted)
        .expect("pgrep probe count")
        .trim()
        .parse()
        .expect("probe count parses");
    assert_eq!(probes, 4, "worker polls until the popup reports gone");
    let lines = call_lines(&stubs.log);
    assert!(
        lines.iter().any(|line| line.starts_with("grim ")),
        "grim runs, but only after the wait: {lines:?}"
    );
    let _ = std::fs::remove_dir_all(&stubs.dir);
}
