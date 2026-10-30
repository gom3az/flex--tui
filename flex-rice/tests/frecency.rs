//! Frecency wiring: the store file reaches the ranked view, and fixed tabs
//! ignore it.
//!
//! The `proc` menu supplies live filterable rows (ids picked dynamically,
//! so the test is hermetic); a synthetic two-tab menu proves the per-tab
//! gate in both directions.
//!
//! Fixture paths travel in process env (`FLEX_USAGE_FILE`), so the live
//! phases run sequentially inside one test — parallel tests must not share
//! process env.

use flex_core::{CharSetName, Menu, Peaks, Row, RowId, Tab, ThemeName};
use flex_rice::runner::{build_menu, Provider, StyleOptions};
use flex_rice::usage;

fn style(frecency: bool) -> StyleOptions {
    StyleOptions {
        filter_mode: flex_core::filter::FilterMode::Spec,
        char_set: CharSetName::Default,
        theme: ThemeName::Default,
        peaks: Peaks::Auto,
        frecency,
    }
}

fn fixture(name: &str, contents: &str) -> String {
    let dir = std::env::temp_dir().join(format!("flex-frecency-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(format!("{name}.tsv"));
    std::fs::write(&path, contents).expect("fixture");
    path.to_string_lossy().into_owned()
}

fn first_visible_index(menu: &Menu) -> usize {
    let visible = menu.app.visible_rows();
    visible.first().copied().expect("menu has rows")
}

#[test]
fn store_reaches_the_ranked_view() {
    let probe = build_menu(Provider::Proc, style(true)).expect("menu builds");
    let tab = probe.app.active_tab().expect("proc has a tab");
    assert!(tab.filterable, "proc is the searchable vehicle");
    assert!(tab.rows.len() > 1, "need room to reorder");
    // Pick a stable existing process (preferring init PID 1 if not at index 0)
    // rather than the very last row, which may be an ephemeral test worker that
    // terminates before the menu rebuild.
    let target_id = tab
        .rows
        .iter()
        .skip(1)
        .find(|r| r.id.as_str() == "1")
        .or_else(|| tab.rows.get(1))
        .expect("rows")
        .id
        .as_str()
        .to_owned();

    let now = usage::now_secs();
    let path = fixture("live", &format!("proc\t5.0\t{now}\t{target_id}\n"));
    std::env::set_var(usage::USAGE_ENV, &path);

    // Enabled: the recorded row leads with an empty filter.
    let menu = build_menu(Provider::Proc, style(true)).expect("menu builds");
    assert!(
        !menu.app.usage.is_empty(),
        "table loads for the menu provider"
    );
    // `proc` sorts by RSS, so the recorded row's index shifts between the
    // probe build and this one; compare against its *current* index rather
    // than the probe's.
    let current = menu.app.active_tab().expect("proc has a tab");
    let expected = current
        .rows
        .iter()
        .position(|row| row.id.as_str() == target_id)
        .expect("recorded row still listed");
    assert_eq!(
        first_visible_index(&menu),
        expected,
        "most-used on top at open"
    );

    // Disabled: provider order despite the store.
    let menu = build_menu(Provider::Proc, style(false)).expect("menu builds");
    assert!(menu.app.usage.is_empty(), "disabled loads nothing");
    assert!(!menu.app.use_frecency);
    assert_eq!(first_visible_index(&menu), 0, "provider order preserved");

    std::env::remove_var(usage::USAGE_ENV);
}

#[test]
fn fixed_and_ephemeral_tabs_ignore_learned_entries() {
    let now = usage::now_secs();
    let searchable = Tab::with_rows(
        "apps",
        vec![
            Row::new(RowId::new("s1"), "alpha"),
            Row::new(RowId::new("s2"), "beta"),
        ],
    );
    let mut fixed = Tab::with_rows(
        "actions",
        vec![
            Row::new(RowId::new("f1"), "gamma"),
            Row::new(RowId::new("f2"), "delta"),
        ],
    );
    fixed.filterable = false;
    let mut ephemeral = Tab::with_rows(
        "scan",
        vec![
            Row::new(RowId::new("e1"), "eps"),
            Row::new(RowId::new("e2"), "eta"),
        ],
    );
    ephemeral.learnable = false;
    assert!(Tab::with_rows("t", vec![]).learnable, "opt-out, not opt-in");
    let mut menu = Menu::new("x", vec![searchable, fixed, ephemeral]);
    for (id, rank) in [("s2", 5.0), ("f2", 100.0), ("e2", 100.0)] {
        menu.app.usage.insert(
            id.to_owned(),
            flex_core::filter::UsageEntry {
                rank,
                last_accessed: now,
            },
        );
    }

    // Searchable tab: the used second row leads.
    assert_eq!(menu.app.visible_rows().as_ref(), &[1, 0]);
    // Fixed tab: provider order despite the heavily used second row.
    menu.app.switch_tab(1);
    assert_eq!(menu.app.visible_rows().as_ref(), &[0, 1]);
    // Ephemeral tab: searchable but still provider order.
    menu.app.switch_tab(2);
    assert_eq!(menu.app.visible_rows().as_ref(), &[0, 1]);
}
