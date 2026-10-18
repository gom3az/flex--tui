# flex — UI/UX Spec

Read before implementing any UI/UX element. All row geometry, tokens, states,
and key handling below are normative for `flex-core/src/render.rs`, `flex-core/src/keys.rs`,
`flex-core/src/theme.rs`.

## Viewports

- Design viewports: **640×420** (popup) and **1000×600** (center grid), terminal
  `font_size 10`.
- `TestBackend` golden reference grid: **80×24** (`flex-rice/tests/golden.rs`).
- Layout must remain legible at both viewports; truncation absorbs shrinkage —
  never horizontal scroll, never wrap rows.

## Theme tokens (hardcoded, `flex-core/src/theme.rs`)

```rust
bg:          #1e1e2e  // base surface
fg:          #cdd6f4  // primary text
dim:         #6c7086  // secondary / meta / offline
accent:      #89b4fa  // cursor bar + active tab underline
selected_bg: #313244  // selected-row inversion fill
danger:      #f38ba8  // danger rows, triple-coded (below)
gauge_fill:  #a6e3a1  // gauge/volume fill
```

No external theme files in v1 (no serde/toml). Tokens are `pub const`s on `Theme`.

## Row column math (standard mode)

Each list row of width `W` (cells):

```text
col 0:            `█` selection bar, accent (#89b4fa), rendered OUTSIDE the
                  selected-row inversion (stays accent even when row selected).
col 1:            1-cell left padding (bg).
col 2:            label start (fg; dim when offline/disabled).
label .. avail:   label truncated with `…` (unicode-width aware, `flex-core/src/width.rs`).
avail = W - 2 - 1 - M - 1
  where 2 = bar+pad prefix, 1 = gap before meta, M = meta display-width,
        1 = right pad.
right:            meta text, dim (#6c7086), right-aligned; 1-cell right pad.
selected row:     label+meta cells filled with selected_bg (#313244);
                  bar at col 0 remains accent.
```

Truncation: label is the only shrinking element; meta is never truncated
(meta strings are provider-capped, e.g. `87%`, `wlan0`). If `avail <= 0`,
label renders as empty (meta + bar still visible).

Sub-columns: because a row's meta is one right-aligned string, a provider
whose metas carry several fields must pad them itself to line the fields up
down the list — `wifi::net_meta` does this (signal `NNN%`, fixed-width bar,
security padded to the widest class in the list, reserved state column whose
values are `Connected` / `Saved` / blank, so a saved network visibly will not
ask for a password).
This wifi pattern is the default for every provider: quantitative state renders
as a `NN% [████░░░░]` text gauge in `meta` (via `wifi::bar`), never as
`Row::volume` (which forces 3-line nodes); rows with dropdown targets set
`hide_target_in_header` so the gauge stays visible in the header right column.
`net` (wire-share) and `bt` (battery) follow it; tall detail rows are reserved
for multi-line content such as `notify` message bodies.
Glyphs in a mono row must be single-cell: the UI font is a Nerd Font, so
private-use icons are safe, while colour emoji resolve to a fallback font at
emoji metrics and shift the row (`wifi::security_text`).

## Bare-rows vs standard mode

- **Standard mode** (power/center/clip/wifi): full chrome — tab bar, filter input,
  list with `█` bar + meta column, status/footer line, gauge where applicable.
- **Bare-rows mode** (launcher-like): list rows only — no tab bar, no meta
  column (`M = 0`), no gauge. Same selection bar + inversion. Used when the
  provider sets `bare_rows = true` (e.g. `launch`, and the `center` network/
  bluetooth tabs whose metas duplicate the label).
- Mode flag lives on `Tab`/`Provider`; `render.rs` branches on it. Golden tests
  cover both.

## Widget states

