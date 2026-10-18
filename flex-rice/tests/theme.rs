//! `flex theme` cutover tests (M3): provider rows, key-seq replays, theme
//! golden, `FLEX_TEST` determinism, and the executor matrix.
//!
//! Parity is against the `pick()` path of
//! `scripts/.config/scripts/theme-switcher.sh` (only that path is cut
//! over; `list`/`current`/`activate`/`delete` stay in bash — see
//! `src/providers/theme_.rs`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use flex_core::keys::{handle_key, KeyOutcome, EXIT_CANCELLED};
use flex_core::{backend, run, width, Menu};
use flex_rice::providers::theme_;

fn fixture_entries() -> Vec<theme_::ThemeEntry> {
    vec![
        theme_::ThemeEntry {
            name: "catppuccin-mocha".to_string(),
            wallpaper: "mocha-wall.png".to_string(),
            active: true,
        },
        theme_::ThemeEntry {
            name: "tokyo-night".to_string(),
            wallpaper: "tokyo.jpg".to_string(),
            active: false,
        },
        theme_::ThemeEntry {
            name: "bare".to_string(),
            wallpaper: "(no metadata)".to_string(),
            active: false,
        },
    ]
}

fn fixture_menu() -> Menu {
    flex_rice::menu(
        theme_::PROVIDER,
        vec![theme_::tab_from_entries(&fixture_entries())],
    )
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

// --- Provider fixtures ------------------------------------------------------

#[test]
fn rows_carry_theme_names_wallpapers_and_active_marker() {
    let tab = theme_::tab_from_entries(&fixture_entries());
    assert_eq!(tab.name, theme_::TAB_NAME);
    assert!(tab.bare_rows, "themes use bare rows");
    assert!(!tab.deletable, "theme rows are non-deletable");
    let labels: Vec<(&str, Option<&str>)> = tab
        .rows
        .iter()
        .map(|row| (row.label.as_str(), row.meta.as_deref()))
        .collect();
    assert_eq!(
        labels,
        vec![
            ("catppuccin-mocha", Some("mocha-wall.png  Active")),
            ("tokyo-night", Some("tokyo.jpg")),
            ("bare", Some("(no metadata)")),
        ]
    );
    // The id is a space-free hash of the theme name (B-021), and every row
    // id resolves back to its own label through the same directory scan
    // the theme executor performs before calling `activate`.
    let available = scratch("row-ids");
    for row in &tab.rows {
        std::fs::create_dir_all(available.join(row.label.as_str())).expect("theme dir");
    }
    for row in &tab.rows {
        assert!(
            !row.id.as_str().contains(char::is_whitespace),
            "id is a single token: {:?}",
            row.id.as_str()
        );
        assert_eq!(
            theme_::resolve_name_in(&available, row.id.as_str()).as_deref(),
            Some(row.label.as_str()),
            "row id resolves back to the theme name"
        );
    }
    std::fs::remove_dir_all(&available).expect("cleanup");
}

/// Unique scratch dir per call (tests run in parallel; B-007/B-013).
fn scratch(name: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "flex-theme-test-{}-{name}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn scan_reads_sorted_dirs_and_current_marker() {
    let root = std::env::temp_dir().join(format!("flex-theme-test-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let available = root.join("available");
    let current = root.join("current");
    for theme in ["zulu", "alpha"] {
        let dir = available.join(theme);
        std::fs::create_dir_all(&dir).expect("theme dir");
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"theme_name\": \"{theme}\", \"wallpaper\": \"/walls/{theme}.png\"}}"),
        )
        .expect("metadata");
    }
    // Theme without metadata degrades to the bash fallback label.
    std::fs::create_dir_all(available.join("plain")).expect("plain dir");
    std::fs::create_dir_all(&current).expect("current dir");
    std::fs::write(current.join("metadata.json"), "{\"theme_name\": \"alpha\"}").expect("current");
    let entries = theme_::scan_available(&available, &theme_::current_name_in(&current));
    let _ = std::fs::remove_dir_all(&root);
    let rows: Vec<(&str, &str, bool)> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.wallpaper.as_str(), entry.active))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("alpha", "alpha.png", true),
            ("plain", "(no metadata)", false),
            ("zulu", "zulu.png", false),
        ],
        "sorted dirs (bash find|sort), wallpaper basenames, active mark"
    );
}

