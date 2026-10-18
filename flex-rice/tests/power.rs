//! `flex power` cutover tests (M6): provider rows, danger key-seq replays
//! (the release-gate safety properties), the armed-danger golden,
//! `FLEX_TEST` determinism, and the executor action matrix.
//!
//! Row-set parity is against the deleted
//! `waybar/.config/waybar/power-menu.sh` (see `src/providers/power.rs`).
//! Danger confirm reuses the shared `keys` flow; these replays lock the
//! power surface onto it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use flex_core::keys::{handle_key, KeyOutcome, ARM_CONFIRM_DELAY, ARM_EXPIRE, EXIT_CANCELLED};
use flex_core::{backend, run, width, Menu};
use flex_rice::exec::power as exec_power;
use flex_rice::providers::power;

fn power_menu() -> Menu {
    flex_rice::menu(power::PROVIDER, power::power_tabs())
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn rune(c: char) -> KeyEvent {
    press(KeyCode::Char(c))
}

fn down() -> KeyEvent {
    press(KeyCode::Down)
}

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

// --- Provider rows ----------------------------------------------------------

#[test]
fn row_set_matches_bash_power_rows_exactly() {
    let power = power::power_tab();
    assert_eq!(power.name, power::TAB_POWER);
    assert!(power.bare_rows, "power uses launch-style bare rows");
    assert!(!power.filterable, "power has no search");
    assert!(!power.deletable, "power rows are non-deletable");
    let rows: Vec<(&str, &str, Option<&str>, bool)> = power
        .rows
        .iter()
        .map(|row| {
            (
                row.id.as_str(),
                row.label.as_str(),
                row.meta.as_deref(),
                row.confirmable,
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("lock", "Lock Screen", Some("hyprlock"), false),
            ("suspend", "Suspend", Some("systemctl suspend"), false),
            ("reboot", "Reboot", Some("systemctl reboot"), true),
            ("poweroff", "Power Off", Some("systemctl poweroff"), true),
            ("logout", "Logout", Some("pkill -SIGTERM Hyprland"), false),
        ]
    );

    let profiles = power::profiles_tab_from(None);
    assert_eq!(profiles.name, power::TAB_PROFILES);
    assert!(profiles.bare_rows);
    assert!(!profiles.filterable);
    assert!(!profiles.deletable);
    let prof_rows: Vec<(&str, &str, Option<&str>, bool)> = profiles
        .rows
        .iter()
        .map(|row| {
            (
                row.id.as_str(),
                row.label.as_str(),
                row.meta.as_deref(),
                row.confirmable,
            )
        })
        .collect();
    assert_eq!(
        prof_rows,
        vec![
            (
                "profile:performance",
                "Performance Profile",
                Some("powerprofilesctl set performance"),
                false
            ),
            (
                "profile:balanced",
                "Balanced Profile",
                Some("powerprofilesctl set balanced"),
                false
            ),
            (
                "profile:power-saver",
                "Power Saver Profile",
                Some("powerprofilesctl set power-saver"),
                false
            ),
        ]
    );
}

#[test]
fn default_focus_is_row_zero() {
    let menu = power_menu();
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
    assert_eq!(menu.app.focused_row().expect("row").id.as_str(), "lock");
}

// --- Key-seq replays (danger safety properties) ------------------------------

/// Release-gate safety property: a single `Enter` on a danger row arms
/// but NEVER confirms — for every danger row, from a fresh menu.
#[test]
fn single_enter_on_confirmable_arms_but_never_confirms() {
    for (index, id) in ["reboot", "poweroff"].iter().enumerate() {
        let mut menu = power_menu();
        let t0 = Instant::now();
        for _ in 0..=index + 1 {
            // Focus row `index + 2` (Down from Lock Screen).
            let _ = handle_key(&mut menu, down(), t0);
        }
        assert_eq!(
            menu.app.focused_row().expect("row").id.as_str(),
            *id,
            "focused confirmable row"
        );
        let out = handle_key(&mut menu, press(KeyCode::Enter), t0);
        assert_eq!(out, KeyOutcome::Consumed, "single Enter on {id}");
        assert!(menu.app.is_armed(), "first Enter arms {id}");
    }
}

#[test]
fn second_enter_before_delay_is_swallowed() {
    assert_eq!(ARM_CONFIRM_DELAY, Duration::from_millis(50));
    let mut menu = power_menu();
    let t0 = Instant::now();
    let _ = handle_key(&mut menu, down(), t0);
    let _ = handle_key(&mut menu, down(), t0);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 49));
    assert_eq!(out, KeyOutcome::Consumed, "49 ms Enter is a hold");
    assert!(menu.app.is_armed(), "early Enter stays armed");
}

