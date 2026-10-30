//! `flex-core` — reusable Rust TUI menu engine.
//!
//! The engine selects rows and reports an [`Outcome`]; it never executes side
//! effects and knows nothing about any particular desktop setup. Callers (see
//! the `flex-rice` crate) build the rows, install a [`TickHook`] when those
//! rows need periodic refreshing, then print the `ACTION:` line themselves
//! (see [`backend`]).

pub mod backend;
pub mod charset;
pub mod diag;
pub mod filter;
pub mod keys;
pub mod meter;
pub mod preview;
pub mod render;
pub mod run;
pub mod strings;
pub mod theme;
pub mod width;

pub use charset::{CharSet, CharSetName};
pub use theme::{ColorRole, ColorScheme, Theme, ThemeName};

use std::cell::{Ref, RefCell};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Stable identifier for a [`Row`]'s action.
///
/// For the `clip` provider this is the content-hash hex (Q2) so wrappers can
/// round-trip history entries across runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RowId(pub String);

impl RowId {
    /// Create an id from a precomputed hex string.
    #[must_use]
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// Borrow the inner hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// FNV-1a 64-bit hex of `content` (std-only, stable across runs).
///
/// Used for `clip` rows (Q2) until M2 wires the real provider parsing.
#[must_use]
pub fn content_hash_hex(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in content.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Memoized sanitize+measure of one optional text field (OPT-3).
///
/// Rows mutate their `String` fields directly, so the cache validates by
/// content: a hit does no work, a miss re-sanitizes once. Draw paths hold
/// the owning row guard while rendering, so a frame costs at most one
/// sanitize per changed field and zero per unchanged field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CachedText {
    /// Last-seen raw content (`None` = the field was `None`).
    pub(crate) src: Option<String>,
    /// Sanitized text for `src` (what the draw paths render).
    pub(crate) sanitized: String,
    /// Measure over [`CachedText::sanitized`] (byte ranges index it).
    pub(crate) measured: width::Measured,
}

impl CachedText {
    /// Refresh from `current`, returning the cached pair.
    fn refresh(&mut self, current: Option<&str>) -> Option<(&str, &width::Measured)> {
        if self.src.as_deref() != current {
            if let Some(text) = current {
                let sanitized = width::sanitize_measured(text);
                self.src = Some(text.to_owned());
                self.sanitized = sanitized.text;
                self.measured = sanitized.measured;
            } else {
                self.src = None;
                self.sanitized.clear();
                self.measured = width::Measured::default();
            }
        }
        if self.src.is_some() {
            Some((&self.sanitized, &self.measured))
        } else {
            None
        }
    }
}

/// Memoized sanitized texts of a [`Row`] (OPT-3).
///
/// One guard covers a whole `draw_node`: every label/meta/config/detail/
/// sublabel borrow comes from it, so there is no per-cell sanitize or
/// measure on the draw path.
#[derive(Debug, Clone, Default)]
pub(crate) struct RowTextCache {
    pub(crate) label: CachedText,
    pub(crate) meta: CachedText,
    pub(crate) config: CachedText,
    pub(crate) detail: CachedText,
    pub(crate) sublabel: CachedText,
}

/// A row's selectable target (wiremix `view::Target` + its title).
///
/// Shown right-aligned in the header (with the `default_stream` marker `◇`
/// when [`Target::is_default`]) and listed in the row's dropdown
/// (`Enter`/`c`), mirroring upstream `node_targets` + `DropdownWidget`.
#[derive(Debug, Clone)]
pub struct Target {
    /// Opaque id reported on the `ACTION:TARGET` line.
    pub id: RowId,
    /// Display title (dropdown item + header right column).
    pub title: String,
    /// Upstream `Target::Default`: prefix the header title with `◇`.
    pub is_default: bool,
    /// Memoized sanitize+measure of [`Target::title`] (OPT-3).
    title_cache: RefCell<CachedText>,
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.title == other.title && self.is_default == other.is_default
    }
}

impl Eq for Target {}

impl Target {
    #[must_use]
    pub fn new(id: RowId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            is_default: false,
            title_cache: RefCell::default(),
        }
    }

    /// Build the upstream `Target::Default` entry (`◇` in the header).
    #[must_use]
    pub fn default_target(id: RowId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            is_default: true,
            title_cache: RefCell::default(),
        }
    }

    /// Borrow the sanitized title, refreshing the memo first (OPT-3).
    ///
    /// Hold the guard while rendering: the header and the dropdown share
    /// it, so a title is sanitized at most once per content change.
    pub(crate) fn title_texts(&self) -> Ref<'_, CachedText> {
        {
            let mut cache = self.title_cache.borrow_mut();
            cache.refresh(Some(&self.title));
        }
        self.title_cache.borrow()
    }
}

/// Peak levels for one row, as upstream `node.peaks`/`node.positions` express
/// them: values are linear amplitudes, and the channel count picks the meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowPeaks {
    /// One averaged channel (upstream mono rendering).
    Mono(f32),
    /// Left/right channels (upstream stereo rendering).
    Stereo(f32, f32),
}

