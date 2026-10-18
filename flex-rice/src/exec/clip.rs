//! `clip` executor: the clipboard-history copy/delete/pin-toggle flow.
//!
//! Port of the retired `flex-clip.sh` wrapper: after the menu selects a row,
//! the executor handles the select, delete and toggle outcomes (clip rows are
//! deletable, so unlike `shot`/`theme`/`wallpaper`/`launch` all three are
//! live), validates the id (`^[0-9a-f]+$`, the content-hash hex), resolves it
//! back to the stored line (the same
//! [`resolve`](crate::providers::clip::resolve) scan), and:
//!
//! - `select`: decodes `<NEWLINE>` back to newlines and pipes the entry
//!   (plus a trailing newline, the wrapper's `printf '%s\n'`) into `wl-copy`
//!   (stdout/stderr to `/dev/null`), then
//!   `notify-send -a Cliphist "Copied to clipboard" <preview>`;
//! - `delete`: removes every line exactly equal to the entry from both the
//!   history and pins files (`LC_ALL=C grep -aFxv`, via a `.tmp` + `mv`
//!   rewrite, skipped per file when the file is absent), then
//!   `notify-send -a Cliphist "Deleted" <preview>`;
//! - `toggle`: creates the pins parent dir, touches a missing pins file,
//!   then removes the entry (`Unpinned`) or appends it (`Pinned to history`)
//!   depending on whether it is already pinned, plus the matching notify.
//!
//! The preview is the first 50 bytes of the entry (the wrapper's
//! `head -c 50`).
//!
//! Snapshot convention (the [`exec::shot`](super::shot) template): [`plan`]
//! builds the [`Step`]s from resolved inputs, [`describe`] renders one step
//! as a single line — `wl-copy` (decoded entry on stdin), `remove <raw>
//! from history and pins` / `remove <raw> from pins` (exact-line file
//! scrubs), `append <raw> to pins` (pin), and
//! `notify-send -a Cliphist <summary> <preview>` — unit tests pin the lines,
//! and integration tests diff the stub-`PATH` call logs against the same
//! shapes plus byte-compare the resulting store files.
//!
//! Six deliberate departures from the wrapper (all tested):
//!
//! - No subprocess: the hash is resolved in-process with the same
//!   [`resolve`](crate::providers::clip::resolve) the library exposes, so
//!   the resolution semantics are identical with one fewer spawn.
//! - Exit-code normalisation: the wrapper `exec`s under `set -e` so a tool
//!   failure propagates verbatim; the port maps every failure through the
//!   shared runner, so any tool failure exits `1` with the single
//!   `flex: error:` prefix. There is no quiet-cancel step (unlike `shot`'s
//!   `slurp`): a failing copy/delete/toggle is always a loud error.
//! - `noop` short-circuits to `Ok` (the B-026 convention shared with
//!   `theme`/`launch`); the wrapper would reject it as a `bad id` since it
//!   is not hex. Unreachable in practice: an empty store exits 130 in the
//!   shared runner before any menu is built, so no `noop` row ever exists.
//! - Byte-split multibyte previews are lossy: `head -c 50` can cut a UTF-8
//!   sequence mid-character and pass the raw split bytes on; the port
//!   lossy-decodes the 50-byte prefix instead, so the notify arg is always
//!   valid UTF-8 (`U+FFFD` at a split point). ASCII previews — every real
//!   history line's first 50 bytes — are byte-identical.
//! - Unchanged store files are left untouched (mtime preserved): the wrapper
//!   always rewrites via `.tmp` + `mv`, which is byte-identical content
//!   whenever the file ends with a newline; the port skips the write when
//!   the scrubbed bytes equal the input bytes.
//! - Retention: the wrapper never capped the history file, so `add` enforces
//!   a 200-entry / 7-day cap (pins exempt) via a `$CLIPHIST_TS`-style
//!   timestamp sidecar (`cliphist.ts`, one epoch per history line).
//!   Overrides: `$CLIPHIST_MAX_ENTRIES`, `$CLIPHIST_MAX_AGE_SECS`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::flag;

use crate::providers::clip;
use crate::spawn::RetryExec as _;

/// Which menu outcome is being executed: clip is the only provider (besides
/// `wifi`) whose rows are deletable, so the binary
/// maps `Chosen`/`Delete`/`Toggle` onto these three ops and bails on
/// `Target` (clip rows carry no dropdown targets, and the wrapper has no
/// `ACTION:TARGET` arm either).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipOp {
    /// Copy the entry to the clipboard (`Chosen`).
    Copy,
    /// Remove the entry from history and pins (`Delete`).
    Delete,
    /// Pin/unpin the entry (`Toggle`).
    Toggle,
}

/// A validated clip action id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipAction {
    /// The `noop` empty-scan placeholder: exit `0`, never resolve or touch
    /// the store (the B-026 convention; see the module docs for why the
    /// wrapper would disagree).
    Noop,
    /// A content-hash hex id to resolve back to the stored line.
    Id(String),
}

impl ClipAction {
    /// Validate a menu action id, mirroring the wrapper's id check
    /// (`^[0-9a-f]+$`). `None` is the wrapper's `bad id` exit; `noop` is
    /// the empty-scan placeholder the other executors short-circuit.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        if id == "noop" {
            return Some(Self::Noop);
        }
        if !id.is_empty()
            && id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            Some(Self::Id(id.to_string()))
        } else {
            None
        }
    }
}

/// One clip step: a tool spawn or an exact-line store edit (pure data, no
/// process started, no file touched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Pipe the decoded entry (plus trailing newline) into `wl-copy`
    /// (stdout/stderr to `/dev/null`, like the wrapper's redirect).
    WlCopy,
    /// Remove every line exactly equal to the entry from the history file
    /// and (`pins_only == false`) or only (`pins_only == true`) the pins
    /// file (the wrapper's `grep -aFxv` + `.tmp` + `mv` per file).
    Scrub {
        /// Full stored (`<NEWLINE>`-encoded) line to remove.
        raw: String,
        /// Whether only the pins file is scrubbed (unpin) rather than both
        /// files (delete).
        pins_only: bool,
    },
    /// Append the entry to the pins file (the wrapper appends the
    /// `printf '%s\n'` output to the pins file).
    AppendPin {
        /// Full stored (`<NEWLINE>`-encoded) line to append.
        raw: String,
    },
    /// `notify-send -a Cliphist <summary> <preview>`.
    Notify {
        /// `Copied to clipboard` / `Deleted` / `Pinned to history` /
        /// `Unpinned` (the wrapper's four summaries, verbatim).
        summary: String,
        /// First-50-bytes preview of the entry.
        preview: String,
    },
}

