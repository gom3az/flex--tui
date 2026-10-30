//! Toast overlay host: one kitty window, one stacked column of cards.
//!
//! The daemon no longer spawns a renderer per notification — that produced one
//! window per arrival, all pinned to the same coordinates, so a burst left only
//! the newest visible. Instead the daemon appends a [`ToastEvent`] to a runtime
//! spool and asks [`ensure_host`] for the single renderer, which drains the
//! spool, stacks up to [`TOAST_MAX_VISIBLE`] cards, queues the rest, counts each
//! card down, and exits when idle.
//!
//! Stored events carry only an id: the host re-reads the notification store, so
//! a `replaces_id` update or a dismissal in the center is reflected without a
//! second copy of the content. Transient events (flex's own feedback) carry the
//! item, because nothing is persisted for them.
//!
//! The window is fully transparent and each card paints its own opaque cell
//! background, so banners stay readable over a wallpaper without a large dark
//! rectangle around a one-card stack. Every cell is filled except a single
//! quadrant in each corner cell: the corner block's missing quarter is the
//! chamfer, which keeps the silhouette cut at 45° without leaving a notch that
//! reads as a clipped card.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flex_core::diag;
use serde::{Deserialize, Serialize};

use super::notify::{self, NotificationItem, NotifyState, Urgency};

/// Auto-dismiss delay used when a sender asked for the server default.
pub const TOAST_DEFAULT_SECS: u64 = 4;
/// Critical alerts retire sooner than ordinary ones: they are pinned in the
/// feed until dismissed, so the banner only has to interrupt, not persist.
pub const TOAST_CRITICAL_SECS: u64 = 3;
/// Upper bound for a sender-requested toast lifetime: a banner must never pin
/// itself to the screen for minutes because of a stray timeout.
pub const TOAST_MAX_SECS: u64 = 30;
/// Cards visible at once (maximum active slots).
pub const TOAST_SLOTS: usize = 3;
/// Legacy alias for single-stack limits.
pub const TOAST_MAX_VISIBLE: usize = 3;
/// Banners queued behind the visible stack before the oldest is dropped.
pub const TOAST_MAX_PENDING: usize = 5;

/// A queued card that waited longer than this is dropped: it is already in the
/// center and the badge, and a late banner is just noise.
const TOAST_PENDING_TTL_SECS: u64 = 5;
/// Poll interval for the spool and the store.
const TOAST_TICK: Duration = Duration::from_millis(200);
/// Events older than this are never shown: a crashed host must not replay a
/// stale banner minutes later.
const SPOOL_STALE_SECS: u64 = 10;
/// Crash debris older than this is deleted on the next enqueue.
const SPOOL_PRUNE_SECS: u64 = 600;
/// `pkill -f` pattern for toast processes.
const HOST_PATTERN: &str = "flex-notify.*toast";
/// Kitty flags shared by the unified host window.
///
/// The window itself is fully transparent: the card paints its own fill, and
/// kitty only applies `background_opacity` to cells whose background equals the
/// default background. `background=#000000` pins that default so the card fill
/// can never be mistaken for it (which would punch the card back out).
const TOAST_KITTY_ARGS: &[&str] = &[
    "--class",
    "flex-notify-toast",
    "-o",
    "font_size=11",
    "-o",
    "remember_window_size=no",
    "-o",
    "initial_window_width=50c",
    "-o",
    "initial_window_height=10c",
    "-o",
    "background=#000000",
    "-o",
    "background_opacity=0.0",
    "-o",
    "window_padding_width=0",
];

/// Opaque fill painted behind every card cell.
///
/// Without it the card is only borders and glyphs, and a transparent window
/// leaves the wallpaper visible *through* the card.
const CARD_FILL: &str = "\x1b[48;2;24;24;37m"; // #181825
/// Warmer fill for critical alerts, so urgency reads before the border colour.
const CARD_FILL_CRITICAL: &str = "\x1b[48;2;43;26;34m"; // #2b1a22
/// Restore the terminal default background.
///
/// Only the four corner cells use it: the missing quadrant of the corner block
/// is the chamfer, so exactly one quarter of that cell shows the wallpaper.
const BG_DEFAULT: &str = "\x1b[49m";
/// Attribute reset shared by every styled span in a card row.
const SGR_RESET: &str = "\x1b[0m";
/// Dimmed secondary text (queue footer).
const SGR_DIM: &str = "\x1b[2m";

/// One spooled toast request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ToastEvent {
    Stored { id: u32 },
    Transient { item: Box<NotificationItem> },
}

/// Identity of a card in the stack.
///
/// A stored card is keyed by notification id, so `replaces_id` updates the
/// card in place; a transient card is keyed by its spool file, because
/// transient notifications share id `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKey {
    Stored(u32),
    Transient(u64),
}

/// A visible card with its own deadline.
#[derive(Debug)]
struct Card {
    key: CardKey,
    item: NotificationItem,
    shown_at: u64,
    total: u64,
}

/// A card waiting for a visible slot.
#[derive(Debug)]
struct Pending {
    card: Card,
    created: u64,
}

/// Visible cards (newest first) plus the queue behind them.
#[derive(Debug, Default)]
pub struct ToastStack {
    visible: Vec<Card>,
    pending: VecDeque<Pending>,
}

impl ToastStack {
    /// Add or refresh a card. A repeated key replaces the content and restarts
    /// the countdown, which is what a progress notification wants.
    pub fn push(&mut self, key: CardKey, item: NotificationItem, total: u64, now: u64) {
        let total = total.max(1);
        if let Some(card) = self.visible.iter_mut().find(|card| card.key == key) {
            card.item = item;
            card.total = total;
            card.shown_at = now;
            return;
        }
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.card.key == key)
        {
            pending.card.item = item;
            pending.card.total = total;
            pending.created = now;
            return;
        }