/// Peak-meter rendering mode (upstream `Peaks`, `config.rs:97-102`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Peaks {
    /// No meters at all (`--peaks off`).
    Off,
    /// Force mono meters even for stereo rows (`--peaks mono`).
    Mono,
    /// Stereo when the row has two channels, mono otherwise (upstream default).
    #[default]
    Auto,
}

/// A single selectable menu entry.
///
/// The bool fields are independent row attributes (danger, offline, default,
/// muted), not a flag bag: they mirror distinct upstream `Node` properties.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Row {
    /// Opaque action id handed to wrappers via the `ACTION:` line.
    pub id: RowId,
    /// Human-readable label (may be truncated at render time).
    pub label: String,
    /// Right-aligned metadata (e.g. `87%`, `wlan0`), used when the row has no
    /// [`Row::targets`]. Rendered in the `node_target` style (terminal
    /// default, matching upstream) — not dimmed.
    pub meta: Option<String>,
    /// Danger rows (shutdown/reboot/…) are triple-coded at render time.
    pub confirmable: bool,
    /// Offline rows (e.g. center `— offline` when `nmcli` finds no Wi-Fi
    /// interface) render dim like [`Menu::offline`], but per-row: one
    /// offline tab must not dim its siblings. Wrappers treat them as
    /// no-ops (`noop` id, mirroring the bash `noop) :` arm).
    pub offline: bool,
    /// Default row: draws the `default_device` marker (`◇`) in the header's
    /// marker column (upstream `Node::is_default_sink`/`is_default_source`).
    pub is_default: bool,
    /// Cube-root volume in `0.0..` (upstream's mean channel volume after
    /// `cbrt`), e.g. `Some(0.85)` renders the label `85%`. `None` leaves the
    /// detail line without a volume bar.
    pub volume: Option<f32>,
    /// Muted rows show `muted` in the volume label area (upstream
    /// `Node::mute`).
    pub muted: bool,
    /// Peak levels for the detail line's meter (upstream `Node::peaks`).
    pub peaks: Option<RowPeaks>,
    /// Device-style config line (`▼ profile`) drawn on the detail line
    /// (upstream `DeviceWidget`, `device_widget.rs:141-153`).
    pub config: Option<String>,
    /// Plain text detail line (e.g. notification body) drawn on line 3 without `▼`.
    pub detail: Option<String>,
    /// Middle line text (e.g. notification summary) drawn on line 2 of a 3-line node.
    pub sublabel: Option<String>,
    /// Dropdown targets, in display order (upstream `node_targets`). Empty
    /// means the row has no dropdown and the header shows [`Row::meta`].
    pub targets: Vec<Target>,
    /// Highlighted target when the dropdown opens (upstream `node_targets`
    /// returns the current target's position); also the title shown in the
    /// header. Clamped to `targets.len() - 1`.
    pub target_index: usize,
    /// Absolute path of the image this row previews, when the provider shows
    /// one (`wallpaper`). Display-only: the preview pane
    /// ([`preview`](crate::preview)) reads it, while ids, labels, filtering
    /// and the `ACTION:` contract are unaffected.
    pub preview_image: Option<String>,
    /// When true, the row's targets are not drawn in the header right column,
    /// leaving space for [`Row::meta`] (e.g. notification timestamps).
    pub hide_target_in_header: bool,
    /// Compact 1-line node rendering with 0 spacing (e.g. child notification thread rows).
    pub compact: bool,
    /// Memoized sanitized texts (OPT-3); ignored by `PartialEq`.
    text_cache: RefCell<RowTextCache>,
}

impl PartialEq for Row {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.label == other.label
            && self.meta == other.meta
            && self.confirmable == other.confirmable
            && self.offline == other.offline
            && self.is_default == other.is_default
            && self.volume == other.volume
            && self.muted == other.muted
            && self.peaks == other.peaks
            && self.config == other.config
            && self.detail == other.detail
            && self.sublabel == other.sublabel
            && self.targets == other.targets
            && self.target_index == other.target_index
            && self.preview_image == other.preview_image
            && self.hide_target_in_header == other.hide_target_in_header
            && self.compact == other.compact
    }
}

