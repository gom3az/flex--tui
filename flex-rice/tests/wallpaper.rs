//! `flex wallpaper` cutover tests: provider rows, scan parity with the bash
//! `find … | sort -u` pipeline, preview-pane geometry, key-seq replays,
//! goldens at 80x24, `FLEX_TEST` determinism and the executor matrix.
//!
//! Parity is against `scripts/.config/scripts/wallpaper-picker.sh` (the fzf +
//! kitty-icat picker this replaces): same roots, same `-maxdepth 2 -iname`
//! filter, same label (`basename`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::Terminal;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicU64, Ordering};

use flex_core::keys::{handle_key, KeyOutcome, EXIT_CANCELLED};
use flex_core::preview;
use flex_core::{backend, render, run, width, Menu, Row};
use flex_rice::providers::wallpaper;

/// Long name: wider than the list column once the preview pane is reserved,
/// so truncation proves where the list ends.
const LONG_NAME: &str = "a-very-long-wallpaper-name-that-crosses-the-pane-column.jpg";

/// Per-call sequence: cargo runs tests in parallel, so a fixed scratch name
/// would let sibling tests delete each other's fixtures (B-007 class).
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique scratch directory for one test.
fn scratch(name: &str) -> std::path::PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "flex-wallpaper-test-{}-{seq}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Two wallpapers in `Wallpapers`, one in `Screenshots` (bash root order).
fn fixture_entries(root: &std::path::Path) -> Vec<wallpaper::WallpaperEntry> {
    let walls = root.join("Pictures/Wallpapers");
    let shots = root.join("Pictures/Screenshots");
    std::fs::create_dir_all(&walls).expect("walls dir");
    std::fs::create_dir_all(&shots).expect("shots dir");
    let active = walls.join("sunset.jpg");
    wallpaper::scan(&[walls, shots], Some(&active))
}

fn write_image(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(path, [0xff, 0xd8, 0xff, 0xe0]).expect("write image");
}

