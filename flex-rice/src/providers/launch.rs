//! Launch provider (M2): `.desktop` scan into rows for `flex launch`.
//!
//! Row-set parity with the retired `scripts/.config/scripts/app-cache.sh`
//! (the script has since been removed; these rules are the source of truth):
//!
//! - Directories (in order): `/usr/share/applications`, then
//!   `$HOME/.local/share/applications`. Later directories win on duplicate
//!   desktop-ids (user overrides system), matching the bash assoc-array.
//! - Parsed keys: `Name` (first wins), `Exec` (first wins), `Terminal`
//!   (last wins), `NoDisplay`/`Hidden` (last wins, skip when `true`).
//!   `OnlyShowIn`/`NotShowIn` are parsed and ignored (shown everywhere,
//!   exactly like the bash version which never reads them). `Icon` (first
//!   wins) is kept for [`find_app`]: notification server `app_icon` hints
//!   name-app resolution uses it.
//! - Skipped (never panic): unreadable files, missing `Name`/`Exec`,
//!   `NoDisplay`/`Hidden` set. Only the unreadable/field-less class is
//!   malformed and logs to stderr; `NoDisplay`/`Hidden` entries are skipped
//!   by design and stay silent (202 of them on the reference host).
//! - Order: byte-lexicographic sort of the `name\texec\tterm` line, where
//!   `exec` is the field-code-stripped form — the same bytes `sort` sees in
//!   the bash cache (locale `sort` may differ for non-ASCII names; ASCII
//!   order, the reference-data case, is identical).
//!
//! `Exec` is stored **raw** (`%U`/`%F`/… preserved): the launch executor
//! strips field codes exactly like `launch_app_row` does. Row ids
//! are [`entry_id`] hashes of the desktop-id — space-free, because the
//! `ACTION:` protocol delimits the id on whitespace and a `.desktop` file
//! may legally be named `My App.desktop` (B-021). The launch executor maps
//! such an id back to the desktop-id with the library resolver, then
//! re-resolves it to the `.desktop` file to recover `Exec` + `Terminal`.

use std::path::{Path, PathBuf};

use flex_core::{content_hash_hex, Row, RowId, Tab};

/// Provider name for the `ACTION:` line.
pub const PROVIDER: &str = "launch";
/// Tab title (matches the bash `Launchers` tab).
pub const TAB_NAME: &str = "Launchers";
/// Meta text for terminal apps (matches the bash `Terminal` meta).
pub const TERMINAL_META: &str = "Terminal";

/// One parsed `.desktop` entry (raw `Exec`, `%` codes preserved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// Desktop-id (`firefox.desktop`); the [`Row`] id is [`entry_id`] of it.
    pub id: String,
    /// Display name (`Name`).
    pub name: String,
    /// Raw command line (`Exec`, field codes intact for the wrapper).
    pub exec: String,
    /// Whether `Terminal` is `true` (last occurrence wins).
    pub terminal: bool,
    /// Icon name or path (`Icon`, first occurrence). Only used to resolve a
    /// notification `app_icon` hint back to an entry ([`find_app`]).
    pub icon: Option<String>,
}

/// Application directories in bash order (system, then user override).
#[must_use]
pub fn app_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/share/applications")];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            dirs.push(PathBuf::from(home).join(".local/share/applications"));
        }
    }
    dirs
}

/// Scan `dirs` (in order) into sorted entries.
///
/// Later directories overwrite earlier ones on duplicate desktop-ids.
/// Never panics and never fails: unreadable directories/files are skipped
/// (files log to stderr); the result may be empty.
#[must_use]
pub fn scan_dirs(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    scan(dirs, &|path| {
        flex_core::diag::warn(&format!(
            "flex: launch: skipping malformed entry {}",
            path.display()
        ));
    })
}

/// [`scan_dirs`] without diagnostics, for the id lookup: the entry set must
/// be identical, but a wrapper-driven lookup has nothing to report.
fn scan_silently(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    scan(dirs, &|_| {})
}