impl Row {
    /// Build a plain (non-danger, meta-less) row.
    #[must_use]
    pub fn new(id: RowId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            meta: None,
            confirmable: false,
            offline: false,
            is_default: false,
            volume: None,
            muted: false,
            peaks: None,
            config: None,
            detail: None,
            sublabel: None,
            targets: Vec::new(),
            target_index: 0,
            preview_image: None,
            hide_target_in_header: false,
            compact: false,
            text_cache: RefCell::default(),
        }
    }

    /// Build a row with right-aligned metadata.
    #[must_use]
    pub fn with_meta(id: RowId, label: impl Into<String>, meta: impl Into<String>) -> Self {
        Self {
            meta: Some(meta.into()),
            ..Self::new(id, label)
        }
    }

    /// Build a confirmable row (requires armed confirm via `keys`).
    #[must_use]
    pub fn confirmable(id: RowId, label: impl Into<String>) -> Self {
        Self {
            confirmable: true,
            ..Self::new(id, label)
        }
    }

    /// Build an offline placeholder row (`— offline`, dim, wrapper no-op).
    #[must_use]
    pub fn offline_placeholder(id: RowId, label: impl Into<String>) -> Self {
        Self {
            offline: true,
            ..Self::new(id, label)
        }
    }

    /// Build a row with a volume bar (`volume` = cube-root volume, `1.0` =
    /// `100%`).
    #[must_use]
    pub fn with_volume(id: RowId, label: impl Into<String>, volume: f32) -> Self {
        Self {
            volume: Some(volume),
            ..Self::new(id, label)
        }
    }

    /// Build a row with dropdown targets and the current target index.
    #[must_use]
    pub fn with_targets(
        id: RowId,
        label: impl Into<String>,
        targets: Vec<Target>,
        current: usize,
    ) -> Self {
        Self {
            targets,
            target_index: current,
            ..Self::new(id, label)
        }
    }

    /// Build a row with a plain text detail line (no `▼` icon).
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Build a row with middle line text (sublabel).
    #[must_use]
    pub fn with_sublabel(mut self, sublabel: impl Into<String>) -> Self {
        self.sublabel = Some(sublabel.into());
        self
    }

    /// Build a device-style row whose detail line is `▼ config`.
    #[must_use]
    pub fn with_config(id: RowId, label: impl Into<String>, config: impl Into<String>) -> Self {
        Self {
            config: Some(config.into()),
            ..Self::new(id, label)
        }
    }

    /// The row's current target, if it has any.
    #[must_use]
    pub fn current_target(&self) -> Option<&Target> {
        if self.targets.is_empty() {
            return None;
        }
        let index = self.target_index.min(self.targets.len() - 1);
        self.targets.get(index)
    }

    /// Borrow this row's sanitized texts, refreshing stale entries (OPT-3).
    ///
    /// Hold the guard while rendering the row: every label/meta/config/
    /// detail/sublabel borrow comes from it, so one guard covers a whole
    /// `draw_node` with no per-cell sanitize or measure.
    pub(crate) fn texts(&self) -> Ref<'_, RowTextCache> {
        {
            let mut cache = self.text_cache.borrow_mut();
            cache.label.refresh(Some(&self.label));
            cache.meta.refresh(self.meta.as_deref());
            cache.config.refresh(self.config.as_deref());
            cache.detail.refresh(self.detail.as_deref());
            cache.sublabel.refresh(self.sublabel.as_deref());
        }
        self.text_cache.borrow()
    }

    /// Mark the row as the default one (`◇` marker).
    #[must_use]
    pub fn default_marked(mut self) -> Self {
        self.is_default = true;
        self
    }

    /// Mark the row muted (`muted` in the volume label area).
    #[must_use]
    pub fn muted(mut self) -> Self {
        self.muted = true;
        self
    }

    /// Attach peak levels (`RowPeaks::Stereo`/`Mono`, linear amplitudes).
    #[must_use]
    pub fn with_peaks(mut self, peaks: RowPeaks) -> Self {
        self.peaks = Some(peaks);
        self
    }

    /// Attach the row's preview image path (see [`Row::preview_image`]).
    #[must_use]
    pub fn with_preview_image(mut self, path: impl Into<String>) -> Self {
        self.preview_image = Some(path.into());
        self
    }

    /// Hide targets from the header's right column so [`Row::meta`] is shown.
    #[must_use]
    pub fn hide_target_in_header(mut self) -> Self {
        self.hide_target_in_header = true;
        self
    }
}

/// An open target dropdown (upstream `ObjectList::dropdown_state`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DropdownState {
    /// Provider-row index the dropdown is open for.
    pub row: usize,
    /// Highlighted target index within that row's `targets`.
    pub selected: usize,
    /// First visible target index (upstream ratatui `ListState` offset),
    /// maintained by the renderer as the highlight moves.
    pub top: usize,
}

/// Independent per-tab interaction state.
///
/// Tab switching preserves (and restores) all of this; nothing here leaks
/// across tabs.
#[derive(Debug, Default)]
pub struct TabState {
    /// Focused row index within the tab's current filtered view.
    pub focus: usize,
    /// First visible row of the filtered view (scroll offset).
    pub scroll: usize,
    /// Current filter text for this tab.
    pub filter: String,
    /// Marked rows, as provider-row indices (stable across refilters).
    pub marked: BTreeSet<usize>,
    /// When the danger flow was armed (`keys::handle_key`), if armed.
    pub armed_at: Option<Instant>,
    /// `Delete` was pressed once; `Delete`/`Enter` deletes, `Esc` cancels.
    pub confirm_pending: bool,
    /// Open target dropdown, if any (upstream `dropdown_state`).
    pub dropdown: Option<DropdownState>,
    /// Memoized fuzzy filter results to avoid O(M*N) recomputes on every frame.
    pub cached_hits: std::cell::RefCell<FilterCache>,
}