        let card = Card {
            key,
            item,
            shown_at: now,
            total,
        };
        if self.visible.len() < TOAST_MAX_VISIBLE {
            self.visible.insert(0, card);
        } else {
            if self.pending.len() >= TOAST_MAX_PENDING {
                self.pending.pop_front();
            }
            self.pending.push_back(Pending { card, created: now });
        }
    }

    /// Expire finished and stale cards, promote queued ones. Returns whether
    /// the visible set changed.
    pub fn tick(&mut self, now: u64) -> bool {
        let before = self.shape();
        self.visible
            .retain(|card| now < card.shown_at.saturating_add(card.total));
        self.pending
            .retain(|pending| now.saturating_sub(pending.created) < TOAST_PENDING_TTL_SECS);
        while self.visible.len() < TOAST_MAX_VISIBLE {
            let Some(mut pending) = self.pending.pop_front() else {
                break;
            };
            pending.card.shown_at = now;
            self.visible.insert(0, pending.card);
        }
        self.shape() != before
    }

    /// Drop stored cards whose notification was dismissed or expired, and
    /// refresh ones whose content changed (a replacement resets the timer).
    pub fn sync_stored(&mut self, state: &NotifyState, now: u64) -> bool {
        let before = self.shape();

        for card in &mut self.visible {
            let CardKey::Stored(id) = card.key else {
                continue;
            };
            match state
                .notifications
                .iter()
                .find(|item| item.id == id && item.is_active(now))
            {
                Some(item) => {
                    if *item != card.item {
                        card.item = item.clone();
                        card.shown_at = now;
                    }
                }
                None => card.total = 0,
            }
        }
        self.visible.retain(|card| card.total > 0);

        self.pending.retain(|pending| match pending.card.key {
            CardKey::Stored(id) => state
                .notifications
                .iter()
                .any(|item| item.id == id && item.is_active(now)),
            CardKey::Transient(_) => true,
        });

        self.shape() != before
    }

    /// Whether there is nothing to render or wait for.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.visible.is_empty() && self.pending.is_empty()
    }

    /// Visible cards, newest first.
    fn visible(&self) -> &[Card] {
        &self.visible
    }

    /// Number of cards waiting for a slot.
    fn queued(&self) -> usize {
        self.pending.len()
    }

    fn shape(&self) -> (Vec<CardKey>, usize) {
        (
            self.visible.iter().map(|card| card.key).collect(),
            self.pending.len(),
        )
    }
}

/// Auto-dismiss delay for a toast card at `now`.
#[must_use]
pub fn toast_timeout_secs(item: &NotificationItem, now: u64) -> u64 {
    if let Some(at) = item.expires_at {
        return at.saturating_sub(now).clamp(1, TOAST_MAX_SECS);
    }
    if item.urgency == Urgency::Critical {
        TOAST_CRITICAL_SECS
    } else {
        TOAST_DEFAULT_SECS
    }
}

/// Directory holding pending toast events.
///
/// A custom state file keeps the spool beside it, so tests (and a second
/// daemon) never share a spool with the live one.
#[must_use]
pub fn spool_dir(state_path: Option<&Path>) -> PathBuf {
    base_dir(state_path).join("flex-notify-toasts")
}

fn base_dir(state_path: Option<&Path>) -> PathBuf {
    state_path
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(
            || {
                std::env::var("XDG_RUNTIME_DIR")
                    .map_or_else(|_| std::env::temp_dir(), PathBuf::from)
            },
            Path::to_path_buf,
        )
}

fn lock_path(state_path: Option<&Path>) -> PathBuf {
    base_dir(state_path).join("flex-notify-toasts.lock")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs())
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_millis())
}

fn ambient_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

fn path_env_or_ambient(path_env: Option<&str>) -> String {
    path_env.map_or_else(ambient_path, str::to_string)
}

fn warn_once(message: &str) {
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.set(()).is_ok() {
        diag::warn(message);
    }
}

/// Queue a stored notification for display.
///
/// The id must exist in the store: a manual `toast <id>` for a notification
/// that has already been cleared should do nothing rather than wake a host.
pub fn enqueue_stored(id: u32, state_path: Option<&Path>, path_env: Option<&str>) {
    let state = notify::load_state(state_path);
    if !state.notifications.iter().any(|item| item.id == id) {
        return;
    }
    enqueue(&ToastEvent::Stored { id }, state_path, path_env);
}

/// Queue a transient card (flex's own feedback, never persisted).
pub fn enqueue_transient(
    item: &NotificationItem,
    state_path: Option<&Path>,
    path_env: Option<&str>,
) {
    enqueue(
        &ToastEvent::Transient {
            item: Box::new(item.clone()),
        },
        state_path,
        path_env,
    );
}

fn enqueue(event: &ToastEvent, state_path: Option<&Path>, path_env: Option<&str>) {
    let path_env = path_env_or_ambient(path_env);
    // The drawer already shows everything; a banner on top of it is noise.
    if crate::popup::class_is_open_with(crate::popup::DRAWER_CLASS, &path_env) {
        return;
    }
    let dir = spool_dir(state_path);
    prune_spool(&dir, now_secs());
    limit_pending_spool(&dir);
    if let Err(err) = write_event_at(&dir, event) {
        warn_once(&format!("flex-notify: toast spool unavailable: {err}"));
        return;
    }
    ensure_host(&path_env, state_path);
}