/// Shared scan: the entry set and its ordering live here once.
fn scan(dirs: &[PathBuf], report_malformed: &dyn Fn(&Path)) -> Vec<DesktopEntry> {
    use std::collections::BTreeMap;
    let mut seen: BTreeMap<String, DesktopEntry> = BTreeMap::new();
    for dir in dirs {
        for path in desktop_files(dir) {
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            match read_entry(&path) {
                Parsed::Entry {
                    name,
                    exec,
                    terminal,
                    icon,
                } => {
                    seen.insert(
                        id.clone(),
                        DesktopEntry {
                            id,
                            name,
                            exec,
                            terminal,
                            icon,
                        },
                    );
                }
                // Skipped by design (`NoDisplay`/`Hidden`): not a problem to
                // report, or `flex launch` floods stderr on every open.
                Parsed::Hidden => {}
                Parsed::Malformed => report_malformed(&path),
            }
        }
    }
    let mut entries: Vec<DesktopEntry> = seen.into_values().collect();
    // `sort_by_cached_key`, not `sort_by_key`: the key allocates three
    // times (`sort_line` builds a `String`, `strip_field_codes` a
    // `Vec<char>` and another `String`) and `sort_by_key` re-runs it for
    // every comparison (B-023).
    entries.sort_by_cached_key(sort_line);
    entries
}

/// Load rows from the real application directories (system + user).
#[must_use]
pub fn load() -> Vec<Row> {
    rows(&scan_dirs(&app_dirs()))
}

/// Map entries to [`Row`]s: id = [`entry_id`], label = `Name`,
/// meta = `Terminal` on terminal apps (data parity with the bash meta).
#[must_use]
pub fn rows(entries: &[DesktopEntry]) -> Vec<Row> {
    entries
        .iter()
        .map(|entry| {
            let id = RowId::new(entry_id(&entry.id));
            if entry.terminal {
                Row::with_meta(id, entry.name.clone(), TERMINAL_META)
            } else {
                Row::new(id, entry.name.clone())
            }
        })
        .collect()
}

/// Row/action id for a desktop-id: its content hash.
///
/// A desktop-id is a file name, so it may contain whitespace
/// (`My App.desktop`), and the `ACTION:` protocol delimits the id with
/// spaces — the wrapper would read `My`. Hashing keeps the id a single
/// token, exactly like `clip`/`wallpaper`; [`resolve_id`] maps it back.
#[must_use]
pub fn entry_id(desktop_id: &str) -> String {
    content_hash_hex(desktop_id)
}

/// Resolve a row id back to its desktop-id (the launch executor's
/// in-process lookup, like `clip::resolve`).
///
/// Searched over the same entry set [`rows`] is built from, so only ids
/// that can actually be on screen resolve. Returns `None` for unknown ids
/// and for ids containing `/` (path traversal is never resolved).
#[must_use]
pub fn resolve_id(hash: &str) -> Option<String> {
    resolve_id_in(&app_dirs(), hash)
}

/// [`resolve_id`] over explicit directories (tests/fixtures).
#[must_use]
pub fn resolve_id_in(dirs: &[PathBuf], hash: &str) -> Option<String> {
    if hash.is_empty() || hash.contains('/') {
        return None;
    }
    scan_silently(dirs)
        .into_iter()
        .find(|entry| entry_id(&entry.id) == hash)
        .map(|entry| entry.id)
}

/// Placeholder row id for an empty scan is the shared `noop` (see
/// [`super::empty_row`]); this is its bash-exact label.
pub const NO_APPS_LABEL: &str = "(No applications found)";

/// Build the `Launchers` tab: bare-rows mode, non-deletable rows.
///
/// An empty scan yields the `noop` placeholder row rather than a blank menu
/// (B-026): `Enter` on it is a no-op in the launch executor.
#[must_use]
pub fn launch_tab() -> Tab {
    Tab::with_rows(TAB_NAME, tab_rows(&scan_dirs(&app_dirs())))
}

/// Build a `Launchers` tab from pre-scanned entries (tests/replays).
#[must_use]
pub fn tab_from_entries(entries: &[DesktopEntry]) -> Tab {
    Tab::with_rows(TAB_NAME, tab_rows(entries))
}