#[test]
fn second_enter_at_delay_confirms_reboot() {
    let mut menu = power_menu();
    let t0 = Instant::now();
    let _ = handle_key(&mut menu, down(), t0);
    let _ = handle_key(&mut menu, down(), t0);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 50));
    assert_eq!(out, KeyOutcome::Select);
    assert!(!menu.app.is_armed(), "confirm disarms");
    assert_eq!(menu.app.focused_row().expect("row").id.as_str(), "reboot");
}

#[test]
fn arm_expires_after_five_seconds_then_rearms() {
    assert_eq!(ARM_EXPIRE, Duration::from_secs(5));
    let mut menu = power_menu();
    let t0 = Instant::now();
    for _ in 0..3 {
        let _ = handle_key(&mut menu, down(), t0);
    }
    assert_eq!(menu.app.focused_row().expect("row").id.as_str(), "poweroff");
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    // Stale arm + Enter: expiry disarms first, then the key re-arms.
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 5_000));
    assert_eq!(out, KeyOutcome::Consumed, "expired arm never confirms");
    assert!(menu.app.is_armed(), "confirmable Enter re-arms fresh");
    // Tick expiry path.
    assert!(menu.tick(at(t0, 10_000)));
    assert!(!menu.app.is_armed());
}

#[test]
fn other_key_disarms_and_applies() {
    let mut menu = power_menu();
    let t0 = Instant::now();
    let _ = handle_key(&mut menu, down(), t0);
    let _ = handle_key(&mut menu, down(), t0);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 10));
    assert_eq!(out, KeyOutcome::Consumed, "hold swallowed");
    let out = handle_key(&mut menu, down(), at(t0, 20));
    assert_eq!(out, KeyOutcome::Consumed);
    assert!(!menu.app.is_armed(), "Down disarms");
    assert_eq!(menu.app.focused_row().expect("row").id.as_str(), "poweroff");
}

#[test]
fn plain_rows_select_on_first_enter() {
    for (downs, id) in [(0, "lock"), (1, "suspend"), (4, "logout")] {
        let mut menu = power_menu();
        let base = run::test_base();
        let mut keys = vec![down(); downs];
        keys.push(press(KeyCode::Enter));
        let outcome = run::replay_keys(&mut menu, &keys, base);
        assert_eq!(outcome, KeyOutcome::Select, "{id} selects at once");
        assert_eq!(menu.app.focused_row().expect("row").id.as_str(), id);
    }
}

#[test]
fn typing_is_ignored_enter_selects_focused() {
    // Power has no search: typing never filters, so Enter selects the
    // focused row (lock) instead of arming a filtered danger row.
    let mut menu = power_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[rune('r'), rune('e'), rune('b'), press(KeyCode::Enter)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Select, "no filter: Enter selects");
    assert!(!menu.app.is_armed(), "nothing arms without navigation");
    assert_eq!(menu.app.focused_row().expect("row").id.as_str(), "lock");
    assert_eq!(
        menu.app.active_state().expect("state").filter,
        "",
        "filter stays empty"
    );
}

#[test]
fn keyseq_esc_cancels_with_no_action() {
    let mut menu = power_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Esc)], base);
    assert_eq!(outcome, KeyOutcome::Quit(EXIT_CANCELLED));
}

#[test]
fn delete_never_fires_on_power_rows() {
    let mut menu = power_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[press(KeyCode::Delete), press(KeyCode::Delete)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Consumed, "Delete is dead on power");
    assert!(
        !menu.app.active_state().expect("state").confirm_pending,
        "no confirm arms on non-deletable tabs"
    );
}