/// Build the [`Step`]s for `op` over the already-resolved `raw` line (no
/// process started, no file touched).
///
/// `pinned` is the entry's current pin state and is read only for
/// [`ClipOp::Toggle`] (`true` → unpin scrub, `false` → pin append).
#[must_use]
pub fn plan(op: ClipOp, raw: &str, pinned: bool) -> Vec<Step> {
    let preview = preview(raw);
    match op {
        ClipOp::Copy => vec![
            Step::WlCopy,
            Step::Notify {
                summary: String::from("Copied to clipboard"),
                preview,
            },
        ],
        ClipOp::Delete => vec![
            Step::Scrub {
                raw: raw.to_string(),
                pins_only: false,
            },
            Step::Notify {
                summary: String::from("Deleted"),
                preview,
            },
        ],
        ClipOp::Toggle if pinned => vec![
            Step::Scrub {
                raw: raw.to_string(),
                pins_only: true,
            },
            Step::Notify {
                summary: String::from("Unpinned"),
                preview,
            },
        ],
        ClipOp::Toggle => vec![
            Step::AppendPin {
                raw: raw.to_string(),
            },
            Step::Notify {
                summary: String::from("Pinned to history"),
                preview,
            },
        ],
    }
}

/// Render one [`Step`] as a single snapshot line.
///
/// This is the template snapshot convention: unit tests pin these lines per
/// id (pure, no spawn), and the integration tests diff the stub-`PATH` call
/// logs against the same shapes. `WlCopy` carries its payload on stdin (the
/// decoded entry plus trailing newline), so the line is just `wl-copy` and
/// the stdin bytes are pinned separately by the `copied.bin` assertions.
#[must_use]
pub fn describe(step: &Step) -> String {
    match step {
        Step::WlCopy => String::from("wl-copy"),
        Step::Scrub { raw, pins_only } => {
            if *pins_only {
                format!("remove {raw} from pins")
            } else {
                format!("remove {raw} from history and pins")
            }
        }
        Step::AppendPin { raw } => format!("append {raw} to pins"),
        Step::Notify { summary, preview } => {
            format!("notify-send -a Cliphist {summary} {preview}")
        }
    }
}

/// Render a whole [`plan`] as snapshot lines (see [`describe`]).
#[must_use]
pub fn describe_plan(steps: &[Step]) -> Vec<String> {
    steps.iter().map(describe).collect()
}

/// First-50-bytes preview of `raw` (the wrapper's `head -c 50`).
///
/// Byte-based on purpose, lossy-decoded so the notify arg is always valid
/// UTF-8 (see the module docs: a multibyte character straddling byte 50
/// surfaces as `U+FFFD` instead of raw split bytes).
#[must_use]
pub fn preview(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let end = bytes.len().min(50);
    match bytes.get(..end) {
        Some(prefix) => String::from_utf8_lossy(prefix).into_owned(),
        None => String::new(),
    }
}

/// `wl-copy` stdin for `raw`: `<NEWLINE>` decoded back to newlines plus the
/// wrapper's `printf '%s\n'` trailing newline.
#[must_use]
pub fn copy_stdin(raw: &str) -> Vec<u8> {
    let mut decoded = clip::decode(raw);
    decoded.push('\n');
    decoded.into_bytes()
}

/// The ambient `PATH`, empty when unset (tool resolution then fails cleanly
/// instead of inheriting a surprising default).
fn ambient_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Resolve `name` against `path_env` (`:`-separated, shell-style).
///
/// Returns the first entry naming an existing file, so stub-`PATH` tests can
/// shadow the real tools without touching the process env.
fn resolve_tool(name: &str, path_env: &str) -> Option<PathBuf> {
    path_env
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
}

/// Run one tool with `args` and `stdin_bytes` on stdin.
///
/// Stdout/stderr go to `/dev/null` (the wrapper's `wl-copy` redirect). A
/// non-zero exit is `Ok` with `ok: false`, not an error — callers map it.
/// Messages carry no `flex:` prefix; the runner reports them.
///
/// # Errors
///
/// When the tool is missing from `path_env`, stdin cannot be piped, or the
/// spawn/wait itself fails.
fn tool_piped(path_env: &str, name: &str, args: &[String], stdin_bytes: &[u8]) -> Result<bool> {
    let Some(bin) = resolve_tool(name, path_env) else {
        anyhow::bail!("clip: {name} not found on PATH");
    };
    let mut child = Command::new(&bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn_retrying()
        .with_context(|| format!("clip: failed to run {name}"))?;
    {
        let Some(stdin) = child.stdin.as_mut() else {
            anyhow::bail!("clip: failed to pipe to {name}");
        };
        stdin
            .write_all(stdin_bytes)
            .with_context(|| format!("clip: failed to pipe to {name}"))?;
    }
    let status = child
        .wait()
        .with_context(|| format!("clip: failed to run {name}"))?;
    Ok(status.success())
}

/// Run one tool with `args`.
///
/// Stdout/stderr are inherited, like the wrapper's bare `notify-send`
/// (feedback reaches the popup); stdin is nulled. A non-zero exit is `Ok`
/// with `ok: false`, not an error — callers map it. Messages carry no
/// `flex:` prefix; the runner reports them.
///
/// # Errors
///
/// When the tool is missing from `path_env` or the spawn itself fails.
fn tool(path_env: &str, name: &str, args: &[String]) -> Result<bool> {
    let Some(bin) = resolve_tool(name, path_env) else {
        anyhow::bail!("clip: {name} not found on PATH");
    };
    let status = Command::new(&bin)
        .args(args)
        .stdin(Stdio::null())
        .status_retrying()
        .with_context(|| format!("clip: failed to run {name}"))?;
    Ok(status.success())
}

/// `.tmp` sibling the scrub/append rewrites go through (the wrapper's
/// `"${HISTFILE}.tmp"` / `"${PINFILE}.tmp"` + `mv`).
fn tmp_for(path: &Path) -> PathBuf {
    let mut buf = path.as_os_str().to_owned();
    buf.push(".tmp");
    PathBuf::from(buf)
}

/// Split store bytes into content lines: file order, trailing-newline
/// agnostic (a missing trailing newline is normalised to one on rewrite,
/// exactly like the wrapper's `grep` + `mv` output).
fn store_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines
}

/// Whether any store line is exactly `raw` (the wrapper's `grep -qFx`).
fn store_contains(bytes: &[u8], raw: &[u8]) -> bool {
    store_lines(bytes).contains(&raw)
}