/// Composite cache key for fuzzy filter memoization.
///
/// Cache-invalidation rule (OPT-2, frozen): the key is `(mode, filter,
/// rows_ptr, rows_len, first_id)`. A `Vec` reallocation changes the pointer
/// and therefore misses even when the content is identical — this is
/// deliberate. In-place row mutation that keeps pointer/len/first-id must
/// call [`TabState::invalidate_filter_cache`]; never silently broaden the
/// key to content hashing.
pub type FilterCacheKey = (filter::FilterMode, String, usize, usize, Option<RowId>);
/// Memoized filter cache payload: `(cache_key, shared matching indices)`.
///
/// Hits share one `Arc<[usize]>` instead of cloning the index `Vec` on every
/// cache hit and on every store (OPT-2). `Arc` (not `Rc`): tabs cross
/// threads via `spawn_blocking`, so the cache must stay `Send`.
pub type FilterCache = Option<(FilterCacheKey, Arc<[usize]>)>;

impl TabState {
    pub fn invalidate_filter_cache(&self) {
        *self.cached_hits.borrow_mut() = None;
    }
    /// Whether the danger flow is currently armed.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed_at.is_some()
    }

    pub fn arm(&mut self, now: Instant) {
        self.armed_at = Some(now);
    }

    /// Disarm the danger flow; returns whether it was armed.
    pub fn disarm(&mut self) -> bool {
        self.armed_at.take().is_some()
    }
}

/// A named tab (one provider view) holding its rows plus state.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Tab {
    /// Tab title shown in the tab bar.
    pub name: String,
    /// Rows owned by this tab; filtering happens in-process.
    pub rows: Vec<Row>,
    /// Launcher-like lists set this to hide tab bar / meta / gauge.
    pub bare_rows: bool,
    /// Whether the tab shows a filter line and accepts type-to-filter.
    /// Fixed-choice menus (`power`) opt out: five rows need no search.
    pub filterable: bool,
    /// Whether matches on this tab learn usage ranking.
    ///
    /// Independent of [`Tab::filterable`]: searchable lists with ephemeral
    /// rows (pids, MACs, scan snapshots) keep the search box but opt out
    /// here, while fixed menus opt out of both. Defaults to `true`.
    pub learnable: bool,
    /// Whether the `Delete` key arms the delete-confirm flow on this tab.
    /// Launch/power rows are never deletable (`false`); `clip` opts in.
    pub deletable: bool,
    /// Independent interaction state (preserved across tab switches).
    pub state: TabState,
    /// Memoized sanitize+measure of [`Tab::name`] (OPT-3).
    name_cache: RefCell<CachedText>,
}

impl Tab {
    /// Borrow the sanitized tab name, refreshing the memo first (OPT-3).
    ///
    /// The tab bar holds these guards while rendering, so tab titles are
    /// sanitized at most once per content change instead of per frame.
    pub(crate) fn name_texts(&self) -> Ref<'_, CachedText> {
        {
            let mut cache = self.name_cache.borrow_mut();
            cache.refresh(Some(&self.name));
        }
        self.name_cache.borrow()
    }
}

impl Tab {
    #[must_use]
    pub fn empty(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rows: Vec::new(),
            bare_rows: true,
            filterable: true,
            learnable: true,
            deletable: false,
            state: TabState::default(),
            name_cache: RefCell::default(),
        }
    }

    #[must_use]
    pub fn with_rows(name: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            name: name.into(),
            rows,
            bare_rows: true,
            filterable: true,
            learnable: true,
            deletable: false,
            state: TabState::default(),
            name_cache: RefCell::default(),
        }
    }
}

/// Input mode: filter editing vs pure navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Keystrokes edit the filter (default).
    #[default]
    Normal,
    /// Keystrokes navigate (`j`/`k`, etc.); runes are ignored.
    Navigate,
}

/// Gauge/toggle widget state (e.g. volume in `center`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gauge {
    /// Widget label (`"Volume"`, …).
    pub label: String,
    /// Fill level 0–100.
    pub value: u8,
    /// Mute toggle (`[●]` unmuted / `[○]` muted).
    pub muted: bool,
    /// Online fill vs dim `— offline` (Q7).
    pub online: bool,
}

impl Gauge {
    /// Build an online unmuted gauge, clamping `value` to 0–100.
    #[must_use]
    pub fn new(label: impl Into<String>, value: u8) -> Self {
        Self {
            label: label.into(),
            value: value.min(100),
            muted: false,
            online: true,
        }
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
    }
}

