# Changelog

All notable changes to `flex` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- `flex-clip add`: bound the `wl-paste` paste wait to 5 s. A wedged
  Wayland selection blocked `wl-paste` forever and the unbounded wait wedged
  `add` under `wl-paste --watch` for 14 h, silently dropping every later
  copy. A stuck paste now fails loudly (`flex: error: clip: wl-paste timed
  out …`) and the next selection change retries; stdout drains on a side
  thread so healthy pastes larger than the 64 KiB pipe buffer still read.

### Changed
- `flex-net` Bandwidth and `flex-bt` Devices now render compact 1-line rows
  like `flex-wifi`: wire-share and battery render as `NN% [████░░░░]` text
  gauges in `meta` instead of `Row::volume` bars. Compact rows are the
  documented default for all providers going forward (tall detail rows
  opt-in only, e.g. `notify` message bodies).

## [1.0.1] — Runtime performance pass

Behavior-preserving optimizations across the filter, render, event-loop,
and provider polling paths. No ranking, rendering, or wire-protocol
changes; all existing goldens and gates hold.

### Changed
- Filter (`flex-core`): single-pass needle folding, single-char query fast
  path, `Vec::with_capacity` on hit paths, and frecency pre-resolve so the
  sort comparator no longer hits the usage `HashMap` per comparison.
- Render (`flex-core`): tab bar reuses a thread-local width buffer and
  re-borrows titles per tab (no per-frame `Vec<Ref>`/`Vec<u16>`), ASCII
  fast path in `put_str` skipping the `unicode-width` lookup, one-shot
  reserve for volume bars, single-pass dropdown width scan.
- Event loop (`flex-core`): preview path borrows instead of cloning per
  frame; reused `interval` timer instead of a fresh `sleep` future per
  iteration.
- `/proc` polling (`flex-rice` proc/net): borrowed pid-name parsing,
  single-`rfind` stat parse, early-break status scan, moved (not cloned)
  service keys, single-pass Waybar tooltip escape.
- Spawn hot paths (`flex-rice` bt/wifi): allocation-free
  case-insensitive sink matching (one fold per device), memoized wifi
  signal bars (101-entry cache).
- Startup scans (`flex-rice`): single-provider usage load (`load_one`,
  no whole-store build + clone), allocation-free usage TSV split,
  byte-scan `.desktop` field-code stripping.

Per-provider-binary migration: the providers now execute **in-process**
in Rust, and the shell wrappers are retired. The `ACTION:` wire protocol is no
longer consumed by anything — each binary selects a row and runs its effect
directly.

### Added
- `flex-shot` recording options: the four recording rows collapse to Area /
  Full Recording, and picking one opens a drill-in submenu (Audio on/muted,
  quality Light/Balanced/High mapping to 2M/5M/10M bitrates, framerate
  30/60) over `FLEX_REC_AUDIO`/`FLEX_REC_QUALITY`/`FLEX_REC_FPS` env
  defaults (audio on, balanced, 30 fps — today's `*-rec-audio` command
  shape). The confirm row carries the live settings, so the defaults path
  is Enter-Enter. Retired `*-rec-audio` ids survive as hidden aliases
  (audio forced on); default recordings keep the legacy `RECORDING_START`
  argv, so custom helpers still parse.
- Notification Center copies images: any drawer row referencing an image
  file (body-text paths and `image-path` hint thumbnails, feed/threads/
  history) offers `Copy Image`, which puts the file bytes on the clipboard
  with their MIME type (`wl-copy --type <mime>`, magic-byte sniff for
  PNG/JPEG/GIF/WebP/BMP plus extension fallback incl. SVG). The paste
  survives the drawer exiting (`wl-copy` serves forked from memory); a file
  lost since listing time reports `Image not found` instead of failing
  silent. Screenshots now copy as typed `image/png` and their notification
  carries an `image-path` hint (preview + Copy Image target).
- `flex-shot` waits for the popup to disappear (bounded 1 s, plus a 500 ms
  settle for the compositor close animation) before capturing: the worker
  ran concurrently with the popup close, so a fullscreen shot photographed
  the flex TUI itself, its fade, or the blank-restored window (and a window
  shot could measure the popup as the active window). The wait is gated on
  the inherited `POPUP_KITTY=1` marker, so it always applies in real flows
  even when the first probe already finds the popup gone.
- Clip retention: `flex-clip add` keeps the history within 200 entries
  (newest win) and drops unpinned lines older than 7 days, enforced on every
  append (the `watch` daemon inherits it). Pinned entries are exempt. Ages
  live in a `~/.cache/cliphist.ts` timestamp sidecar (one epoch per history
  line); a missing sidecar counts as fresh so legacy history survives the
  first run. Overrides: `CLIPHIST_TS`, `CLIPHIST_MAX_ENTRIES`,
  `CLIPHIST_MAX_AGE_SECS`.
