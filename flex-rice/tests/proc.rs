//! `flex-proc` executor: the `kill(1)` argv for each signal, against a stub
//! `kill` on `PATH`.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flex_core::keys::{handle_key, KeyOutcome};
use flex_core::{run, CharSetName, Peaks, ThemeName};
use flex_rice::exec::proc::{self, Signal};
use flex_rice::runner::{build_menu, Provider, StyleOptions};

/// Unique scratch dir per call (tests run in parallel).
fn scratch(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("flex-proc-{}-{name}-{seq}", std::process::id()));
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

/// Stub `kill` logging its argv to `kill.log`.
fn install_kill_stub(dir: &Path) -> PathBuf {
    let log = dir.join("kill.log");
    write_exe(
        &dir.join("kill"),
        &format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}.tmp'\nmv '{}.tmp' '{}'\n",
            log.display(),
            log.display(),
            log.display(),
        ),
    );
    log
}

fn path_env(dir: &Path) -> String {
    dir.display().to_string()
}

#[test]
fn signal_invokes_kill_with_the_selected_signal() {
    let dir = scratch("signal");
    let log = install_kill_stub(&dir);
    proc::signal("1234", Signal::Term, Some(&path_env(&dir))).expect("signal");
    proc::signal("1234", Signal::Kill, Some(&path_env(&dir))).expect("signal");
    let logged = std::fs::read_to_string(&log).expect("kill log");
    assert_eq!(
        logged.lines().collect::<Vec<_>>(),
        vec!["-s", "KILL", "1234"],
        "the last call is the SIGKILL",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn signal_fails_cleanly_without_kill_on_path() {
    let dir = scratch("missing");
    assert!(
        proc::signal("1", Signal::Kill, Some(&path_env(&dir))).is_err(),
        "a missing kill must error, not silently succeed",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn toggle_stops_a_pid_with_no_readable_state() {
    let dir = scratch("toggle");
    let log = install_kill_stub(&dir);
    // A pid this high cannot exist, so the state read is `None` -> SIGSTOP.
    proc::toggle("4294967290", Some(&path_env(&dir))).expect("toggle");
    let logged = std::fs::read_to_string(&log).expect("kill log");
    assert_eq!(
        logged.lines().collect::<Vec<_>>(),
        vec!["-s", "STOP", "4294967290"],
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_pids_are_rejected_before_any_spawn() {
    let dir = scratch("bad-pid");
    let log = install_kill_stub(&dir);
    assert!(proc::signal("nope", Signal::Term, Some(&path_env(&dir))).is_err());
    assert!(!log.exists(), "an invalid pid must not reach the kill stub");
    let _ = std::fs::remove_dir_all(&dir);
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn rune(c: char) -> KeyEvent {
    press(KeyCode::Char(c))
}

fn default_style() -> StyleOptions {
    StyleOptions {
        filter_mode: flex_core::filter::FilterMode::Spec,
        char_set: CharSetName::Default,
        theme: ThemeName::Default,
        peaks: Peaks::Auto,
        frecency: true,
    }
}

/// The native menu contains rows and arms the danger confirm on
/// the first Enter, selecting (SIGTERM) on the mature second.
#[test]
fn proc_menu_confirms_the_selected_row() {
    let mut menu = build_menu(Provider::Proc, default_style()).expect("proc menu");
    let rows = &menu.app.active_tab().expect("tab").rows;
    assert!(!rows.is_empty(), "proc menu has rows");
    let (index, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.confirmable && r.targets.is_empty())
        .expect("confirmable row without targets exists");
    let row_id = row.id.clone();
    menu.app.active_tab_mut().expect("tab").state.focus = index;

    let t0 = run::test_base();
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Enter), t0),
        KeyOutcome::Consumed,
        "first Enter only arms the danger confirm"
    );
    assert!(menu.app.is_armed());
    assert_eq!(
        handle_key(
            &mut menu,
            press(KeyCode::Enter),
            t0 + std::time::Duration::from_secs(1)
        ),
        KeyOutcome::Select,
        "mature second Enter selects the row"
    );
    assert_eq!(menu.app.focused_row().expect("row").id, row_id);
}

/// The label carries the comm/pid/service, so the engine's filter matches digits/chars.
#[test]
fn proc_filter_matches_labels() {
    let mut menu = build_menu(Provider::Proc, default_style()).expect("proc menu");
    let rows = &menu.app.active_tab().expect("tab").rows;
    assert!(!rows.is_empty(), "proc menu has rows");
    let label = &rows[0].label;
    let query: String = label
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(3)
        .collect();
    let keys: Vec<KeyEvent> = query.chars().map(rune).collect();
    let _ = run::replay_keys(&mut menu, &keys, run::test_base());
    let visible = menu.app.visible_rows();
    assert!(
        !visible.is_empty(),
        "filtering by query keeps matching rows visible"
    );
}

#[test]
fn proc_tab_enables_visible_search_and_deletable() {
    let tab = flex_rice::providers::proc::proc_tab();
    assert!(
        !tab.bare_rows,
        "proc must use standard rows for visible search bar"
    );
    assert!(tab.filterable, "proc tab must be filterable");
    assert!(tab.deletable, "proc tab must be deletable for SIGKILL");
}

#[test]
fn proc_metas_carry_cpu_text_gauges_and_stay_compact() {
    let tab = flex_rice::providers::proc::proc_tab();
    assert!(!tab.rows.is_empty(), "proc menu has rows");
    for row in &tab.rows {
        let meta = row.meta.as_deref().expect("proc row carries a meta");
        assert!(
            meta.contains('%') && meta.contains('[') && meta.contains(']'),
            "wifi-style CPU gauge in meta: {row:?}"
        );
        assert_eq!(
            row.volume, None,
            "gauge is text, row stays compact: {row:?}"
        );
        if row.id.as_str().starts_with("service:") {
            assert!(
                row.hide_target_in_header,
                "aggregate totals own the service header: {row:?}"
            );
        }
    }
}

#[test]
fn service_toggle_expands_and_collapses() {
    let svc = "test-service.service";
    assert!(!flex_rice::providers::proc::is_service_expanded(svc));
    flex_rice::providers::proc::toggle_service_expanded(svc);
    assert!(flex_rice::providers::proc::is_service_expanded(svc));
    flex_rice::providers::proc::toggle_service_expanded(svc);
    assert!(!flex_rice::providers::proc::is_service_expanded(svc));
}