/// What the binary reports to its wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A row was chosen: `(provider, action_id, label)`.
    Chosen {
        /// Provider name (`power`, `launch`, …).
        provider: String,
        /// Opaque action id (`RowId` hex).
        action_id: String,
        /// Human label (escaped on the `ACTION:` line).
        label: String,
    },
    /// User cancelled (`Esc`, `q` in NORMAL + empty filter).
    Cancelled,
    /// A deletable row was confirmed for deletion: `(provider, action_id, label)`.
    ///
    /// Reported on the `ACTION:DELETE` line; the wrapper performs the removal.
    Delete {
        /// Provider name (`clip`, …).
        provider: String,
        /// Opaque action id (`RowId` hex).
        action_id: String,
        /// Human label (escaped on the `ACTION:DELETE` line).
        label: String,
    },
    /// A pin was toggled on a deletable row: `(provider, action_id, label)`.
    ///
    /// Reported on the `ACTION:TOGGLE` line; the wrapper flips the pin.
    Toggle {
        /// Provider name (`clip`, …).
        provider: String,
        /// Opaque action id (`RowId` hex).
        action_id: String,
        /// Human label (escaped on the `ACTION:TOGGLE` line).
        label: String,
    },
    /// A dropdown target was chosen: `(provider, row, target, title)`.
    ///
    /// Reported on the `ACTION:TARGET` line; the wrapper applies the routing
    /// change.
    Target {
        /// Provider name (`center`, …).
        provider: String,
        /// Opaque action id of the row the dropdown belongs to.
        row: String,
        /// Opaque id of the chosen target.
        target: String,
        /// Human title of the chosen target (escaped on the line).
        title: String,
    },
    /// Quit the menu with an exit code (no output).
    ///
    /// `130` is the user-cancelled code (`Esc`, `q` in NORMAL + empty
    /// filter, `Ctrl-c`); other codes are runtime quits.
    Quit {
        /// Process exit code to quit with.
        code: i32,
    },
}

/// Runtime application state (tabs + mode + help overlay).
///
/// Selection, scroll, filter, marks, and arming live per tab
/// ([`TabState`]); this struct only holds what is shared.
#[derive(Debug, Default)]
pub struct App {
    /// All tabs; the active tab is `tabs[active]`.
    pub tabs: Vec<Tab>,
    /// Index into `tabs` of the active tab.
    pub active: usize,
    /// Input mode.
    pub mode: Mode,
    /// Whether the help overlay is open.
    pub help_open: bool,
    /// First visible help line (upstream `help_position`); the help overlay
    /// scrolls with the movement keys while open.
    pub help_scroll: usize,
    /// Ranking engine for the filtered view (`Spec` default; `Legacy`
    /// preserves provider order via `--filter-mode=legacy`).
    pub filter_mode: filter::FilterMode,
    /// Whether the zoxide-style frecency overlay may reorder matches
    /// (opt-out via `--no-frecency` / `FLEX_NO_FRECENCY`). Defaults to
    /// `true` in [`App::with_tabs`]; the derived `Default` is `false`,
    /// so menus must go through `with_tabs` (or set this explicitly).
    /// This is the global switch only: [`App::visible_rows`] additionally
    /// requires the active tab to be filterable and learnable, so fixed
    /// menus (`power`, `shot`, `profile`, `notify`, net's speedtest tab)
    /// and ephemeral lists (`net`, `wifi`, `bt`, `center`) never rerank.
    pub use_frecency: bool,
    /// Learned usage table (row-id string → rank + timestamp), loaded once
    /// per invocation by the caller. Empty (or disabled) ranks exactly like
    /// [`filter::rank_all`]. Immutable for the life of the menu, so the
    /// [`FilterCacheKey`] needs no usage component (see `visible_rows`).
    pub usage: filter::UsageTable,
}

impl App {
    /// Create an app with no tabs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an app with tabs, activating the first one.
    #[must_use]
    pub fn with_tabs(tabs: Vec<Tab>) -> Self {
        Self {
            tabs,
            active: 0,
            mode: Mode::default(),
            help_open: false,
            help_scroll: 0,
            filter_mode: filter::FilterMode::default(),
            use_frecency: true,
            usage: filter::UsageTable::new(),
        }
    }

    /// Borrow the active tab, if any.
    #[must_use]
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// Mutably borrow the active tab, if any.
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// Borrow the active tab's state, if any.
    #[must_use]
    pub fn active_state(&self) -> Option<&TabState> {
        self.active_tab().map(|tab| &tab.state)
    }