- `flex-notify`: Native right-side Notification Center drawer (`-m` / `flex notify`),
  Waybar JSON polling module (`--status`), CLI operations (`send`, `clear-all`, `toggle-dnd`),
  and background D-Bus listener daemon (`daemon`).
  - Implements `org.freedesktop.Notifications` (`/org/freedesktop/Notifications`)
    for direct ingestion of `notify-send` and desktop app alerts.
  - Live MPRIS synchronization with playback controls and twitch/youtube scrubber.
  - Regex-free actionable entity extractors (OTP 2FA codes, URLs, hex color codes).
  - Subtle notification audio cues for incoming normal and critical alerts with
    automatic DND suppression and app-level muting.
- `flex-mixer`: Interactive PipeWire/WirePlumber audio and microphone mixer popup.
- `flex-net`: Live network interface and bandwidth telemetry monitor.
- `flex-bt`: Bluetooth device manager with pairing and battery status probes.
- `flex-clip add|pin|unpin|current`, `flex-theme list|current|activate|delete`
  and `flex-wallpaper set <path>`: the retired `cliphist.sh` /
  `theme-switcher.sh` / `set-wallpaper.sh` entry points as non-interactive
  verbs (`flex <provider> <verb>` forwards through the dispatcher).
- `flex-proc`: a native process manager (filter `/proc`; Enter = SIGTERM,
  Delete = SIGKILL, `m` = stop/continue) replacing the htop-based
  `kill-menu.sh`. `FLEX_PROC_KTHREADS` includes kernel threads.
- `flex-record`: the `recording-start.sh`/`recording-status.sh`/
  `recording-stop.sh` helpers as one binary (the drop-in `[-a] [-g GEOM]
  FILE` start shape plus `status`/`stop`); `FLEX_RECORD_INFO` overrides the
  registry path.
- `flex popup <variant> <cmd…>`: the dispatcher's popup toggle/spawn helper
  (used by dotfiles `kill-menu.sh`), keyed on the variant class.
- `flex-rice/src/exec/*.rs`: one executor module per provider, porting the
  matching wrapper's side effects into the binary. The binaries stay thin
  (`runner::popup_guard` → `runner::build_menu` → `flex_core::run::run_capture`
  → `exec::<provider>::execute`).
- `flex-rice/tests/entrypoints.rs`: the `--help`/`--version` contract for the
  binaries (replacing `tests/wrappers.rs`).
- Zoxide-style usage ranking (frecency): choices are recorded per provider
  (`$FLEX_USAGE_FILE`, else `$HOME/.cache/flex/usage.tsv`) and reorder
  matches within their fuzzy tier (tiers still outrank use; empty-filter
  views surface most-used first). On by default; `--no-frecency` or
  `$FLEX_NO_FRECENCY` disables ranking and recording. Fixed menus (`power`,
  `shot`, `profile`, `notify`) and ephemeral lists (`net`, `wifi`, `bt`,
  `center`) opt out via `Tab::learnable = false`; `--print-action` never
  records.

### Changed
- The shell call path is fully retired: `cliphist.sh`, `kill-menu.sh`,
  `theme-switcher.sh`, `set-wallpaper.sh` and `recording-*.sh` are gone, and
  `flex-shot`'s recording helper defaults to the `flex-record` binary.
- `flex-proc` joins the wide popup variant (`flex-menu-wide`).
- `setup.sh` links the eleven release binaries into `~/.local/bin`, and
  `setup.sh --check` is the gate that all eleven resolve to executables.
- The providers are `flex-<provider>` binaries over the shared `runner`,
  alongside the `flex` compat dispatcher and the `flex-record` helper —
  eleven entry points in all.
- Popups are terminal-agnostic: the host terminal comes from `$TERMINAL`
  (kitty template; unknown/empty warns once and falls back to kitty, never
  exits `1`).
- Popup window classes are `flex-menu` / `flex-menu-wide` (the old
  `kitty-menu*` names are gone); toggle stays keyed on the variant.
- `--print-action` is the only remaining producer of an `ACTION:` line: an
  end-to-end row→action probe with no execution.
- `Terminal=true` desktop entries are hosted by `$TERMINAL` (the
  `<terminal> -e <cmd…>` form — no popup class, font override or
  `POPUP_KITTY` marker) instead of a hardcoded `kitty -e`; the variable is
  read only for those entries, so a plain GUI launch never consults it.