/// Remove every line exactly equal to `raw` from `path` (the wrapper's
/// `LC_ALL=C grep -aFxv -- "$raw" file > file.tmp || true; mv file.tmp
/// file`).
///
/// A missing/unreadable file is skipped (the wrapper's `[[ -f … ]]` guard);
/// an unchanged file is left untouched (byte-identical to the wrapper's
/// rewrite whenever the input ends with a newline — see the module docs).
/// Returns whether the file was rewritten.
///
/// # Errors
///
/// When the `.tmp` rewrite or the rename fails.
fn scrub_file(path: &Path, raw: &[u8]) -> Result<bool> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(false);
    };
    if bytes.is_empty() {
        return Ok(false);
    }
    let kept: Vec<&[u8]> = store_lines(&bytes)
        .into_iter()
        .filter(|line| *line != raw)
        .collect();
    let mut rewritten: Vec<u8> = kept.join(&b'\n');
    if !kept.is_empty() {
        rewritten.push(b'\n');
    }
    if rewritten == bytes {
        return Ok(false);
    }
    let tmp = tmp_for(path);
    std::fs::write(&tmp, &rewritten)
        .with_context(|| format!("clip: cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("clip: cannot update {}", path.display()))?;
    Ok(true)
}

/// Append `raw` plus a trailing newline to `path` (the wrapper's
/// `printf '%s\n' "$raw" >> "$file"`), creating parent dirs and the file
/// first (the wrapper's `mkdir -p` + `touch`).
///
/// # Errors
///
/// When dirs cannot be created or the rewrite fails.
fn append_line(path: &Path, raw: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("clip: cannot create {}", parent.display()))?;
        }
    }
    let mut bytes = std::fs::read(path).unwrap_or_default();
    bytes.extend_from_slice(raw);
    bytes.push(b'\n');
    let tmp = tmp_for(path);
    std::fs::write(&tmp, &bytes)
        .with_context(|| format!("clip: cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("clip: cannot update {}", path.display()))?;
    Ok(())
}

/// Is `raw` currently pinned (exact-line match in the pins file)?
///
/// A missing/unreadable pins file counts as unpinned (the wrapper's
/// `grep -q … 2>/dev/null` is false there too, so the entry gets appended).
#[must_use]
pub fn is_pinned(pins: &Path, raw: &str) -> bool {
    std::fs::read(pins).is_ok_and(|bytes| store_contains(&bytes, raw.as_bytes()))
}

// === history retention ===
//
// The retired wrapper never capped the history file; `add` enforces two
// caps on every append so the store stays bounded:
//
// - count: keep the newest [`clip::MAX_HISTORY_ENTRIES`] lines
//   (`$CLIPHIST_MAX_ENTRIES` override);
// - age: drop unpinned lines older than [`clip::MAX_HISTORY_AGE_SECS`]
//   seconds (`$CLIPHIST_MAX_AGE_SECS` override).
//
// Pinned lines (exact-line membership in the pins file) are exempt from both
// caps. Ages live in the timestamp sidecar ([`clip::ts_path`], one decimal
// unix-epoch line per history line, same order); a missing sidecar or an
// unparseable epoch counts as fresh, so the first enforcement after upgrade
// never mass-deletes legacy history — those lines get stamped with `now` and
// a full fresh lease instead. Re-adding an existing line refreshes its epoch
// (last-seen age). Files are rewritten atomically (`.tmp` + rename, like the
// other store edits) and left untouched when nothing changes (the
// mtime-preservation convention from the module docs).

/// Positive env override in seconds/entries, else `default`
/// (missing/empty/garbage/zero all fall back).
fn positive_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|parsed| *parsed > 0)
        .unwrap_or(default)
}

/// Positive `usize` env override, else `default` (same fallback rule; kept
/// separate from [`positive_env`] so no `u64`→`usize` cast is needed).
fn positive_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|parsed| *parsed > 0)
        .unwrap_or(default)
}

/// Unix now in whole seconds (a broken clock yields `0`, and the saturating
/// age math below then keeps everything — fail-open, never mass-delete).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Sidecar epochs aligned to the history lines: `None`/missing/garbage slots
/// become `now` (fresh). Always exactly `count` long.
fn read_epochs(ts: &Path, count: usize, now: u64) -> Vec<u64> {
    let bytes = std::fs::read(ts).unwrap_or_default();
    let mut epochs: Vec<u64> = store_lines(&bytes)
        .iter()
        .map(|line| {
            std::str::from_utf8(line)
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(now)
        })
        .collect();
    epochs.resize(count, now);
    epochs.truncate(count);
    epochs
}

/// Atomic rewrite (`.tmp` + rename, creating parent dirs), like
/// [`append_line`] and [`scrub_file`].
///
/// # Errors
///
/// When the write or the rename fails.
fn rewrite(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("clip: cannot create {}", parent.display()))?;
        }
    }
    let tmp = tmp_for(path);
    std::fs::write(&tmp, bytes).with_context(|| format!("clip: cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("clip: cannot update {}", path.display()))?;
    Ok(())
}

/// Enforce the retention caps with env overrides and the real clock (what
/// [`add_with`] and the delete path call).
///
/// Returns whether any store file changed.
///
/// # Errors
///
/// When a store rewrite fails.
pub fn enforce_retention(hist: &Path, pins: &Path, ts: &Path) -> Result<bool> {
    enforce_retention_with(
        hist,
        pins,
        ts,
        positive_env_usize(clip::HIST_MAX_ENV, clip::MAX_HISTORY_ENTRIES),
        positive_env(clip::HIST_MAX_AGE_ENV, clip::MAX_HISTORY_AGE_SECS),
        now_secs(),
    )
}

/// [`enforce_retention`] with explicit caps and clock (the test seam).
///
/// Pinned lines are exempt from both caps; when pins alone exceed the count
/// cap they are all kept. A missing/empty history file is a no-op (there is
/// nothing to trim, and no sidecar is created for a store that has no lines).
///
/// Returns whether any store file changed.
///
/// # Errors
///
/// When a store rewrite fails.
pub fn enforce_retention_with(
    hist: &Path,
    pins: &Path,
    ts: &Path,
    max_entries: usize,
    max_age_secs: u64,
    now: u64,
) -> Result<bool> {
    let Ok(bytes) = std::fs::read(hist) else {
        return Ok(false);
    };
    if bytes.is_empty() {
        return Ok(false);
    }
    let lines = store_lines(&bytes);
    let pins_bytes = std::fs::read(pins).unwrap_or_default();
    let pinned: std::collections::HashSet<&[u8]> = store_lines(&pins_bytes).into_iter().collect();
    let epochs = read_epochs(ts, lines.len(), now);
    let mut keep: Vec<bool> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            pinned.contains(*line) || now.saturating_sub(epochs[index]) <= max_age_secs
        })
        .collect();
    let kept = keep.iter().filter(|kept| **kept).count();
    if kept > max_entries {
        let mut drop_budget = kept - max_entries;
        for (index, line) in lines.iter().enumerate() {
            if drop_budget == 0 {
                break;
            }
            if keep[index] && !pinned.contains(*line) {
                keep[index] = false;
                drop_budget -= 1;
            }
        }
    }
    let evicted = keep.contains(&false);
    let ts_count = std::fs::read(ts).map_or(0, |ts_bytes| store_lines(&ts_bytes).len());
    if !evicted && ts_count == lines.len() {
        return Ok(false);
    }
    if evicted {
        let mut rewritten: Vec<u8> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if keep[index] {
                rewritten.extend_from_slice(line);
                rewritten.push(b'\n');
            }
        }
        rewrite(hist, &rewritten)?;
    }
    let mut ts_rewritten: Vec<u8> = Vec::new();
    for (index, epoch) in epochs.iter().enumerate() {
        if !evicted || keep[index] {
            ts_rewritten.extend_from_slice(epoch.to_string().as_bytes());
            ts_rewritten.push(b'\n');
        }
    }
    rewrite(ts, &ts_rewritten)?;
    Ok(true)
}