fn fixture_menu(preview_on: bool) -> Menu {
    let root = scratch(if preview_on {
        "fixture-on"
    } else {
        "fixture-off"
    });
    write_image(&root.join("Pictures/Wallpapers/sunset.jpg"));
    write_image(&root.join("Pictures/Wallpapers").join(LONG_NAME));
    write_image(&root.join("Pictures/Screenshots/grim-2026-09-15.png"));
    let entries = fixture_entries(&root);
    let mut menu = flex_rice::menu(
        wallpaper::PROVIDER,
        vec![wallpaper::tab_from_entries(&entries)],
    );
    menu.preview = preview_on;
    // Rows keep absolute paths as strings; rendering and key handling never
    // touch the files, so the fixture tree can go away with the menu built.
    let _ = std::fs::remove_dir_all(&root);
    menu
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Filesystem-free entries (paths need not exist for row/key tests), sorted
/// the way [`wallpaper::scan`] sorts.
fn synthetic_entries() -> Vec<wallpaper::WallpaperEntry> {
    [
        ("/screens/grim-2026-09-15.png", "Screenshots", false),
        ("/walls/night.png", "Wallpapers", false),
        ("/walls/sunset.jpg", "Wallpapers", true),
    ]
    .into_iter()
    .map(|(path, dir, active)| wallpaper::WallpaperEntry {
        path: std::path::PathBuf::from(path),
        name: std::path::Path::new(path)
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned(),
        dir: dir.to_string(),
        active,
    })
    .collect()
}

/// Symbols of row `y`, skipping ratatui's continuation cells.
fn row_text(buf: &Buffer, y: u16, w: u16) -> String {
    let mut out = String::new();
    for x in 0..w {
        let cell = buf.cell((x, y)).expect("cell in frame");
        if !cell.skip {
            out.push_str(cell.symbol());
        }
    }
    out
}

fn draw(menu: &mut Menu, w: u16, h: u16) -> (Buffer, Option<Rect>) {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    let mut area = Rect::default();
    terminal
        .draw(|frame| {
            area = frame.area();
            render::render(frame, menu);
        })
        .expect("render frame");
    let pane = render::preview_area(area, menu);
    (terminal.backend().buffer().clone(), pane)
}

// --- Provider rows ----------------------------------------------------------

#[test]
fn rows_carry_hash_ids_labels_meta_and_preview_paths() {
    let root = scratch("rows");
    write_image(&root.join("Pictures/Wallpapers/sunset.jpg"));
    write_image(&root.join("Pictures/Screenshots/grim.png"));
    let walls = root.join("Pictures/Wallpapers");
    let shots = root.join("Pictures/Screenshots");
    let active = walls.join("sunset.jpg");
    let entries = wallpaper::scan(&[walls.clone(), shots.clone()], Some(&active));
    let rows = wallpaper::rows(&entries);
    let _ = std::fs::remove_dir_all(&root);

    let shapes: Vec<(&str, &str, Option<&str>, Option<&str>)> = rows
        .iter()
        .map(|row| {
            (
                row.id.as_str(),
                row.label.as_str(),
                row.meta.as_deref(),
                row.preview_image.as_deref(),
            )
        })
        .collect();
    assert_eq!(shapes.len(), 2);
    // Sorted by full path, like the bash `sort -u` (`Screenshots` < `Wallpapers`).
    assert_eq!(
        shapes[0],
        (
            wallpaper::entry_id(&shots.join("grim.png")).as_str(),
            "grim.png",
            Some("Screenshots"),
            Some(shots.join("grim.png").to_string_lossy().as_ref()),
        )
    );
    assert_eq!(
        shapes[1],
        (
            wallpaper::entry_id(&walls.join("sunset.jpg")).as_str(),
            "sunset.jpg",
            Some("Wallpapers  Active"),
            Some(walls.join("sunset.jpg").to_string_lossy().as_ref()),
        ),
        "the wallpaper in use is marked like the theme provider, and previews"
    );
    // Ids are 16-char hex: safe as space-delimited `ACTION:` fields.
    for row in &rows {
        assert_eq!(row.id.as_str().len(), 16);
        assert!(row.id.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }
}

#[test]
fn tab_is_standard_non_deletable_and_preview_carrying() {
    let menu = fixture_menu(false);
    let tab = menu.app.active_tab().expect("tab");
    assert_eq!(tab.name, wallpaper::TAB_NAME);
    assert_eq!(wallpaper::TAB_NAME, "Wallpapers");
    assert!(
        !tab.bare_rows,
        "meta column + tab bar are part of the design"
    );
    assert!(tab.filterable, "type-to-filter like every list provider");
    assert!(
        !tab.deletable,
        "wallpapers are never deleted from the picker"
    );
    assert!(
        tab.rows.iter().all(|row| row.preview_image.is_some()),
        "every row can drive the preview pane"
    );
}

#[test]
fn scan_matches_the_bash_find_pipeline() {
    let root = scratch("scan");
    let walls = root.join("Wallpapers");
    let nested = walls.join("nature");
    std::fs::create_dir_all(&nested).expect("nested dir");
    std::fs::create_dir_all(walls.join("deeper/still")).expect("deep dir");
    // `-iname` is case-insensitive and covers four suffixes.
    for name in [
        "b.PNG",
        "a.jpg",
        "c.jpeg",
        "d.WebP",
        "notes.txt",
        "e.gif",
        "noext",
    ] {
        std::fs::write(walls.join(name), b"x").expect("write");
    }
    std::fs::write(nested.join("deep.png"), b"x").expect("write");
    std::fs::write(walls.join("deeper/still/too-deep.png"), b"x").expect("write");
    // `find -type f` does not follow symlinks: a linked image is skipped.
    std::os::unix::fs::symlink(nested.join("deep.png"), walls.join("link.jpg")).expect("symlink");

    let entries = wallpaper::scan(std::slice::from_ref(&walls), None);
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        names,
        vec!["a.jpg", "b.PNG", "c.jpeg", "d.WebP", "deep.png"],
        "sorted by path, depth 1 + 2 only, symlinks and non-images skipped"
    );
}

#[test]
fn duplicate_roots_and_repeated_files_are_deduped() {
    let root = scratch("dedup");
    let walls = root.join("Wallpapers");
    write_image(&walls.join("only.png"));
    let entries = wallpaper::scan(&[walls.clone(), walls.clone()], None);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(entries.len(), 1, "bash `sort -u` semantics");
}

#[test]
fn missing_roots_yield_no_rows_without_panicking() {
    let missing = scratch("missing").join("nope");
    assert_eq!(wallpaper::scan(std::slice::from_ref(&missing), None), []);
    assert!(wallpaper::resolve_in(&[missing], "0123456789abcdef").is_none());
}

#[test]
fn resolve_round_trips_ids_to_paths() {
    let root = scratch("resolve");
    let walls = root.join("Wallpapers");
    let image = walls.join("with space.jpg");
    write_image(&image);
    let entries = wallpaper::scan(std::slice::from_ref(&walls), None);
    let id = wallpaper::entry_id(&entries[0].path);
    assert_eq!(
        wallpaper::resolve_in(std::slice::from_ref(&walls), &id).as_deref(),
        Some(image.as_path()),
        "the library resolver returns the absolute path"
    );
    assert!(wallpaper::resolve_in(&[walls], "deadbeefdeadbeef").is_none());

    // A file that vanished since the menu was drawn resolves to nothing.
    std::fs::remove_file(&image).expect("remove");
    assert!(wallpaper::resolve_in(std::slice::from_ref(&root), &id).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn current_wallpaper_falls_back_to_hyprpaper_conf() {
    // The state file is exercised through `WALLPAPER_STATE` in production
    // only; the conf parser is the pure fallback and is pinned here.
    assert_eq!(
        wallpaper::parse_hyprpaper_conf("preload = /p/a.jpg\nwallpaper = ,/w/b.png\n"),
        Some(std::path::PathBuf::from("/w/b.png"))
    );
    assert_eq!(wallpaper::parse_hyprpaper_conf(""), None);
}

#[test]
fn default_focus_is_row_zero() {
    let menu = fixture_menu(false);
    assert_eq!(menu.app.active_tab().expect("tab").state.focus, 0);
}

// --- Preview pane plumbing --------------------------------------------------

#[test]
fn render_reports_the_reserved_pane_only_when_asked() {
    let mut with = fixture_menu(true);
    let (_, pane) = draw(&mut with, 80, 24);
    assert_eq!(
        pane,
        Some(Rect::new(44, 3, 36, 20)),
        "80 wide: 45% pane right-aligned, sharing the 20-row list area"
    );

    let mut without = fixture_menu(false);
    let (_, pane) = draw(&mut without, 80, 24);
    assert_eq!(pane, None, "non-wallpaper menus keep the full-width list");
}

#[test]
fn pane_region_stays_blank_and_the_list_narrows() {
    let mut menu = fixture_menu(true);
    let (buf, pane) = draw(&mut menu, 80, 24);
    let pane = pane.expect("pane");
    for y in pane.y..pane.y + pane.height {
        let cells = row_text(&buf, y, 80);
        assert_eq!(
            cells[pane.x as usize..].trim(),
            "",
            "row {y} must leave the image pane empty: {cells:?}"
        );
    }
    // The chrome below keeps the full frame width.
    assert!(
        row_text(&buf, 23, 80).contains("[Wallpapers]"),
        "tab bar spans the frame"
    );

    // The long name is truncated inside the narrowed list…
    let all: String = (1..22).map(|y| row_text(&buf, y, 80)).collect();
    assert!(
        !all.contains(LONG_NAME),
        "label does not cross into the pane"
    );

    // …and fits when no pane is reserved.
    let mut wide = fixture_menu(false);
    let (buf, pane) = draw(&mut wide, 80, 24);
    assert_eq!(pane, None);
    let all: String = (1..22).map(|y| row_text(&buf, y, 80)).collect();
    assert!(
        all.contains(LONG_NAME),
        "without a pane the list uses the whole width"
    );
}

#[test]
fn pane_needs_a_wide_enough_frame() {
    let mut menu = fixture_menu(true);
    let (_, pane) = draw(&mut menu, 40, 24);
    assert_eq!(pane, None, "40 columns: list keeps priority");
    let mut menu = fixture_menu(true);
    let (_, pane) = draw(&mut menu, 125, 30);
    assert_eq!(pane, Some(Rect::new(69, 3, 56, 26)));
}

#[test]
fn preview_split_is_pure_geometry() {
    // No terminal involved: the same math `render` uses.
    let area = Rect::new(0, 0, 125, 30);
    assert_eq!(
        preview::pane(area, 27, true),
        Some(Rect::new(69, 0, 56, 27))
    );
    assert_eq!(preview::pane(area, 27, false), None);
    // The gutter column sits between the rows and the image.
    let pane = preview::pane(area, 27, true).expect("pane");
    assert_eq!(pane.x - 1, 68, "one gutter column");
}

// --- Key-seq replays --------------------------------------------------------

#[test]
fn keyseq_down_enter_selects_the_second_wallpaper() {
    let mut menu = fixture_menu(true);
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[press(KeyCode::Down), press(KeyCode::Enter)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Select);
    let row = menu.app.focused_row().expect("focused row");
    assert!(std::path::Path::new(&row.label)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg")));
    assert_eq!(menu.provider, "wallpaper");
    // The focus moved, so the pane follows the new image.
    assert!(row.preview_image.is_some());
}

#[test]
fn keyseq_filter_then_enter_selects_the_screenshot() {
    let mut menu = fixture_menu(true);
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[
            press(KeyCode::Char('g')),
            press(KeyCode::Char('r')),
            press(KeyCode::Char('i')),
            press(KeyCode::Char('m')),
            press(KeyCode::Enter),
        ],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Select);
    assert_eq!(
        menu.app.focused_row().expect("row").label,
        "grim-2026-09-15.png"
    );
}

#[test]
fn keyseq_esc_cancels_with_no_action() {
    let mut menu = fixture_menu(true);
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Esc)], base);
    assert_eq!(outcome, KeyOutcome::Quit(EXIT_CANCELLED));
}