- Theme activation (`flex-theme`, `flex-center`) and wallpaper setting
  (`flex-wallpaper`) are now in-process: the activator copies the theme into
  `current/`, relinks the per-app config targets and reloads Waybar/Hyprland/
  Kitty, and the setter talks to the hyprpaper socket and rewrites
  `hyprpaper.conf` + the ml4w cache. `THEME_SWITCHER`/`SET_WALLPAPER` are now
  override-only seams (unset runs the native path); the standalone
  `theme-switcher.sh`/`set-wallpaper.sh` scripts are no longer on the call
  path.

### Fixed
- Providers no longer abort when a diagnostic write fails. `eprintln!`/
  `println!` panic on a write error — a hung-up popup pty gives `EIO`, a
  closed pipe `EPIPE` — and the release profile is `panic = "abort"`, so a
  provider whose stderr was a deleted `/dev/pts/N` died with SIGABRT before
  it could spawn its popup (live: a Waybar started before
  `hl.env("TERMINAL", …)` kept a deleted pts on fd 2, and every
  `flex-wifi`/`flex-power` on-click aborted). Runtime prints now go through
  `flex_core::diag`, which discards the error; locked by
  `flex-rice/tests/diagnostics.rs` (`/dev/full` on stderr must exit 1, not
  die by signal).

### Removed
- `flex-center` (control center menu + binary): volume/brightness gauges,
  network/bluetooth/power/theme aggregation tabs, `Command::Center`,
  `Provider::Center`, and the `SUPER+X` bind (now unbound).
  `WifiNet`/nmcli parsing, `wifi_meta_body`, `bar`, and the `snapshot`
  seam moved into `providers::wifi` / `providers.rs`; `NotifyWhen`
  moved to `exec`; wifi/bt menus are behavior-identical.
- `flex-rice/wrappers/` (all eight `flex-*.sh` scripts) and
  `flex-rice/tests/wrappers.rs`; the executor tests are the surviving
  coverage.
- The `ACTION:` wire protocol from the call path — nothing parses stdout.
- The hidden `flex <provider> --resolve` CLI lookups; the executors resolve
  row ids in-process via the library resolver functions
  (`clip::resolve`, `wallpaper::resolve`, `launch::resolve_id`,
  `theme_::resolve_name`).
- The shellcheck CI step (no `.sh` remains in this repo).

Monorepo consolidation: `flex-core` and `flex-rice` live in this repo as
workspace members with a path dependency — no git tag to bump, no `[patch]`
override, one `cargo test` (345 passed, 1 ignored), one `Cargo.lock`, one
`CHANGELOG.md`. `unicode-width` is hoisted to `[workspace.dependencies]` and
the release profile lives at the workspace root only (verified: the release
binary is byte-identical in size to the pre-move build, `stripped`). The
`gom3az/flex-core` repository is archived with a pointer here.

### Fixed
- B-027: engine-raised errors no longer double the prefix
  (`flex: error: flex: cannot open /dev/tty …`). The eight `anyhow` contexts
  in `flex-core/src/backend.rs` and `flex-core/src/run.rs` dropped their own
  `flex:` prefix, so `flex-rice/src/main.rs` owns the single `flex: error:`.
  Locked by `engine_errors_are_reported_with_a_single_prefix` (setsid-detached
  real binary; stable-prefix + single-count assertion, since the trailing
  `(os error N)` text is environment-dependent). The direct `flex: preview: …`
  diagnostic is intentionally unchanged — it never passes through `main`.

Review pass over the findings in `Docs/Bug_tracking.md`. `flex launch` and
`flex center` no longer print a false `skipping malformed entry` line for
every `.desktop` file that is skipped *by design* — 202 of them on the
reference host, on every open; the parser now returns a three-state result
(`Entry`/`Hidden`/`Malformed`) and only genuinely broken files are reported
(B-020). Launcher and theme row ids became **space-free content hashes** with
hidden `flex launch --resolve` / `flex theme --resolve` lookups, so an app or
theme named `My App`/`My Theme` no longer produces an `ACTION:` line whose id
token the wrappers mis-split (B-021; `flex-launch.sh`, `flex-theme.sh` and
`flex-center.sh` resolve before acting). The `--resolve` unknown-id message
carries `flex: error:` exactly once (B-022), and the `.desktop` sort now uses
`sort_by_cached_key`, cutting the allocating key function from 1054 calls to
84 on the reference tree (B-023). The clipboard picker no longer holds the
history twice — dedup works on the content hash and the tab previews each
line as it drops it — which took `flex clip`'s peak RSS from ~17.6 MB to
~11.1 MB on the reference 3.6 MB / 3227-line store, with the live bash parity
probe unchanged (B-024). An empty provider is no longer a blank screen:
`flex launch` and `flex theme` show the same `noop` placeholder row the
control center already used (`(No applications found)` / `(No themes
found)`), and the `flex-launch`/`flex-theme` wrappers treat `noop` as a
no-op like every other wrapper (B-026).