/// Refresh the sidecar epoch of the history line exactly equal to `raw`
/// (re-add = last-seen). A no-op when the line is absent; the sidecar is
/// left untouched when the epoch already equals `now` (mtime convention).
///
/// # Errors
///
/// When the sidecar rewrite fails.
fn refresh_epoch(hist: &Path, ts: &Path, raw: &[u8], now: u64) -> Result<()> {
    let Ok(bytes) = std::fs::read(hist) else {
        return Ok(());
    };
    let lines = store_lines(&bytes);
    let Some(index) = lines.iter().position(|line| *line == raw) else {
        return Ok(());
    };
    let mut epochs = read_epochs(ts, lines.len(), now);
    if epochs[index] == now {
        return Ok(());
    }
    epochs[index] = now;
    let mut rewritten: Vec<u8> = Vec::new();
    for epoch in &epochs {
        rewritten.extend_from_slice(epoch.to_string().as_bytes());
        rewritten.push(b'\n');
    }
    rewrite(ts, &rewritten)
}

/// History line indices exactly equal to `raw` (what [`scrub_file`] removes,
/// so the sidecar can drop the same slots).
fn removed_indices(hist: &Path, raw: &[u8]) -> Vec<usize> {
    let Ok(bytes) = std::fs::read(hist) else {
        return Vec::new();
    };
    store_lines(&bytes)
        .iter()
        .enumerate()
        .filter(|(_, line)| **line == raw)
        .map(|(index, _)| index)
        .collect()
}

/// Drop the sidecar slots at `removed` (best-effort alignment: indices past
/// a short sidecar are ignored, surplus trailing sidecar lines are kept for
/// [`enforce_retention`] to normalise).
///
/// # Errors
///
/// When the sidecar rewrite fails.
fn drop_epochs(ts: &Path, removed: &[usize]) -> Result<()> {
    if removed.is_empty() {
        return Ok(());
    }
    let Ok(bytes) = std::fs::read(ts) else {
        return Ok(());
    };
    if bytes.is_empty() {
        return Ok(());
    }
    let lines = store_lines(&bytes);
    let mut rewritten: Vec<u8> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !removed.contains(&index) {
            rewritten.extend_from_slice(line);
            rewritten.push(b'\n');
        }
    }
    if rewritten == bytes {
        return Ok(());
    }
    rewrite(ts, &rewritten)
}

/// What [`execute`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteReport {
    /// The op that ran (`None` when `noop` short-circuited first — the
    /// wrapper exits before resolving, so this does too).
    pub op: Option<ClipOp>,
    /// Resolved stored line (`None` for `noop`).
    pub raw: Option<String>,
    /// Post-run pin state (`Some` only for [`ClipOp::Toggle`]).
    pub pinned: Option<bool>,
}

/// Copy/delete/toggle the selected entry: validate the id, short-circuit
/// `noop`, resolve the hash to its stored line in-process, and run the
/// [`plan`] steps.
/// `path_env` shadows the ambient `PATH` when `Some` (the stub seam tests
/// use); `None` inherits it.
///
/// # Errors
///
/// When the id is malformed (`bad id`, like the wrapper), the hash resolves
/// to nothing (`unknown id`), a store rewrite fails, or a tool is missing
/// or fails. Messages carry no `flex:` prefix; the runner reports them.
pub fn execute(op: ClipOp, action_id: &str, path_env: Option<&str>) -> Result<ExecuteReport> {
    let Some(action) = ClipAction::parse(action_id) else {
        anyhow::bail!("clip: bad id '{action_id}'");
    };
    let ClipAction::Id(id) = action else {
        return Ok(ExecuteReport {
            op: None,
            raw: None,
            pinned: None,
        });
    };
    let raw = clip::resolve(&id);
    let Some(raw) = raw else {
        anyhow::bail!("clip: unknown id '{id}'");
    };
    if raw.is_empty() {
        anyhow::bail!("clip: unknown id '{id}'");
    }
    execute_with(
        op,
        &raw,
        &clip::hist_path(),
        &clip::pins_path(),
        &clip::ts_path(),
        path_env,
    )
}

/// Copy/delete/toggle half of [`execute`], with the resolved inputs given
/// (no env reads, no resolve): the shape integration tests drive with a
/// stub `PATH` and scratch store files.
///
/// A history scrub also drops the matching sidecar epochs so the timestamp
/// file stays aligned with the history file.
///
/// # Errors
///
/// When a store rewrite fails, or a tool is missing or exits non-zero
/// (always loud — there is no quiet-cancel step). Messages carry no `flex:`
/// prefix.
pub fn execute_with(
    op: ClipOp,
    raw: &str,
    hist: &Path,
    pins: &Path,
    ts: &Path,
    path_env: Option<&str>,
) -> Result<ExecuteReport> {
    let path_env = path_env.map_or_else(ambient_path, str::to_string);
    let pinned = op == ClipOp::Toggle && is_pinned(pins, raw);
    for step in plan(op, raw, pinned) {
        match step {
            Step::WlCopy => {
                if !tool_piped(&path_env, "wl-copy", &[], &copy_stdin(raw))? {
                    anyhow::bail!("clip: wl-copy failed");
                }
            }
            Step::Scrub { raw, pins_only } => {
                if !pins_only {
                    let removed = removed_indices(hist, raw.as_bytes());
                    scrub_file(hist, raw.as_bytes())?;
                    drop_epochs(ts, &removed)?;
                }
                scrub_file(pins, raw.as_bytes())?;
            }
            Step::AppendPin { raw } => {
                append_line(pins, raw.as_bytes())?;
            }
            Step::Notify { summary, preview } => {
                let args = vec![
                    String::from("-a"),
                    String::from("Cliphist"),
                    summary.clone(),
                    preview.clone(),
                ];
                if !tool(&path_env, "notify-send", &args)? {
                    anyhow::bail!("clip: notify-send failed");
                }
            }
        }
    }
    Ok(ExecuteReport {
        op: Some(op),
        raw: Some(raw.to_string()),
        pinned: if op == ClipOp::Toggle {
            Some(!pinned)
        } else {
            None
        },
    })
}