/// [`rows`] plus the empty-scan placeholder (B-026).
fn tab_rows(entries: &[DesktopEntry]) -> Vec<Row> {
    let mut rows = rows(entries);
    if rows.is_empty() {
        rows.push(super::empty_row(NO_APPS_LABEL));
    }
    rows
}

/// Resolve a desktop-id to its raw `(Exec, terminal)` pair.
///
/// User directory wins over system (bash overwrite order); `%` codes are
/// preserved for the wrapper to strip. Returns `None` for unknown ids or
/// ids containing `/` (path traversal is never resolved).
#[must_use]
pub fn find_exec(id: &str) -> Option<(String, bool)> {
    find_exec_in(&app_dirs(), id)
}

/// [`find_exec`] over explicit directories (tests/fixtures).
#[must_use]
pub fn find_exec_in(dirs: &[PathBuf], id: &str) -> Option<(String, bool)> {
    if id.is_empty() || id.contains('/') {
        return None;
    }
    // Later dirs win in `scan_dirs`; for lookup, earlier (user) hits win,
    // so search in reverse (user first).
    for dir in dirs.iter().rev() {
        let path = dir.join(id);
        if let Parsed::Entry { exec, terminal, .. } = read_entry(&path) {
            return Some((exec, terminal));
        }
    }
    None
}

/// Normalize an app name, desktop-id or icon for comparison: lowercase with
/// every non-alphanumeric character dropped (`"Crash Reporting System"` →
/// `"crashreportingsystem"`, `org.kde.Foo.desktop` → `orgkdefoodesktop`).
fn normalize_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Resolve an application *display name* to an installed desktop entry.
///
/// Notification servers hand over an application name, not an executable:
/// `"Crash Reporting System"` (a KDE notifyrc name) has no binary and no
/// desktop `Name`, only a shared `Icon=tools-report-bug`. [`find_app`] tries,
/// in order, the desktop-id (`firefox.desktop` / `firefox`), the `Name`
/// (`"Firefox"`), then — as a last resort — the `app_icon` hint matched
/// against the entry's `Icon`.
///
/// Only entries [`scan_dirs`] would show are considered, so `NoDisplay` and
/// `Hidden` entries never hijack the lookup.
#[must_use]
pub fn find_app(name: &str, icon: Option<&str>) -> Option<DesktopEntry> {
    find_app_in(&app_dirs(), name, icon)
}

/// [`find_app`] over explicit directories (tests/fixtures).
#[must_use]
pub fn find_app_in(dirs: &[PathBuf], name: &str, icon: Option<&str>) -> Option<DesktopEntry> {
    let wanted = normalize_key(name);
    if wanted.is_empty() {
        return None;
    }
    // Icon hints may be paths (`file:///…/icon.png`); only bare names match
    // an `Icon=` key.
    let icon_key = icon
        .filter(|hint| !hint.is_empty() && !hint.contains('/'))
        .map(normalize_key)
        .filter(|key| !key.is_empty());
    let entries = scan_silently(dirs);

    // 1. Desktop-id, with or without the `.desktop` suffix.
    if let Some(entry) = entries.iter().find(|entry| {
        let stem = entry.id.strip_suffix(".desktop").unwrap_or(&entry.id);
        normalize_key(&entry.id) == wanted || normalize_key(stem) == wanted
    }) {
        return Some(entry.clone());
    }

    // 2. Display name.
    if let Some(entry) = entries
        .iter()
        .find(|entry| normalize_key(&entry.name) == wanted)
    {
        return Some(entry.clone());
    }

    // 3. Icon hint (the notifyrc case: the display name matches nothing and
    // the shared `Icon=` is the only link to the entry).
    let icon_key = icon_key?;
    entries
        .iter()
        .find(|entry| {
            entry
                .icon
                .as_deref()
                .is_some_and(|entry_icon| normalize_key(entry_icon) == icon_key)
        })
        .cloned()
}

const QUOTE_CHAR: char = '"';
const ESCAPE_CHAR: char = '\\';
const FIELD_CODE_PREFIX: u8 = b'%';
const SPACE_BYTE: u8 = b' ';