Network-dialog cutover: Waybar's network click now opens `flex wifi` instead of
a script that could only print "nmtui is not installed". The picker keeps the
rofi dialog's affordances (radio on/off, disconnect, signal/security per
network) as flex rows, and `nmcli` still owns every side effect.
Wallpaper cutover: the last fzf surface (`SUPER+W`) is now `flex wallpaper`.
The picker lists the same images the bash script did and shows the focused one
in a kitty-graphics preview pane, so previews survive the move off
`kitty +kitten icat`.

Wiremix compliance pass: flex now renders the upstream design system at
commit `cbdc90f` instead of an approximation of it. `Docs/design_system.md` is
rewritten with a `file:line` citation per rule, and `tests/compliance.rs` pins
one assertion per citation.

Workspace split: `flex/` is now a cargo workspace, cut along the line that
matters for reuse. **`flex-core`** is the engine — rendering, filtering, keys,
the design system, kitty-graphics previews — with no machine-specific path, so
`cargo package -p flex-core` produces a publishable crate. **`flex-rice`** is
this rice's glue: the eight providers, the `flex` binary and the wrappers,
marked `publish = false`. Dependencies run one way, and the engine's last reach
into providers is gone: `Menu::tick`'s hardcoded `center`/`wifi` dispatch is
now the `Menu::on_tick` seam taking a `TickHook`, with `flex-rice::menu()`
installing the dispatcher (build menus with it, or those two providers stop
refreshing). Every test kept its subject — 331 pass, with the same per-suite
counts as before — the wrappers stay covered by Rust tests in
`flex-rice/tests`, and the binary still lands at `target/release/flex`, so the
`~/.local/bin/flex` symlink and all 13 wrapper references are untouched.

Engine extraction: `flex-core` now lives in its own repository,
[gom3az/flex-core], and is consumed here as a git dependency pinned by tag
(`flex-core = { git = …, tag = "v1.0.0" }` in `[workspace.dependencies]`;
`Cargo.lock` pins the exact revision, `777d6fc`). The extracted crate is
unchanged apart from a standalone manifest and its `repository` URL, builds and
passes its 109 tests on its own, and packages cleanly. What stays here is the
rice half: `flex-rice` (providers, `flex` binary, wrappers) with its 222 tests.
The workspace root is kept with a single member **on purpose** — it pins
`target/` at `flex/target/`, which `~/.local/bin/flex` resolves through, and
keeps `flex-rice/wrappers/` where the 13 config references and
`tests/wrappers.rs` expect it. Flattening `flex-rice/` into `flex/` would move
the wrappers and break every keybind, which `04e2599` demonstrated.

`flex-core` has no CI yet: pushing `.github/workflows/` needs a GitHub token
with the `workflow` scope, which this checkout's token lacks. The workflow
(fmt → clippy `-D warnings` → test) is written and ready to add.

[gom3az/flex-core]: https://github.com/gom3az/flex (retired; the engine now lives in this repo at `flex-core/`)

### Added
- `flex wifi` provider (`src/providers/wifi.rs`): the network dialog. Rows are
  `Turn Wi-Fi Off`/`Turn Wi-Fi On` (radio state read from `nmcli radio wifi`,
  command as meta), `Disconnect from {ssid}` when a scan reports `IN-USE` `*`,
  then one row per scanned network (id `wifi`, label = SSID, meta = the
  column layout below). Standard rows (the meta column is the point),
  filterable, never deletable. Degradation matches the `center` `Networks`
  tab: failed `nmcli` or no Wi-Fi device → one dim `— offline` row, empty scan
  → the bash `(No Wi-Fi networks)` parenthetical, radio down → only
  `Turn Wi-Fi On`. `$WIFI_RADIO_FILE`/`$WIFI_NMCLI_DEVICES_FILE`/
  `$WIFI_NMCLI_WIFI_FILE` are the fixture seams (shared `center::snapshot`
  contract).
- `wrappers/flex-wifi.sh`: popup wrapper for `flex wifi` (compact `menu`
  variant). Owns every `nmcli` call: radio on/off, `device disconnect`,
  open-network connect, and the `/dev/tty` password prompt for secured
  networks (`FLEX_WIFI_PASSWORD` seam); selecting the network you are already
  on disconnects it, like the rofi picker. `NMCLI`/`NOTIFY_SEND` override the
  commands for tests.