    /// Switch tabs, preserving all per-tab state (focus/scroll/filter/
    /// marked/armed). Out-of-range indices are ignored; the newly shown
    /// focus is clamped into its filtered view.
    pub fn switch_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        self.clamp_focus();
    }

    /// The active tab's open dropdown, if any.
    #[must_use]
    pub fn dropdown(&self) -> Option<&DropdownState> {
        self.active_state()
            .and_then(|state| state.dropdown.as_ref())
    }

    /// Mutable access to the active tab's open dropdown.
    pub fn dropdown_mut(&mut self) -> Option<&mut DropdownState> {
        self.active_tab_mut()?.state.dropdown.as_mut()
    }

    /// Open the focused row's dropdown (upstream `Action::ActivateDropdown`).
    ///
    /// No-op when the view is empty or the focused row has no
    /// [`Row::targets`]. The highlight starts on the row's current target,
    /// like upstream's `node_targets` position. Returns whether a dropdown
    /// is now open.
    pub fn open_dropdown(&mut self) -> bool {
        let Some(row_index) = self.focused_original_index() else {
            return false;
        };
        let Some(row) = self.active_tab().and_then(|tab| tab.rows.get(row_index)) else {
            return false;
        };
        if row.targets.is_empty() {
            return false;
        }
        let selected = row.target_index.min(row.targets.len() - 1);
        if let Some(tab) = self.active_tab_mut() {
            tab.state.dropdown = Some(DropdownState {
                row: row_index,
                selected,
                top: 0,
            });
        }
        true
    }

    /// Close the active tab's dropdown; returns whether one was open.
    pub fn close_dropdown(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        tab.state.dropdown.take().is_some()
    }

    /// Move the open dropdown's highlight by `delta`, clamped to the list.
    ///
    /// Returns whether a dropdown is open (the highlight may be unchanged at
    /// either end).
    pub fn move_dropdown(&mut self, delta: isize) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        let Some(dropdown) = tab.state.dropdown.as_mut() else {
            return false;
        };
        let Some(row) = tab.rows.get(dropdown.row) else {
            return false;
        };
        let len = row.targets.len();
        if len > 0 {
            #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
            let next = (dropdown.selected as isize + delta).clamp(0, len as isize - 1);
            dropdown.selected = next.cast_unsigned();
        }
        true
    }

    /// The highlighted target of the open dropdown, plus its index.
    #[must_use]
    pub fn highlighted_target(&self) -> Option<(usize, &Target)> {
        let dropdown = self.dropdown()?;
        let row = self.active_tab()?.rows.get(dropdown.row)?;
        if row.targets.is_empty() {
            return None;
        }
        let index = dropdown.selected.min(row.targets.len() - 1);
        Some((index, &row.targets[index]))
    }

    /// Commit the highlighted target on the row the dropdown is open for and
    /// close the dropdown; returns the chosen target.
    ///
    /// The row's [`Row::target_index`] is updated so the header shows the new
    /// target immediately (upstream re-reads it from `PipeWire` state instead).
    pub fn commit_dropdown(&mut self) -> Option<(RowId, RowId, String)> {
        let dropdown = self.dropdown()?.clone();
        let tab = self.active_tab_mut()?;
        let row = tab.rows.get_mut(dropdown.row)?;
        if row.targets.is_empty() {
            return None;
        }
        let index = dropdown.selected.min(row.targets.len() - 1);
        row.target_index = index;
        let target = &row.targets[index];
        let chosen = (row.id.clone(), target.id.clone(), target.title.clone());
        tab.state.dropdown = None;
        Some(chosen)
    }

    /// Cycle to the next (`delta = +1`) or previous tab, wrapping.
    ///
    /// Tab/row counts are far below `isize::MAX`, so the index casts below
    /// cannot wrap or lose sign in practice.
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    pub fn cycle_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let count = self.tabs.len() as isize;
        let next = (self.active as isize + delta).rem_euclid(count);
        self.switch_tab(next as usize);
    }

    /// Ranked provider-row indices for the active tab's filter.
    ///
    /// Empty filter preserves provider order; otherwise score-descending
    /// with original-index tie-break (see [`filter::rank_all`]). In
    /// [`filter::FilterMode::Legacy`] every hit scores equally, so the
    /// tie-break keeps provider order (no score reordering).
    ///
    /// Frecency ([`filter::rank_all_scored`]) applies to `Spec` only, and
    /// only when [`App::use_frecency`] is set, the active tab is
    /// filterable and learnable, and [`App::usage`] is non-empty — every
    /// other combination ranks exactly like [`filter::rank_all`]. Fixed
    /// tabs keep provider order and ignore learned entries; so do
    /// searchable tabs with ephemeral rows (`net`, `wifi`, `bt`). The table is
    /// immutable for the life of the menu (recording happens after the
    /// event loop exits), so the [`FilterCacheKey`] needs no usage
    /// component: a cached view stays valid and in-process clock drift is
    /// ignored (sessions last seconds; boosts only move at hour/day/week
    /// boundaries).
    ///
    /// Cache (OPT-2): hits return a shared `Arc<[usize]>` — no index `Vec`
    /// is cloned on hit or on store. The probe compares against the stored
    /// key with borrows only (`filter` as `&str`, first id by reference),
    /// so hits allocate nothing; only a miss clones the filter `String`
    /// and the first `RowId` for the stored key. See [`FilterCacheKey`] for
    /// the frozen invalidation rule.
    #[must_use]
    pub fn visible_rows(&self) -> Arc<[usize]> {
        let Some(tab) = self.active_tab() else {
            return Arc::from(Vec::<usize>::new());
        };
        let filter = &tab.state.filter;
        let mode = self.filter_mode;
        let rows_ptr = tab.rows.as_ptr() as usize;
        let rows_len = tab.rows.len();
        {
            let cache = tab.state.cached_hits.borrow();
            if let Some((old_key, old_hits)) = cache.as_ref() {
                let hit = old_key.0 == mode
                    && old_key.1.as_str() == filter.as_str()
                    && old_key.2 == rows_ptr
                    && old_key.3 == rows_len
                    && old_key.4.as_ref() == tab.rows.first().map(|row| &row.id);
                if hit {
                    return Arc::clone(old_hits);
                }
            }
        }
        let hits = match mode {
            filter::FilterMode::Spec => {
                if self.use_frecency && tab.filterable && tab.learnable && !self.usage.is_empty() {
                    filter::rank_all_scored(filter, &tab.rows, &self.usage, usage_now())
                } else {
                    filter::rank_all(filter, &tab.rows)
                }
            }
            filter::FilterMode::Legacy => filter::rank_all_legacy(filter, &tab.rows),
        };
        let result: Arc<[usize]> = hits
            .into_iter()
            .map(|hit| hit.index)
            .collect::<Vec<usize>>()
            .into();
        let key = (
            mode,
            filter.clone(),
            rows_ptr,
            rows_len,
            tab.rows.first().map(|row| row.id.clone()),
        );
        *tab.state.cached_hits.borrow_mut() = Some((key, Arc::clone(&result)));
        result
    }

    /// Number of rows in the active filtered view.
    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.visible_rows().len()
    }

    /// Provider-row index under focus, if the view is non-empty.
    #[must_use]
    pub fn focused_original_index(&self) -> Option<usize> {
        let visible = self.visible_rows();
        if visible.is_empty() {
            return None;
        }
        let focus = self
            .active_state()
            .map_or(0, |s| s.focus.min(visible.len() - 1));
        visible.get(focus).copied()
    }

    /// Row under focus, if any.
    #[must_use]
    pub fn focused_row(&self) -> Option<&Row> {
        let index = self.focused_original_index()?;
        self.active_tab()?.rows.get(index)
    }

    /// Clamp the active focus into its filtered view.
    pub fn clamp_focus(&mut self) {
        let len = self.visible_len();
        self.clamp_focus_with(len);
    }

    /// Clamp the active focus into a view of `len` rows.
    ///
    /// Render-path variant (OPT-2): the frame already computed the visible
    /// view once, so callers pass `visible.len()` instead of recomputing it.
    pub fn clamp_focus_with(&mut self, len: usize) {
        if let Some(state) = self.active_tab_mut().map(|tab| &mut tab.state) {
            state.focus = if len == 0 {
                0
            } else {
                state.focus.min(len - 1)
            };
        }
    }

    /// Move focus by `delta` rows, wrapping around the filtered view.
    ///
    /// Counts are far below `isize::MAX` and `rem_euclid` is non-negative,
    /// so the index casts below cannot wrap or lose sign in practice.
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    pub fn move_focus(&mut self, delta: isize) {
        let len = self.visible_len();
        if len == 0 {
            if let Some(state) = self.active_tab_mut().map(|tab| &mut tab.state) {
                state.focus = 0;
            }
            return;
        }
        if let Some(state) = self.active_tab_mut().map(|tab| &mut tab.state) {
            let next = (state.focus as isize + delta).rem_euclid(len as isize);
            state.focus = next as usize;
        }
    }

    /// Move focus by `delta` rows, clamping at the ends (page keys).
    ///
    /// Counts are far below `isize::MAX` and the result is clamped to
    /// `[0, len - 1]`, so the casts below cannot wrap or lose sign.
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    pub fn move_focus_clamped(&mut self, delta: isize) {
        let len = self.visible_len();
        if let Some(state) = self.active_tab_mut().map(|tab| &mut tab.state) {
            if len == 0 {
                state.focus = 0;
                return;
            }
            let next = state.focus as isize + delta;
            state.focus = next.clamp(0, len as isize - 1) as usize;
        }
    }

    /// Scroll the active tab so focus is visible.
    /// `entries_visible` is the number of entries that fit in the viewport.
    pub fn ensure_visible(&mut self, entries_visible: usize) {
        let len = self.visible_len();
        self.ensure_visible_with(entries_visible, len);
    }

    /// Scroll the active tab so focus is visible, given a precomputed view.
    ///
    /// Render-path variant (OPT-2): the frame already computed the visible
    /// view once, so callers pass `visible.len()` instead of recomputing it.
    /// `visible_len` is the length of that same view.
    pub fn ensure_visible_with(&mut self, entries_visible: usize, visible_len: usize) {
        self.clamp_focus_with(visible_len);
        if let Some(state) = self.active_tab_mut().map(|tab| &mut tab.state) {
            if entries_visible == 0 {
                state.scroll = 0;
                return;
            }
            if state.focus < state.scroll {
                state.scroll = state.focus;
            } else if state.focus >= state.scroll + entries_visible {
                state.scroll = state.focus + 1 - entries_visible;
            }
        }
    }

    /// Whether the active tab's danger flow is armed.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.active_state().is_some_and(TabState::is_armed)
    }

    /// Disarm the active tab; returns whether it was armed.
    pub fn disarm(&mut self) -> bool {
        self.active_tab_mut().is_some_and(|tab| tab.state.disarm())
    }

    /// Periodic tick (1 s gauge cadence, Q7): expire a stale arm
    /// ([`keys::ARM_EXPIRE`]).
    ///
    /// Never touches focus/scroll/filter/marks — ticks must not steal focus.
    /// Returns whether an arm expired on this tick.
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        let expired = tab.state.armed_at.is_some_and(|since| {
            now.checked_duration_since(since)
                .is_some_and(|age| age >= keys::ARM_EXPIRE)
        });
        if expired {
            tab.state.disarm();
            true
        } else {
            false
        }
    }
}