// --- Power golden -------------------------------------------------------------

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
fn power_default_view_golden_at_80x24() {
    let mut menu = power_menu();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    terminal
        .draw(|frame| flex_core::render::render(frame, &mut menu))
        .expect("render frame");
    let buf = terminal.backend().buffer().clone();
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
    // List rows sit below the reserved `•••` indicator line.
    let first_y = flex_core::render::LIST_INDICATOR_ROWS / 2;
    assert!(
        row_text(&buf, first_y, 80).starts_with('░'),
        "row 0 is the first list row (no tab bar)"
    );
    let first = row_text(&buf, first_y, 80);
    assert!(first.contains("Lock Screen"), "row 0: {first:?}");
    assert!(
        !first.contains("hyprlock"),
        "bare rows hide metas: {first:?}"
    );
    let mut all = String::new();
    for y in 0..24 {
        all.push_str(&row_text(&buf, y, 80));
    }
    assert!(!all.contains('›'), "no filter line");
    assert!(!all.contains("navigate"), "no hints line");
    assert!(all.contains("Reboot"), "Reboot row renders");
}

#[test]
fn armed_confirmable_row_golden_at_80x24() {
    let mut menu = power_menu();
    let t0 = Instant::now();
    // Focus Reboot (Down, Down) and arm with a single Enter.
    let _ = handle_key(&mut menu, down(), t0);
    let _ = handle_key(&mut menu, down(), t0);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Enter), t0),
        KeyOutcome::Consumed
    );
    assert!(menu.app.is_armed());
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    terminal
        .draw(|frame| flex_core::render::render(frame, &mut menu))
        .expect("render frame");
    let buf = terminal.backend().buffer().clone();
    // Armed danger rows show `-- confirm` text. Power rows carry no volume,
    // peaks or config data, so the tab renders flex's compact nodes: one line
    // per entry plus one blank gap row.
    let metrics = flex_core::render::node_metrics(&menu);
    assert_eq!(metrics, flex_core::render::NodeMetrics::COMPACT);
    let header = flex_core::render::LIST_INDICATOR_ROWS / 2 + 2 * metrics.pitch();
    let armed = row_text(&buf, header, 80);
    assert!(armed.contains("confirm"), "armed context: {armed:?}");
    // Compact nodes show only `selector_top` on their single line.
    let selector = buf.cell((0, header)).expect("selector cell");
    assert_eq!(selector.symbol(), "░");
    assert_eq!(
        selector.fg,
        flex_core::theme::Theme::DEFAULT
            .selector
            .fg
            .expect("selector sets a fg")
    );
    let gap = buf.cell((0, header + 1)).expect("gap cell");
    assert_eq!(gap.symbol(), " ", "the gap row carries nothing");
}

// --- FLEX_TEST determinism ----------------------------------------------------

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
    let mut first = power_menu();
    let mut second = power_menu();
    assert_eq!(run_script(&mut first), run_script(&mut second));
    assert_eq!(
        first.app.focused_row().expect("row").id,
        second.app.focused_row().expect("row").id
    );
}

// --- exec::power tests -------------------------------------------------------