// === `cliphist.sh` add/pin/unpin/current verbs (the non-TUI entry points) ===
//
// The retired wrapper's `add`/`pin`/`unpin`/`read_current` are ported here so
// `wl-paste --watch` and the `pin` bind no longer need bash. Semantics mirror
// `cliphist.sh` exactly (encode `<NEWLINE>`, exact-line dedupe, `.tmp`+`mv`
// scrub, the verbatim notify messages), with one documented departure shared
// with the ACTION path: previews are the first 50 bytes of the *decoded* text,
// lossy-decoded (never a raw byte split).

/// Send a `notify-send -a Cliphist <summary> <body>` line.
///
/// # Errors
///
/// When `notify-send` is missing or exits non-zero. Messages carry no
/// `flex:` prefix.
fn notify(path_env: &str, summary: &str, body: &str) -> Result<()> {
    let args = vec![
        String::from("-a"),
        String::from("Cliphist"),
        summary.to_string(),
        body.to_string(),
    ];
    if !tool(path_env, "notify-send", &args)? {
        anyhow::bail!("clip: notify-send failed");
    }
    Ok(())
}

/// Read the current clipboard via `wl-paste` (stdout captured, stderr
/// discarded), mirroring `cliphist.sh:11`.
///
/// A non-zero `wl-paste` exit is not an error: the wrapper pipes through `tr`
/// under `|| true`, so whatever stdout it produced is used.
///
/// The wait is bounded ([`WL_PASTE_TIMEOUT`]): a wedged Wayland selection
/// (dead owner, never-ending stream) blocks `wl-paste` forever, and the
/// unbounded `.output()` wait wedged `flex-clip add` under `wl-paste --watch`
/// for 14 h (Sep 2026 incident — the watcher's serial spawn meant every later
/// copy was silently dropped). A timeout is an error, never a partial entry:
/// the next selection change spawns a fresh `add`.
/// Large pastes are drained on a side thread: entries can exceed the 64 KiB
/// pipe buffer (an 84 KiB line sits in the reference store), so waiting
/// without draining would deadlock a healthy paste.
///
/// # Errors
///
/// When `wl-paste` is missing from `path_env`, the spawn itself fails, the
/// child cannot be reaped, its output cannot be collected, or it does not
/// exit in time.
fn read_clipboard(path_env: &str) -> Result<String> {
    read_clipboard_with(path_env, WL_PASTE_TIMEOUT)
}

/// Bound on one `wl-paste` paste: local IPC answers in milliseconds, so this
/// only ever fires on a wedged selection.
const WL_PASTE_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace for the drain thread to hand over stdout after the child exits.
const WL_PASTE_DRAIN: Duration = Duration::from_secs(2);
/// Poll interval while waiting for `wl-paste` (the popup poll's 50 ms step).
const WL_PASTE_POLL: Duration = Duration::from_millis(50);

/// [`read_clipboard`] with an explicit bound (the test seam).
///
/// # Errors
///
/// As [`read_clipboard`].
fn read_clipboard_with(path_env: &str, timeout: Duration) -> Result<String> {
    let Some(bin) = resolve_tool("wl-paste", path_env) else {
        anyhow::bail!("clip: wl-paste not found on PATH");
    };
    let mut child = Command::new(&bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn_retrying()
        .context("clip: failed to run wl-paste")?;
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            use std::io::Read as _;
            let _ = out.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .context("clip: failed to wait for wl-paste")?
            .is_some()
        {
            // Exit status is deliberately ignored (see above); whatever
            // stdout arrived is the entry.
            match rx.recv_timeout(WL_PASTE_DRAIN) {
                Ok(bytes) => {
                    return Ok(String::from_utf8_lossy(&bytes).into_owned());
                }
                Err(_) => anyhow::bail!("clip: wl-paste output unreadable"),
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "clip: wl-paste timed out after {}s (clipboard owner not answering; re-copy to retry)",
                timeout.as_secs()
            );
        }
        std::thread::sleep(WL_PASTE_POLL);
    }
}

/// Decode the current-entry file the way `read_current` does: strip `NUL`s,
/// drop trailing newlines, decode `<NEWLINE>` → newlines. Missing/empty →
/// `""`.
#[must_use]
fn current_decoded(current: &Path) -> String {
    let Ok(bytes) = std::fs::read(current) else {
        return String::new();
    };
    let stripped: Vec<u8> = bytes.iter().copied().filter(|byte| *byte != 0).collect();
    let text = String::from_utf8_lossy(&stripped);
    clip::decode(text.trim_end_matches('\n'))
}

/// Overwrite the current-entry file with the encoded line plus a newline
/// (`echo "$multiline" > "$CURRFILE"`), creating parent dirs.
///
/// # Errors
///
/// When the parent dir cannot be created or the write fails.
fn write_current(current: &Path, encoded: &str) -> Result<()> {
    if let Some(parent) = current.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("clip: cannot create {}", parent.display()))?;
        }
    }
    let mut bytes = encoded.as_bytes().to_vec();
    bytes.push(b'\n');
    std::fs::write(current, &bytes)
        .with_context(|| format!("clip: cannot write {}", current.display()))?;
    Ok(())
}

/// Exact-line membership in a store file (`grep -Fxq`).
#[must_use]
fn file_contains(path: &Path, raw: &[u8]) -> bool {
    std::fs::read(path).is_ok_and(|bytes| store_contains(&bytes, raw))
}

/// The verb text for `pin`/`unpin`: a non-empty argument, else the decoded
/// current entry (the wrapper's `[[ -n "${2:-}" ]]` guard around
/// `read_current`).
#[must_use]
fn verb_text(text: Option<&str>, current: &Path) -> String {
    match text {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => current_decoded(current),
    }
}

/// `cliphist.sh add`: capture the current clipboard (NULs and trailing
/// newlines stripped), encode newlines, overwrite the current entry, then
/// append to history unless the exact line is already present.
///
/// An empty clipboard is a no-op (exit 0), like the wrapper.
///
/// # Errors
///
/// When `wl-paste` is missing/fails or a store write fails. Messages carry no
/// `flex:` prefix; the runner reports them.
pub fn add(path_env: Option<&str>) -> Result<()> {
    let path_env = path_env.map_or_else(ambient_path, str::to_string);
    let clipboard = read_clipboard(&path_env)?;
    add_with(
        &clipboard,
        &clip::hist_path(),
        &clip::current_path(),
        &clip::pins_path(),
        &clip::ts_path(),
        Some(&path_env),
    )
}

