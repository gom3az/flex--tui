//! `flex clip` cutover tests (M4): provider ingest, key-seq replays, clip
//! golden, `FLEX_TEST` determinism, and the bash parity probe.
//!
//! Parity is against the `pick()` path of
//! `scripts/.config/scripts/cliphist.sh` (only that path is cut over;
//! `add`/`pin`/`unpin` stay in bash — see `src/providers/clip.rs`).

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use flex_core::keys::{handle_key, KeyOutcome, EXIT_CANCELLED};
use flex_core::{backend, content_hash_hex, run, width, Menu, Mode};
use flex_rice::providers::clip;

/// Unique scratch dir per call (tests run in parallel; a shared name
/// would race `remove_dir_all` against a sibling's writes).
fn scratch(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "flex-clip-test-{}-{name}-{seq}",
        std::process::id()
    ))
}

/// Binary corpus exercising every cleaning rule:
/// NUL / `0x1F` / ESC stripping, tab expansion, invalid UTF-8 (lossy),
/// empty-line drops, glob characters (literal), `<NEWLINE>` multiline
/// entries, duplicates, and a 200-char long line (120-char preview).
fn write_corpus(dir: &Path) -> (PathBuf, PathBuf) {
    let hist = dir.join("cliphist");
    let pins = dir.join("cliphist.pins");
    let mut hist_bytes: Vec<u8> = Vec::new();
    hist_bytes.extend_from_slice(b"first entry\n");
    hist_bytes.extend_from_slice(b"\n"); // dropped: empty line
    hist_bytes.extend_from_slice("pinned one\n".as_bytes()); // dup of a pin
    hist_bytes.extend_from_slice(b"bin\x00a\x1fry\x1b\tend\xff\xfe\n");
    hist_bytes.extend_from_slice("glob *[abc]? {x,y}\n".as_bytes());
    hist_bytes.extend_from_slice("alpha<NEWLINE>beta gamma\n".as_bytes());
    hist_bytes.extend_from_slice(format!("{}\n", "L".repeat(200)).as_bytes());
    hist_bytes.extend_from_slice(b"terminal tools\n");
    hist_bytes.extend_from_slice(b"second entry\n");
    std::fs::write(&hist, hist_bytes).expect("write hist fixture");
    std::fs::write(&pins, "pinned one\nalpha<NEWLINE>beta gamma\n*zpinned*\n")
        .expect("write pins fixture");
    (hist, pins)
}

fn corpus_entries() -> (PathBuf, Vec<clip::ClipEntry>) {
    let dir = scratch("corpus");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let (hist, pins) = write_corpus(&dir);
    let entries = clip::load_entries_in(&hist, &pins);
    (dir, entries)
}