const KEY_NAME: &str = "Name";
const KEY_EXEC: &str = "Exec";
const KEY_ICON: &str = "Icon";
const KEY_TERMINAL: &str = "Terminal";
const KEY_NO_DISPLAY: &str = "NoDisplay";
const KEY_HIDDEN: &str = "Hidden";
const VALUE_TRUE: &str = "true";

/// Strip `.desktop` field codes (` %U`, ` %F`, …) and surrounding spaces.
///
/// The regex is `sed -E 's/ %[A-Za-z]//g; s/^ *//; s/ *$//'`.
#[must_use]
pub fn strip_field_codes(exec: &str) -> String {
    // Byte scan without the old `chars().collect::<Vec<char>>`: field codes
    // are ASCII (`" %X"`), so operate on bytes and only decode once via the
    // output `String`. Non-ASCII bytes pass through untouched.
    let bytes = exec.as_bytes();
    let mut out = String::with_capacity(exec.len());
    let mut index = 0_usize;
    while index < bytes.len() {
        let is_code = bytes[index] == SPACE_BYTE
            && bytes.get(index + 1) == Some(&FIELD_CODE_PREFIX)
            && bytes.get(index + 2).is_some_and(u8::is_ascii_alphabetic);
        if is_code {
            index += 3;
            continue;
        }
        // ASCII fast path; multibyte sequences are pushed as whole chars.
        let c = exec[index..].chars().next().unwrap_or(' ');
        out.push(c);
        index += c.len_utf8();
    }
    // Trim without a second alloc when already trimmed.
    let trimmed = out.trim_matches(' ');
    if trimmed.len() == out.len() {
        out
    } else {
        trimmed.to_string()
    }
}

/// State of the `FreeDesktop` Exec line tokenizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TokenizerState {
    #[default]
    Unquoted,
    InQuotes,
    EscapedUnquoted,
    EscapedInQuotes,
}

#[derive(Debug, Default)]
struct TokenAccumulator {
    buffer: String,
    active: bool,
}

impl TokenAccumulator {
    fn mark_active(&mut self) {
        self.active = true;
    }

    fn push_char(&mut self, c: char) {
        self.buffer.push(c);
        self.active = true;
    }

    fn flush(&mut self, target: &mut Vec<String>) {
        if !self.active {
            return;
        }
        target.push(std::mem::take(&mut self.buffer));
        self.active = false;
    }
}

/// `FreeDesktop` Exec command line tokenizer.
#[derive(Debug, Default)]
pub struct ExecTokenizer {
    state: TokenizerState,
    accumulator: TokenAccumulator,
    tokens: Vec<String>,
}

impl ExecTokenizer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn tokenize(mut self, input: &str) -> Vec<String> {
        for ch in input.chars() {
            self.step(ch);
        }
        self.finalize();
        self.tokens
    }

    fn step(&mut self, ch: char) {
        match (self.state, ch) {
            (TokenizerState::Unquoted, ESCAPE_CHAR) => {
                self.state = TokenizerState::EscapedUnquoted;
                self.accumulator.mark_active();
            }
            (TokenizerState::InQuotes, ESCAPE_CHAR) => {
                self.state = TokenizerState::EscapedInQuotes;
                self.accumulator.mark_active();
            }
            (TokenizerState::Unquoted, QUOTE_CHAR) => {
                self.state = TokenizerState::InQuotes;
                self.accumulator.mark_active();
            }
            (TokenizerState::InQuotes, QUOTE_CHAR) => {
                self.state = TokenizerState::Unquoted;
                self.accumulator.mark_active();
            }
            (TokenizerState::EscapedUnquoted, c) => {
                self.accumulator.push_char(c);
                self.state = TokenizerState::Unquoted;
            }
            (TokenizerState::EscapedInQuotes, c) => {
                self.accumulator.push_char(c);
                self.state = TokenizerState::InQuotes;
            }
            (TokenizerState::Unquoted, c) if c.is_whitespace() => {
                self.accumulator.flush(&mut self.tokens);
            }
            (TokenizerState::Unquoted | TokenizerState::InQuotes, c) => {
                self.accumulator.push_char(c);
            }
        }
    }

    fn finalize(&mut self) {
        match self.state {
            TokenizerState::EscapedUnquoted | TokenizerState::EscapedInQuotes => {
                self.accumulator.push_char(ESCAPE_CHAR);
            }
            TokenizerState::Unquoted | TokenizerState::InQuotes => {}
        }
        self.accumulator.flush(&mut self.tokens);
    }
}

