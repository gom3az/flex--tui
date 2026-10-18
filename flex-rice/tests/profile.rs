//! `flex profile` tests: provider rows, executor action matrix, dry-run, prefix contracts.

use flex_core::Menu;
use flex_rice::exec::profile as exec_profile;
use flex_rice::providers::profile;

fn profile_menu() -> Menu {
    flex_rice::menu(profile::PROVIDER, vec![profile::profile_tab_from(None)])
}

#[test]
fn row_set_matches_profile_rows() {
    let tab = profile::profile_tab_from(None);
    assert_eq!(tab.name, profile::TAB_NAME);
    assert!(!tab.filterable, "profile tab has no search");
    assert!(!tab.deletable, "profile rows are non-deletable");
    let rows: Vec<(&str, &str, Option<&str>, bool)> = tab
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
            (
                "performance",
                "Performance Profile",
                Some("powerprofilesctl set performance"),
                false
            ),
            (
                "balanced",
                "Balanced Profile",
                Some("powerprofilesctl set balanced"),
                false
            ),
            (
                "power-saver",
                "Power Saver Profile",
                Some("powerprofilesctl set power-saver"),
                false
            ),
        ]
    );

    let active_tab = profile::profile_tab_from(Some("balanced"));
    assert_eq!(active_tab.state.focus, 1);
    assert_eq!(
        active_tab.rows[1].meta.as_deref(),
        Some("powerprofilesctl set balanced  Active")
    );
    assert!(active_tab.rows[1].is_default);
}

#[test]
fn default_focus_is_row_zero() {
    let menu = profile_menu();
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
    assert_eq!(
        menu.app.focused_row().expect("row").id.as_str(),
        "performance"
    );
}

#[test]
fn executor_describe_matches_powerprofilesctl() {
    assert_eq!(
        exec_profile::describe(&exec_profile::Step::PowerprofilesctlSet(
            "performance".to_string()
        )),
        "powerprofilesctl set performance"
    );
    assert_eq!(
        exec_profile::describe(&exec_profile::Step::PowerprofilesctlSet(
            "balanced".to_string()
        )),
        "powerprofilesctl set balanced"
    );
    assert_eq!(
        exec_profile::describe(&exec_profile::Step::PowerprofilesctlSet(
            "power-saver".to_string()
        )),
        "powerprofilesctl set power-saver"
    );
}

#[test]
fn executor_plan_snapshot() {
    let cases = [
        (
            exec_profile::PlannedAction::Performance,
            vec![
                "would run: powerprofilesctl set performance",
                "would run: tuned-adm profile throughput-performance",
            ],
        ),
        (
            exec_profile::PlannedAction::Balanced,
            vec![
                "would run: powerprofilesctl set balanced",
                "would run: tuned-adm profile balanced",
            ],
        ),
        (
            exec_profile::PlannedAction::PowerSaver,
            vec![
                "would run: powerprofilesctl set power-saver",
                "would run: tuned-adm profile powersave",
            ],
        ),
    ];
    for (planned, expected_lines) in cases {
        let steps = exec_profile::plan(&planned);
        let lines: Vec<String> = steps
            .iter()
            .map(|s| format!("would run: {}", exec_profile::describe(s)))
            .collect();
        assert_eq!(lines, expected_lines, "{planned:?}");
    }
}

#[test]
fn executor_rejects_bad_and_unknown_ids_without_its_own_prefix() {
    for (id, expected) in [
        ("", "profile: bad id ''"),
        ("bad/id", "profile: bad id 'bad/id'"),
        ("bad\nid", "profile: bad id 'bad\nid'"),
        ("format", "profile: unknown action: format"),
    ] {
        let err = exec_profile::execute(id, None).expect_err("must fail");
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

#[test]
fn binary_errors_carry_a_single_prefix() {
    let output = std::process::Command::new("setsid")
        .arg(env!("CARGO_BIN_EXE_flex-profile"))
        .env("POPUP_KITTY", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-profile without a pty");
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