#[test]
fn missing_dirs_yield_no_rows_without_panicking() {
    let missing =
        std::env::temp_dir().join(format!("flex-theme-test-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);
    assert_eq!(theme_::scan_available(&missing, ""), []);
    assert_eq!(theme_::current_name_in(&missing), "");
}

#[test]
fn default_focus_is_row_zero() {
    let menu = fixture_menu();
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
}

// --- Key-seq replays ----------------------------------------------------------

#[test]
fn keyseq_down_enter_selects_second_theme() {
    let mut menu = fixture_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[press(KeyCode::Down), press(KeyCode::Enter)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Select);
    let row = menu.app.focused_row().expect("focused row");
    assert_eq!(row.label, "tokyo-night");
    assert!(
        !row.id.as_str().contains(char::is_whitespace),
        "row id is a single hash token: {:?}",
        row.id.as_str()
    );
    assert_ne!(row.id.as_str(), row.label, "the name is not the id (B-021)");
    // The retired wrapper's ACTION: line for this selection was
    // `ACTION: theme <row-hash> tokyo-night` (exit 0); the theme executor
    // resolves the hash in-process via the library resolver before
    // activating.
    assert_eq!(menu.provider, "theme");
}

#[test]
fn keyseq_filter_then_enter_selects_active_theme() {
    let mut menu = fixture_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[
            press(KeyCode::Char('c')),
            press(KeyCode::Char('a')),
            press(KeyCode::Char('t')),
            press(KeyCode::Enter),
        ],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Select);
    let row = menu.app.focused_row().expect("focused row");
    assert_eq!(row.label, "catppuccin-mocha");
    assert_ne!(row.id.as_str(), row.label, "the name is not the id (B-021)");
}

#[test]
fn keyseq_esc_cancels_with_no_action() {
    let mut menu = fixture_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Esc)], base);
    assert_eq!(outcome, KeyOutcome::Quit(EXIT_CANCELLED));
}

#[test]
fn delete_never_fires_on_theme_rows() {
    let mut menu = fixture_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[press(KeyCode::Delete), press(KeyCode::Delete)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Consumed, "Delete is dead on theme");
    assert!(
        !menu.app.active_state().expect("state").confirm_pending,
        "no confirm arms on non-deletable tabs"
    );
}

// --- Theme golden --------------------------------------------------------------

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
fn theme_default_view_golden_at_80x24() {
    let mut menu = fixture_menu();
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
    // With top filter chrome: y=0 margin, y=1 filter, y=2 separator, y=3 indicator, y=4 first entry
    let first_y = 3 + flex_core::render::LIST_INDICATOR_ROWS / 2;
    assert!(
        row_text(&buf, first_y, 80).starts_with('░'),
        "row 4 is the first list row"
    );
    let mut all = String::new();
    for y in 0..24 {
        all.push_str(&row_text(&buf, y, 80));
    }
    assert!(all.contains('›'), "filter line rendered");
    assert!(
        all.contains("catppuccin-mocha"),
        "theme label visible: {all:?}"
    );
}

// --- FLEX_TEST seed extension -----------------------------------------------------

#[test]
fn flex_test_replays_are_deterministic_across_bases() {
    assert_ne!(backend::FLEX_TEST_SEED, 0);
    let script = [press(KeyCode::Down), press(KeyCode::Enter)];
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
    let mut first = fixture_menu();
    let mut second = fixture_menu();
    assert_eq!(run_script(&mut first), run_script(&mut second));
    assert_eq!(
        first.app.focused_row().expect("row").id,
        second.app.focused_row().expect("row").id
    );
}

// --- Empty-scan placeholder (B-026) -------------------------------------------