/// Parse an `Exec` command line into tokens according to the `FreeDesktop`
/// Desktop Entry Specification.
///
/// Handles double-quoted tokens, backslash escaping, and whitespace separation.
#[must_use]
pub fn parse_exec(exec: &str) -> Vec<String> {
    ExecTokenizer::default().tokenize(exec)
}

/// A parsed desktop command representing an executable program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl DesktopCommand {
    /// Parse an `Exec` command line according to the `FreeDesktop` Desktop Entry Specification.
    ///
    /// Field codes (`%f`, `%U`, etc.) are stripped first, then the remaining tokens
    /// are parsed handling quotes, whitespace, and escape sequences.
    ///
    /// Returns `None` if the resulting command is empty or has a blank executable program.
    #[must_use]
    pub fn parse(raw_exec: &str) -> Option<Self> {
        let stripped = strip_field_codes(raw_exec);
        let mut tokens = parse_exec(&stripped).into_iter();
        let program = tokens.next().filter(|p| !p.is_empty())?;
        let args = tokens.collect();
        Some(Self { program, args })
    }
}

/// Sorted `.desktop` (`*.desktop`, filename order) files under `dir`.
fn desktop_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "desktop"))
        .collect();
    files.sort();
    files
}

/// Outcome of parsing one `.desktop` file.
///
/// The three states are distinct on purpose: "hidden by design" and
/// "broken" are different facts, and only the second one deserves a
/// diagnostic ([B-020]).
///
/// [B-020]: ../../../Docs/Bug_tracking.md
#[derive(Debug, Clone, PartialEq, Eq)]
enum Parsed {
    /// Usable launcher entry: `Name` and `Exec` present and non-empty.
    Entry {
        /// Display name (`Name`, first occurrence).
        name: String,
        /// Raw command line (`Exec`, field codes intact).
        exec: String,
        /// `Terminal` flag (last occurrence wins).
        terminal: bool,
        /// `Icon` (first occurrence), kept for [`find_app`].
        icon: Option<String>,
    },
    /// Skipped by design: `NoDisplay=true` or `Hidden=true`. Silent.
    Hidden,
    /// Skipped because the file is unreadable or lacks `Name`/`Exec`.
    /// The only case worth a stderr diagnostic.
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopKey {
    Name,
    Exec,
    Icon,
    Terminal,
    NoDisplay,
    Hidden,
    Other,
}

impl DesktopKey {
    fn parse(key: &str) -> Self {
        match key {
            KEY_NAME => Self::Name,
            KEY_EXEC => Self::Exec,
            KEY_ICON => Self::Icon,
            KEY_TERMINAL => Self::Terminal,
            KEY_NO_DISPLAY => Self::NoDisplay,
            KEY_HIDDEN => Self::Hidden,
            _ => Self::Other,
        }
    }
}

#[derive(Default)]
struct EntryBuilder {
    name: Option<String>,
    exec: Option<String>,
    icon: Option<String>,
    terminal: bool,
    nodisplay: bool,
    hidden: bool,
}

impl EntryBuilder {
    fn apply_field(&mut self, key: DesktopKey, value: &str) {
        match key {
            DesktopKey::Name => {
                self.name.get_or_insert_with(|| value.to_string());
            }
            DesktopKey::Exec => {
                self.exec.get_or_insert_with(|| value.to_string());
            }
            DesktopKey::Icon => {
                if !value.is_empty() {
                    self.icon.get_or_insert_with(|| value.to_string());
                }
            }
            DesktopKey::Terminal => self.terminal = value == VALUE_TRUE,
            DesktopKey::NoDisplay => self.nodisplay = value == VALUE_TRUE,
            DesktopKey::Hidden => self.hidden = value == VALUE_TRUE,
            DesktopKey::Other => {}
        }
    }

