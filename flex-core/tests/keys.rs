//! Key state-machine replays (`src/keys.rs`): priority chain, Q1/Q6,
//! danger-timing, delete-confirm, per-tab restore, tick focus.
//!
//! NOTE on `FLEX_TEST` seeds: `Docs/Implementation.md` names a deterministic
//! seed but lists no explicit seed cases, so M1 defined the replay table in
//! `flex_test_seed_table` as the seed set (fixed `Instant` base, no sleeps).
//! M6 closed the `TODO(M6)` gap: the table now covers ~80% of the key-space
//! matrix (29 behaviors: filter editing incl. `Ctrl-w` word shapes,
//! tab-switch wrap matrix, shifted nav runes, `F1` without gauge, empty-view
//! keys, out-of-range digits, NAVIGATE mark persistence). The remainder is
//! covered by targeted tests below (danger timing, delete-confirm, empty
//! apps, `F1` with gauge, tick focus) rather than the table.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use flex_core::keys::{handle_key, KeyOutcome, ARM_CONFIRM_DELAY, ARM_EXPIRE, EXIT_CANCELLED};
use flex_core::{backend, Menu, Mode, Row, RowId, Tab};

/// Shared fixture: apps (filterable) + power (danger) + clip tabs.
fn fixture() -> Menu {
    let apps = Tab::with_rows(
        "apps",
        vec![
            Row::new(RowId::new("id-firefox"), "Firefox"),
            Row::new(RowId::new("id-first-aid"), "First Aid"),
            Row::new(RowId::new("id-confirm"), "Confirm"),
            Row::new(RowId::new("id-calf"), "Calf"),
            Row::new(RowId::new("id-fridge"), "Fridge"),
        ],
    );
    let power = Tab::with_rows(
        "power",
        vec![
            Row::confirmable(RowId::new("id-shutdown"), "Shutdown"),
            Row::confirmable(RowId::new("id-reboot"), "Reboot"),
            Row::new(RowId::new("id-logout"), "Logout"),
        ],
    );
    let clip = Tab::with_rows(
        "clip",
        vec![
            Row::new(RowId::new("id-a"), "alpha"),
            Row::new(RowId::new("id-b"), "beta"),
        ],
    );
    Menu::new("test", vec![apps, power, clip])
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn rune(c: char) -> KeyEvent {
    press(KeyCode::Char(c))
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn repeat(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Repeat,
        state: KeyEventState::NONE,
    }
}

fn base() -> Instant {
    Instant::now()
}

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

fn filter_of(menu: &Menu) -> &str {
    &menu.app.active_tab().expect("tab").state.filter
}

// --- FLEX_TEST seed table -------------------------------------------------

/// One deterministic replay: keys with ms offsets from a fixed base.
struct SeedCase {
    name: &'static str,
    keys: Vec<(KeyEvent, u64)>,
    outcomes: Vec<KeyOutcome>,
    filter: &'static str,
    focus: usize,
    tab: usize,
}

fn seed_cases() -> Vec<SeedCase> {
    let mut cases = seed_filter_cases();
    cases.extend(seed_filter_cases_m6());
    cases.extend(seed_tab_cases());
    cases.extend(seed_tab_cases_m6());
    cases
}

/// Filter/runes half of the seed table.
fn seed_filter_cases() -> Vec<SeedCase> {
    vec![
        SeedCase {
            name: "type-fir-filters",
            keys: vec![(rune('f'), 0), (rune('i'), 10), (rune('r'), 20)],
            outcomes: vec![
                KeyOutcome::Consumed,
                KeyOutcome::Consumed,
                KeyOutcome::Consumed,
            ],
            filter: "fir",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "esc-clears-filter",
            keys: vec![(rune('f'), 0), (rune('i'), 10), (press(KeyCode::Esc), 20)],
            outcomes: vec![KeyOutcome::Consumed; 3],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "esc-empty-quits",
            keys: vec![(press(KeyCode::Esc), 0)],
            outcomes: vec![KeyOutcome::Quit(EXIT_CANCELLED)],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "q-quits-when-empty",
            keys: vec![(rune('q'), 0)],
            outcomes: vec![KeyOutcome::Quit(EXIT_CANCELLED)],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "q-types-when-filtered",
            keys: vec![(rune('f'), 0), (rune('q'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "fq",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "digit-switches-when-empty",
            keys: vec![(rune('2'), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 1,
        },
        SeedCase {
            name: "digit-types-when-filtered",
            keys: vec![(rune('f'), 0), (rune('2'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "f2",
            focus: 0,
            tab: 0,
        },
    ]
}

/// Filter/runes M6 extension: quit chords, empty-view keys, `Ctrl-w`
/// word shapes, shifted runes (split out for `too_many_lines`).
fn seed_filter_cases_m6() -> Vec<SeedCase> {
    vec![
        SeedCase {
            name: "upper-q-quits-when-empty",
            keys: vec![(rune('Q'), 0)],
            outcomes: vec![KeyOutcome::Quit(EXIT_CANCELLED)],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "ctrl-c-quits-from-filtered",
            keys: vec![(rune('f'), 0), (ctrl('c'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Quit(EXIT_CANCELLED)],
            filter: "f",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "backspace-on-empty-filter",
            keys: vec![(press(KeyCode::Backspace), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "enter-on-empty-view-consumed",
            keys: vec![
                (rune('z'), 0),
                (rune('z'), 10),
                (rune('z'), 20),
                (press(KeyCode::Enter), 30),
            ],
            outcomes: vec![KeyOutcome::Consumed; 4],
            filter: "zzz",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "ctrl-w-kills-word",
            keys: vec![
                (rune('f'), 0),
                (rune('i'), 10),
                (rune('r'), 20),
                (rune('e'), 30),
                (rune(' '), 40),
                (rune('w'), 50),
                (rune('a'), 60),
                (rune('l'), 70),
                (rune('l'), 80),
                (ctrl('w'), 90),
            ],
            outcomes: vec![KeyOutcome::Consumed; 10],
            filter: "fire ",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "ctrl-w-clears-single-word",
            keys: vec![
                (rune('f'), 0),
                (rune('i'), 10),
                (rune('r'), 20),
                (rune('e'), 30),
                (ctrl('w'), 40),
            ],
            outcomes: vec![KeyOutcome::Consumed; 5],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "shifted-rune-types-in-normal",
            keys: vec![(rune('J'), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "J",
            focus: 0,
            tab: 0,
        },
    ]
}

/// Tab-switch/help/mode half of the seed table.
fn seed_tab_cases() -> Vec<SeedCase> {
    vec![
        SeedCase {
            name: "alt-digit-always-switches",
            keys: vec![(rune('f'), 0), (alt('2'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 1,
        },
        SeedCase {
            name: "tab-cycles-and-wraps",
            keys: vec![
                (press(KeyCode::Tab), 0),
                (press(KeyCode::Tab), 10),
                (press(KeyCode::Tab), 20),
            ],
            outcomes: vec![KeyOutcome::Consumed; 3],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "shift-tab-cycles-back",
            keys: vec![(press(KeyCode::BackTab), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 2,
        },
        SeedCase {
            name: "help-toggles",
            keys: vec![(rune('?'), 0), (rune('?'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "ctrl-o-then-j-navigates",
            keys: vec![(ctrl('o'), 0), (rune('j'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "",
            focus: 1,
            tab: 0,
        },
    ]
}

/// Tab-switch/help/mode M6 extension: shifted nav runes, `Left`/`Right`
/// wrap matrix, out-of-range digits, gaugeless `F1`, mark persistence,
/// page anchors (split out for `too_many_lines`).
fn seed_tab_cases_m6() -> Vec<SeedCase> {
    vec![
        SeedCase {
            name: "ctrl-o-then-shift-j-navigates",
            keys: vec![(ctrl('o'), 0), (rune('J'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "",
            focus: 1,
            tab: 0,
        },
        SeedCase {
            name: "q-ignored-in-navigate",
            keys: vec![(ctrl('o'), 0), (rune('q'), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "left-wraps-to-last-tab",
            keys: vec![(press(KeyCode::Left), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 2,
        },
        SeedCase {
            name: "right-cycle-wraps-around",
            keys: vec![
                (press(KeyCode::Right), 0),
                (press(KeyCode::Right), 10),
                (press(KeyCode::Right), 20),
            ],
            outcomes: vec![KeyOutcome::Consumed; 3],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "alt-digit-out-of-range",
            keys: vec![(alt('9'), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "digit-9-out-of-range-when-empty",
            keys: vec![(rune('9'), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "f1-without-gauge-consumed",
            keys: vec![(press(KeyCode::F(1)), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "mark-persists-across-tabs",
            keys: vec![
                (ctrl('o'), 0),
                (rune('m'), 10),
                (press(KeyCode::Tab), 20),
                (press(KeyCode::BackTab), 30),
            ],
            outcomes: vec![KeyOutcome::Consumed; 4],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "end-then-pageup-clamps",
            keys: vec![(press(KeyCode::End), 0), (press(KeyCode::PageUp), 10)],
            outcomes: vec![KeyOutcome::Consumed, KeyOutcome::Consumed],
            filter: "",
            focus: 0,
            tab: 0,
        },
        SeedCase {
            name: "end-anchors-to-last-row",
            keys: vec![(press(KeyCode::End), 0)],
            outcomes: vec![KeyOutcome::Consumed],
            filter: "",
            focus: 4,
            tab: 0,
        },
    ]
}

#[test]
fn flex_test_seed_table() {
    // Deterministic replay: fixed Instant base, injected timestamps.
    assert_ne!(backend::FLEX_TEST_SEED, 0);
    assert_eq!(backend::poll_timeout(), Duration::from_secs(1));
    let origin = base();
    let cases = seed_cases();
    assert!(
        cases.len() >= 29,
        "seed table must hold the M1+M6 replay set"
    );
    for case in &cases {
        let name = case.name;
        let mut menu = fixture();
        let mut got = Vec::new();
        for (key, ms) in &case.keys {
            got.push(handle_key(&mut menu, *key, at(origin, *ms)));
        }
        assert_eq!(got, case.outcomes, "outcomes for {name}");
        assert_eq!(filter_of(&menu), case.filter, "filter for {name}");
        assert_eq!(
            menu.app.active_tab().expect("tab").state.focus,
            case.focus,
            "focus for {name}"
        );
        assert_eq!(menu.app.active, case.tab, "tab for {name}");
        if case.name == "help-toggles" {
            assert!(!menu.app.help_open, "help closes on second toggle");
        }
        if case.name == "ctrl-o-then-j-navigates"
            || case.name == "ctrl-o-then-shift-j-navigates"
            || case.name == "q-ignored-in-navigate"
        {
            assert_eq!(menu.app.mode, Mode::Navigate);
        }
        if case.name == "mark-persists-across-tabs" {
            assert_eq!(menu.app.mode, Mode::Navigate, "mode sticks");
            assert!(
                menu.app.tabs[0].state.marked.contains(&0),
                "mark survives tab switches"
            );
        }
        if case.name == "f1-without-gauge-consumed" {
            assert!(menu.gauge.is_none(), "no gauge to mute");
        }
    }
}

// --- Danger timing --------------------------------------------------------

fn goto_power(menu: &mut Menu) {
    let _ = handle_key(menu, rune('2'), base());
    assert_eq!(menu.app.active, 1);
}

#[test]
fn non_filterable_tab_ignores_typing_but_keeps_control_runes() {
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    menu.app.active_tab_mut().expect("power tab").filterable = false;
    let out = handle_key(&mut menu, rune('x'), t0);
    assert_eq!(out, KeyOutcome::Consumed);
    assert_eq!(
        menu.app.active_state().expect("state").filter,
        "",
        "typing never lands in the filter on power"
    );
    let _ = handle_key(&mut menu, rune('?'), t0);
    assert!(menu.app.help_open, "? still toggles help");
    let out = handle_key(&mut menu, rune('q'), t0);
    assert_eq!(
        out,
        KeyOutcome::Quit(EXIT_CANCELLED),
        "q still quits with empty filter"
    );
}

#[test]
fn single_enter_on_confirmable_arms_but_never_confirms() {
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let out = handle_key(&mut menu, press(KeyCode::Enter), t0);
    assert_eq!(out, KeyOutcome::Consumed);
    assert!(menu.app.is_armed(), "first Enter arms");
}

#[test]
fn second_enter_before_delay_is_swallowed() {
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 49));
    assert_eq!(out, KeyOutcome::Consumed);
    assert!(menu.app.is_armed(), "early Enter stays armed");
}

#[test]
fn second_enter_at_delay_confirms() {
    assert_eq!(ARM_CONFIRM_DELAY, Duration::from_millis(50));
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 50));
    assert_eq!(out, KeyOutcome::Select);
    assert!(!menu.app.is_armed(), "confirm disarms");
}

#[test]
fn enter_hold_repeat_is_swallowed() {
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    // Terminal autorepeat arrives as Repeat (or rapid Press); both swallow.
    let out = handle_key(&mut menu, repeat(KeyCode::Enter), at(t0, 200));
    assert_eq!(out, KeyOutcome::Consumed);
    assert!(menu.app.is_armed(), "hold never confirms");
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 10));
    assert_eq!(out, KeyOutcome::Consumed);
    assert!(menu.app.is_armed());
}

#[test]
fn other_key_disarms_and_applies() {
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    let out = handle_key(&mut menu, press(KeyCode::Down), at(t0, 10));
    assert_eq!(out, KeyOutcome::Consumed);
    assert!(!menu.app.is_armed(), "Down disarms");
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 1);
}

#[test]
fn arm_expires_after_five_seconds_via_injected_instant() {
    assert_eq!(ARM_EXPIRE, Duration::from_secs(5));
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    // Stale arm + Enter: expiry disarms first, then the key re-arms.
    let out = handle_key(&mut menu, press(KeyCode::Enter), at(t0, 5_000));
    assert_eq!(out, KeyOutcome::Consumed, "expired arm never confirms");
    assert!(menu.app.is_armed(), "confirmable Enter re-arms fresh");
    // Tick expiry path.
    let expired = menu.tick(at(t0, 10_000));
    assert!(expired);
    assert!(!menu.app.is_armed());
}

#[test]
fn esc_disarms_before_exit() {
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Enter), t0);
    let out = handle_key(&mut menu, press(KeyCode::Esc), at(t0, 10));
    assert_eq!(out, KeyOutcome::Consumed);
    assert!(!menu.app.is_armed());
    let out = handle_key(&mut menu, press(KeyCode::Esc), at(t0, 20));
    assert_eq!(out, KeyOutcome::Quit(EXIT_CANCELLED));
}

// --- Delete confirm --------------------------------------------------------

#[test]
fn delete_flow() {
    let t0 = base();
    // Delete flow only arms on deletable tabs (M2 `Tab::deletable`;
    // launch/power leave it `false`). Opt the fixture tabs in.
    let fresh = || {
        let mut menu = fixture();
        for tab in &mut menu.app.tabs {
            tab.deletable = true;
        }
        menu
    };
    // Delete, Delete → deletes.
    let mut menu = fresh();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Down), t0);
    let _ = handle_key(&mut menu, press(KeyCode::Down), t0);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Delete), t0),
        KeyOutcome::Consumed
    );
    assert!(menu.app.active_state().expect("s").confirm_pending);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Delete), t0),
        KeyOutcome::Delete
    );
    assert!(!menu.app.active_state().expect("s").confirm_pending);

    // Delete, Enter → deletes.
    let mut menu = fresh();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Delete), t0);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Enter), t0),
        KeyOutcome::Delete
    );

    // Delete, Esc → cancels (consumed, no exit since pending cancelled).
    let mut menu = fresh();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Delete), t0);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Esc), t0),
        KeyOutcome::Consumed
    );
    assert!(!menu.app.active_state().expect("s").confirm_pending);

    // Delete, rune → cancels AND types.
    let mut menu = fresh();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Delete), t0);
    assert_eq!(handle_key(&mut menu, rune('x'), t0), KeyOutcome::Consumed);
    assert!(!menu.app.active_state().expect("s").confirm_pending);
    assert_eq!(filter_of(&menu), "x");
}

// --- Navigation / marks / tabs ---------------------------------------------

#[test]
fn navigate_mode_nav_and_mark() {
    let mut menu = fixture();
    menu.app.mode = Mode::Navigate;
    let t0 = base();
    assert_eq!(handle_key(&mut menu, rune('j'), t0), KeyOutcome::Consumed);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 1);
    assert_eq!(handle_key(&mut menu, rune('k'), t0), KeyOutcome::Consumed);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
    assert_eq!(handle_key(&mut menu, rune('m'), t0), KeyOutcome::Consumed);
    assert!(menu
        .app
        .active_tab()
        .expect("tab")
        .state
        .marked
        .contains(&0));
    assert_eq!(handle_key(&mut menu, rune('m'), t0), KeyOutcome::Consumed);
    assert!(!menu
        .app
        .active_tab()
        .expect("tab")
        .state
        .marked
        .contains(&0));
    // h/l switch tabs.
    assert_eq!(handle_key(&mut menu, rune('l'), t0), KeyOutcome::Consumed);
    assert_eq!(menu.app.active, 1);
    assert_eq!(handle_key(&mut menu, rune('h'), t0), KeyOutcome::Consumed);
    assert_eq!(menu.app.active, 0);
    // Other runes are ignored (never leak into the filter).
    assert_eq!(handle_key(&mut menu, rune('z'), t0), KeyOutcome::Consumed);
    assert_eq!(filter_of(&menu), "");
}

#[test]
fn normal_mode_j_types() {
    let mut menu = fixture();
    let t0 = base();
    assert_eq!(handle_key(&mut menu, rune('j'), t0), KeyOutcome::Consumed);
    assert_eq!(filter_of(&menu), "j");
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
}

#[test]
fn tab_switch_restores_per_tab_state() {
    let mut menu = fixture();
    let t0 = base();
    for c in ['f', 'i', 'r'] {
        let _ = handle_key(&mut menu, rune(c), t0);
    }
    let _ = handle_key(&mut menu, press(KeyCode::Down), t0);
    assert_eq!(filter_of(&menu), "fir");
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 1);
    // Mark the focused row (needs NAVIGATE briefly).
    menu.app.mode = Mode::Navigate;
    let _ = handle_key(&mut menu, rune('m'), t0);
    menu.app.mode = Mode::Normal;
    // Switch away and back: filter + focus + marks restore.
    let _ = handle_key(&mut menu, press(KeyCode::Tab), t0);
    assert_eq!(menu.app.active, 1);
    assert_eq!(filter_of(&menu), "");
    let _ = handle_key(&mut menu, press(KeyCode::Down), t0);
    let _ = handle_key(&mut menu, press(KeyCode::BackTab), t0);
    assert_eq!(menu.app.active, 0);
    assert_eq!(filter_of(&menu), "fir");
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 1);
    assert!(!menu.app.active_tab().expect("tab").state.marked.is_empty());
}

#[test]
fn filter_editing_keys() {
    let mut menu = fixture();
    let t0 = base();
    for c in ['f', 'i', 'r', 'e'] {
        let _ = handle_key(&mut menu, rune(c), t0);
    }
    assert_eq!(filter_of(&menu), "fire");
    let _ = handle_key(&mut menu, press(KeyCode::Backspace), t0);
    assert_eq!(filter_of(&menu), "fir");
    let _ = handle_key(&mut menu, ctrl('w'), t0);
    assert_eq!(filter_of(&menu), "");
    for c in ['a', 'b'] {
        let _ = handle_key(&mut menu, rune(c), t0);
    }
    let _ = handle_key(&mut menu, ctrl('u'), t0);
    assert_eq!(filter_of(&menu), "");
}

#[test]
fn ctrl_np_move_in_any_mode() {
    let mut menu = fixture();
    let t0 = base();
    let _ = handle_key(&mut menu, ctrl('n'), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 1);
    let _ = handle_key(&mut menu, ctrl('p'), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
    menu.app.mode = Mode::Navigate;
    let _ = handle_key(&mut menu, ctrl('n'), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 1);
}

#[test]
fn enter_selects_plain_row() {
    let mut menu = fixture();
    let t0 = base();
    goto_power(&mut menu);
    let _ = handle_key(&mut menu, press(KeyCode::Down), t0);
    let _ = handle_key(&mut menu, press(KeyCode::Down), t0);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Enter), t0),
        KeyOutcome::Select
    );
}

#[test]
fn tick_does_not_steal_focus() {
    let mut menu = fixture();
    let t0 = base();
    for c in ['f', 'i'] {
        let _ = handle_key(&mut menu, rune(c), t0);
    }
    let _ = handle_key(&mut menu, press(KeyCode::Down), t0);
    let before = (
        menu.app.active,
        menu.app.active_tab().expect("tab").state.focus,
        menu.app.active_tab().expect("tab").state.scroll,
        filter_of(&menu).to_string(),
        menu.app.visible_rows(),
    );
    let expired = menu.tick(at(t0, 1_000));
    assert!(!expired);
    let after = (
        menu.app.active,
        menu.app.active_tab().expect("tab").state.focus,
        menu.app.active_tab().expect("tab").state.scroll,
        filter_of(&menu).to_string(),
        menu.app.visible_rows(),
    );
    assert_eq!(before, after, "tick must not move focus/selection");
}

#[test]
fn f1_toggles_gauge_mute() {
    use flex_core::Gauge;
    let mut menu = fixture();
    menu.gauge = Some(Gauge::new("Volume", 65));
    let t0 = base();
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::F(1)), t0),
        KeyOutcome::Consumed
    );
    assert!(menu.gauge.as_ref().expect("gauge").muted);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::F(1)), t0),
        KeyOutcome::Consumed
    );
    assert!(!menu.gauge.as_ref().expect("gauge").muted);
}