/// Wall-clock epoch seconds for frecency scoring (`visible_rows`).
///
/// Unreachable-in-practice fallback is `0` (pre-epoch clock): every learned
/// entry then scores at the hourly boost uniformly, so relative order among
/// used rows is preserved and unrecorded rows still sort last.
fn usage_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |age| age.as_secs())
}

/// Per-tick refresh hook, installed by the caller with [`Menu::on_tick`].
///
/// The engine cannot know what a given screen needs refreshed — a volume
/// gauge, a background Wi-Fi scan — so the caller supplies a function that
/// does. It runs after every [`App::tick`], must leave filter, focus and
/// scroll state intact, and defaults to doing nothing.
pub type TickHook = fn(&mut Menu);

/// A configured menu session: provider + [`App`] + widgets.
#[derive(Debug)]
pub struct Menu {
    /// Provider name (`power`, `launch`, …) for the `ACTION:` line.
    pub provider: String,
    /// Tabs + shared UI state.
    pub app: App,
    /// Gauge widget, when the provider shows one (`center`).
    pub gauge: Option<Gauge>,
    /// Offline mode dims labels and renders `— offline` (Q7).
    pub offline: bool,
    /// Glyph set (upstream `char_set`; CLI `--char-set`).
    pub char_set: CharSet,
    /// Style tokens (upstream `theme`; CLI `--theme`).
    pub theme: Theme,
    /// Peak-meter mode (upstream `peaks`; CLI `--peaks`).
    pub peaks: Peaks,
    /// Volume slider ceiling in percent (upstream `max_volume_percent`,
    /// default 150). The volume bar fills `volume / (max/100)` of its width.
    pub max_volume_percent: f32,
    /// Reserve the image preview pane on the right of the list
    /// ([`preview::pane`]). Set by providers whose rows carry
    /// [`Row::preview_image`] and only when the terminal can draw one.
    pub preview: bool,
    /// Asynchronous wakeup handle to awaken the event loop instantly when
    /// background tasks complete.
    pub notify: std::sync::Arc<tokio::sync::Notify>,
    /// Per-tick refresh hook; private so it can only be set through
    /// [`Menu::on_tick`].
    on_tick: Option<TickHook>,
}