- `tests/wifi.rs` (39 tests): fixture row-set parity, offline/empty/radio-off
  degradation, the live `$WIFI_*` seam path, instant-open/placeholder/scan-swap
  behaviour, saved-profile prompting (profile reuse, ethernet namesakes,
  rejected credentials) + state markers, key-seq replays
  (filter/navigate/Esc/delete-dead), the 80x24 standard-mode golden, and
  stubbed-pipeline wrapper dispatch (open/secure/saved/backslash-label/
  connected-toggle/noop/cancel/malformed/unknown). `tests/wrappers.rs` covers
  the new wrapper.
- Instant `flex wifi` opening (`wifi::menu`): the picker is built from the
  cached scan (`nmcli … --rescan no`: measured 10 ms vs ~3 s for a triggered
  scan), so the first frame lands in ~30 ms instead of after a blocking scan.
  A background thread runs the real scan and `wifi::refresh_scan` — the `wifi`
  branch of `Menu::tick` — swaps its rows in; focus follows the SSID (matched
  by id + label, read through the filter) and lands on the first network when
  the placeholder is replaced. On a cold cache the list shows a dim
  `Scanning…` row instead of claiming `(No Wi-Fi networks)`; the wrapper's
  connect probe now also reads the cache (`--rescan no`), so Enter no longer
  waits for a second scan.