/// Same policy as `launch`: an empty theme directory shows a `noop`
/// placeholder row instead of a blank menu.
#[test]
fn empty_scan_shows_the_noop_placeholder() {
    let tab = theme_::tab_from_entries(&[]);
    assert_eq!(tab.name, theme_::TAB_NAME);
    assert_eq!(tab.rows.len(), 1, "placeholder instead of a blank menu");
    assert_eq!(tab.rows[0].id.as_str(), flex_rice::providers::NOOP_ID);
    assert_eq!(tab.rows[0].label, theme_::NO_THEMES_LABEL);

    let mut menu = flex_rice::menu(theme_::PROVIDER, vec![tab]);
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Enter)], run::test_base());
    assert_eq!(outcome, KeyOutcome::Select);
    assert_eq!(
        menu.app.focused_row().expect("focused row").id.as_str(),
        flex_rice::providers::NOOP_ID
    );
}

// --- Executor (`flex-rice/src/exec/theme.rs`) ----------------------------------
//
// The theme port of the `exec::shot` template: `ThemeAction::parse` validates
// the id, `execute` resolves the hash in-process via the library resolver
// and runs `<switcher> activate <name>` through the
// `THEME_SWITCHER` seam. No TUI, no pty: stub `PATH` + scratch `HOME`.
//
// Like the shot suite, the popup guard itself lives in the shared runner
// (covered by `popup.rs`), with one binary-level re-exec probe below.