/// Serialises every test that mutates process env (`DRY_RUN`, the `STUB_LOG`
/// seam): the executor tests read them in-process, so a leaked value would
/// race a sibling test (the harness runs tests in parallel). Wrapper tests
/// spawn `bash` with an explicit env and stay lock-free.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Save/restore process env around an executor call (see [`ENV_LOCK`]).
struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn set(pairs: &[(&str, &str)]) -> Self {
        let mut saved = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            saved.push(((*key).to_string(), std::env::var(key).ok()));
            std::env::set_var(key, value);
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in self.saved.drain(..) {
            match prev {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
    }
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Executor stub dir (distinct prefix from the wrapper's [`stub_dir`]).
fn exec_stub_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flex-power-exec-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stub dir");
    dir
}

/// Per-tool logging stub: `basename` + args to `$STUB_LOG`, always exit 0
/// (the retired port's `LOG_STUB`, so wrapper and executor call logs compare
/// byte-for-byte).
const LOG_STUB: &str = r#"#!/usr/bin/env bash
printf '%s %s\n' "$(basename "$0")" "$*" >> "$STUB_LOG"
exit 0
"#;

fn install_log_stubs(dir: &Path) {
    for tool in ["hyprlock", "systemctl", "pkill", "powerprofilesctl"] {
        write_exe(&dir.join(tool), LOG_STUB);
    }
}

/// `PATH` shadow for executor calls: stub dir first, ambient `PATH` after
/// (the stubs are `bash` scripts, so they still need real `basename`).
fn exec_path_env(dir: &Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn read_exec_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

/// Full action matrix: `describe` produces byte-exact command
/// strings matching the wrapper's `would run:` output.
#[test]
fn executor_describe_matches_wrapper_commands() {
    assert_eq!(
        exec_power::describe(&exec_power::Step::Hyprlock),
        "hyprlock"
    );
    assert_eq!(
        exec_power::describe(&exec_power::Step::SystemctlSuspend),
        "systemctl suspend"
    );
    assert_eq!(
        exec_power::describe(&exec_power::Step::SystemctlReboot),
        "systemctl reboot"
    );
    assert_eq!(
        exec_power::describe(&exec_power::Step::SystemctlPoweroff),
        "systemctl poweroff"
    );
    assert_eq!(
        exec_power::describe(&exec_power::Step::PkillHyprland),
        "pkill -SIGTERM Hyprland"
    );
}

/// Full action matrix: `plan` produces the right steps and
/// `describe_plan` matches the wrapper's would-run lines.
#[test]
fn executor_plan_snapshot_matches_wrapper() {
    let cases = [
        (exec_power::PlannedAction::Lock, vec!["would run: hyprlock"]),
        (
            exec_power::PlannedAction::Suspend,
            vec!["would run: systemctl suspend"],
        ),
        (
            exec_power::PlannedAction::Reboot,
            vec!["would run: systemctl reboot"],
        ),
        (
            exec_power::PlannedAction::Off,
            vec!["would run: systemctl poweroff"],
        ),
        (
            exec_power::PlannedAction::Logout,
            vec!["would run: pkill -SIGTERM Hyprland"],
        ),
    ];
    for (planned, expected_lines) in cases {
        let steps = exec_power::plan(&planned);
        let lines: Vec<String> = steps
            .iter()
            .map(|s| format!("would run: {}", exec_power::describe(s)))
            .collect();
        assert_eq!(lines, expected_lines, "{planned:?}");
    }
}

/// Full action matrix through one shared call log: every action id really
/// dispatches its tool, and the call sequence matches the wrapper's shapes
/// (one line per `describe` step).
#[test]
fn executor_matrix_runs_every_action_through_stub_tools() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let dir = exec_stub_dir("matrix");
    install_log_stubs(&dir);
    let log = dir.join("calls.log");
    let _guard = EnvGuard::set(&[("DRY_RUN", "0"), ("STUB_LOG", log.to_str().expect("utf8"))]);
    let path_env = exec_path_env(&dir);
    for (id, detail) in [
        ("lock", "lock"),
        ("suspend", "suspend"),
        ("reboot", "reboot"),
        ("poweroff", "poweroff"),
        ("logout", "logout"),
    ] {
        let report = exec_power::execute(id, Some(&path_env)).expect("executor succeeds");
        assert_eq!(report.action_id, id);
        assert_eq!(report.detail, detail);
        assert_eq!(report.dry_run, None, "{id} really ran");
    }
    assert_eq!(
        read_exec_log(&dir).lines().collect::<Vec<_>>(),
        vec![
            // `hyprlock` takes no args; `LOG_STUB` joins name + space + args.
            "hyprlock ",
            "systemctl suspend",
            "systemctl reboot",
            "systemctl poweroff",
            "pkill -SIGTERM Hyprland",
        ],
        "one tool call per action (describe: one line per step)",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Blast-radius gate: every action id dry-runs to the wrapper's exact
/// `would run: …` line, returns it as the effect, and spawns nothing.
#[test]
fn executor_dry_run_emits_the_wrapper_lines_and_spawns_nothing() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let dir = exec_stub_dir("dry");
    install_log_stubs(&dir);
    let log = dir.join("calls.log");
    let _guard = EnvGuard::set(&[("DRY_RUN", "1"), ("STUB_LOG", log.to_str().expect("utf8"))]);
    let path_env = exec_path_env(&dir);
    for (id, expected) in [
        ("lock", "would run: hyprlock"),
        ("suspend", "would run: systemctl suspend"),
        ("reboot", "would run: systemctl reboot"),
        ("poweroff", "would run: systemctl poweroff"),
        ("logout", "would run: pkill -SIGTERM Hyprland"),
    ] {
        let report = exec_power::execute(id, Some(&path_env)).expect("dry run succeeds");
        assert_eq!(report.dry_run, Some(vec![expected.to_string()]), "{id}");
        assert_eq!(report.detail, id);
    }
    assert!(!log.exists(), "dry-run spawns nothing for any id");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Danger-row dry-run verification: Reboot and Poweroff return their effect
/// and never spawn any tool — the stub log stays unwritten.
#[test]
fn executor_dry_run_danger_rows_never_spawn() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let dir = exec_stub_dir("danger-rows");
    install_log_stubs(&dir);
    let log = dir.join("calls.log");
    let _guard = EnvGuard::set(&[("DRY_RUN", "1"), ("STUB_LOG", log.to_str().expect("utf8"))]);
    let path_env = exec_path_env(&dir);
    for (id, expected) in [
        ("reboot", "would run: systemctl reboot"),
        ("poweroff", "would run: systemctl poweroff"),
    ] {
        let report = exec_power::execute(id, Some(&path_env)).expect("danger dry-run succeeds");
        assert_eq!(report.dry_run, Some(vec![expected.to_string()]), "{id}");
    }
    assert!(!log.exists(), "danger dry-run executes nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed ids (`bad id`, the wrapper's guard) and well-formed but
/// unsupported ids (`unknown action`, the wrapper's `*)` arm) are errors with
/// no `flex:` prefix of their own — the runner adds the single prefix at the
/// binary boundary (see `tests/prefix.rs`).
#[test]
fn executor_rejects_bad_and_unknown_ids_without_its_own_prefix() {
    for (id, expected) in [
        ("", "power: bad id ''"),
        ("bad/id", "power: bad id 'bad/id'"),
        ("bad\nid", "power: bad id 'bad\nid'"),
        ("format", "power: unknown action: format"),
    ] {
        let err = exec_power::execute(id, None).expect_err("must fail");
        let message = format!("{err:#}");
        assert!(
            message.starts_with(expected),
            "{id:?}: {message:?} must start with {expected:?}"
        );
        assert!(
            !message.contains("flex:"),
            "{id:?}: no prefix of its own: {message:?}"
        );
    }
}

/// `execute` accepts every valid action id (the parse path; the dispatch
/// matrix above proves the tools actually run).
#[test]
fn executor_valid_ids_parse_correctly() {
    for id in ["lock", "suspend", "reboot", "poweroff", "logout"] {
        assert!(exec_power::PowerAction::parse(id).is_some(), "{id} parses");
    }
}

/// `is_dry_run` is dry only for the exact value `1` (the wrapper's
/// `[[ "$dry_run" == "1" ]]`), so `0`/empty/unset execute.
#[test]
fn executor_dry_run_flag_is_exactly_one() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let _guard = EnvGuard::set(&[("DRY_RUN", "1")]);
    assert!(exec_power::is_dry_run(), "DRY_RUN=1 is dry");
    std::env::set_var("DRY_RUN", "0");
    assert!(!exec_power::is_dry_run(), "DRY_RUN=0 is not dry");
    std::env::set_var("DRY_RUN", "");
    assert!(!exec_power::is_dry_run(), "DRY_RUN='' is not dry");
    std::env::remove_var("DRY_RUN");
    assert!(!exec_power::is_dry_run(), "unset DRY_RUN is not dry");
}

// --- Binary tests (no pty) ---------------------------------------------------

/// Binary level without a pty: the menu cannot start, so the binary exits 1
/// with exactly one `flex: error:` prefix (the runner owns it; executor
/// errors carry none). `setsid` detaches the controlling terminal so no
/// harness tty can satisfy the TUI init.
#[test]
fn binary_errors_carry_a_single_prefix() {
    let output = std::process::Command::new("setsid")
        .arg(env!("CARGO_BIN_EXE_flex-power"))
        .env("POPUP_KITTY", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-power without a pty");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"", "no stdout on error");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.starts_with("flex: error: "),
        "runner prefix first: {stderr:?}"
    );
    assert_eq!(
        stderr.matches("flex: error:").count(),
        1,
        "exactly one prefix: {stderr:?}"
    );
}

/// Outside a popup the binary re-execs into the `menu` popup (the shared
/// runner guard); the kitty spawn is asserted against a stub `PATH`.
#[test]
fn binary_outside_a_popup_reexecs_into_the_menu_popup() {
    let dir = exec_stub_dir("guard");
    let home = exec_stub_dir("guard-home");
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
    let path_env = exec_path_env(&dir);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_flex-power"))
        .env("PATH", &path_env)
        .env("HOME", &home)
        .env_remove("POPUP_KITTY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-power outside a popup");
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
    let _ = std::fs::remove_dir_all(&home);
}