- `flex-wifi.sh` post-TUI feedback: the password prompt is printed explicitly
  (`Password for <ssid>: ` on stderr, which *is* the popup's terminal) instead
  of relying on `read -p`, whose prompt goes to stderr and was being sent to
  `/dev/null` — the popup used to sit blank and look hung while it waited. An
  empty answer now says `No password entered — not connecting to <ssid>.`
  rather than exiting silently, and `Connecting to <ssid>…` covers the
  multi-second `nmcli` connect.
- `flex wifi` row metas use single-cell Nerd Font lock icons
  (`wifi::security_text`, `nf-fa-lock`/`nf-fa-unlock` — the glyphs the deleted
  rofi picker used) instead of the `🔒`/`🔓` emoji, which no part of the UI font
  covers: those resolve to a colour-emoji fallback font, so they land at emoji
  metrics inside Noto Sans Nerd Font rows and break the meta column.
  `center::wifi_meta_body` keeps the emoji for bash parity (its metas are not
  rendered — the `Networks` tab is bare-rows).
- Saved networks no longer ask for a password. `flex-wifi.sh` reconnected from
  the stored profile whenever `nmcli … connection show` lists an
  `802-11-wireless` profile for that SSID (`is_saved`, name split against the
  last colon so `My\:Net` survives), and only prompts when there is no profile
  or when the stored credentials are rejected — stating that fallback
  (`Saved credentials for <ssid> were rejected — enter the password.`) instead
  of failing silently. The picker shows the state in the meta's own column:
  `Connected`, `Saved`, or nothing (`wifi::state_marker`,
  `wifi::parse_saved_profiles`, `$WIFI_NMCLI_PROFILES_FILE` seam).
- `wifi::net_meta` lays the network metas out in **fixed sub-columns**, because
  the renderer right-aligns one string per row: the signal is padded to three
  digits, the bar is a fixed width, the security class is padded to the widest
  in the list, and the state column (`Connected`/`Saved`) occupies reserved
  cells, so every field lines up down the list instead of each row shifting by
  its own suffix:

  ```text
  100% [████████]   WPA1 WPA2  Connected
   79% [██████░░]   WPA2       Saved
   42% [███░░░░░]   Open
  ```
- `wifi::Snapshot` bundles the four `nmcli` reads the picker is built from
  (radio, devices, scan, saved profiles) into one value, so the row builders
  take a snapshot instead of positional `Option<&str>` arguments.
- `flex wallpaper` provider (`src/providers/wallpaper.rs`) with
  `wallpaper-picker.sh` row-set parity: the two bash roots
  (`~/Pictures/Wallpapers`, `~/Pictures/Screenshots`, `WALLPAPER_DIRS`
  override), `find -maxdepth 2 -type f -iname` semantics (four suffixes,
  symlinks skipped like `find` without `-L`), `sort -u` order, basename
  labels, source-directory meta plus `  Active` on the wallpaper in use
  (`set-wallpaper.sh`'s cache file, `hyprpaper.conf` fallback). Ids are
  FNV-1a hex of the absolute path, like `clip`'s content hashes, so the
  space-delimited `ACTION:` line stays parseable; the hidden
  `flex wallpaper --resolve <id>` lookup turns one back into a path.
- `src/preview.rs`: kitty-graphics preview pane. Pure geometry
  (`preview::pane`, 45 % of the frame, right-aligned, one gutter column,
  dropped on frames under 40 columns), protocol escapes
  (`a=T,f=100,t=f,…,C=1,q=2`, single fixed image id, `<path>` as the base64
  payload), and a derived-PNG cache for sources the protocol cannot carry.
  `Row::preview_image` + `Menu::preview` gate it; `render::preview_area` and
  `render::cursor_position` hand the geometry to the event loop, which paints
  after the frame flush and re-parks the caret. `FLEX_PREVIEW=0|1`,
  `FLEX_PREVIEW_CACHE`, `FLEX_PREVIEW_CONVERT` are the environmental seams;
  a pane that cannot be drawn stays blank and logs one stderr line.
- `wrappers/flex-wallpaper.sh`: resolves the id (rejecting anything that is
  not a 16-char hex hash), checks the file, then hands the path to
  `set-wallpaper.sh` (`SET_WALLPAPER` override) after the TUI exits.
  `tests/wallpaper.rs` (21 tests: row/scan/resolve parity, pane geometry and
  goldens, key-seq replays, determinism, stubbed wrapper dispatch) + 20 unit
  tests in `preview.rs`/`wallpaper.rs`; `tests/wrappers.rs` covers the new
  wrapper's exec bit and popup re-exec.

### Fixed
- Preview images keep their aspect ratio. kitty stretches its payload to fill
  the `c`×`r` box exactly — it does not letterbox — and terminal cells are not
  square, so the pane-shaped box squashed landscape wallpapers into a tall
  smear (a 4:1 test image measured 568×1096 px, aspect 0.52). The pane now
  derives an aspect-correct, area-preferred cell box from the transmitted
  PNG's `IHDR` size and the terminal's cell size, and centres it with
  whole-cell shifts plus sub-cell `X`/`Y` offsets. Re-measured live: 4:1 →
  4.06, 1:4 → 0.25, 16:9 → 1.76, 3:2 JPEG through the converter → 1.53.

### Changed
- `render` reserves the preview pane on the right of the list area only —
  rows, filter line, hints, gauge and tab bar keep their upstream geometry, so
  without `Menu::preview` (every other provider) frames are byte-identical.
- `SUPER+W` now runs `flex-wallpaper.sh` instead of `wallpaper-picker.sh`.
- Picking an image no longer regenerates the theme. `set-wallpaper.sh` dropped
  its `generate-theme.sh` call, so `flex wallpaper` swaps the hyprpaper image
  and leaves the current colors alone; themes change only through `flex theme`
  or an explicit `generate-theme.sh` run. `generate-theme.sh` also lost its
  default `auto` name — an unnamed run refreshes `current/` in place and only
  `--save-as <name>` writes to `available/`.
- Waybar's `network` module `on-click` now runs
  `$HOME/dotfiles/flex/wrappers/flex-wifi.sh`; the 5 s `exec` speed probe on
  the same module is untouched. `rofi/.config/rofi/scripts/wifi.sh` became a
  delegating stub to the wrapper (same pattern as `wallpaper-picker.sh`), so
  binds and scripts that call the old path kept working; `wifi.rasi` and
  `wifi-prompt.rasi` became unused rofi assets.

  *Superseded 2026-09-15:* the whole `rofi` stow package was removed
  (`stow -D rofi`), including that stub and both unused `.rasi` assets — the
  stub had no remaining callers once Waybar was repointed. The RASI pipeline
  followed: `generate_rasi()` (and `generate-static-theme.sh`'s heredoc) are
  gone, `colors.rasi` is out of the saved-theme file list and out of
  `theme-switcher.sh`'s theme-validity check, and the `colors.rasi`/
  `theme.rasi` assets were deleted from every saved theme.

### Removed
- `scripts/.config/scripts/patch-ml4w-wallpaper.sh` (it appended a
  `generate-theme.sh` hook to `~/.config/ml4w/scripts/ml4w-wallpaper`, so
  ML4W-driven wallpaper changes regenerated the theme too) and the 16
  `themes/.config/themes/available/auto-*` themes left behind by the old
  picker flow. `available/` now holds only the three static themes.
- `scripts/.config/scripts/picker-chrome.sh` (fzf chrome; its only sourcer was
  the wallpaper picker). `wallpaper-picker.sh` stays as a delegating stub to
  `flex-wallpaper.sh`, like `cliphist.sh pick()` and `theme-switcher.sh
  pick()`.

- `src/meter.rs`: port of upstream `meter.rs` (-60..+6 dB normalisation,
  inactive/active/overload segments, stereo left/live/right and mono layouts).
  `Row::peaks` feeds it; `--peaks off|mono|auto` matches upstream.
- `src/charset.rs`: the three built-in glyph sets (`default`, `compat`,
  `extracompat`) with upstream's exact values, shipped as `const` tables and
  selected by `--char-set`.
- Target dropdowns: `Target`, `Row::targets`/`target_index`, per-tab
  `DropdownState`, upstream geometry (width = longest target + 4, height =
  min(5, n) + 2, right-aligned one row above the selected entry), `Clear` +
  Rounded border, `> ` highlight symbol, `REVERSED` highlight, `•••` scroll
  markers, and `ACTION:TARGET <provider> <row> <target> <title>` reporting.
  Rows with targets open the list on `Enter`/`c`; `Esc` cancels.
- `•••` list scroll indicators in the reserved header/footer lines, including
  upstream's suppression rule for a partially rendered last entry.
- Device-style rows: `Row::config` renders `▼ profile` on the detail line
  (`DeviceWidget` layout).
- `Theme::get`/`CharSet::get` with `ThemeName`/`CharSetName`, plus `--theme
  default|nocolor|plain` and `--char-set`, matching upstream's flags.
- `tests/compliance.rs` (upstream-pinned constants/geometry) and
  `tests/dropdown.rs` (dropdown replays).

### Changed
- List density: node geometry follows the data via `NodeMetrics`. A tab whose
  rows carry no volume, enabled peaks or config line renders compact nodes
  (1 line + 1 blank row, pitch 2, selector `░`), so the launcher and friends
  show 9 entries at 80x24 instead of 3; any row with detail data restores
  upstream's full 3-line `░▒░` node. Node internals are now written line by
  line rather than through a `Layout` split, which a solver reordered when the
  node area was shorter than the selector.
- **Node row contents now match upstream**: header on node line 1, an empty
  line 2 carrying `▒`, and the volume/meter or config line on line 3. The
  `◇` default marker occupies column 2 and titles start at column 4; the
  right column is the row's current target (or `meta`), right-aligned in the
  terminal default style rather than dimmed.
- The volume line shows the 5-cell right-aligned `NN%` (or `muted`) with the
  bar after it, sized by `max_volume_percent` (default 150 %) — the previous
  hard-coded decorative 75 % bar is gone, so rows without volume data leave
  the detail line empty.
- Oversized titles now get upstream's 3-cell `...` area instead of a single
  `…`, and oversized metadata is clipped by the layout instead of being
  dropped to `…` with a stderr note.
- Tab bar: `title + 2` widths, `[active]` / ` inactive `, inactive tabs keep
  the terminal default foreground, no extra separator, no `BOLD`.
- Help overlay: no title on the border, default-coloured border, `rows + 2`
  capped at 90 % of the list area, border `•••` indicators, scrollable while
  open (`Enter` closes).
- The list reserves a line above and below for `•••`, so the first entry sits
  on row 1 and one row fewer entry fits per screen.
- The gauge's muted state now shows `muted` instead of an unchanging `▮`
  glyph.
- Crate is now `rustfmt`-clean (`cargo fmt --check` reports zero diffs).

### Fixed
- Selected-row selector no longer repeats down the row gap: the two spacing
  lines that separate entries are now drawn blank, so the selection block is
  exactly `░▒░` on the node's three lines (header/empty/detail) instead of
  `░▒░░░` (`draw_node_row` sent `line_in_row` 2, 3 and 4 to the
  selector-drawing spacing row).
- `place_cursor` no longer underflows on degenerate frames (1x1), which
  panicked instead of rendering an empty frame.
- `MUTE_ON`/`MUTE_OFF` were both `▮`, so the gauge's mute indicator never
  changed.

## [1.0.0] - 2026-09-15

M6 power cutover + v1 hardening: all six providers cut over, 184 tests
green in debug AND release, hygiene gates clean.

### Added
- M6 power cutover: `src/providers/power.rs` (static 5-row System/Power
  table with `power-menu.sh` row-set parity — bash-exact order/labels/
  metas/ids, danger on Reboot/Power Off via the shared double-Enter flow)
  + `wrappers/flex-power.sh` (bash-verbatim power commands post-TUI with
  a `DRY_RUN=1` blast-radius gate: echoes `would run: …` instead of
  executing); `SUPER+M` + waybar `custom/power` repointed, `power-menu.sh`
  deleted, then `flex-tui.sh` deleted (zero functional sourcers remain).
  `tests/power.rs` (18 tests: row parity, danger safety replays incl. the
  single-`Enter`-never-confirms release gate, goldens, dry-run + dispatch)
  + 2 provider unit tests.
- M6 hardening: `tests/keys.rs` seed table 12 → 29 cases (`TODO(M6)`
  closed) + empty-app/empty-tab targeted tests; render-path audit (no
  subprocess/`tput`/`stty` in the render path); release suite 184/184;
  `target/release/flex` 984K (`strip`/`lto`/`abort` effective).

### Changed
- Version `1.0.0`. `README.md` cutover table all ✅. Frozen decisions
  recorded (`Docs/Implementation.md`): Q3 50 ms + 5 s danger timing,
  Q8 no serde.

### Removed
- `waybar/power-menu.sh` (superseded by `flex-power.sh`).
- `scripts/flex-tui.sh` (bash engine; last consumer cut over).
- Kept intentionally (out of scope): `popup.sh` (kill-menu /
  wallpaper-picker / wifi still exec it), `picker-chrome.sh`
  (wallpaper-picker fzf flow sources it), `app-cache.sh` (row-set
  reference), `kill-menu.sh` + `wallpaper-picker.sh` (non-flex surfaces).

## [Unreleased]

### Added
- M0 scaffold: crate layout, `ACTION:` protocol contract, `TestBackend` golden harness.
- M2 launch cutover: `.desktop` provider (`src/providers/launch.rs`) with
  `app-cache.sh` row-set parity, `Terminal` meta, `%X`-preserving `Exec`;
  event loop (`src/run.rs`: 1 s poll, `handle_key` dispatch, per-frame
   render, `tick` expiry, `ACTION:`/`ACTION:DELETE`/quit exits, `FLEX_TEST`
   seeded step); hidden `--filter-mode=spec|legacy` escape hatch; `Tab::deletable`
   gate (launch rows non-deletable); `wrappers/flex-launch.sh` wrapper with
   `launch_app_row` launch semantics; `tests/launch.rs` (fixtures, key-seq
   replays, 80x24 golden, seed determinism, live-cache parity probe).
- M3 shot cutover: `src/providers/shot.rs` (7-row `Capture` table, exact bash
  labels/metas, `case`-arm ids) + `wrappers/flex-shot.sh` (capture pipeline
  verbatim, post-TUI execution); `SUPER+s` repointed, `screenshot.sh` deleted;
  `tests/shot.rs` (row parity, golden, replays, stubbed-pipeline wrapper tests).
- M3 theme cutover: `src/providers/theme_.rs` (sorted `available/` scan,
  wallpaper-basename meta + `Active` mark, std-only JSON) +
  `wrappers/flex-theme.sh` (`exec theme-switcher.sh activate`); `SUPER+T`
  repointed, `pick()` reduced to a delegating stub (`list/current/activate/
  delete/rofi` intact); `tests/theme.rs` (fixtures incl. live-shaped scan,
  golden, replays, dispatch wrapper tests).
- M4 clip cutover: `src/providers/clip.rs` (byte-safe `cliphist` ingest —
  `NUL`/`0x1F`/`ESC` dropped, `\t`→space, lossy UTF-8, pins-first then
  `tac` history, 120-char preview, content-hash ids, `📌 Pinned` meta,
  standard deletable rows, `CLIPHIST_FILE`/`CLIPHIST_PINS` overrides,
  hidden `flex clip --resolve` lookup) + `KeyOutcome::Toggle` /
  `ACTION:TOGGLE` (NAVIGATE `m` on deletable tabs; `backend::emit_toggle`,
  `run` handling) + `wrappers/flex-clip.sh` (post-TUI `wl-copy`/delete/
  pin-unpin with `cliphist.sh` semantics); `SUPER+SHIFT+V` repointed,
  `cliphist.sh pick()` a delegating stub (`add`/`pin`/`unpin` intact);
  `tests/clip.rs` (22 tests incl. live-store parity probe) + enforced
  `tests/clip_perf.rs` cold-ingest gate (6.4 ms vs 0.5 s budget).
- M5 center cutover: `src/providers/center.rs` (5-tab `Menu` builder with
  `control-center.sh` row-set parity — Launchers via the shared
  `.desktop` scan, `nmcli`/`bluetoothctl` snapshot parses, static power
  rows with shared danger-arm/confirm, volume/brightness gauge rows +
  theme rows; `CENTER_*` file seams; 1 s in-place gauge tick, R7) +
  `Row::offline` per-row dim flag (`lib.rs`/`render.rs`) +
  `wrappers/flex-center.sh` (launch/wifi-connect+password/bt-toggle/
  power-ops/mute/theme-activate with `control-center.sh` semantics);
  `SUPER+X` repointed, `control-center.sh` deleted (`app-cache.sh` kept
  as row-set reference, `flex-tui.sh`/`popup.sh` kept for power/kill/
  wallpaper flows); `tests/center.rs` (27 tests: per-tab fixtures,
  125x30 golden + gauge states, key-seq replays incl. 60-tick soak,
  stubbed-pipeline wrapper tests) + `tests/fixtures/center/` snapshots.