/// [`add`] with the clipboard text and paths given (the test seam).
///
/// Appends the entry (with a `now` sidecar epoch) unless already present —
/// a re-add refreshes its epoch instead — then enforces retention.
///
/// # Errors
///
/// When a store write fails.
pub fn add_with(
    clipboard: &str,
    hist: &Path,
    current: &Path,
    pins: &Path,
    ts: &Path,
    path_env: Option<&str>,
) -> Result<()> {
    let stripped: String = clipboard.chars().filter(|ch| *ch != '\0').collect();
    let trimmed = stripped.trim_end_matches('\n');
    if trimmed.is_empty() {
        return Ok(());
    }
    let encoded = clip::encode(trimmed);
    let prev_current = current_decoded(current);
    write_current(current, &encoded)?;
    let now = now_secs();
    if file_contains(hist, encoded.as_bytes()) {
        refresh_epoch(hist, ts, encoded.as_bytes(), now)?;
    } else {
        append_line(hist, encoded.as_bytes())?;
        append_line(ts, now.to_string().as_bytes())?;
    }
    enforce_retention(hist, pins, ts)?;
    if prev_current != trimmed {
        if let Some(pe) = path_env {
            if resolve_tool("wl-copy", pe).is_some() {
                let stdin = copy_stdin(&encoded);
                let _ = tool_piped(pe, "wl-copy", &[], &stdin);
            }
        }
    }
    Ok(())
}

/// `cliphist.sh pin`: pin the current entry, or the given non-empty text.
///
/// # Errors
///
/// When the pins rewrite fails or `notify-send` fails. Messages carry no
/// `flex:` prefix; the runner reports them.
pub fn pin(text: Option<&str>, path_env: Option<&str>) -> Result<()> {
    let path_env = path_env.map_or_else(ambient_path, str::to_string);
    let decoded = verb_text(text, &clip::current_path());
    pin_with(&decoded, &clip::pins_path(), &path_env)
}

/// [`pin`] with the decoded text and pins path given (the test seam).
///
/// # Errors
///
/// When the pins rewrite fails or `notify-send` fails.
pub fn pin_with(decoded: &str, pins: &Path, path_env: &str) -> Result<()> {
    if decoded.is_empty() {
        return notify(
            path_env,
            "Nothing to pin",
            "Clipboard is empty — copy something first",
        );
    }
    let encoded = clip::encode(decoded);
    let shown = preview(decoded);
    if is_pinned(pins, &encoded) {
        return notify(path_env, "Already pinned", &shown);
    }
    append_line(pins, encoded.as_bytes())?;
    notify(path_env, "Pinned to history", &shown)
}

/// `cliphist.sh unpin`: unpin the current entry, or the given non-empty text.
///
/// # Errors
///
/// When the pins scrub fails or `notify-send` fails. Messages carry no
/// `flex:` prefix; the runner reports them.
pub fn unpin(text: Option<&str>, path_env: Option<&str>) -> Result<()> {
    let path_env = path_env.map_or_else(ambient_path, str::to_string);
    let decoded = verb_text(text, &clip::current_path());
    unpin_with(&decoded, &clip::pins_path(), &path_env)
}

/// [`unpin`] with the decoded text and pins path given (the test seam).
///
/// # Errors
///
/// When the pins scrub fails or `notify-send` fails.
pub fn unpin_with(decoded: &str, pins: &Path, path_env: &str) -> Result<()> {
    if decoded.is_empty() {
        return notify(path_env, "Nothing to unpin", "Clipboard is empty");
    }
    if !pins.exists() {
        return notify(path_env, "Nothing unpinned", "No pinned items exist");
    }
    let encoded = clip::encode(decoded);
    let shown = preview(decoded);
    if is_pinned(pins, &encoded) {
        scrub_file(pins, encoded.as_bytes())?;
        notify(path_env, "Unpinned", &shown)
    } else {
        notify(path_env, "Not pinned", &shown)
    }
}

/// `cliphist.sh` `read_current`: print the decoded current entry plus a
/// trailing newline (nothing when the entry is empty).
///
/// # Errors
///
/// When stdout cannot be written. Messages carry no `flex:` prefix.
pub fn current() -> Result<()> {
    let decoded = current_decoded(&clip::current_path());
    if decoded.is_empty() {
        return Ok(());
    }
    let mut out = std::io::stdout();
    out.write_all(decoded.as_bytes())
        .context("clip: cannot write current entry")?;
    out.write_all(b"\n")
        .context("clip: cannot write current entry")?;
    Ok(())
}