#[test]
fn home_end_page_wrap_focus() {
    let mut menu = fixture();
    let t0 = base();
    let _ = handle_key(&mut menu, press(KeyCode::End), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 4);
    let _ = handle_key(&mut menu, press(KeyCode::Home), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
    let _ = handle_key(&mut menu, press(KeyCode::PageDown), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 4);
    let _ = handle_key(&mut menu, press(KeyCode::PageDown), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 4);
    let _ = handle_key(&mut menu, press(KeyCode::PageUp), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
    let _ = handle_key(&mut menu, press(KeyCode::Up), t0);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 4);
}

// --- Empty apps/tabs ----------------------------------------------------------

#[test]
fn empty_app_keys_never_panic() {
    let mut menu = Menu::new("test", vec![]);
    let t0 = base();
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Enter), t0),
        KeyOutcome::Consumed
    );
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Down), t0),
        KeyOutcome::Consumed
    );
    assert_eq!(
        handle_key(&mut menu, rune('2'), t0),
        KeyOutcome::Consumed,
        "digit with no tabs has nothing to switch to"
    );
    assert_eq!(
        handle_key(&mut menu, rune('q'), t0),
        KeyOutcome::Consumed,
        "q with no tabs has no filter state to quit on"
    );
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Esc), t0),
        KeyOutcome::Quit(EXIT_CANCELLED)
    );
    assert!(!menu.tick(t0), "tick on empty app expires nothing");
}

#[test]
fn single_empty_tab_keys_are_safe() {
    let mut menu = Menu::new("test", vec![Tab::empty("e")]);
    let t0 = base();
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Enter), t0),
        KeyOutcome::Consumed,
        "Enter on an empty view selects nothing"
    );
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Down), t0),
        KeyOutcome::Consumed
    );
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
    assert_eq!(
        handle_key(&mut menu, press(KeyCode::Esc), t0),
        KeyOutcome::Quit(EXIT_CANCELLED)
    );
}