#[test]
fn delete_never_fires_on_wallpaper_rows() {
    let mut menu = fixture_menu(false);
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[press(KeyCode::Delete), press(KeyCode::Delete)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Consumed, "Delete is dead here");
    assert!(!menu.app.active_state().expect("state").confirm_pending);
}

#[test]
fn flex_test_replays_are_deterministic() {
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
    // Same entries, two menus: ids and outcomes must match exactly.
    let entries = synthetic_entries();
    let mut first = flex_rice::menu(
        wallpaper::PROVIDER,
        vec![wallpaper::tab_from_entries(&entries)],
    );
    let mut second = flex_rice::menu(
        wallpaper::PROVIDER,
        vec![wallpaper::tab_from_entries(&entries)],
    );
    assert_eq!(run_script(&mut first), run_script(&mut second));
    assert_eq!(
        first.app.focused_row().expect("row").id,
        second.app.focused_row().expect("row").id
    );
}

// --- Goldens ----------------------------------------------------------------

/// Every row is exactly the frame width in display cells, and the layout is
/// the standard one: filter line, `•••` line, entries, hints, tab bar.
#[test]
fn wallpaper_default_view_golden_at_80x24() {
    let mut menu = fixture_menu(true);
    let (buf, _) = draw(&mut menu, 80, 24);
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
    // With filter chrome at top: y=0 margin, y=1 filter, y=2 separator, y=3 indicator, y=4 first entry
    let first_y = 4;
    assert!(
        row_text(&buf, first_y, 80).starts_with('░'),
        "selected entry carries the selector at column 0"
    );
    let meta_row = row_text(&buf, first_y, 80);
    assert!(
        meta_row.contains("Wallpapers") || meta_row.contains("Screenshots"),
        "meta column shows the source directory: {meta_row:?}"
    );
    // Standard chrome is present (unlike the bare-rows providers).
    let all: String = (0..24).map(|y| row_text(&buf, y, 80)).collect();
    assert!(all.contains('›'), "filter line rendered");
    assert!(all.contains("[Wallpapers]"), "tab bar rendered");
}