/// Upstream `max_volume_percent` default (`config.rs:261-263`).
pub const DEFAULT_MAX_VOLUME_PERCENT: f32 = 150.0;

impl Default for Menu {
    fn default() -> Self {
        Self {
            provider: String::new(),
            app: App::default(),
            gauge: None,
            offline: false,
            char_set: CharSet::default(),
            theme: Theme::default(),
            peaks: Peaks::default(),
            max_volume_percent: DEFAULT_MAX_VOLUME_PERCENT,
            preview: false,
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            on_tick: None,
        }
    }
}

impl Menu {
    #[must_use]
    pub fn new(provider: impl Into<String>, tabs: Vec<Tab>) -> Self {
        Self {
            provider: provider.into(),
            app: App::with_tabs(tabs),
            ..Self::default()
        }
    }

    /// Install the per-tick refresh hook (builder style).
    ///
    /// See [`TickHook`]; `flex-rice` passes a dispatcher that refreshes the
    /// `center` gauges and picks up a finished `wifi` scan.
    #[must_use]
    pub fn on_tick(mut self, hook: TickHook) -> Self {
        self.on_tick = Some(hook);
        self
    }

    #[must_use]
    pub fn notifier(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.notify.clone()
    }

    /// Awaken the interactive event loop to redraw/tick immediately.
    pub fn notify(&self) {
        self.notify.notify_one();
    }

    /// Periodic tick; delegates to [`App::tick`] (never steals focus), then
    /// runs the caller's [`TickHook`] if one was installed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let expired = self.app.tick(now);
        if let Some(hook) = self.on_tick {
            hook(self);
        }
        expired
    }
}

/// Deterministic synthetic clipboard corpus (M1 perf spike seed).
///
/// `count` rows of `word word #i`-style labels from an LCG seeded by `seed`
/// (std-only, no RNG dep). Shared by `benches/rerank.rs` and
/// `tests/clip_perf.rs` so both measure the same fixture shape.
///
/// # Panics
///
/// Never panics: word-table indices are always reduced modulo its length.
#[must_use]
pub fn synthetic_clip_corpus(count: usize, seed: u64) -> Vec<Row> {
    const WORDS: &[&str] = &[
        "firefox",
        "terminal",
        "clipboard",
        "screenshot",
        "volume",
        "network",
        "bluetooth",
        "calendar",
        "password",
        "config",
        "deploy",
        "branch",
        "merge",
        "review",
        "issue",
        "window",
        "theme",
        "icon",
        "launcher",
        "session",
    ];
    let mut state = seed | 1;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as usize
    };
    (0..count)
        .map(|i| {
            let first = WORDS[next() % WORDS.len()];
            let second = WORDS[next() % WORDS.len()];
            let label = format!("{first} {second} #{i}");
            Row::new(RowId::new(content_hash_hex(&label)), label)
        })
        .collect()
}