/// `flex-clip watch`: run a watcher loop that listens for Wayland selection
/// updates by spawning `wl-paste --type text --watch flex-clip add`.
///
/// # Errors
///
/// When `wl-paste` is missing from `path_env`, spawning `wl-paste` fails, or
/// waiting for the child process fails. Messages carry no `flex:` prefix; the
/// runner reports them.
pub fn watch(path_env: Option<&str>) -> Result<()> {
    let path_env = path_env.map_or_else(ambient_path, str::to_string);
    let Some(wl_paste) = resolve_tool("wl-paste", &path_env) else {
        anyhow::bail!("clip: wl-paste not found on PATH");
    };
    let clip_bin = resolve_tool("flex-clip", &path_env)
        .map_or_else(|| String::from("flex-clip"), |p| p.display().to_string());

    let mut child = Command::new(&wl_paste)
        .args(["--type", "text", "--watch", &clip_bin, "add"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn_retrying()
        .with_context(|| "clip: failed to run wl-paste")?;

    let term = Arc::new(AtomicBool::new(false));
    let _ = flag::register(SIGINT, Arc::clone(&term));
    let _ = flag::register(SIGTERM, Arc::clone(&term));
    let _ = flag::register(SIGHUP, Arc::clone(&term));

    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| "clip: failed to wait for wl-paste")?
        {
            if status.success() {
                return Ok(());
            }
            anyhow::bail!("clip: wl-paste watcher exited with status {status}");
        }
        if term.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_accept_lowercase_hex_of_any_length() {
        assert_eq!(
            ClipAction::parse("0123456789abcdef"),
            Some(ClipAction::Id(String::from("0123456789abcdef"))),
        );
        assert_eq!(
            ClipAction::parse("deadbeef"),
            Some(ClipAction::Id(String::from("deadbeef"))),
        );
        assert_eq!(ClipAction::parse("noop"), Some(ClipAction::Noop));
    }

    #[test]
    fn ids_reject_the_wrapper_bad_id_set() {
        for bad in [
            "",
            "a/b",
            "a\nb",
            "/lead",
            "trail/",
            "ABCDEF",
            "Noop",
            "deadbeef!",
        ] {
            assert_eq!(ClipAction::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn plan_snapshot_copy_is_wl_copy_plus_notify() {
        assert_eq!(
            describe_plan(&plan(ClipOp::Copy, "hello", false)),
            vec![
                "wl-copy",
                "notify-send -a Cliphist Copied to clipboard hello",
            ],
        );
    }

    #[test]
    fn plan_snapshot_delete_scrubs_both_files() {
        assert_eq!(
            describe_plan(&plan(ClipOp::Delete, "doomed", false)),
            vec![
                "remove doomed from history and pins",
                "notify-send -a Cliphist Deleted doomed",
            ],
        );
    }

    #[test]
    fn plan_snapshot_toggle_pins_or_unpins() {
        assert_eq!(
            describe_plan(&plan(ClipOp::Toggle, "flip", false)),
            vec![
                "append flip to pins",
                "notify-send -a Cliphist Pinned to history flip",
            ],
        );
        assert_eq!(
            describe_plan(&plan(ClipOp::Toggle, "flip", true)),
            vec![
                "remove flip from pins",
                "notify-send -a Cliphist Unpinned flip",
            ],
        );
    }

    #[test]
    fn preview_is_the_first_fifty_bytes() {
        assert_eq!(preview("short"), "short");
        let long = "L".repeat(200);
        assert_eq!(preview(&long), "L".repeat(50));
    }

    #[test]
    fn preview_never_panics_on_a_multibyte_split() {
        // `é` is 2 bytes: 49 ASCII bytes + `é` straddles byte 50, so the
        // lossy prefix ends in `U+FFFD` instead of raw split bytes (module
        // docs: deliberate departure from `head -c 50`).
        let raw = format!("{}{}", "a".repeat(49), "éclair");
        assert_eq!(preview(&raw), format!("{}{}", "a".repeat(49), "�"));
    }

    #[test]
    fn copy_stdin_decodes_the_placeholder_and_adds_a_newline() {
        assert_eq!(
            copy_stdin("alpha<NEWLINE>beta gamma"),
            b"alpha\nbeta gamma\n".to_vec(),
        );
        assert_eq!(copy_stdin("plain"), b"plain\n".to_vec());
    }

    #[test]
    fn store_lines_handles_a_missing_trailing_newline() {
        assert_eq!(
            store_lines(b"a\nb\n"),
            vec![b"a".as_slice(), b"b".as_slice()]
        );
        assert_eq!(store_lines(b"a\nb"), vec![b"a".as_slice(), b"b".as_slice()]);
        assert_eq!(store_lines(b""), [] as [&[u8]; 0]);
    }

    #[test]
    fn store_contains_matches_exact_lines_only() {
        assert!(store_contains(b"flip me\nother\n", b"flip me"));
        assert!(!store_contains(b"flip me extended\nother\n", b"flip me"));
        assert!(!store_contains(b"", b"flip me"));
    }

    /// Unique scratch dir for one test (tests run in parallel).
    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "flex-clip-verbs-{}-{name}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Install a no-op `notify-send` stub in `dir` and return it as a `PATH`
    /// value (so `pin_with`/`unpin_with` report without a live tool).
    fn notify_stub(dir: &Path) -> String {
        let stub = dir.join("notify-send");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("write notify stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod notify stub");
        }
        dir.display().to_string()
    }

    #[test]
    fn add_encodes_strips_nuls_and_dedupes() {
        let dir = scratch("add");
        let hist = dir.join("hist");
        let current = dir.join("current");
        let pins = dir.join("pins");
        let ts = dir.join("ts");
        add_with("alpha\nbeta\0\n", &hist, &current, &pins, &ts, None).expect("add");
        // NUL stripped, trailing newline trimmed, internal newline encoded.
        assert_eq!(
            std::fs::read(&current).expect("current"),
            b"alpha<NEWLINE>beta\n".to_vec(),
        );
        assert_eq!(
            std::fs::read(&hist).expect("hist"),
            b"alpha<NEWLINE>beta\n".to_vec(),
        );
        // Second add of the same entry updates current but does not re-append.
        add_with("alpha\nbeta\n", &hist, &current, &pins, &ts, None).expect("add again");
        assert_eq!(
            std::fs::read(&hist).expect("hist"),
            b"alpha<NEWLINE>beta\n".to_vec(),
            "exact-line dedupe",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_of_an_empty_clipboard_is_a_no_op() {
        let dir = scratch("add-empty");
        let hist = dir.join("hist");
        let current = dir.join("current");
        let pins = dir.join("pins");
        let ts = dir.join("ts");
        add_with("\n", &hist, &current, &pins, &ts, None).expect("add");
        assert!(!hist.exists());
        assert!(!current.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Install an executable `wl-paste` stub with `body` in `dir` and return
    /// the dir as a `PATH` value (so `read_clipboard` resolves the stub).
    fn paste_stub(dir: &Path, body: &str) -> String {
        let stub = dir.join("wl-paste");
        std::fs::write(&stub, body).expect("write wl-paste stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod wl-paste stub");
        }
        dir.display().to_string()
    }

    #[test]
    fn read_clipboard_times_out_on_a_hung_paste() {
        // Live incident (Sep 2026): a wedged Wayland selection blocked
        // `wl-paste` forever and the unbounded wait wedged `flex-clip add`
        // under `wl-paste --watch` for 14 h, silently dropping every later
        // copy. The wait must be bounded.
        let dir = scratch("hung-paste");
        let path_env = paste_stub(&dir, "#!/bin/sh\nsleep 30\n");
        let start = std::time::Instant::now();
        let err = read_clipboard_with(&path_env, Duration::from_millis(300))
            .expect_err("a hung paste must fail, not hang");
        assert!(
            err.to_string().contains("timed out"),
            "the timeout must say so: {err:#}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "bounded wait, not the stub's 30 s"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_clipboard_drains_pastes_bigger_than_the_pipe_buffer() {
        // Entries can exceed the 64 KiB pipe buffer (84 KiB in the reference
        // store): stdout must drain concurrently with the wait, or a healthy
        // large paste deadlocks against a full pipe.
        let dir = scratch("big-paste");
        let path_env = paste_stub(
            &dir,
            "#!/bin/sh\nhead -c 100000 /dev/zero | tr '\\000' 'x'\n",
        );
        let got = read_clipboard(&path_env).expect("big paste reads");
        assert_eq!(got.len(), 100_000, "every byte drained");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_trims_to_newest_and_exempts_pins() {
        let dir = scratch("retention-count");
        let hist = dir.join("hist");
        let pins = dir.join("pins");
        let ts = dir.join("ts");
        std::fs::write(&hist, b"one\ntwo\nthree\nfour\nfive\n").expect("hist");
        std::fs::write(&pins, b"one\n").expect("pins");
        std::fs::write(&ts, b"100\n100\n100\n100\n100\n").expect("ts");
        let changed = enforce_retention_with(&hist, &pins, &ts, 3, u64::MAX, 200).expect("enforce");
        assert!(changed, "two lines must be evicted");
        assert_eq!(
            std::fs::read(&hist).expect("hist"),
            b"one\nfour\nfive\n".to_vec(),
            "pinned oldest survives, newest win",
        );
        assert_eq!(
            std::fs::read(&ts).expect("ts"),
            b"100\n100\n100\n".to_vec(),
            "sidecar stays aligned",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_evicts_old_unpinned_but_keeps_old_pinned() {
        let dir = scratch("retention-age");
        let hist = dir.join("hist");
        let pins = dir.join("pins");
        let ts = dir.join("ts");
        std::fs::write(&hist, b"keep\nstale\nfresh\n").expect("hist");
        std::fs::write(&pins, b"keep\n").expect("pins");
        std::fs::write(&ts, b"0\n0\n990\n").expect("ts");
        let changed = enforce_retention_with(&hist, &pins, &ts, 200, 60, 1_000).expect("enforce");
        assert!(changed, "the stale line must be evicted");
        assert_eq!(
            std::fs::read(&hist).expect("hist"),
            b"keep\nfresh\n".to_vec(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_treats_a_missing_sidecar_as_fresh() {
        let dir = scratch("retention-legacy");
        let hist = dir.join("hist");
        let pins = dir.join("pins");
        let ts = dir.join("ts");
        std::fs::write(&hist, b"legacy\nlines\n").expect("hist");
        // No sidecar: legacy history must survive its first enforcement even
        // under a tiny age cap, gaining a fresh lease instead.
        let changed =
            enforce_retention_with(&hist, &pins, &ts, 200, 1, 1_000_000).expect("enforce");
        assert!(changed, "the sidecar is created");
        assert_eq!(
            std::fs::read(&hist).expect("hist"),
            b"legacy\nlines\n".to_vec(),
            "legacy lines are kept",
        );
        assert_eq!(
            std::fs::read(&ts).expect("ts"),
            b"1000000\n1000000\n".to_vec(),
            "missing epochs are stamped with now",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_is_a_no_op_without_history() {
        let dir = scratch("retention-empty");
        let hist = dir.join("hist");
        let pins = dir.join("pins");
        let ts = dir.join("ts");
        assert!(
            !enforce_retention_with(&hist, &pins, &ts, 200, 60, 1_000).expect("missing"),
            "a missing history file changes nothing",
        );
        std::fs::write(&hist, b"").expect("empty hist");
        assert!(
            !enforce_retention_with(&hist, &pins, &ts, 200, 60, 1_000).expect("empty"),
            "an empty history file changes nothing",
        );
        assert!(!ts.exists(), "no sidecar is created for an empty store");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readd_refreshes_the_epoch_instead_of_appending() {
        let dir = scratch("retention-refresh");
        let hist = dir.join("hist");
        let current = dir.join("current");
        let pins = dir.join("pins");
        let ts = dir.join("ts");
        std::fs::write(&hist, b"alpha\n").expect("hist");
        std::fs::write(&ts, b"1\n").expect("ts");
        add_with("alpha\n", &hist, &current, &pins, &ts, None).expect("re-add");
        assert_eq!(
            std::fs::read(&hist).expect("hist"),
            b"alpha\n".to_vec(),
            "no duplicate line",
        );
        let epoch: u64 = std::fs::read_to_string(&ts)
            .expect("ts")
            .trim()
            .parse()
            .expect("epoch parses");
        assert!(epoch > 1 && epoch <= now_secs(), "epoch refreshed to now");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_drops_the_matching_sidecar_slots() {
        let dir = scratch("retention-delete");
        let hist = dir.join("hist");
        let ts = dir.join("ts");
        std::fs::write(&hist, b"a\nb\nc\n").expect("hist");
        std::fs::write(&ts, b"10\n20\n30\n").expect("ts");
        let removed = removed_indices(&hist, b"b");
        assert_eq!(removed, vec![1]);
        drop_epochs(&ts, &removed).expect("drop");
        assert_eq!(std::fs::read(&ts).expect("ts"), b"10\n30\n".to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_decoded_decodes_the_stored_entry() {
        let dir = scratch("current");
        let current = dir.join("current");
        std::fs::write(&current, b"a<NEWLINE>b\n").expect("write");
        assert_eq!(current_decoded(&current), "a\nb");
        assert_eq!(current_decoded(&dir.join("missing")), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verb_text_prefers_a_non_empty_argument() {
        let dir = scratch("verb-text");
        let current = dir.join("current");
        std::fs::write(&current, b"stored\n").expect("write");
        assert_eq!(verb_text(Some("given"), &current), "given");
        assert_eq!(verb_text(Some(""), &current), "stored");
        assert_eq!(verb_text(None, &current), "stored");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pin_appends_once_and_reports_state() {
        let dir = scratch("pin");
        let path_env = notify_stub(&dir);
        let pins = dir.join("pins");
        pin_with("entry", &pins, &path_env).expect("pin");
        assert_eq!(std::fs::read(&pins).expect("pins"), b"entry\n".to_vec());
        pin_with("entry", &pins, &path_env).expect("pin again");
        assert_eq!(
            std::fs::read(&pins).expect("pins"),
            b"entry\n".to_vec(),
            "already-pinned does not re-append",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpin_scrubs_only_a_present_entry() {
        let dir = scratch("unpin");
        let path_env = notify_stub(&dir);
        let pins = dir.join("pins");
        std::fs::write(&pins, b"keep\ndrop\n").expect("write");
        unpin_with("drop", &pins, &path_env).expect("unpin");
        assert_eq!(std::fs::read(&pins).expect("pins"), b"keep\n".to_vec());
        unpin_with("absent", &pins, &path_env).expect("unpin absent");
        assert_eq!(
            std::fs::read(&pins).expect("pins"),
            b"keep\n".to_vec(),
            "not-pinned leaves the file untouched",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_decode_round_trip() {
        assert_eq!(clip::encode("a\nb"), "a<NEWLINE>b");
        assert_eq!(clip::decode("a<NEWLINE>b"), "a\nb");
    }

    #[test]
    fn watch_bails_when_wl_paste_is_missing() {
        let err = watch(Some("")).expect_err("wl-paste missing");
        assert!(format!("{err:#}").contains("wl-paste not found on PATH"));
    }
}