    fn build(self) -> Parsed {
        if self.nodisplay || self.hidden {
            return Parsed::Hidden;
        }
        let Some(name) = self.name.filter(|v| !v.is_empty()) else {
            return Parsed::Malformed;
        };
        let Some(exec) = self.exec.filter(|v| !v.is_empty()) else {
            return Parsed::Malformed;
        };
        Parsed::Entry {
            name,
            exec,
            terminal: self.terminal,
            icon: self.icon,
        }
    }
}

/// Parse one `.desktop` file into a [`Parsed`] outcome.
///
/// Whole-file key scan (like the bash `grep -E` prefilter, not section
/// aware): first `Name`/`Exec` win, last `Terminal`/`NoDisplay`/`Hidden`
/// win. Lossy UTF-8: undecodable bytes become `U+FFFD` instead of failing
/// the whole scan.
fn read_entry(path: &Path) -> Parsed {
    let Ok(bytes) = std::fs::read(path) else {
        return Parsed::Malformed;
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut builder = EntryBuilder::default();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some((raw_key, value)) = line.split_once('=') else {
            continue;
        };
        builder.apply_field(DesktopKey::parse(raw_key), value);
    }
    builder.build()
}

/// Bash-cache sort line: `name\texec-stripped\tterm`.
fn sort_line(entry: &DesktopEntry) -> String {
    let term = if entry.terminal { "true" } else { "false" };
    format!("{}\t{}\t{term}", entry.name, strip_field_codes(&entry.exec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_field_codes_matches_bash_sed() {
        // `sed -E 's/ %[A-Za-z]//g; s/^ *//; s/ *$//'` on each sample.
        for (raw, stripped) in [
            ("firefox %U", "firefox"),
            ("myapp %F --foo", "myapp --foo"),
            ("run %u %i %c", "run"),
            ("no-codes --flag", "no-codes --flag"),
            ("  padded %U  ", "padded"),
            ("100% coverage", "100% coverage"),
            ("%U leading", "%U leading"),
        ] {
            assert_eq!(strip_field_codes(raw), stripped, "strip {raw:?}");
        }
    }

    #[test]
    fn parse_exec_handles_quotes_escapes_and_whitespace() {
        assert_eq!(parse_exec(""), Vec::<String>::new());
        assert_eq!(parse_exec("   "), Vec::<String>::new());
        assert_eq!(parse_exec("firefox"), vec!["firefox"]);
        assert_eq!(parse_exec("firefox %U"), vec!["firefox", "%U"]);
        assert_eq!(
            parse_exec("\"/opt/android-studio/bin/studio\""),
            vec!["/opt/android-studio/bin/studio"]
        );
        assert_eq!(
            parse_exec("\"/opt/android-studio/bin/studio\" %f"),
            vec!["/opt/android-studio/bin/studio", "%f"]
        );
        assert_eq!(
            parse_exec("myapp --title \"Hello World\" --flag"),
            vec!["myapp", "--title", "Hello World", "--flag"]
        );
        assert_eq!(
            parse_exec("\"/path with spaces/bin\" \"arg\\\"with\\\"quote\""),
            vec!["/path with spaces/bin", "arg\"with\"quote"]
        );
        assert_eq!(
            parse_exec("--opt=\"val with spaces\""),
            vec!["--opt=val with spaces"]
        );
    }

    #[test]
    fn terminal_flag_last_wins_and_first_name_exec_win() {
        let dir = fixture_dir("inline");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let path = dir.join("multi.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nName=First\nName=Second\nExec=one\nExec=two\nTerminal=false\nTerminal=true\n",
        )
        .expect("write fixture");
        let parsed = read_entry(&path);
        let Parsed::Entry { exec, terminal, .. } = parsed else {
            panic!("parses: {parsed:?}");
        };
        assert_eq!(exec, "one");
        assert!(terminal, "last Terminal wins");
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn parse_separates_hidden_by_design_from_malformed() {
        // One fixture per outcome class (see `tests/fixtures/launch/`).
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("launch");
        assert_eq!(
            read_entry(&dir.join("nodisplay-app.desktop")),
            Parsed::Hidden,
            "NoDisplay=true is skipped by design"
        );
        assert_eq!(
            read_entry(&dir.join("hidden-app.desktop")),
            Parsed::Hidden,
            "Hidden=true is skipped by design"
        );
        assert_eq!(
            read_entry(&dir.join("noexec-app.desktop")),
            Parsed::Malformed,
            "missing Exec is malformed"
        );
        assert_eq!(
            read_entry(&dir.join("malformed.desktop")),
            Parsed::Malformed,
            "no keys at all is malformed"
        );
        assert_eq!(
            read_entry(&dir.join("missing.desktop")),
            Parsed::Malformed,
            "unreadable file is malformed"
        );
        assert!(
            matches!(
                read_entry(&dir.join("onlyshow-app.desktop")),
                Parsed::Entry { .. }
            ),
            "OnlyShowIn/NotShowIn are ignored, not a skip"
        );
    }

    #[test]
    fn hidden_wins_over_an_incomplete_body() {
        // A file that is both hidden and field-less is skipped *by design*,
        // so it must not be reported as malformed (B-020).
        let dir = fixture_dir("hidden-incomplete");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let path = dir.join("both.desktop");
        std::fs::write(&path, "[Desktop Entry]\nName=Gone\nHidden=true\n").expect("write");
        assert_eq!(read_entry(&path), Parsed::Hidden);
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn find_app_resolves_display_names_ids_and_icon_hints() {
        let dir = fixture_dir("find-app");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(
            dir.join("firefox.desktop"),
            "[Desktop Entry]\nName=Firefox\nExec=firefox %U\nIcon=firefox\n",
        )
        .expect("write firefox");
        std::fs::write(
            dir.join("org.kde.drkonqi.coredump.gui.desktop"),
            "[Desktop Entry]\nName=Crashed Processes Viewer\nExec=drkonqi-coredump-gui\nIcon=tools-report-bug\n",
        )
        .expect("write viewer");
        // `NoDisplay` entries are never resolution candidates, even when the
        // icon matches: they are skipped by design (the lookup shares the
        // launch scan).
        std::fs::write(
            dir.join("org.kde.drkonqi.desktop"),
            "[Desktop Entry]\nName=Dr Konqi\nExec=/usr/libexec/drkonqi\nIcon=tools-report-bug\nNoDisplay=true\n",
        )
        .expect("write hidden handler");
        let dirs = vec![dir.clone()];

        for name in ["firefox", "firefox.desktop", "Firefox"] {
            let entry = find_app_in(&dirs, name, None).unwrap_or_else(|| panic!("resolves {name}"));
            assert_eq!(entry.id, "firefox.desktop", "{name}");
        }
        // The name match wins over the icon hint.
        let named = find_app_in(&dirs, "Firefox", Some("tools-report-bug")).expect("name first");
        assert_eq!(named.id, "firefox.desktop");

        // notifyrc display name, no executable and no desktop `Name`: only
        // the icon hint links it to an entry.
        let by_icon = find_app_in(&dirs, "Crash Reporting System", Some("tools-report-bug"))
            .expect("icon hint match");
        assert_eq!(by_icon.id, "org.kde.drkonqi.coredump.gui.desktop");
        assert_eq!(by_icon.exec, "drkonqi-coredump-gui");

        assert_eq!(find_app_in(&dirs, "Unknown App", None), None);
        assert_eq!(
            find_app_in(&dirs, "Crash Reporting System", Some("/tmp/icon.png")),
            None,
            "icon paths never match an Icon= name"
        );
        assert_eq!(find_app_in(&dirs, "", Some("tools-report-bug")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Scratch dir for inline unit fixtures (integration fixtures live in
    /// `tests/fixtures/launch/` and are covered by `tests/launch.rs`).
    fn fixture_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flex-launch-unit-{}-{name}", std::process::id()))
    }
}