fn corpus_menu() -> (PathBuf, Menu) {
    let (dir, entries) = corpus_entries();
    let menu = flex_rice::menu(clip::PROVIDER, vec![clip::tab_from_entries(&entries)]);
    (dir, menu)
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn rune(c: char) -> KeyEvent {
    press(KeyCode::Char(c))
}

fn nav(menu: &mut Menu) {
    menu.app.mode = Mode::Navigate;
}

// --- Provider ingest --------------------------------------------------------

#[test]
fn binary_garbage_is_cleaned_without_panicking() {
    let (dir, entries) = corpus_entries();
    let fulls: Vec<&str> = entries.iter().map(|entry| entry.full.as_str()).collect();
    assert!(
        fulls.contains(&"binary end��"),
        "NUL/0x1F/ESC stripped, tab expanded, invalid bytes lossy: {fulls:?}"
    );
    assert!(
        !fulls.iter().any(|line| line.is_empty()),
        "empty lines dropped"
    );
    assert!(
        !fulls.iter().any(|line| line.contains('\0')),
        "no NUL survives"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn glob_labels_render_literally_and_resolve_losslessly() {
    let (dir, entries) = corpus_entries();
    let glob = entries
        .iter()
        .find(|entry| entry.full.contains("*[abc]?"))
        .expect("glob entry ingested");
    assert!(
        glob.row.label.contains("*[abc]?"),
        "glob chars literal in label: {:?}",
        glob.row.label
    );
    let (hist, pins) = (dir.join("cliphist"), dir.join("cliphist.pins"));
    assert_eq!(
        clip::resolve_in(&hist, &pins, glob.row.id.as_str()).as_deref(),
        Some(glob.full.as_str()),
        "hash id resolves back to the exact stored line"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_truncates_at_120_chars_but_full_is_kept() {
    let (dir, entries) = corpus_entries();
    let long = entries
        .iter()
        .find(|entry| entry.full.len() > clip::PREVIEW_CHARS)
        .expect("long entry ingested");
    assert_eq!(
        long.row.label.chars().count(),
        clip::PREVIEW_CHARS,
        "preview is exactly 120 chars"
    );
    assert_eq!(long.full.chars().count(), 200, "full text preserved");
    assert_eq!(
        long.row.id.as_str(),
        content_hash_hex(&long.full).as_str(),
        "Q2: id is the content-hash hex"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pins_first_with_meta_then_history_newest_first() {
    let (dir, entries) = corpus_entries();
    let labels: Vec<&str> = entries
        .iter()
        .map(|entry| entry.row.label.as_str())
        .collect();
    assert_eq!(
        &labels[..3],
        &["pinned one", "alpha<NEWLINE>beta gamma", "*zpinned*"],
        "pins first in file order: {labels:?}"
    );
    for entry in &entries[..3] {
        assert!(entry.pinned);
        assert_eq!(
            entry.row.meta.as_deref(),
            Some(clip::PINNED_META),
            "pins carry the bash 📌 mark"
        );
    }
    assert_eq!(
        &labels[3..],
        &[
            "second entry",
            "terminal tools",
            &"L".repeat(clip::PREVIEW_CHARS),
            "glob *[abc]? {x,y}",
            "binary end��",
            "first entry",
        ],
        "history newest-first (tac), deduped against pins: {labels:?}"
    );
    assert!(
        entries[3..].iter().all(|entry| !entry.pinned),
        "history rows are unpinned"
    );
    assert!(
        entries[3..].iter().all(|entry| entry.row.meta.is_none()),
        "history rows carry no meta"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn action_ids_are_content_hashes_and_widths_are_precomputed() {
    let (dir, entries) = corpus_entries();
    for entry in &entries {
        assert_eq!(
            entry.row.id.as_str(),
            content_hash_hex(&entry.full).as_str(),
            "Q2 hash id for {:?}",
            entry.full
        );
        assert_eq!(
            entry.measured.width,
            width::str_width(&entry.row.label),
            "precomputed width matches label for {:?}",
            entry.row.label
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_files_yield_no_rows_without_panicking() {
    let missing = scratch("missing");
    let _ = std::fs::remove_dir_all(&missing);
    assert!(clip::load_entries_in(&missing.join("h"), &missing.join("p")).is_empty());
    assert_eq!(clip::tab_from_entries(&[]).rows, []);
}

#[test]
fn clip_tab_is_standard_deletable() {
    let (_, entries) = corpus_entries();
    let tab = clip::tab_from_entries(&entries);
    assert_eq!(tab.name, clip::TAB_NAME);
    assert!(tab.bare_rows, "clip uses bare rows");
    assert!(tab.deletable, "clip opts into the Delete+confirm flow");
}

#[test]
fn env_overrides_select_the_store() {
    // Serialised with the executor tests below (shared process env).
    let _env = exec_env_lock();
    let dir = scratch("env");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let (hist, pins) = write_corpus(&dir);
    std::env::set_var(clip::HIST_ENV, &hist);
    std::env::set_var(clip::PINS_ENV, &pins);
    let entries = clip::load_entries();
    std::env::remove_var(clip::HIST_ENV);
    std::env::remove_var(clip::PINS_ENV);
    assert_eq!(entries.len(), 9, "env store loads 3 pins + 6 history");
    assert_eq!(entries[0].full, "pinned one");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn decode_restores_newlines_for_copy() {
    assert_eq!(
        clip::decode("alpha<NEWLINE>beta gamma"),
        "alpha\nbeta gamma"
    );
}

// --- Key-seq replays ----------------------------------------------------------

#[test]
fn keyseq_filter_then_enter_selects_with_hash_id() {
    let (dir, mut menu) = corpus_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(
        &mut menu,
        &[
            rune('t'),
            rune('e'),
            rune('r'),
            rune('m'),
            press(KeyCode::Enter),
        ],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Select);
    let row = menu.app.focused_row().expect("focused row");
    assert_eq!(row.label, "terminal tools");
    assert_eq!(
        row.id.as_str(),
        content_hash_hex("terminal tools").as_str(),
        "ACTION: clip <hash> carries the content hash"
    );
    assert_eq!(menu.provider, "clip");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn normal_m_types_into_filter_nav_m_toggles_pin() {
    let (dir, mut menu) = corpus_menu();
    let base = run::test_base();
    // NORMAL `m` must keep editing the filter (clipboard text has m's).
    let outcome = run::replay_keys(&mut menu, &[rune('m')], base);
    assert_eq!(outcome, KeyOutcome::Consumed);
    assert_eq!(
        menu.app.active_tab().expect("tab").state.filter,
        "m",
        "NORMAL m types"
    );
    menu.app.active_tab_mut().expect("tab").state.filter.clear();
    // NAVIGATE `m` on the deletable clip tab exits with TOGGLE.
    nav(&mut menu);
    let outcome = run::replay_keys(&mut menu, &[rune('m')], base);
    assert_eq!(outcome, KeyOutcome::Toggle, "NAVIGATE m toggles pin");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nav_m_still_marks_on_non_deletable_tabs() {
    let mut menu = flex_rice::menu(
        "theme",
        vec![flex_core::Tab::with_rows(
            "t",
            vec![flex_core::Row::new(flex_core::RowId::new("a"), "alpha")],
        )],
    );
    nav(&mut menu);
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[rune('m')], base);
    assert_eq!(outcome, KeyOutcome::Consumed, "legacy mark path kept");
    assert!(
        menu.app.active_state().expect("state").marked.contains(&0),
        "row marked in place"
    );
}

#[test]
fn delete_arms_confirm_then_deletes_esc_cancels() {
    let (dir, mut menu) = corpus_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Delete)], base);
    assert_eq!(outcome, KeyOutcome::Consumed, "first Delete arms");
    assert!(
        menu.app.active_state().expect("state").confirm_pending,
        "confirm armed in-TUI (bash y/N parity)"
    );
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Delete)], base);
    assert_eq!(outcome, KeyOutcome::Delete, "second Delete confirms");
    // Fresh menu: Delete then Esc cancels the pending confirm.
    let (dir_b, mut menu_b) = corpus_menu();
    let outcome = run::replay_keys(
        &mut menu_b,
        &[press(KeyCode::Delete), press(KeyCode::Esc)],
        base,
    );
    assert_eq!(outcome, KeyOutcome::Consumed, "Esc cancels, no delete");
    assert!(
        !menu_b.app.active_state().expect("state").confirm_pending,
        "pending cleared"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[test]
fn keyseq_esc_cancels_with_no_action() {
    let (dir, mut menu) = corpus_menu();
    let base = run::test_base();
    let outcome = run::replay_keys(&mut menu, &[press(KeyCode::Esc)], base);
    assert_eq!(outcome, KeyOutcome::Quit(EXIT_CANCELLED));
    let _ = std::fs::remove_dir_all(&dir);
}

// --- Clip golden ---------------------------------------------------------------

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
fn clip_default_view_golden_at_80x24() {
    let (dir, mut menu) = corpus_menu();
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
    // With top filter chrome: y=0 margin, y=1 filter, y=2 separator, y=3 indicator, y=4 first entry
    assert_eq!(row_text(&buf, 0, 80).trim(), "", "row 0 is top margin");
    assert!(
        row_text(&buf, 1, 80).contains('›'),
        "row 1 has top filter prompt"
    );
    assert!(
        row_text(&buf, 1, 80).contains("Search"),
        "row 1 has search placeholder"
    );
    assert!(row_text(&buf, 2, 80).contains('╌'), "row 2 has separator");
    let first_y = 3 + flex_core::render::LIST_INDICATOR_ROWS / 2;
    assert!(
        row_text(&buf, first_y, 80).starts_with('░'),
        "row 4 is the first list row"
    );
    let mut all = String::new();
    for y in 0..24 {
        all.push_str(&row_text(&buf, y, 80));
    }
    assert!(all.contains('›'), "filter line is present");
    assert!(!all.contains("Pinned"), "bare rows hide meta");
    let _ = std::fs::remove_dir_all(&dir);
}

// --- FLEX_TEST seed extension ---------------------------------------------------

#[test]
fn flex_test_replays_are_deterministic_across_bases() {
    assert_ne!(backend::FLEX_TEST_SEED, 0);
    let script = [rune('t'), rune('e'), press(KeyCode::Enter)];
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
    let (dir_a, mut first) = corpus_menu();
    let (dir_b, mut second) = corpus_menu();
    assert_eq!(run_script(&mut first), run_script(&mut second));
    assert_eq!(
        first.app.focused_row().expect("row").id,
        second.app.focused_row().expect("row").id
    );
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

// --- Bash parity probe (live store; skips when absent) ---------------------------

/// Compare the Rust ingest against the retired bash `build_clip_rows`
/// cleaning pipeline on reference data (`$CLIPHIST_FILE` or
/// `~/.cache/cliphist`).
///
/// Bash assoc-array pin order is unspecified, so sets are compared sorted
/// while the Rust side additionally asserts pins-first + history order.
#[test]
fn parity_with_bash_pipeline_on_reference_data() {
    // Serialised with the executor tests: they mutate/restore `CLIPHIST_FILE`
    // and `HOME` (and delete their fixtures) while this probe reads them, so
    // without the lock it can compare bash against a sibling's vanished store.
    let _env = exec_env_lock();
    let home = std::env::var("HOME").unwrap_or_default();
    let hist = std::env::var(clip::HIST_ENV).unwrap_or_else(|_| format!("{home}/.cache/cliphist"));
    let pins =
        std::env::var(clip::PINS_ENV).unwrap_or_else(|_| format!("{home}/.cache/cliphist.pins"));
    if !Path::new(&hist).is_file() {
        eprintln!("parity probe: no history file at {hist}, skipping");
        return;
    }
    let script = format!(
        "{{ if [[ -s \"{pins}\" ]]; then tr -d '\\000\\037\\033' < \"{pins}\" | tr '\\t' ' '; fi; \
           if [[ -s \"{hist}\" ]]; then tac \"{hist}\"; fi; }} \
         | tr -d '\\000\\037\\033' | tr '\\t' ' ' | LC_ALL=C grep -av '^$' || true"
    );
    let bash_out = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("bash pipeline runs");
    assert!(bash_out.status.success());
    // Normalize through the same lossy UTF-8 step the provider applies:
    // bash preserves invalid bytes raw while Rust surfaces U+FFFD
    // (documented lossy contract, same as the launch provider). What the
    // probe pins is everything else: cleaning, dedup, and ordering.
    let mut bash_lines: Vec<String> = bash_out
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect();
    bash_lines.sort();
    bash_lines.dedup();
    let entries = clip::load_entries_in(Path::new(&hist), Path::new(&pins));
    let mut rust_lines: Vec<String> = entries.iter().map(|entry| entry.full.clone()).collect();
    rust_lines.sort();
    rust_lines.dedup();
    eprintln!(
        "parity probe: bash {} unique lines, rust {} entries",
        bash_lines.len(),
        rust_lines.len()
    );
    assert_eq!(rust_lines, bash_lines, "cleaned line sets must match bash");
    // Rust-side order invariants (bash pins-first + tac history).
    let pin_count = entries.iter().take_while(|entry| entry.pinned).count();
    assert!(
        entries.iter().skip(pin_count).all(|entry| !entry.pinned),
        "all pins precede history"
    );
}

// --- Resolve lookup (B-021) ------------------------------------------------------

/// An unknown hash resolves to nothing through the library resolver. The
/// `main`-owned single `flex: error:` prefix now applies at the executor
/// boundary, so the resolver itself just returns `None`.
#[test]
fn unknown_hash_resolves_to_none() {
    let (dir, _) = corpus_entries();
    let (hist, pins) = (dir.join("cliphist"), dir.join("cliphist.pins"));
    assert!(
        clip::resolve_in(&hist, &pins, "deadbeef").is_none(),
        "an id absent from the store resolves to None"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --- Executor (`flex-rice/src/exec/clip.rs`) ----------------------------------
//
// The clip port of the `exec::shot` template: `ClipAction::parse` validates
// the id (`^[0-9a-f]+$`, plus the shared `noop` short-circuit), `execute`
// resolves the hash in-process via the library resolver and runs the
// copy/delete/toggle steps through the stub-`PATH` tool seam
// (`wl-copy` piped, `notify-send` inherited) plus exact-line store edits
// (`CLIPHIST_FILE`/`CLIPHIST_PINS`, empty values fall back like the
// wrapper's `${VAR:-default}`). No TUI, no pty: stub `PATH` + scratch
// stores. New process-env mutations are held under `EXEC_ENV_LOCK` (shared
// with the one pre-existing env-mutating test above).
//
// Like the shot suite, the popup guard itself lives in the shared runner
// (covered by `popup.rs`), with one binary-level re-exec probe below.

/// Serialises the executor tests below: each mutates `CLIPHIST_FILE` /
/// `CLIPHIST_PINS` (the seams `execute` reads) and the harness runs tests in
/// parallel, so the mutations are held under one lock with restores in
/// [`ExecEnvGuard`].
static EXEC_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The process-env lock with poison recovery: a panicking sibling must not
/// fail every other holder with a `PoisonError` that masks the real failure
/// (its [`ExecEnvGuard`]s already restored the env during unwinding). The
/// parity probe below also holds this so it never reads a sibling's fixture.
fn exec_env_lock() -> std::sync::MutexGuard<'static, ()> {
    EXEC_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Save/restore one process env var around an executor call.
struct ExecEnvGuard {
    key: &'static str,
    old: Option<String>,
}

fn set_exec_env(key: &'static str, value: &std::path::Path) -> ExecEnvGuard {
    let old = std::env::var(key).ok();
    std::env::set_var(key, value);
    ExecEnvGuard { key, old }
}

impl Drop for ExecEnvGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Write an executable stub script.
fn write_exe(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("stub script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

fn exec_stub_path(dir: &std::path::Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// `wl-copy` stub capturing stdin to `copied.bin` plus `notify-send` stub,
/// both appending `argv`-joined call lines (the wrapper-parity shape).
fn install_clip_tool_stubs(dir: &std::path::Path) {
    write_exe(
        &dir.join("wl-copy"),
        &format!(
            "#!/usr/bin/env bash\ncat > \"{}/copied.bin\"\necho \"wl-copy $@\" >> \"{}/calls.log\"\n",
            dir.display(),
            dir.display()
        ),
    );
    write_exe(
        &dir.join("notify-send"),
        &format!(
            "#!/usr/bin/env bash\necho \"notify-send $@\" >> \"{}/calls.log\"\n",
            dir.display()
        ),
    );
}

/// Scratch store + stubs for one executor case: returns the dir, the store
/// paths, and the stub `PATH`. Callers hold `EXEC_ENV_LOCK` and set the two
/// store env vars themselves (so the test body shows the seam wiring).
fn setup_exec_case(name: &str, hist: &[u8], pins: &[u8]) -> (PathBuf, PathBuf, PathBuf, String) {
    let dir = scratch(&format!("exec-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stub dir");
    install_clip_tool_stubs(&dir);
    let hist_path = dir.join("cliphist");
    let pins_path = dir.join("cliphist.pins");
    std::fs::write(&hist_path, hist).expect("hist");
    std::fs::write(&pins_path, pins).expect("pins");
    let path_env = exec_stub_path(&dir);
    (dir, hist_path, pins_path, path_env)
}

fn read_call_log(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

/// Full copy matrix: placeholder, glob, and long entries all copy decoded
/// bytes through the `wl-copy` seam with one notify each, stores untouched.
#[test]
fn executor_copy_matrix_copies_decoded_bytes_through_wl_copy() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let long = "L".repeat(200);
    let raws = [
        "alpha<NEWLINE>beta gamma".to_string(),
        "glob *[abc]? {x,y}".to_string(),
        long.clone(),
    ];
    let mut hist = String::new();
    for raw in &raws {
        hist.push_str(raw);
        hist.push('\n');
    }
    let (dir, hist_path, pins_path, path_env) = setup_exec_case("copy", hist.as_bytes(), b"");
    let _hist_env = set_exec_env(clip::HIST_ENV, &hist_path);
    let _pins_env = set_exec_env(clip::PINS_ENV, &pins_path);
    for raw in &raws {
        let id = content_hash_hex(raw);
        let report = flex_rice::exec::clip::execute(ClipOp::Copy, &id, Some(&path_env))
            .expect("copy succeeds");
        assert_eq!(report.op, Some(ClipOp::Copy));
        assert_eq!(report.raw.as_deref(), Some(raw.as_str()));
        assert_eq!(report.pinned, None, "copy reports no pin state");
        let copied = std::fs::read(dir.join("copied.bin")).expect("wl-copy stdin");
        let mut expected = clip::decode(raw).into_bytes();
        expected.push(b'\n');
        assert_eq!(
            copied, expected,
            "placeholder decoded, trailing newline kept"
        );
    }
    assert_eq!(
        read_call_log(&dir),
        format!(
            "wl-copy \n\
             notify-send -a Cliphist Copied to clipboard alpha<NEWLINE>beta gamma\n\
             wl-copy \n\
             notify-send -a Cliphist Copied to clipboard glob *[abc]? {{x,y}}\n\
             wl-copy \n\
             notify-send -a Cliphist Copied to clipboard {}\n",
            "L".repeat(50),
        ),
        "one piped wl-copy plus one notify per entry; 50-byte preview on the long one \
         (describe: `wl-copy`, `notify-send -a Cliphist <summary> <preview>`)",
    );
    assert_eq!(
        std::fs::read(&hist_path).expect("hist"),
        hist.as_bytes(),
        "copy never touches the history file"
    );
    assert_eq!(
        std::fs::read(&pins_path).expect("pins"),
        b"",
        "copy never touches the pins file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Delete removes the exact line from both files and notifies once.
#[test]
fn executor_delete_scrubs_both_store_files() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let (dir, hist_path, pins_path, path_env) = setup_exec_case(
        "delete",
        b"doomed entry\nkeep me\ndoomextended\n",
        b"doomed entry\nkeep pin\n",
    );
    let _hist_env = set_exec_env(clip::HIST_ENV, &hist_path);
    let _pins_env = set_exec_env(clip::PINS_ENV, &pins_path);
    let id = content_hash_hex("doomed entry");
    let report = flex_rice::exec::clip::execute(ClipOp::Delete, &id, Some(&path_env))
        .expect("delete succeeds");
    assert_eq!(report.op, Some(ClipOp::Delete));
    assert_eq!(report.raw.as_deref(), Some("doomed entry"));
    assert_eq!(report.pinned, None, "delete reports no pin state");
    assert_eq!(
        std::fs::read(&hist_path).expect("hist"),
        b"keep me\ndoomextended\n",
        "exact-line match only, rest kept byte-identical"
    );
    assert_eq!(
        std::fs::read(&pins_path).expect("pins"),
        b"keep pin\n",
        "pins scrubbed too"
    );
    assert_eq!(
        read_call_log(&dir),
        "notify-send -a Cliphist Deleted doomed entry\n",
        "delete is one notify, no wl-copy \
         (describe: `remove <raw> from history and pins`)"
    );
    assert!(
        !dir.join("cliphist.tmp").exists() && !dir.join("cliphist.pins.tmp").exists(),
        "tmp rewrites are renamed away"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Delete skips a missing file instead of creating it (the wrapper's
/// `[[ -f … ]]` guard).
#[test]
fn executor_delete_skips_a_missing_history_file() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let (dir, hist_path, pins_path, path_env) =
        setup_exec_case("delete-missing", b"doomed entry\n", b"doomed entry\n");
    std::fs::remove_file(&hist_path).expect("remove hist");
    let _hist_env = set_exec_env(clip::HIST_ENV, &hist_path);
    let _pins_env = set_exec_env(clip::PINS_ENV, &pins_path);
    let id = content_hash_hex("doomed entry");
    flex_rice::exec::clip::execute(ClipOp::Delete, &id, Some(&path_env)).expect("delete succeeds");
    assert!(!hist_path.exists(), "missing history stays missing");
    assert_eq!(
        std::fs::read(&pins_path).expect("pins"),
        b"",
        "pins still scrubbed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Toggle appends an unpinned entry, then removes it again; reports carry
/// the post-run pin state.
#[test]
fn executor_toggle_pins_then_unpins() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let (dir, hist_path, pins_path, path_env) =
        setup_exec_case("toggle", b"flip me\nother\n", b"existing pin\n");
    let _hist_env = set_exec_env(clip::HIST_ENV, &hist_path);
    let _pins_env = set_exec_env(clip::PINS_ENV, &pins_path);
    let id = content_hash_hex("flip me");
    let report =
        flex_rice::exec::clip::execute(ClipOp::Toggle, &id, Some(&path_env)).expect("pin succeeds");
    assert_eq!(report.pinned, Some(true), "now pinned");
    assert_eq!(
        std::fs::read(&pins_path).expect("pins"),
        b"existing pin\nflip me\n",
        "toggle appends the pin"
    );
    let report = flex_rice::exec::clip::execute(ClipOp::Toggle, &id, Some(&path_env))
        .expect("unpin succeeds");
    assert_eq!(report.pinned, Some(false), "now unpinned");
    assert_eq!(
        std::fs::read(&pins_path).expect("pins"),
        b"existing pin\n",
        "toggle removes the pin"
    );
    assert_eq!(
        read_call_log(&dir),
        "notify-send -a Cliphist Pinned to history flip me\n\
         notify-send -a Cliphist Unpinned flip me\n",
        "pin then unpin notifies each way (describe: `append <raw> to pins` / \
         `remove <raw> from pins`)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Toggle creates a missing pins file including parent dirs (the wrapper's
/// `mkdir -p` + `touch`).
#[test]
fn executor_toggle_creates_a_missing_pins_file() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let (dir, hist_path, _pins_path, path_env) =
        setup_exec_case("toggle-new", b"flip me\n", b"existing pin\n");
    let nested = dir.join("nested").join("cliphist.pins");
    let _hist_env = set_exec_env(clip::HIST_ENV, &hist_path);
    let _pins_env = set_exec_env(clip::PINS_ENV, &nested);
    let id = content_hash_hex("flip me");
    let report =
        flex_rice::exec::clip::execute(ClipOp::Toggle, &id, Some(&path_env)).expect("pin succeeds");
    assert_eq!(report.pinned, Some(true));
    assert_eq!(
        std::fs::read(&nested).expect("pins"),
        b"flip me\n",
        "missing pins file created with the entry"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `noop` placeholder exits 0 before resolving or touching anything
/// (B-026): even unusable store paths cannot fail it, for every op.
#[test]
fn executor_noop_short_circuits_before_store_and_tools() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let dir = scratch("exec-noop");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stub dir");
    let missing_hist = dir.join("no-such-hist");
    let missing_pins = dir.join("no-such-pins");
    let _hist_env = set_exec_env(clip::HIST_ENV, &missing_hist);
    let _pins_env = set_exec_env(clip::PINS_ENV, &missing_pins);
    for op in [ClipOp::Copy, ClipOp::Delete, ClipOp::Toggle] {
        let report = flex_rice::exec::clip::execute(op, "noop", Some(&exec_stub_path(&dir)))
            .expect("noop is Ok");
        assert_eq!(report.op, None, "{op:?}: no op ran");
        assert_eq!(report.raw, None, "{op:?}: nothing resolved");
        assert_eq!(report.pinned, None, "{op:?}: no pin state");
    }
    assert!(
        !missing_hist.exists() && !missing_pins.exists(),
        "noop creates no store files"
    );
    assert!(!dir.join("calls.log").exists(), "noop spawns no tools");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed ids (`bad id`, mirroring the wrapper check) and well-formed but
/// unresolvable hashes (`unknown id`) are errors with no `flex:` prefix of
/// their own — the runner adds the single prefix at the binary boundary.
#[test]
fn executor_rejects_bad_and_unknown_ids_without_its_own_prefix() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let (dir, hist_path, pins_path, path_env) = setup_exec_case("bad-ids", b"real entry\n", b"");
    let _hist_env = set_exec_env(clip::HIST_ENV, &hist_path);
    let _pins_env = set_exec_env(clip::PINS_ENV, &pins_path);
    let ghost_id = content_hash_hex("ghost entry");
    let cases = [
        ("", "clip: bad id ''"),
        ("a/b", "clip: bad id 'a/b'"),
        ("a\nb", "clip: bad id 'a\nb'"),
        ("ABCDEF", "clip: bad id 'ABCDEF'"),
        ("deadbeef!", "clip: bad id 'deadbeef!'"),
        (ghost_id.as_str(), "clip: unknown id"),
    ];
    for op in [ClipOp::Copy, ClipOp::Delete, ClipOp::Toggle] {
        for (id, expected) in &cases {
            let err = flex_rice::exec::clip::execute(op, id, Some(&path_env))
                .expect_err("bad/unknown id must fail");
            let message = format!("{err:#}");
            assert!(
                message.starts_with(expected),
                "{op:?} {id:?}: unexpected message: {message:?}"
            );
            assert!(
                !message.contains("flex:"),
                "no runner prefix below the runner: {message:?}"
            );
        }
    }
    assert!(
        !dir.join("calls.log").exists(),
        "no id failure reaches the tools"
    );
    assert_eq!(
        std::fs::read(&hist_path).expect("hist"),
        b"real entry\n",
        "no id failure touches the store"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A failing or missing tool is a loud error, never a quiet cancel: clip
/// has no `slurp`-like user-cancellable step, so every tool failure exits
/// non-zero through the runner (the menu-cancel 130 lives in the binary's
/// `Outcome::Cancelled` arm, shared with shot, and needs a pty).
#[test]
fn executor_tool_failure_is_a_loud_error_not_a_quiet_cancel() {
    use flex_rice::exec::clip::ClipOp;
    let _env = exec_env_lock();
    let (dir, hist_path, pins_path, path_env) = setup_exec_case("tool-fail", b"entry one\n", b"");
    write_exe(&dir.join("wl-copy"), "#!/usr/bin/env bash\nexit 3\n");
    let _hist_env = set_exec_env(clip::HIST_ENV, &hist_path);
    let _pins_env = set_exec_env(clip::PINS_ENV, &pins_path);
    let id = content_hash_hex("entry one");
    let err = flex_rice::exec::clip::execute(ClipOp::Copy, &id, Some(&path_env))
        .expect_err("a failing wl-copy must fail");
    assert_eq!(format!("{err:#}"), "clip: wl-copy failed");
    // Missing tool with a tool-less PATH: resolution fails cleanly.
    let bare = dir.join("bare");
    std::fs::create_dir_all(&bare).expect("bare dir");
    let err = flex_rice::exec::clip::execute(ClipOp::Copy, &id, Some(&bare.display().to_string()))
        .expect_err("a missing wl-copy must fail");
    assert_eq!(format!("{err:#}"), "clip: wl-copy not found on PATH");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Binary level without a pty: the menu builds from the scratch store, then
/// the select loop cannot start, so the binary exits 1 with exactly one
/// `flex: error:` prefix (the runner owns it; executor errors carry none).
/// `setsid` detaches the controlling terminal so no harness tty can satisfy
/// the TUI init.
#[test]
fn binary_errors_carry_a_single_prefix() {
    let home = scratch("clip-binary-error");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".cache")).expect("cache dir");
    std::fs::write(home.join(".cache/cliphist"), b"entry one\n").expect("hist");
    let output = std::process::Command::new("setsid")
        .arg(env!("CARGO_BIN_EXE_flex-clip"))
        .env("HOME", &home)
        .env("POPUP_KITTY", "1")
        .env_remove("CLIPHIST_FILE")
        .env_remove("CLIPHIST_PINS")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-clip without a pty");
    let _ = std::fs::remove_dir_all(&home);
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
    let path_env = exec_stub_path(&dir);
    let saved_path = std::env::var("PATH").ok();
    std::env::set_var("PATH", &path_env);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("scratch HOME");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_flex-clip"))
        .env("HOME", &home)
        .env_remove("POPUP_KITTY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run flex-clip outside a popup");
    match saved_path {
        Some(value) => std::env::set_var("PATH", value),
        None => std::env::remove_var("PATH"),
    }
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