/// Write an executable stub script.
fn write_exe(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("stub script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Save/restore one process env var around an executor call (tests run in
/// parallel; only the theme executor reads these two keys).
struct EnvGuard {
    key: &'static str,
    old: Option<String>,
}

/// Serialises the executor tests below: each mutates `HOME`/`THEME_SWITCHER`
/// (the seams `execute` reads) and the harness runs tests in parallel, so
/// the mutations are held under one lock with restores in [`EnvGuard`].
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn set_env(key: &'static str, value: &std::path::Path) -> EnvGuard {
    let old = std::env::var(key).ok();
    std::env::set_var(key, value);
    EnvGuard { key, old }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Scratch `HOME` with `available/<theme>/` dirs (one with metadata, one
/// space-bearing per B-021, one bare).
fn install_theme_home(tag: &str) -> std::path::PathBuf {
    let home = scratch(&format!("exec-home-{tag}"));
    let available = home.join(".config/themes/available");
    for theme in ["alpha", "Tokyo Night", "plain"] {
        std::fs::create_dir_all(available.join(theme)).expect("theme dir");
    }
    std::fs::write(
        available.join("alpha").join("metadata.json"),
        "{\"theme_name\": \"alpha\", \"wallpaper\": \"/walls/alpha.png\"}",
    )
    .expect("metadata");
    home
}

/// `THEME_SWITCHER` stub logging every argv element bracketed (the existing
/// dispatch idiom: a split name cannot masquerade as one argument).
fn install_switcher(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let switcher = dir.join(name);
    let log = dir.join(format!("{name}.log"));
    write_exe(
        &switcher,
        &format!(
            "#!/usr/bin/env bash\n{{ for arg in \"$@\"; do printf '[%s]' \"$arg\"; done; printf '\\n'; }} >> \"{}\"\n",
            log.display()
        ),
    );
    switcher
}

fn stub_path_env(dir: &std::path::Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Full action-id matrix: every theme (including the space-bearing B-021
/// name) activates through the switcher seam as one `activate` call.
#[test]
fn executor_matrix_activates_every_theme_through_the_switcher_seam() {
    let _env = ENV_LOCK.lock().expect("env lock");
    let dir = scratch("exec-matrix");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let home = install_theme_home("matrix");
    let switcher = install_switcher(&dir, "theme-switcher.sh");
    let _home = set_env("HOME", &home);
    let _switcher_env = set_env("THEME_SWITCHER", &switcher);
    let path_env = stub_path_env(&dir);
    for name in ["alpha", "Tokyo Night", "plain"] {
        let id = theme_::entry_id(name);
        let report =
            flex_rice::exec::theme::execute(&id, Some(&path_env)).expect("activate succeeds");
        assert_eq!(report.name.as_deref(), Some(name));
        assert_eq!(report.switcher.as_deref(), Some(switcher.as_path()));
    }
    let log = std::fs::read_to_string(dir.join("theme-switcher.sh.log")).expect("call log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        vec![
            "[activate][alpha]",
            "[activate][Tokyo Night]",
            "[activate][plain]",
        ],
        "one activate call per id, names intact (describe: `<switcher> activate <name>`)",
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// The `noop` placeholder exits 0 before resolving or calling (B-026): even
/// a missing `HOME` cannot fail it, like the wrapper's early exit.
#[test]
fn executor_noop_short_circuits_before_the_switcher() {
    let _env = ENV_LOCK.lock().expect("env lock");
    let dir = scratch("exec-noop");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let missing_home = dir.join("no-such-home");
    let switcher = install_switcher(&dir, "theme-switcher.sh");
    let _home = set_env("HOME", &missing_home);
    let _switcher_env = set_env("THEME_SWITCHER", &switcher);
    let report =
        flex_rice::exec::theme::execute("noop", Some(&stub_path_env(&dir))).expect("noop is Ok");
    assert_eq!(
        report,
        flex_rice::exec::theme::ExecuteReport {
            switcher: None,
            name: None,
        }
    );
    assert!(
        !dir.join("theme-switcher.sh.log").exists(),
        "theme-switcher.sh is never called"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed ids (`bad id`, mirroring the wrapper check) and well-formed but
/// unresolvable hashes (`unknown id`) are errors with no `flex:` prefix of
/// their own — the runner adds the single prefix at the binary boundary.
#[test]
fn executor_rejects_bad_and_unknown_ids_without_its_own_prefix() {
    let _env = ENV_LOCK.lock().expect("env lock");
    let dir = scratch("exec-bad");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let home = install_theme_home("bad");
    let switcher = install_switcher(&dir, "theme-switcher.sh");
    let _home = set_env("HOME", &home);
    let _switcher_env = set_env("THEME_SWITCHER", &switcher);
    let path_env = stub_path_env(&dir);
    let ghost_id = theme_::entry_id("ghost");
    let cases = [
        ("", "theme: bad id ''"),
        ("a/b", "theme: bad id 'a/b'"),
        ("a\nb", "theme: bad id 'a\nb'"),
        (ghost_id.as_str(), "theme: unknown id"),
    ];
    for (id, expected) in cases {
        let err = flex_rice::exec::theme::execute(id, Some(&path_env))
            .expect_err("bad/unknown id must fail");
        let message = format!("{err:#}");
        assert!(
            message.starts_with(expected),
            "unexpected message for {id:?}: {message:?}"
        );
        assert!(
            !message.contains("flex:"),
            "no runner prefix below the runner: {message:?}"
        );
    }
    assert!(
        !dir.join("theme-switcher.sh.log").exists(),
        "no id failure reaches the switcher"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// A failing switcher is a loud error, never a quiet cancel: theme has no
/// `slurp`-like user-cancellable step, so every tool failure exits non-zero
/// through the runner (the menu-cancel 130 lives in the binary's
/// `Outcome::Cancelled` arm, shared with shot, and needs a pty).
#[test]
fn executor_tool_failure_is_a_loud_error_not_a_quiet_cancel() {
    let _env = ENV_LOCK.lock().expect("env lock");
    let dir = scratch("exec-fail");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let home = install_theme_home("fail");
    let switcher = dir.join("theme-switcher.sh");
    write_exe(&switcher, "#!/usr/bin/env bash\nexit 3\n");
    let _home = set_env("HOME", &home);
    let _switcher_env = set_env("THEME_SWITCHER", &switcher);
    let id = theme_::entry_id("alpha");
    let err = flex_rice::exec::theme::execute(&id, Some(&stub_path_env(&dir)))
        .expect_err("a failing switcher must fail");
    assert_eq!(format!("{err:#}"), "theme: activate alpha failed");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// Native activation (no `THEME_SWITCHER`): copies the theme files, refreshes
/// the compat/config symlinks, updates `theme_name`, and best-effort reloads.
#[test]
fn native_activation_copies_links_symlinks_and_reloads() {
    let dir = scratch("native-stubs");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let home = scratch("native-home");
    let available = home.join(".config/themes/available/demo");
    std::fs::create_dir_all(&available).expect("theme dir");
    let sources = [
        ("theme.css", "body { color: red; }\n"),
        ("theme.lua", "return {}\n"),
        ("kitty.conf", "foreground #fff\n"),
        ("yazi.toml", "theme = 'demo'\n"),
        ("tmux-colors.conf", "set -g status-bg red\n"),
        ("nvim-colors.lua", "return {}\n"),
        ("nvim-hl.lua", "return {}\n"),
    ];
    for (name, body) in sources {
        std::fs::write(available.join(name), body).expect("theme source");
    }
    std::fs::write(
        available.join("metadata.json"),
        "{\n    \"theme_name\": \"old\",\n    \"wallpaper\": \"/walls/old.png\"\n}\n",
    )
    .expect("metadata");
    std::fs::write(available.join("extra.txt"), "extra\n").expect("extra file");

    let log = dir.join("calls.log");
    let body = format!(
        "#!/usr/bin/env bash\nprintf '%s %s\\n' \"$(basename \"$0\")\" \"$*\" >> \"{}\"\nexit 0\n",
        log.display()
    );
    for tool in ["pgrep", "pkill", "hyprctl", "killall", "notify-send"] {
        write_exe(&dir.join(tool), &body);
    }
    let path_env = stub_path_env(&dir);

    flex_rice::exec::theme::activate_theme_native_in("demo", &home, &path_env)
        .expect("native activation succeeds");

    let current = home.join(".config/themes/current");
    assert_eq!(
        std::fs::read(current.join("theme.css")).expect("copied css"),
        std::fs::read(available.join("theme.css")).expect("source css"),
    );
    assert!(current.join("extra.txt").is_file(), "extra file copied");
    assert_eq!(
        std::fs::read_link(current.join("colors.css")).expect("colors.css symlink"),
        std::path::Path::new("theme.css"),
        "compat symlink is relative",
    );
    let metadata = std::fs::read_to_string(current.join("metadata.json")).expect("metadata");
    assert!(
        metadata.contains("/walls/old.png"),
        "other key retained: {metadata:?}"
    );
    assert_eq!(
        theme_::current_name_in(&current),
        "demo",
        "theme_name updated"
    );

    for (target, source) in [
        (".config/waybar/theme.css", "theme.css"),
        (".config/hypr/theme.lua", "theme.lua"),
        (".config/kitty/current-theme.conf", "kitty.conf"),
        (".config/yazi/theme.toml", "yazi.toml"),
        (".config/tmux/tmux-colors.conf", "tmux-colors.conf"),
        (".config/nvim/lua/theme.lua", "nvim-colors.lua"),
        (".config/nvim/lua/nvim-hl.lua", "nvim-hl.lua"),
    ] {
        let target = home.join(target);
        assert!(target.is_symlink(), "{target:?} is a symlink");
        assert_eq!(
            std::fs::read_link(&target).expect("read link"),
            current.join(source),
            "{target:?} points at current/{source}",
        );
        assert!(target.exists(), "{target:?} resolves");
    }

    let logged = std::fs::read_to_string(&log).expect("stub call log");
    for line in [
        "pkill -SIGUSR2 waybar",
        "hyprctl reload",
        "killall -SIGUSR1 kitty",
        "notify-send -a Theme Switcher -i preferences-desktop-color Theme Activated Switched to: demo",
    ] {
        assert!(logged.contains(line), "missing {line:?} in {logged:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// Binary level without a pty: the menu cannot start, so the binary exits 1
/// with exactly one `flex: error:` prefix (the runner owns it; executor
/// errors carry none). `setsid` detaches the controlling terminal so no
/// harness tty can satisfy the TUI init.
#[test]
fn binary_errors_carry_a_single_prefix() {
    let output = std::process::Command::new("setsid")
        .arg(env!("CARGO_BIN_EXE_flex-theme"))
        .env("POPUP_KITTY", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-theme without a pty");
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
    let dir = scratch("exec-guard");
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
    let path_env = stub_path_env(&dir);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("scratch HOME");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_flex-theme"))
        .env("PATH", &path_env)
        .env("HOME", &home)
        .env_remove("POPUP_KITTY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-theme outside a popup");
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