#[test]
fn row_without_a_utf8_path_loses_only_its_preview() {
    let mut entries = vec![wallpaper::WallpaperEntry {
        path: std::path::PathBuf::from("/tmp/\u{fffd}.jpg"),
        name: "x.jpg".to_string(),
        dir: "Wallpapers".to_string(),
        active: false,
    }];
    let rows = wallpaper::rows(&entries);
    assert_eq!(rows[0].preview_image.as_deref(), Some("/tmp/\u{fffd}.jpg"));
    // A path that cannot be UTF-8 at all keeps its row, minus the preview.
    entries[0].path =
        std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/\xff.jpg".to_vec()));
    let rows: Vec<Row> = wallpaper::rows(&entries);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "x.jpg");
    assert!(rows[0].preview_image.is_none());
}

// --- Executor (`flex-rice/src/exec/wallpaper.rs`) ------------------------------
//
// The wallpaper port of the `exec::shot` template: `WallpaperAction::parse`
// validates the 16-char lowercase-hex id, `execute` resolves the hash
// in-process via the library resolver and runs
// `<setter> <path>` through the `SET_WALLPAPER` seam. No TUI, no pty: stub
// `PATH` + scratch dirs.
//
// Like the theme suite, the popup guard itself lives in the shared runner
// (covered by `popup.rs`), with one binary-level re-exec probe below.