fn limit_pending_spool(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    if paths.len() > TOAST_MAX_PENDING {
        let drop_count = paths.len() - TOAST_MAX_PENDING;
        for path in paths.iter().take(drop_count) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Whether a host holds the lock (and is therefore already rendering).
fn host_alive(state_path: Option<&Path>) -> bool {
    let Ok(file) = open_lock(state_path) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Spawn the host window when one is not already running.
fn ensure_host(path_env: &str, state_path: Option<&Path>) {
    if host_alive(state_path) {
        return;
    }
    let Some(bin) = notify::sibling_binary("flex-notify", path_env) else {
        return;
    };
    let Some(sh_bin) = resolve_tool("sh", path_env) else {
        return;
    };

    let kitty = format!("kitty {}", TOAST_KITTY_ARGS.join(" "));
    let mut cmd = Command::new(sh_bin);
    cmd.arg("-c");
    if let Some(path) = state_path {
        cmd.arg(format!("{kitty} -e \"$0\" --state-file \"$1\" toasts &"))
            .arg(bin)
            .arg(path);
    } else {
        cmd.arg(format!("{kitty} -e \"$0\" toasts &")).arg(bin);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("PATH", path_env);

    let _ = crate::spawn::spawn_and_reap(&mut cmd);
}

/// Compatibility bridge for slot execution.
pub fn run_slot_host(state_path: Option<&Path>, _slot: usize) {
    run_host(state_path, false);
}

fn write_event_at(dir: &Path, event: &ToastEvent) -> std::io::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(dir)?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("{:013}-{:06}.json", now_millis(), seq % 1_000_000);
    let tmp = dir.join(format!(".{name}.tmp"));
    let final_path = dir.join(name);
    let bytes = serde_json::to_vec(event).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

/// Unique key from a spool file name (`<millis>-<seq>.json`).
fn event_key(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".json")?;
    let (millis, seq) = stem.split_once('-')?;
    Some(millis.parse::<u64>().ok()? * 1_000_000 + seq.parse::<u64>().ok()?)
}

fn prune_spool(dir: &Path, now: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let expired = name
            .strip_suffix(".json")
            .and_then(|stem| stem.split_once('-'))
            .and_then(|(millis, _)| millis.parse::<u64>().ok())
            .is_some_and(|millis| now.saturating_sub(millis / 1_000) > SPOOL_PRUNE_SECS);
        let stale_tmp = name.starts_with('.')
            && Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"));
        if expired || stale_tmp {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Open (creating if needed) the host lock file for `state_path`.
fn open_lock(state_path: Option<&Path>) -> std::io::Result<File> {
    let path = lock_path(state_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Take the single-host lock, or `None` when another host owns it.
fn acquire_host_lock(state_path: Option<&Path>) -> Option<File> {
    let file = open_lock(state_path).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Close every banner and clear the queue.
///
/// Called when the notification center opens: the drawer supersedes the toast
/// stack, and a queued event must not replay after it closes.
pub fn dismiss_all(path_env: &str) {
    let _ = std::fs::remove_dir_all(spool_dir(None));
    let Some(bin) = resolve_tool("pkill", path_env) else {
        return;
    };
    let mut cmd = Command::new(bin);
    cmd.args(["-f", HOST_PATTERN])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = cmd.status();
}

fn resolve_tool(name: &str, path_env: &str) -> Option<PathBuf> {
    path_env
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
}

/// Run the toast host (`flex-notify toasts`).
///
/// `once` renders a single frame to stdout instead of opening a window loop;
/// it is the test seam and a way to inspect the stack from a shell.
pub fn run_host(state_path: Option<&Path>, once: bool) {
    let dir = spool_dir(state_path);
    if once {
        let now = now_secs();
        let state = notify::load_state(state_path);
        let mut stack = ToastStack::default();
        drain_spool(&mut stack, &dir, &state, now);
        let _ = stack.sync_stored(&state, now);
        let _ = stack.tick(now);
        let (width, _) = terminal_size();
        let text = render_stack(&stack, now, width, usize::MAX);
        let mut out = std::io::stdout();
        if !text.is_empty() {
            let _ = writeln!(out, "{text}");
        }
        let _ = out.flush();
        return;
    }

    let Some(_lock) = acquire_host_lock(state_path) else {
        return;
    };

    let mut stack = ToastStack::default();
    let mut last_second = u64::MAX;
    let mut last_shape = (Vec::new(), 0);

    let start_epoch = now_secs();
    let start_instant = Instant::now();

    loop {
        let now = start_epoch + start_instant.elapsed().as_secs();
        let state = notify::load_state(state_path);
        drain_spool(&mut stack, &dir, &state, now);
        let _ = stack.sync_stored(&state, now);
        let _ = stack.tick(now);

        if stack.is_empty() {
            break;
        }

        let shape = stack.shape();
        if now != last_second || shape != last_shape {
            let (width, rows) = terminal_size();
            let text = render_stack(&stack, now, width, rows);
            draw(&text, rows);
            last_second = now;
            last_shape = shape;
        }

        std::thread::sleep(TOAST_TICK);
    }

    clear_screen();
}

fn drain_spool(stack: &mut ToastStack, dir: &Path, state: &NotifyState, now: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    for path in paths {
        let Some(key) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(event_key)
        else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let written_secs = (key / 1_000_000) / 1_000;
        let too_old = now.saturating_sub(written_secs) > SPOOL_STALE_SECS;
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ToastEvent>(&bytes).ok());

        match parsed {
            Some(ToastEvent::Stored { id }) => {
                let item = state
                    .notifications
                    .iter()
                    .find(|item| item.id == id && item.is_active(now));
                match item {
                    Some(item) => {
                        stack.push(
                            CardKey::Stored(id),
                            item.clone(),
                            toast_timeout_secs(item, now),
                            now,
                        );
                        let _ = std::fs::remove_file(&path);
                    }
                    // The daemon saves the store before it spools the event, so
                    // a miss here means this host read the store a moment too
                    // early (or the entry was dismissed in between). Keep the
                    // file for the next tick instead of dropping the banner.
                    None if too_old => {
                        let _ = std::fs::remove_file(&path);
                    }
                    None => {}
                }
            }
            Some(ToastEvent::Transient { item }) => {
                if !too_old {
                    let item = *item;
                    let total = toast_timeout_secs(&item, now);
                    stack.push(CardKey::Transient(key), item, total, now);
                }
                let _ = std::fs::remove_file(&path);
            }
            // A malformed file is debris: never replay it.
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Render the column: as many cards as `rows` allows, then the queue count.
///
/// The host cannot resize its own window portably (Hyprland 0.56 dropped the
/// pixel-resize dispatcher and ignores client resize requests), so the window
/// rule owns the height and this trims the stack to it. A short window still
/// shows one card plus `+N`; a taller rule shows two or three.
#[must_use]
pub fn render_stack(stack: &ToastStack, now: u64, width: usize, rows: usize) -> String {
    let total = stack.visible().len();
    let mut hidden = stack.queued();
    let mut lines: Vec<String> = Vec::new();
    let target_width = width.max(36);

    for (index, card) in stack.visible().iter().enumerate() {
        let remaining = card.shown_at.saturating_add(card.total).saturating_sub(now);
        let mut card_lines: Vec<String> = format_card(&card.item, remaining, card.total, width)
            .lines()
            .map(str::to_string)
            .collect();
        let more_after = index + 1 < total || hidden > 0;
        // One transparent row between cards keeps two filled cards from merging
        // into a single block.
        let gap = usize::from(!lines.is_empty());
        let needed = card_lines.len() + usize::from(more_after) + gap;
        // The first card is always drawn (a too-short window clips its bottom
        // border); later ones only when they fit whole.
        if index > 0 && lines.len() + needed > rows {
            hidden += total - index;
            break;
        }
        if gap == 1 {
            lines.push(String::new());
        }

        if lines.len() + card_lines.len() > rows {
            let bottom_border = card_lines.pop().unwrap_or_default();
            card_lines.truncate(rows.saturating_sub(lines.len() + 1));
            card_lines.push(bottom_border);
        }

        lines.extend(card_lines);
    }

    if hidden > 0 && lines.len() < rows {
        if !lines.is_empty() && lines.len() + 2 <= rows {
            lines.push(String::new());
        }
        // The queue count rides the same fill so it reads as a card footer
        // rather than as text floating on the wallpaper.
        lines.push(paint_line(
            &format!("{SGR_DIM}  +{hidden} more{SGR_RESET}"),
            target_width,
            CARD_FILL,
        ));
    }
    lines.join("\n")
}

/// Terminal size in cells, with a sane fallback when stdout is a pipe.
fn terminal_size() -> (usize, usize) {
    crossterm::terminal::size().map_or((50, 24), |(cols, rows)| {
        (usize::from(cols).clamp(36, 120), usize::from(rows).max(1))
    })
}

/// Draw a frame in place, hiding the cursor.
fn draw(text: &str, terminal_rows: usize) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?25l\x1b[H");
    let mut rows = 0;
    for (idx, line) in text.lines().enumerate() {
        let _ = write!(out, "\x1b[{};1H{line}\x1b[K", idx + 1);
        rows = idx + 1;
    }
    if rows < terminal_rows {
        let _ = write!(out, "\x1b[{};1H\x1b[J", rows + 1);
    }
    let _ = out.flush();
}

fn clear_screen() {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[2J\x1b[H\x1b[?25h");
    let _ = out.flush();
}

/// Display columns of `text`, ignoring the SGR sequences these rows use.
fn visual_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;

    let mut width = 0;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Every escape here is a CSI ending in `m`; skip it whole.
            for esc in chars.by_ref() {
                if esc == 'm' {
                    break;
                }
            }
            continue;
        }
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    width
}

/// Paint `fill` behind every cell of one rendered row and pad it to `width`.
///
/// The fill has to be re-applied after *every* `\x1b[0m`: a row is a sequence of
/// independently styled spans, and a bare reset drops back to the default
/// (transparent) background, which would stripe the wallpaper through the card.
fn paint_line(line: &str, width: usize, fill: &str) -> String {
    let resumed = format!("{SGR_RESET}{fill}");
    let mut painted = String::with_capacity(line.len() + width * 2 + 32);
    painted.push_str(fill);
    painted.push_str(&line.replace(SGR_RESET, &resumed));
    let used = visual_width(line);
    if used < width {
        painted.push_str(&" ".repeat(width - used));
    }
    painted.push_str(SGR_RESET);
    painted
}

/// Format one toast card. Total width: `width` columns (minimum 36).
///
/// The header carries the sender and the live countdown; the bottom border is
/// the lifetime bar. Actions are not rendered as buttons — the window never
/// takes focus, so a `[1]` label would advertise a keypress that cannot arrive.
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn format_card(item: &NotificationItem, remaining: u64, total: u64, width: usize) -> String {
    use unicode_width::UnicodeWidthStr as _;

    let target_width = width.max(36);
    let inner_width = target_width.saturating_sub(4);

    let (border_ansi, bar_ansi, icon, icon_ansi) = match item.urgency {
        Urgency::Critical => ("\x1b[1;31m", "\x1b[1;31m", "󰀦", "\x1b[1;31m"),
        Urgency::Normal => ("\x1b[90m", "\x1b[37m", "󰂚", "\x1b[37m"),
        Urgency::Low => ("\x1b[2;37m", "\x1b[2;37m", "󰂞", "\x1b[2;37m"),
    };

    let reset = SGR_RESET;
    let bold = "\x1b[1m";
    let dim = "\x1b[2m";
    let fill = match item.urgency {
        Urgency::Critical => CARD_FILL_CRITICAL,
        Urgency::Normal | Urgency::Low => CARD_FILL,
    };

    let app = truncate_width(&item.app_name, target_width.saturating_sub(18));
    // "▟─ " + icon + " " before the app name.
    let left_width = 4 + icon.width() + app.width();

    let countdown = format!("{remaining}s");
    let right_width = countdown.width() + 4; // "s ─▙"
    let fill_len = target_width.saturating_sub(left_width + right_width);
    let fill_dashes = "─".repeat(fill_len);

    // Chamfered corners: each corner cell is a 3/4 block whose missing quadrant
    // points outward, so the card silhouette is cut at 45° and the frame still
    // runs unbroken through the block. Top-left loses its upper-left quadrant,
    // top-right its upper-right, and so on.
    let (top_left, top_right) = ("▟", "▙");
    let (bottom_left, bottom_right) = ("▜", "▛");

    let top_line = format!(
        "{BG_DEFAULT}{border_ansi}{top_left}{fill}─ {reset}{icon_ansi}{icon}{reset} {bold}{app}{reset} {border_ansi}{fill_dashes}{reset}{dim}{countdown}{reset} {border_ansi}─{BG_DEFAULT}{border_ansi}{top_right}{reset}"
    );

    let make_content_row = |spans: &str, width_used: usize| -> String {
        let pad = " ".repeat(inner_width.saturating_sub(width_used));
        format!("{border_ansi}│{reset} {spans}{pad} {border_ansi}│{reset}")
    };

    let mut lines = Vec::new();
    lines.push(top_line);

    let summary = item.summary.replace('\n', " ");
    let summary = truncate_width(&summary, inner_width);
    lines.push(make_content_row(
        &format!("{bold}{summary}{reset}"),
        summary.width(),
    ));

    let body = item.body.trim();
    if !body.is_empty() {
        let (wrapped, truncated) = wrap_text(body, inner_width, 2);
        let last = wrapped.len().saturating_sub(1);
        for (idx, line) in wrapped.iter().enumerate() {
            let mut line = line.clone();
            if truncated && idx == last {
                while line.width() + 1 > inner_width {
                    line.pop();
                }
                line.push('…');
            }
            let used = line.width();
            lines.push(make_content_row(&format!("{dim}{line}{reset}"), used));
        }
    }

    if let Some(progress) = item.progress {
        let pct = (progress.clamp(0.0, 1.0) * 100.0) as usize;
        let bar_max = inner_width.saturating_sub(17).max(4);
        let filled = (bar_max * pct) / 100;
        let empty = bar_max.saturating_sub(filled);
        let bar = format!(
            "Progress: [{bar_ansi}{}{reset}{dim}{}{reset}] {pct:>3}%",
            "█".repeat(filled),
            "░".repeat(empty)
        );
        let used = format!("Progress: [{}] {pct:>3}%", " ".repeat(bar_max)).width();
        lines.push(make_content_row(&bar, used));
    }

    if !item.actions.is_empty() {
        let hint = truncate_width("󰌌 Actions in the notification center", inner_width);
        let used = hint.width();
        lines.push(make_content_row(&format!("{dim}{hint}{reset}"), used));
    }

    let bar_width = target_width.saturating_sub(2);
    let filled = (bar_width as u64)
        .saturating_mul(remaining.min(total))
        .checked_div(total)
        .map_or(0, |spent| usize::try_from(spent).unwrap_or(bar_width))
        .min(bar_width);
    let empty = bar_width.saturating_sub(filled);
    let bottom = format!(
        "{BG_DEFAULT}{border_ansi}{bottom_left}{fill}{bar_ansi}{}{border_ansi}{}{BG_DEFAULT}{border_ansi}{bottom_right}{reset}",
        "━".repeat(filled),
        "─".repeat(empty)
    );
    lines.push(bottom);

    lines
        .iter()
        .map(|line| paint_line(line, target_width, fill))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncate to `max` display columns, marking the cut with `…`.
#[must_use]
pub fn truncate_width(text: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    if text.width() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width + 1 > max {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('…');
    out
}

/// Calculate the required window lines to display `item` at `width` columns.
#[must_use]
pub fn card_required_rows(item: &NotificationItem, width: usize) -> usize {
    let target_width = width.max(36);
    let inner_width = target_width.saturating_sub(4);
    let mut rows = 2; // Top border + Summary
    let body = item.body.trim();
    if !body.is_empty() {
        let (wrapped, _) = wrap_text(body, inner_width, 2);
        rows += wrapped.len();
    }
    if item.progress.is_some() {
        rows += 1;
    }
    if !item.actions.is_empty() {
        rows += 1;
    }
    rows += 1; // Bottom border
    rows
}

/// Split `text` into at most `max_lines` lines of `width` columns, wrapping on
/// whitespace and preserving explicit newlines. Returns whether content was dropped.
#[must_use]
pub fn wrap_text(text: &str, width: usize, max_lines: usize) -> (Vec<String>, bool) {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr as _};

    if max_lines == 0 || width == 0 {
        return (Vec::new(), !text.trim().is_empty());
    }

    let mut lines = Vec::new();
    let mut truncated = false;
    let paragraphs: Vec<&str> = text
        .split('\n')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();

    for (p_idx, paragraph) in paragraphs.iter().enumerate() {
        let mut current_line = String::new();
        let mut current_width = 0usize;
        let words: Vec<&str> = paragraph.split_whitespace().collect();

        for word in &words {
            let word_w = word.width();
            if current_line.is_empty() {
                if word_w <= width {
                    current_line.push_str(word);
                    current_width = word_w;
                } else {
                    for ch in word.chars() {
                        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if current_width + ch_w > width {
                            lines.push(std::mem::take(&mut current_line));
                            current_width = 0;
                            if lines.len() == max_lines {
                                truncated = true;
                                break;
                            }
                        }
                        current_line.push(ch);
                        current_width += ch_w;
                    }
                    if truncated {
                        break;
                    }
                }
            } else if current_width + 1 + word_w <= width {
                current_line.push(' ');
                current_line.push_str(word);
                current_width += 1 + word_w;
            } else {
                lines.push(std::mem::take(&mut current_line));
                if lines.len() == max_lines {
                    truncated = true;
                    break;
                }
                if word_w <= width {
                    current_line.push_str(word);
                    current_width = word_w;
                } else {
                    current_width = 0;
                    for ch in word.chars() {
                        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if current_width + ch_w > width {
                            lines.push(std::mem::take(&mut current_line));
                            current_width = 0;
                            if lines.len() == max_lines {
                                truncated = true;
                                break;
                            }
                        }
                        current_line.push(ch);
                        current_width += ch_w;
                    }
                    if truncated {
                        break;
                    }
                }
            }
        }

        if truncated {
            break;
        }

        if !current_line.is_empty() {
            lines.push(current_line);
            if lines.len() == max_lines {
                if p_idx + 1 < paragraphs.len() {
                    truncated = true;
                }
                break;
            }
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    (lines, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: u32, body: &str) -> NotificationItem {
        let mut item = NotificationItem::new(id, "Cargo", "Build finished", body, Urgency::Normal);
        item.timestamp = 100;
        item
    }

    #[test]
    fn toast_timeout_follows_the_sender_deadline_within_bounds() {
        let now = 100;
        let mut item = sample(1, "");
        assert_eq!(toast_timeout_secs(&item, now), TOAST_DEFAULT_SECS);

        item.urgency = Urgency::Critical;
        assert_eq!(toast_timeout_secs(&item, now), TOAST_CRITICAL_SECS);

        item.urgency = Urgency::Normal;
        item.expires_at = Some(now + 2);
        assert_eq!(toast_timeout_secs(&item, now), 2);
        item.expires_at = Some(now + 999);
        assert_eq!(toast_timeout_secs(&item, now), TOAST_MAX_SECS);
        item.expires_at = Some(now);
        assert_eq!(toast_timeout_secs(&item, now), 1);
        item.expires_at = Some(now - 50);
        assert_eq!(toast_timeout_secs(&item, now), 1);
    }

    #[test]
    fn card_lines_share_one_width_and_drop_the_button_labels() {
        use unicode_width::UnicodeWidthStr as _;

        let mut item = sample(
            1,
            "one two three four five six seven eight nine ten eleven twelve thirteen",
        );
        item.actions = vec![crate::exec::notify::NotificationAction {
            id: "open".to_string(),
            title: "Open".to_string(),
        }];

        let card = format_card(&item, 2, 4, 50);
        for line in card.lines() {
            let clean = strip_ansi(line);
            assert_eq!(clean.width(), 50, "ragged line: {clean:?}");
        }
        assert!(card.contains("2s"), "the header carries the countdown");
        assert!(
            !card.contains("[1]"),
            "a non-focusable window must not advertise keypresses"
        );
        assert!(card.contains("notification center"));
        assert!(card.contains('━'), "the bottom border is the lifetime bar");
    }

    #[test]
    fn toast_window_keeps_the_transparency_contract() {
        // The window must stay see-through so only the painted card shows, and
        // the default background must differ from CARD_FILL or kitty would
        // treat the fill as "default" and make it transparent as well.
        assert!(TOAST_KITTY_ARGS.contains(&"background_opacity=0.0"));
        assert!(TOAST_KITTY_ARGS.contains(&"background=#000000"));
        assert_ne!(CARD_FILL, CARD_FILL_CRITICAL);
    }

    #[test]
    fn card_paints_an_opaque_fill_under_every_cell() {
        use unicode_width::UnicodeWidthStr as _;

        let mut item = sample(
            1,
            "Root partition capacity reached 98% and the card must stay readable",
        );
        item.actions = vec![crate::exec::notify::NotificationAction {
            id: "clean".to_string(),
            title: "Clean".to_string(),
        }];
        let card = format_card(&item, 2, 4, 50);

        for line in card.lines() {
            assert!(
                line.starts_with(CARD_FILL),
                "row does not open with the card fill: {line:?}"
            );
            assert_eq!(strip_ansi(line).width(), 50, "ragged row: {line:?}");
            // Each span ends with a reset; the fill must come straight back or
            // the wallpaper shows through the card. The final reset is the
            // line terminator and needs no fill after it.
            for (idx, _) in line.match_indices(SGR_RESET) {
                let rest = &line[idx + SGR_RESET.len()..];
                if rest.is_empty() {
                    continue;
                }
                assert!(
                    rest.starts_with(CARD_FILL),
                    "unpainted reset leaves a hole: {line:?}"
                );
            }
        }
    }

    #[test]
    fn card_chamfers_its_corners_with_quadrant_blocks() {
        let card = format_card(&sample(1, "body"), 4, 4, 50);
        let plain = strip_ansi(&card);
        let mut lines = plain.lines();
        let top = lines.next().expect("top border");
        let bottom = plain.lines().last().expect("bottom border");

        // The corner block is 3/4 of a cell, so exactly one quadrant of each
        // corner cell restores the default (transparent) background.
        assert!(top.starts_with("▟─ "), "{top:?}");
        assert!(top.ends_with("─▙"), "{top:?}");
        assert!(bottom.starts_with('▜'), "{bottom:?}");
        assert!(bottom.ends_with('▛'), "{bottom:?}");

        for line in card.lines() {
            assert!(
                line.starts_with(CARD_FILL),
                "row opens on the fill: {line:?}"
            );
        }
        // Only the four corner cells go transparent; a fifth one would be a hole.
        assert_eq!(
            card.matches(BG_DEFAULT).count(),
            4,
            "the chamfer is one transparent quadrant per corner"
        );
        assert!(
            !plain.contains('╭')
                && !plain.contains('╮')
                && !plain.contains('╰')
                && !plain.contains('╯'),
            "rounded corners are gone"
        );
    }

    #[test]
    fn critical_cards_use_a_warmer_fill() {
        let mut item = sample(1, "boom");
        item.urgency = Urgency::Critical;
        let card = format_card(&item, 4, 4, 50);
        assert!(card
            .lines()
            .all(|line| line.starts_with(CARD_FILL_CRITICAL)));
        assert!(
            !card.contains(CARD_FILL),
            "critical cards must not use the neutral fill"
        );
    }

    #[test]
    fn stacked_cards_are_separated_by_a_transparent_row() {
        let mut stack = ToastStack::default();
        stack.push(CardKey::Stored(1), sample(1, "First"), 4, 10);
        stack.push(CardKey::Stored(2), sample(2, "Second"), 4, 10);

        let rendered = render_stack(&stack, 10, 50, 10);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 9, "two 4-row cards plus one gap row");
        assert!(
            lines[4].is_empty(),
            "gap row is transparent: {:?}",
            lines[4]
        );
        assert!(lines[3].contains('▛'));
        assert!(lines[5].contains('▟'));
    }

    #[test]
    fn queued_footer_is_filled_and_padded() {
        use unicode_width::UnicodeWidthStr as _;

        let mut stack = ToastStack::default();
        for id in 1..=5 {
            stack.push(CardKey::Stored(id), sample(id, ""), 4, 10);
        }
        assert_eq!(stack.queued(), 2);

        let rendered = render_stack(&stack, 10, 50, 10);
        let footer = rendered
            .lines()
            .find(|line| line.contains("more"))
            .expect("queue footer");
        assert!(footer.starts_with(CARD_FILL), "footer rides the fill");
        assert_eq!(strip_ansi(footer).width(), 50, "footer is padded");
        assert_eq!(strip_ansi(footer).trim_end(), "  +3 more");
    }

    #[test]
    fn card_wraps_the_body_to_two_lines_and_counts_down() {
        let item = sample(
            1,
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau",
        );
        let card = format_card(&item, 4, 4, 44);
        assert_eq!(
            card.lines().count(),
            5,
            "top, summary, two body lines, bottom border"
        );
        assert!(card.contains("4s"), "the header counts down");
        assert!(card.contains('…'), "the overflow is marked");
    }

    #[test]
    fn stack_queues_past_the_visible_limit_and_promotes() {
        let mut stack = ToastStack::default();
        for id in 1..=5 {
            stack.push(CardKey::Stored(id), sample(id, ""), 4, 10);
        }
        assert_eq!(stack.visible().len(), TOAST_MAX_VISIBLE);
        assert_eq!(stack.queued(), 2);

        // Newest visible is first; 4 and 5 wait behind it.
        let shown: Vec<CardKey> = stack.visible().iter().map(|card| card.key).collect();
        assert_eq!(
            shown,
            vec![CardKey::Stored(3), CardKey::Stored(2), CardKey::Stored(1)]
        );

        // Expiry frees all three slots; the queue is promoted newest-first.
        let changed = stack.tick(14);
        assert!(changed);
        assert_eq!(stack.queued(), 0);
        let promoted: Vec<CardKey> = stack.visible().iter().map(|card| card.key).collect();
        assert_eq!(promoted, vec![CardKey::Stored(5), CardKey::Stored(4)]);
    }

    #[test]
    fn stack_refreshes_a_replaced_key_without_duplicating_it() {
        let mut stack = ToastStack::default();
        stack.push(CardKey::Stored(7), sample(7, "10%"), 30, 10);
        stack.push(CardKey::Stored(7), sample(7, "90%"), 30, 15);

        assert_eq!(stack.visible().len(), 1);
        assert_eq!(stack.visible()[0].item.body, "90%");
        assert_eq!(stack.visible()[0].shown_at, 15, "the timer restarts");
    }

    #[test]
    fn queued_cards_expire_if_no_slot_opens() {
        let mut stack = ToastStack::default();
        for id in 1..=5 {
            stack.push(CardKey::Stored(id), sample(id, ""), 4, 10);
        }
        assert_eq!(stack.queued(), 2);

        stack.tick(10 + TOAST_PENDING_TTL_SECS + 1);
        assert_eq!(stack.queued(), 0, "a late banner is noise");
    }

    #[test]
    fn stored_cards_close_when_the_notification_is_dismissed() {
        let mut stack = ToastStack::default();
        let item = sample(3, "");
        stack.push(CardKey::Stored(3), item.clone(), 4, 10);
        let mut state = NotifyState::default();
        state.notifications.push(item);

        assert!(!stack.sync_stored(&state, 11));

        state.notifications[0].is_dismissed = true;
        assert!(stack.sync_stored(&state, 12));
        assert!(stack.is_empty());
    }

    #[test]
    fn wrap_text_reports_truncation() {
        let (lines, truncated) = wrap_text("abcdefghij", 4, 2);
        assert_eq!(lines.len(), 2);
        assert!(truncated);

        let (lines, truncated) = wrap_text("abcd", 4, 2);
        assert_eq!(lines, vec!["abcd".to_string()]);
        assert!(!truncated);
    }

    #[test]
    fn transient_events_round_trip_through_the_spool() {
        let dir = std::env::temp_dir().join(format!("flex-toast-spool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut item = sample(0, "/tmp/gone; rm -rf $HOME");
        item.app_name = "flex-notify".to_string();
        let written = write_event_at(
            &dir,
            &ToastEvent::Transient {
                item: Box::new(item.clone()),
            },
        )
        .expect("spool write");
        assert!(written.starts_with(&dir));

        let mut stack = ToastStack::default();
        drain_spool(&mut stack, &dir, &NotifyState::default(), 100);

        assert_eq!(stack.visible().len(), 1);
        assert_eq!(
            stack.visible()[0].item.body,
            "/tmp/gone; rm -rf $HOME",
            "the text travels as JSON, never through a shell"
        );
        assert_eq!(
            std::fs::read_dir(&dir).expect("dir").count(),
            0,
            "the drained event is removed from the spool"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_spool_events_are_skipped_on_start() {
        let dir = std::env::temp_dir().join(format!("flex-toast-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let item = sample(0, "old");
        write_event_at(
            &dir,
            &ToastEvent::Transient {
                item: Box::new(item),
            },
        )
        .expect("write");
        let mut stack = ToastStack::default();
        drain_spool(
            &mut stack,
            &dir,
            &NotifyState::default(),
            now_secs() + SPOOL_STALE_SECS + 1,
        );
        assert!(stack.is_empty(), "a crashed host must not replay banners");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_stack_respects_window_rows_and_shows_queue_indicator() {
        let mut stack = ToastStack::default();
        for id in 1..=3 {
            stack.push(CardKey::Stored(id), sample(id, "short body"), 4, 10);
        }
        // At full height (50 rows), all 3 cards render without queue indicator.
        let full = render_stack(&stack, 10, 50, 50);
        assert!(!full.contains('+'), "no queue indicator when all fit");
        let card_count = strip_ansi(&full).matches("▟─").count();
        assert_eq!(card_count, 3);

        // Limited to 6 rows: the first card takes 4 lines, second card would
        // need a gap row + 4 lines > 2 remaining, so only 1 card renders and
        // "+2 more" is shown below it.
        let clipped = render_stack(&stack, 10, 50, 6);
        assert_eq!(strip_ansi(&clipped).matches("▟─").count(), 1);
        assert!(strip_ansi(&clipped).contains("+2 more"));

        // Limited to 4 rows: exactly 1 card fits, queue indicator omitted to prevent overflow.
        let exact = render_stack(&stack, 10, 50, 4);
        assert_eq!(strip_ansi(&exact).matches("▟─").count(), 1);
        assert!(!exact.contains("+2 more"));
    }

    #[test]
    fn wrap_text_preserves_explicit_newlines() {
        let (lines, truncated) = wrap_text("Line 1\nLine 2", 50, 2);
        assert_eq!(lines, vec!["Line 1".to_string(), "Line 2".to_string()]);
        assert!(!truncated);
    }

    #[test]
    fn wrap_text_breaks_at_word_boundaries() {
        let (lines, truncated) = wrap_text("The quick brown fox jumps over the lazy dog", 15, 2);
        assert_eq!(
            lines,
            vec!["The quick brown".to_string(), "fox jumps over".to_string()]
        );
        assert!(truncated);
    }

    #[test]
    fn card_required_rows_matches_content_shape() {
        let empty = sample(1, "");
        assert_eq!(card_required_rows(&empty, 50), 3, "top, summary, bottom");

        let single = sample(1, "Single line message");
        assert_eq!(
            card_required_rows(&single, 50),
            4,
            "top, summary, body, bottom"
        );

        let two_lines = sample(1, "First line\nSecond line");
        assert_eq!(
            card_required_rows(&two_lines, 50),
            5,
            "top, summary, line 1, line 2, bottom"
        );

        let mut with_actions = sample(1, "Message");
        with_actions.actions = vec![crate::exec::notify::NotificationAction {
            id: "ok".to_string(),
            title: "OK".to_string(),
        }];
        assert_eq!(
            card_required_rows(&with_actions, 50),
            5,
            "top, summary, body, action hint, bottom"
        );
    }

    #[test]
    fn format_card_preserves_two_line_body() {
        let item = sample(1, "First line of content\nSecond line of content");
        let card = format_card(&item, 4, 4, 50);
        let lines: Vec<&str> = card.lines().collect();
        assert_eq!(lines.len(), 5, "top, summary, body 1, body 2, bottom");
        assert!(lines[2].contains("First line of content"));
        assert!(lines[3].contains("Second line of content"));
    }

    #[test]
    fn render_stack_renders_multiple_stacked_cards_with_independent_bars() {
        let mut stack = ToastStack::default();
        stack.push(CardKey::Stored(1), sample(1, "First item body"), 4, 10);
        stack.push(CardKey::Stored(2), sample(2, "Second item body"), 4, 10);

        let rendered = render_stack(&stack, 10, 50, 10);
        let plain = strip_ansi(&rendered);
        assert_eq!(plain.matches("▟─").count(), 2, "both top borders render");
        assert_eq!(plain.matches('▜').count(), 2, "both bottom borders render");
        assert!(rendered.contains("First item body"));
        assert!(rendered.contains("Second item body"));
    }

    #[test]
    fn when_card_expires_stacked_card_shifts_to_top() {
        let mut stack = ToastStack::default();
        let item1 = sample(1, "First notification");
        let item2 = sample(2, "Second notification");
        stack.push(CardKey::Stored(1), item1, 4, 10);
        stack.push(CardKey::Stored(2), item2, 4, 10);

        assert_eq!(stack.visible().len(), 2);

        let mut state = NotifyState::default();
        let mut active2 = sample(2, "Second notification");
        active2.timestamp = 10;
        state.notifications.push(active2);

        let changed = stack.sync_stored(&state, 12);
        assert!(changed);
        assert_eq!(stack.visible().len(), 1);
        assert_eq!(stack.visible()[0].key, CardKey::Stored(2));

        let rendered = render_stack(&stack, 12, 50, 10);
        assert_eq!(strip_ansi(&rendered).matches("▟─").count(), 1);
        assert!(rendered.contains("Second notification"));
    }

    #[test]
    fn limit_pending_spool_drops_oldest_excess_events() {
        let dir = std::env::temp_dir().join(format!("flex-toast-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");

        for i in 1..=7 {
            let item = sample(i, "burst");
            write_event_at(
                &dir,
                &ToastEvent::Transient {
                    item: Box::new(item),
                },
            )
            .expect("write");
        }

        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 7);
        limit_pending_spool(&dir);
        assert_eq!(
            std::fs::read_dir(&dir).expect("dir").count(),
            TOAST_MAX_PENDING
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for ch in input.chars() {
            if ch == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if ch == 'm' {
                    in_escape = false;
                }
            } else {
                out.push(ch);
            }
        }
        out
    }
}