| Widget | States |
|---|---|
| Tab bar | active tab: fg + accent underline; inactive: dim; empty provider: bar still renders (M0 golden) |
| Filter input | NORMAL (keystrokes filter) vs NAVIGATE; placeholder dim when empty |
| List row | normal / selected (inversion) / danger (triple-coded) / disabled-offline (dim) |
| Pending row | `Scanning…` — dim, `noop` id, shown by `wifi` only while the first background scan is in flight (a cold cache must never read as `(No Wi-Fi networks)`); replaced in place when the scan lands |
| Danger row | color `danger` + `!!` prefix + plain-text label suffix (e.g. `!! Shutdown — confirm`); triple-coding = color AND glyph AND text, so color-blind + monochrome terminals still read danger |
| Gauge | online: `gauge_fill` bar + `%`/value meta; offline: dim text `— offline` (Q7), no fill; ticks every 1 s |
| Status/footer | hints (`↑↓ navigate · enter select · esc cancel`), dim; danger context shows confirm hint |

## Image preview pane (`wallpaper` only)

```text
+---------------------------+-----------------+
|  list area (rows)         |  image pane     |   list area height
|  ...                      |  (kitty         |   (rows + `•••` lines)
|  ...                      |   graphics)     |
+---------------------------+-----------------+
|  filter line (full width)                   |
|  hints line (full width)                    |
|  [Wallpapers]  tab bar (full width)         |
+---------------------------------------------+
```

- Reserved **only** when `Menu::preview` is set (the `wallpaper` provider, and
  only on a kitty-compatible terminal). Every other frame is unchanged, so no
  other provider pays for it.
- Pane width = 45 % of the frame, right-aligned, with the list keeping at
  least 24 columns and the pane at least 16; below 40 columns the pane is
  dropped and the list takes the whole width. One gutter column separates the
  two. The pane shares the list height; chrome below it stays full width.
- Overlays (help, dropdowns) stay inside the list column, so they never
  overlap the image; the pane is never covered by TUI chrome.
- The pane is a blank canvas in the ratatui buffer: the image is painted
  out-of-band by `flex-core/src/preview.rs` (kitty graphics protocol) after the frame is
  flushed. It is not part of the golden frame and never affects row geometry,
  filtering, focus or the `ACTION:` line.
- The image keeps its aspect ratio: `c`/`r` are **not** the pane box. kitty
  stretches whatever it is given to fill that box exactly (measured: a 4:1
  image in a 71×69-cell pane came out 568×1096 px), and cells are not square,
  so flex computes the largest whole-cell box whose pixel aspect matches the
  image (`preview::fitted_box`, 3 % tolerance, area-preferred) and centres it
  with whole-cell shifts plus sub-cell `X`/`Y` offsets. Measured after the
  fact: 4:1 → 4.06, 1:4 → 0.25, 16:9 → 1.76, 3:2 → 1.53.
- States: focused row has a previewable image → image scaled into that
  aspect-correct box; non-PNG source → converted once into the PNG cache;
  no kitty graphics / no converter / unwritable cache → pane stays background
  (with one stderr diagnostic, never a modal error); tab too narrow → pane
  absent, list full width.

## Key priority chain (`flex-core/src/keys.rs`)

Order of handling per keystroke (first match wins):

1. `Esc` → cancel (exit 130) — except during danger-confirm hold (requires explicit cancel step).
2. `Enter` → confirm/select (danger rows enter confirm-timing, see M1 spec lock).
3. Danger-confirm keys (timed window only).
4. `Alt-digit` → switch tab (always).
5. Bare `digit` → switch tab **iff filter empty** (Q1), else inserts into filter.
6. `q` → quit **only in NORMAL mode + empty filter** (Q6), else inserts `q`.
7. Navigation (`Up`/`Down`/`Ctrl-n`/`Ctrl-p`/`PgUp`/`PgDn`, vim `j`/`k` in NAVIGATE).
8. Filter editing (printable chars, `Backspace`, `Ctrl-u`, `Ctrl-w`).
9. `F1`/alt toggles (bare-rows preview, gauge mute) — provider-gated.

## Viewport budgets

- Frame budget: rerank + render < 16 ms on the 10k-row clipboard corpus
  (`flex-rice/tests/clip_perf.rs`, `flex-core/benches/rerank.rs`).
- Golden tolerance: pixel-identical `TestBackend` buffer asserts
  (`pretty_assertions` diffs); all goldens run at 80×24 plus viewport-logic
  unit tests for 640×420 / 1000×600 geometry (width math, not pixels).
- Gauge tick: exactly 1 s rerender when a gauge is visible (Q7); no tick-driven
  rerender otherwise (avoids idle CPU burn).