/// Save/restore one process env var around an executor call (tests run in
/// parallel; only the wallpaper executor reads these keys).
struct EnvGuard {
    key: &'static str,
    old: Option<std::ffi::OsString>,
}

/// Serialises the executor tests below: each mutates `WALLPAPER_DIRS`,
/// `SET_WALLPAPER`, `HOME` or `PATH` (the seams `execute` reads) and the
/// harness runs tests in parallel, so the mutations are held under one lock
/// with restores in [`EnvGuard`].
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The serialising lock for env-mutating tests, recovering from a poisoned
/// lock so one panicking sibling does not fail every other holder with a
/// `PoisonError` that masks the real failure. The panicking test's
/// [`EnvGuard`]s already restored the environment during unwinding.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn set_env(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvGuard {
    let old = std::env::var_os(key);
    std::env::set_var(key, value);
    EnvGuard { key, old }
}

fn remove_env(key: &'static str) -> EnvGuard {
    let old = std::env::var_os(key);
    std::env::remove_var(key);
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

/// Scratch `WALLPAPER_DIRS` root holding one PNG, one JPEG and one
/// space-bearing JPEG (the B-021 shape: the id hash keeps the space off the
/// `ACTION:` line).
fn install_wallpaper_dirs(tag: &str) -> std::path::PathBuf {
    let walls = scratch(&format!("exec-dirs-{tag}")).join("Walls");
    for name in ["sunset.jpg", "pick me.jpg", "night.png"] {
        write_image(&walls.join(name));
    }
    walls
}

/// `SET_WALLPAPER` stub logging every argv element bracketed (the theme
/// dispatch idiom: a split path cannot masquerade as one argument).
fn install_setter(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let setter = dir.join(name);
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &setter,
        format!(
            "#!/usr/bin/env bash\n{{ for arg in \"$@\"; do printf '[%s]' \"$arg\"; done; printf '\\n'; }} >> \"{}\"\n",
            log.display()
        ),
    )
    .expect("setter stub");
    std::fs::set_permissions(&setter, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    setter
}

fn stub_path_env(dir: &std::path::Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Executable stub at `dir/name` running `body` (0700 like the other stubs).
fn install_stub(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let stub = dir.join(name);
    std::fs::write(&stub, body).expect("stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    stub
}

/// Lowercase-hex encoding of stdin, so the socat stub logs exact bytes
/// (command substitution would strip the trailing newline).
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// A `socat` stub appending `[arg]… <stdin-hex>\n` per call.
fn install_socat(dir: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    install_stub(
        dir,
        "socat",
        &format!(
            "#!/usr/bin/env bash\n{{ for arg in \"$@\"; do printf '[%s]' \"$arg\"; done; printf ' '; od -An -v -tx1 | tr -d ' \\n'; printf '\\n'; }} >> '{}'\n",
            log.display()
        ),
    )
}

/// A stub logging every argv element bracketed, one line per call.
fn install_argv_logger(
    dir: &std::path::Path,
    name: &str,
    log: &std::path::Path,
) -> std::path::PathBuf {
    install_stub(
        dir,
        name,
        &format!(
            "#!/usr/bin/env bash\n{{ for arg in \"$@\"; do printf '[%s]' \"$arg\"; done; printf '\\n'; }} >> '{}'\n",
            log.display()
        ),
    )
}

/// Full action-id matrix: every wallpaper (including the space-bearing name)
/// sets through the setter seam as one call with the path intact.
#[test]
fn executor_matrix_sets_every_wallpaper_through_the_setter_seam() {
    let _env = env_lock();
    let dir = scratch("exec-matrix");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let walls = install_wallpaper_dirs("matrix");
    let setter = install_setter(&dir, "set-wallpaper.sh");
    let _dirs = set_env("WALLPAPER_DIRS", &walls);
    let _setter_env = set_env("SET_WALLPAPER", &setter);
    let path_env = stub_path_env(&dir);
    for name in ["sunset.jpg", "pick me.jpg", "night.png"] {
        let image = walls.join(name);
        let id = wallpaper::entry_id(&image);
        let report =
            flex_rice::exec::wallpaper::execute(&id, Some(&path_env)).expect("set succeeds");
        assert_eq!(report.path.as_path(), image.as_path());
        assert_eq!(report.setter.as_deref(), Some(setter.as_path()));
    }
    let log = std::fs::read_to_string(dir.join("set-wallpaper.sh.log")).expect("call log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        vec![
            format!("[{}]", walls.join("sunset.jpg").display()),
            format!("[{}]", walls.join("pick me.jpg").display()),
            format!("[{}]", walls.join("night.png").display()),
        ],
        "one setter call per id, paths intact (describe: `<setter> <path>`)",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `SET_WALLPAPER` seam: the override wins, and an empty or unset value
/// returns `None` so `execute` runs the ported native setter instead.
#[test]
fn executor_setter_seam_prefers_override_then_home_default() {
    let _env = env_lock();
    let dir = scratch("exec-seam");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let setter = install_setter(&dir, "set-wallpaper.sh");
    let _setter_env = set_env("SET_WALLPAPER", &setter);
    assert_eq!(
        flex_rice::exec::wallpaper::set_wallpaper_override(),
        Some(setter.clone())
    );

    let _empty = set_env("SET_WALLPAPER", "");
    assert_eq!(
        flex_rice::exec::wallpaper::set_wallpaper_override(),
        None,
        "empty falls back to the native setter"
    );

    let _no_setter = remove_env("SET_WALLPAPER");
    assert_eq!(
        flex_rice::exec::wallpaper::set_wallpaper_override(),
        None,
        "unset falls back to the native setter"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Native socket path: a real `.hyprpaper.sock` gets the connection test,
/// `preload`, and `wallpaper` socat calls in order, and the conf and ml4w cache
/// are written byte-exact.
#[test]
fn executor_native_sets_via_the_hyprpaper_socket_and_cache() {
    let _env = env_lock();
    let dir = scratch("exec-native-socket");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let home = dir.join("home");
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(home.join(".config/hypr")).expect("hypr conf dir");
    std::fs::create_dir_all(home.join(".cache/ml4w/hyprland-dotfiles")).expect("cache dir");
    let socket = runtime.join("hypr/.hyprpaper.sock");
    std::fs::create_dir_all(socket.parent().expect("socket parent")).expect("run dir");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind .hyprpaper.sock");

    install_stub(&dir, "pgrep", "#!/usr/bin/env bash\nexit 0\n");
    let socat_log = dir.join("socat.log");
    install_socat(&dir, &socat_log);

    let image = dir.join("wall sun.jpg");
    write_image(&image);
    flex_rice::exec::wallpaper::set_wallpaper_native_in(
        &image,
        &home,
        &runtime,
        &stub_path_env(&dir),
    )
    .expect("native set succeeds");

    let socat = std::fs::read_to_string(&socat_log).expect("socat log");
    let calls: Vec<&str> = socat.lines().collect();
    let target = format!("[-][UNIX-CONNECT:{}] ", socket.display());
    assert_eq!(
        calls,
        vec![
            format!("{target}{}", hex(b"\n")),
            format!(
                "{target}{}",
                hex(format!("preload {}\n", image.display()).as_bytes())
            ),
            format!(
                "{target}{}",
                hex(format!("wallpaper ,{}\n", image.display()).as_bytes())
            ),
        ],
        "socket test, preload, then wallpaper against the same socket",
    );

    assert_eq!(
        std::fs::read(home.join(".config/hypr/hyprpaper.conf")).expect("conf"),
        format!("preload = {0}\nwallpaper = , {0}\n", image.display()).as_bytes(),
    );
    assert_eq!(
        std::fs::read(home.join(".cache/ml4w/hyprland-dotfiles/current_wallpaper")).expect("cache"),
        format!("{}\n", image.display()).as_bytes(),
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Native restart path: when `pgrep` reports no hyprpaper, `killall` runs,
/// then `hyprpaper` is spawned detached (through `setsid -f` here), and the
/// conf is still persisted.
#[test]
fn executor_native_restarts_hyprpaper_when_not_running() {
    let _env = env_lock();
    let dir = scratch("exec-native-restart");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let home = dir.join("home");
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(home.join(".config/hypr")).expect("hypr conf dir");
    std::fs::create_dir_all(&runtime).expect("run dir");

    install_stub(&dir, "pgrep", "#!/usr/bin/env bash\nexit 1\n");
    let killall_log = dir.join("killall.log");
    install_argv_logger(&dir, "killall", &killall_log);
    let hyprpaper_log = dir.join("hyprpaper.log");
    install_stub(
        &dir,
        "hyprpaper",
        &format!(
            "#!/usr/bin/env bash\nprintf '[hyprpaper]\\n' >> '{}'\n",
            hyprpaper_log.display()
        ),
    );
    let setsid_log = dir.join("setsid.log");
    install_stub(
        &dir,
        "setsid",
        &format!(
            "#!/usr/bin/env bash\n{{ for arg in \"$@\"; do printf '[%s]' \"$arg\"; done; printf '\\n'; }} >> '{}'\nshift\nexec '{}'\n",
            setsid_log.display(),
            dir.join("hyprpaper").display()
        ),
    );

    let image = dir.join("night.png");
    write_image(&image);
    flex_rice::exec::wallpaper::set_wallpaper_native_in(
        &image,
        &home,
        &runtime,
        &stub_path_env(&dir),
    )
    .expect("native set succeeds");

    assert_eq!(
        std::fs::read_to_string(&killall_log).expect("killall log"),
        "[hyprpaper]\n",
    );
    assert_eq!(
        std::fs::read_to_string(&setsid_log).expect("setsid log"),
        "[-f][hyprpaper]\n",
    );
    assert_eq!(
        std::fs::read_to_string(&hyprpaper_log).expect("hyprpaper log"),
        "[hyprpaper]\n",
        "setsid execs hyprpaper, which is detached with stdio nulled",
    );
    assert_eq!(
        std::fs::read(home.join(".config/hypr/hyprpaper.conf")).expect("conf"),
        format!("preload = {0}\nwallpaper = , {0}\n", image.display()).as_bytes(),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed ids (`bad id`, mirroring the wrapper regex) and well-formed but
/// unresolvable hashes (`unknown id`) are errors with no `flex:` prefix of
/// their own — the runner adds the single prefix at the binary boundary.
#[test]
fn executor_rejects_bad_and_unknown_ids_without_its_own_prefix() {
    let _env = env_lock();
    let dir = scratch("exec-bad");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let walls = install_wallpaper_dirs("bad");
    let setter = install_setter(&dir, "set-wallpaper.sh");
    let _dirs = set_env("WALLPAPER_DIRS", &walls);
    let _setter_env = set_env("SET_WALLPAPER", &setter);
    let path_env = stub_path_env(&dir);
    let ghost_id = wallpaper::entry_id(&walls.join("ghost.jpg"));
    let cases = [
        ("", "wallpaper: bad id ''"),
        ("nope", "wallpaper: bad id 'nope'"),
        ("0123456789ABCDEF", "wallpaper: bad id"),
        ("0123456789abcde", "wallpaper: bad id"),
        ("a/b", "wallpaper: bad id 'a/b'"),
        ("a\nb", "wallpaper: bad id 'a\nb'"),
        (ghost_id.as_str(), "wallpaper: unknown id"),
    ];
    for (id, expected) in cases {
        let err = flex_rice::exec::wallpaper::execute(id, Some(&path_env))
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
        !dir.join("set-wallpaper.sh.log").exists(),
        "no id failure reaches the setter"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failing setter is a loud error, never a quiet cancel: wallpaper has no
/// `slurp`-like user-cancellable step, so every tool failure exits non-zero
/// through the runner (the menu-cancel 130 lives in the binary's
/// `Outcome::Cancelled` arm, shared with shot/theme, and needs a pty).
#[test]
fn executor_tool_failure_is_a_loud_error_not_a_quiet_cancel() {
    let _env = env_lock();
    let dir = scratch("exec-fail");
    std::fs::create_dir_all(&dir).expect("stub dir");
    let walls = install_wallpaper_dirs("fail");
    let setter = dir.join("set-wallpaper.sh");
    std::fs::write(&setter, "#!/usr/bin/env bash\nexit 3\n").expect("failing stub");
    std::fs::set_permissions(&setter, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let _dirs = set_env("WALLPAPER_DIRS", &walls);
    let _setter_env = set_env("SET_WALLPAPER", &setter);
    let image = walls.join("sunset.jpg");
    let id = wallpaper::entry_id(&image);
    let err = flex_rice::exec::wallpaper::execute(&id, Some(&stub_path_env(&dir)))
        .expect_err("a failing setter must fail");
    assert_eq!(
        format!("{err:#}"),
        format!("wallpaper: set {} failed", image.display())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binary level without a pty: the menu cannot start, so the binary exits 1
/// with exactly one `flex: error:` prefix (the runner owns it; executor
/// errors carry none). `setsid` detaches the controlling terminal so no
/// harness tty can satisfy the TUI init.
///
/// A fixture store is installed because an *empty* store exits 130 with the
/// `no wallpapers found` diagnostic before the tty probe; relying on the
/// ambient `$HOME` made this test pass only on machines that happened to have
/// wallpapers (it exited 130 in CI, poisoning the env lock for its siblings).
#[test]
fn binary_errors_carry_a_single_prefix() {
    let _env = env_lock();
    let home = scratch("binary-prefix-home");
    std::fs::create_dir_all(&home).expect("home dir");
    let walls = install_wallpaper_dirs("binary-prefix");
    let _dirs = set_env("WALLPAPER_DIRS", &walls);
    let _home = set_env("HOME", &home);
    let output = std::process::Command::new("setsid")
        .arg(env!("CARGO_BIN_EXE_flex-wallpaper"))
        .env("POPUP_KITTY", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-wallpaper without a pty");
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

/// Outside a popup the binary re-execs into the `menu-wide` popup (the
/// shared runner guard); the kitty spawn is asserted against a stub `PATH`.
#[test]
fn binary_outside_a_popup_reexecs_into_the_wide_popup() {
    let _env = env_lock();
    let dir = scratch("exec-guard");
    let home = scratch("exec-guard-home");
    std::fs::create_dir_all(&dir).expect("stub dir");
    std::fs::create_dir_all(&home).expect("home dir");
    let kitty_log = dir.join("kitty.log");
    std::fs::write(dir.join("pgrep"), "#!/usr/bin/env bash\nexit 1\n").expect("pgrep stub");
    std::fs::set_permissions(dir.join("pgrep"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::fs::write(
        dir.join("kitty"),
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}.tmp'\nmv '{}.tmp' '{}'\n",
            kitty_log.display(),
            kitty_log.display(),
            kitty_log.display(),
        ),
    )
    .expect("kitty stub");
    std::fs::set_permissions(dir.join("kitty"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    let path_env = stub_path_env(&dir);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_flex-wallpaper"))
        .env("PATH", &path_env)
        .env("HOME", &home)
        .env_remove("POPUP_KITTY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-wallpaper outside a popup");
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
            && logged.contains(&String::from("flex-menu-wide"))
            && logged.contains(&String::from("POPUP_KITTY=1")),
        "re-exec uses the menu-wide popup template: {logged:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
