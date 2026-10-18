# Flex Design System

> **Source of truth**: [wiremix](https://github.com/tsowell/wiremix) by tsowell,
> pinned at commit `cbdc90f` (branch `main`).
>
> Every rule below cites the upstream file and line it came from. Flex's UI must
> conform to this document; if a rule changes, the upstream citation changes with
> it. Elements that have **no** upstream counterpart are listed in §10 as flex
> extensions — everything else is a port and must match.
>
> **Last synced**: 2026-09-15 (`cbdc90f`). Compliance is enforced by
> `flex-rice/tests/golden.rs` (rendered detail) and `flex-core/tests/compliance.rs` (constants and
> geometry, one assertion per upstream citation).

---

## 1. Main frame

```
┌──────────────────────────────────────────────────────────┐
│                        •••                               │ ← 1 line, list_more
│  ░ ◇ Node title                        target/meta       │ ← node line 1 (header)
│  ▒                                                       │ ← node line 2 (empty)
│  ░   85% ━━━━━━━━━━━╌╌╌╌╌ ▮▮▮▮▮                          │ ← node line 3 (detail)
│                        •••                               │ ← 1 line, list_more
├──────────────────────────────────────────────────────────┤
│  [Playback] Recording Output Input Configuration         │ ← tab bar, 1 line
└──────────────────────────────────────────────────────────┘
```

| Rule | Upstream |
|---|---|
| List area on top, tab bar as the last line (`Min(0)`, `Length(1)`) | `app.rs:746-756` |
| List reserves one line above and one below for the scroll indicator | `object_list.rs:301-311` |
| Tab bar is **always at the bottom** — never the top | `app.rs:747-751` |
| Each tab occupies `title width + 2` cells: `[active]`, ` inactive ` | `app.rs:757-790` |
| No extra separator between tabs, no underline | `app.rs:763-790` |
| `•••` (`list_more`) centred in the reserved rows when rows are hidden | `object_list.rs:478-517` |

Flex adds chrome rows between the list and the tab bar (gauge, filter, hints) —
see §10. They are not part of the wiremix frame.

**Not ported**: mouse interaction. Upstream registers mouse areas for every row,
the scroll buttons, the dropdown and the help overlay (`node_widget.rs:98-128`,
`object_list.rs:459-476`); flex's event loop ignores mouse events
(`flex-core/src/run.rs`).

---

## 2. Node row

Node height is **3 lines**, spacing between nodes is **2 lines** — 5 visual rows
per entry (`node_widget.rs:53-60`).

**Flex extension — node metrics follow the data.** Upstream always has a detail
line to draw, because every PipeWire node carries volumes. A flex tab whose
rows carry no detail data (no volume, no enabled peaks, no config line) would
render one inked line per five rows, so it uses *compact* metrics instead:
**1 line + 1 blank row** (pitch 2), and the selector degenerates to its top
glyph `░` — the same thing upstream's `SelectorWidget` draws when its area is
one row tall. The moment any row in the tab has detail data, the tab renders
upstream's full 3-line node. Both modes run the same widget code; only the node
area height differs (`render::NodeMetrics`, §10).

**Provider default — compact wifi-style rows.** New and updated providers
render 1-line compact rows: quantitative info (signal, wire share, battery)
goes in the header `meta` as a text gauge (`NN% [████░░░░]`, via
`wifi::bar(val, max, 8)`), never in `Row::volume`/`config`/`detail`/`sublabel`.
Multi-field metas are padded to one column geometry so fields line up down the
list (`wifi::net_meta`). Rows that carry dropdown targets set
`hide_target_in_header`, so the gauge-bearing `meta` — not the current target
title — owns the header right column (the dropdown still lists every target).
Tall 3-line detail rows are opt-in only, for rows whose content genuinely needs
more than one line (e.g. the `notify` feed's message body).

```
col:   0    1    2     3    4 ...                         W-2  W-1
line 1  ░    ·    ◇     ·    Node title ................  target/meta
line 2  ▒    ·    ·     ·    (empty)
line 3  ░    ·    ·     ·     85%  ━━━━━━━━╌╌╌╌╌  ▮▮▮▮▮
line 4  ·    ·    ·     ·    (gap between entries — never carries a selector)
line 5  ·    ·    ·     ·    (gap between entries)
```

Compact metrics (no detail data anywhere in the tab):

```
line 1  ░    ·    ◇     ·    Node title ................  target/meta
line 2  ·    ·    ·     ·    (gap between entries)
```

| Rule | Upstream |
|---|---|
| 3 lines tall, 2 lines spacing | `node_widget.rs:53-60` |
| col 0 is a 1-cell selector column (`Length(1)`, rest `Min(0)`) | `node_widget.rs:132-143` |
| Header on node line 1, **empty** line 2, detail line 3 (`spacing(1)`, `Flex::Legacy`) | `node_widget.rs:147-157` |
| The gap lines between entries carry nothing | `Layout::spacing(spacing)` (`object_list.rs:519-527`) |
| Row background is untouched: selection is shown by the selector only | `node_widget.rs:213-236` |
| Header has a 1-cell horizontal margin on both sides | `node_widget.rs:309-318` |
| Marker column (col 2): `◇` (`default_device`) or a space | `node_widget.rs:281-295` |
| col 3 is a separator space; the title starts at col 4 | `node_widget.rs:290-295` |
| Right column: the row's current target, right-aligned, `◇ `-prefixed when it is the default target | `node_widget.rs:258-279`, `345-347` |
| Title too wide → a 3-cell `...` (three ASCII dots) is inserted before the right column | `node_widget.rs:323-342` |
| Row is selectable by mouse: left = select, right = set default, scroll = volume | `node_widget.rs:98-128` (not ported, §10) |

### Selection: `░▒░`

| Rule | Upstream |
|---|---|
| `selector_top` on node line 1, `selector_middle` on line 2, `selector_bottom` on line 3 | `node_widget.rs:217-234` |
| Only the selected node draws it; nothing is drawn otherwise | `node_widget.rs:215` |
| No colour inversion / no background change on the selected row | `node_widget.rs:213-236` |

Because the middle line is empty, the rendered block is `░` (title line), `▒`
(empty), `░` (volume/config line) — **exactly three rows**, never repeated down
the gap lines.

---

## 3. Detail line

Node line 3 holds exactly one of: the volume/meter widgets, the device config
line, or nothing.

### Volume + meters (nodes)

| Rule | Upstream |
|---|---|
| Padding `Length(2)`, then the volume area, then a trailing `Fill(1)` | `node_widget.rs:166-179` |
| With peaks: `[pad 2][Fill(4) volume][pad 1][Fill(4) meter][pad 1]` | `node_widget.rs:180-198` |
| Without peaks: `[pad 2][Fill(9) volume][pad 1]` | `node_widget.rs:167-179` |
| Volume area splits into a **5-cell** label + 1 space + the bar (`Min(0)`) | `node_widget.rs:381-390` |
| Label is the percentage, **right-aligned**: `  85%` | `node_widget.rs:396-403` |
| `muted` is drawn over the label area when muted | `node_widget.rs:421-423` |
| Fill count = `round(clamp(volume, 0, max) / max × bar_width)` with `max = max_volume_percent / 100` (default **150%**) | `node_widget.rs:379`, `405-414`, `config.rs:261-263` |
| Bar is `volume_filled.repeat(count)` + `volume_empty.repeat(bar - count)` | `node_widget.rs:409-418` |
| Volume percentage = `cbrt(mean(channel volumes)) × 100` | `node_widget.rs:393-396` |
| Rows without volume data draw no label and no bar | `node_widget.rs:393` |

Flex stores the already-cube-rooted value in `Row::volume` (`0.85` → `85%`), so
the renderer mirrors `node_widget.rs:405-414` without repeating the `cbrt`.

### Peak meters

| Rule | Upstream |
|---|---|
| dB conversion `20·log10(peak)`, clamped to **-60 .. +6 dB** | `meter.rs:12-21` |
| Normalised onto `10^(-60/60) .. 10^(6/60)` | `meter.rs:12-16` |
| Segments: inactive / active (below 0 dB) / overload (above 0 dB) | `meter.rs:23-38` |
| Stereo: left bar grows right-to-left, right bar left-to-right, `meter_center_*` live pair between them, layout `Fill(2) ‖ Length(2) ‖ Fill(2)` with spacing 1 | `meter.rs:41-121` |
| Mono: `Length(1)` live indicator + `Fill(2)` bar | `meter.rs:123-172` |
| Live indicator is styled active whenever levels are known | `meter.rs:104-121` |
| `peaks = off` renders no meter; `peaks = mono` averages stereo rows | `config.rs:97-102`, `node_widget.rs:482` |

### Device config line (device rows)

| Rule | Upstream |
|---|---|
| `    ▼ profile` — 4 spaces, `dropdown_icon`, a space, then the profile text | `device_widget.rs:141-153` |
| Device rows are also 3 lines + 2 spacing with the same `░▒░` selector | `device_widget.rs:37-45`, `97-121` |
| Title line starts with 3 spaces then the title | `device_widget.rs:135-139` |

---

## 4. Colour tokens

All colours are ANSI terminal colours (`ratatui::style::Color`); no hex RGB.
Backgrounds are never painted — the terminal default shows through
(`theme.rs:110-155`).

### `default` theme (`theme.rs:110-155`)

| Token | Value | Usage |
|---|---|---|
| `default_device` / `default_stream` | default | `◇` markers |
| `selector` | `LightCyan` | `░▒░` block |
| `tab` | default | inactive tab text |
| `tab_selected` | `LightCyan` | active tab title |
| `tab_marker` | `LightCyan` | `[` `]` around the active tab |
| `list_more` / `dropdown_more` / `help_more` | `DarkGray` | `•••` indicators |
| `node_title` / `node_target` / `volume` | default | titles, right column, `NN%` |
| `volume_empty` | `DarkGray` | unfilled bar |
| `volume_filled` | `LightBlue` | filled bar |
| `meter_inactive` / `meter_center_inactive` | `DarkGray` | meters below zero |
| `meter_active` / `meter_center_active` | `LightGreen` | meters up to 0 dB |
| `meter_overload` | `Red` | meters above 0 dB |
| `config_device` / `config_profile` / `dropdown_icon` | default | device config line |
| `dropdown_border` / `dropdown_item` / `help_border` / `help_item` | default | widget chrome and text |
| `dropdown_selected` | `LightCyan` + `REVERSED` | highlighted dropdown item |

### `nocolor` (`theme.rs:167-198`) and `plain` (`theme.rs:202-230`)

`nocolor` uses **modifiers only**: `BOLD` for selector/tab-selected/tab-marker/
gauge-fill, `DIM` for unfilled bars and inactive meters, `REVERSED | BOLD` for
the dropdown highlight. `plain` sets every token to the terminal default.

**Text modifiers**: `BOLD` and `REVERSED` only where listed above. `BOLD` is
*not* used for the active tab (upstream's default theme never bolds it),
`UNDERLINED` is never used, and `DIM` is not used for the right-hand column.

---

## 5. Character sets

Glyphs are single-cell width. Three built-in sets mirror upstream
(`char_set.rs:130-235`), selected with `--char-set`.

| Element | `default` | `compat` | `extracompat` |
|---|---|---|---|
| `default_device` / `default_stream` | `◇` | `◊` | `*` |
| `selector_top` / `selector_middle` / `selector_bottom` | `░` `▒` `░` | `░` `▒` `░` | `-` `=` `-` |
| `tab_marker_left` / `tab_marker_right` | `[` `]` | `[` `]` | `[` `]` |
| `list_more` | `•••` | `•••` | `~~~` |
| `volume_empty` / `volume_filled` | `╌` `━` | `─` `━` | `-` `=` |
| `meter_left/right_inactive` | `▮` | `┃` | `=` |
| `meter_left/right_active` | `▮` | `┃` | `#` |
| `meter_left/right_overload` | `▮` | `┃` | `!` |
| `meter_center_left/right_inactive` | `▮` | `█` | `[` `]` |
| `meter_center_left/right_active` | `▮` | `█` | `[` `]` |
| `dropdown_icon` / `dropdown_selector` | `▼` `>` | `▼` `>` | `\` `>` |
| `dropdown_more` / `help_more` | `•••` | `•••` | `~~~` |
| `dropdown_border` / `help_border` | Rounded | Plain | Plain |

`╌` is U+254C BOX DRAWINGS LIGHT DOUBLE DASH HORIZONTAL; `━` is U+2501 BOX
DRAWINGS HEAVY HORIZONTAL. The `extracompat` set is ASCII-only.

---

## 6. Tab bar

| Rule | Upstream |
|---|---|
| One line, bottom of the frame | `app.rs:747-751` |
| Width per tab = title width + 2 | `app.rs:757-761` |
| Active tab: `tab_marker` + title (`tab_selected`) + marker | `app.rs:765-778` |
| Inactive tab: ` title ` in the `tab` style (default foreground) | `app.rs:781-787` |

Flex measures the title's **display width** where upstream uses its byte
length; identical for ASCII titles and correct for the non-ASCII ones flex
allows.

---

## 7. Help overlay

| Rule | Upstream |
|---|---|
| `Clear` the area first — no transparency | `app.rs:838`, `help.rs:66`-ish |
| Width: sum of the help column widths, centred horizontally in the list area | `app.rs:817-827` |
| Height: `rows + 2`, capped at **90 %** of the available height, centred vertically | `app.rs:826-836` |
| Border: `help_border` type and style, **no title** | `help.rs:46-53` |
| Scrolls with the movement keys while open; `Enter` closes it | `app.rs:548-560` |
| `•••` (`help_more`) centred on the top/bottom border when text is hidden | `help.rs:79-110` |
| Opening the overlay resets its scroll position to 0 | `app.rs:641` |

---

## 8. Target dropdown

| Rule | Upstream |
|---|---|
| Opened by `Enter`/`c` on a row that has targets; `Esc` cancels | `app.rs:601-603`, README keybindings |
| Width = longest target width + 4 (2 borders + 2-cell highlight symbol) | `node_widget.rs:71-79` |
| Height = `min(5, targets) + 2` | `node_widget.rs:69-81` |
| Right-aligned to the list area, one row **above** the selected object | `node_widget.rs:83-88` |
| `Clear` + `Rounded` block, `dropdown_border` style | `dropdown_widget.rs:66-76` |
| Items use `dropdown_item`; the highlighted item uses `dropdown_selected` | `dropdown_widget.rs:77-79` |
| Highlight symbol `> ` (`dropdown_selector` + a space) | `dropdown_widget.rs:69` |
| `•••` (`dropdown_more`) centred on the borders when the list scrolls | `dropdown_widget.rs:88-135` |
| The highlight starts on the row's current target | `view.rs:904-915` |

Choosing a target reports `ACTION:TARGET <provider> <row-id> <target-id> <title>`
and updates the row's header column. Flex excludes the mouse path
(`dropdown_widget.rs:56-64`, §10).

---

## 9. Behaviour

### Viewport and scrolling

| Rule | Upstream |
|---|---|
| Visible entries = `list_height / (node height + spacing)` — **entry units**, never visual rows, with the pitch taken from the active tab's `NodeMetrics` (§2) | `object_list.rs:246-256` |
| Focus below the viewport → `scroll = focus - (visible - 1)` (selection lands on the bottom row) | `object_list.rs:281-288` |
| Focus above the viewport → `scroll = focus` (selection lands on the top row) | `object_list.rs:289-293` |
| A partially visible last entry is still rendered | `object_list.rs:392-395`, `519-527` |
| The bottom `•••` is suppressed when the last entry is only partially rendered but shows all its important lines | `object_list.rs:497-511` |
| Removing rows clamps `scroll` back into range | `object_list.rs:270-273` |

### Volume

| Rule | Upstream |
|---|---|
| Bar maps `volume / max_volume_percent` onto its width | `node_widget.rs:405-414` |
| Volume steps are `max_volume / bar_width` (mouse path, not ported) | `node_widget.rs:443` |
| Levels stick to 100 % within one step of it (mouse path, not ported) | `node_widget.rs:445-451` |
| Percentage is the cube root of the mean channel volume | `node_widget.rs:393-396` |

### Timing / meters

| Rule | Upstream |
|---|---|
| Peak ballistics attack/release: **300 ms** | `app.rs:235` |
| Default frame rate 60 fps (0 = unlimited) | `config.rs:227-229` |
| Meter range -60 dB .. +6 dB | `meter.rs:12-21` |

---

## 10. Flex extensions (no wiremix counterpart)

These are deliberate additions. They must not contradict §1-§9, and they are the
only elements without an upstream citation:

| Element | Where | Notes |
|---|---|---|
| Filter line `› filter…   3/87` | `render.rs:draw_filter` | Type-to-filter; upstream has no filter (its keybinding help replaces it). Uses `filter_prompt`/`hint` tokens. |
| Hint line under the list | `render.rs:draw_hints` | Confirms danger flows and dropdown state. |
| Compact nodes | `render::NodeMetrics` | The default for every provider: rows carry no volume/peaks/config/detail/sublabel, so tabs render 1 line + 1 blank row (pitch 2) — dense wifi-style lists (9 entries at 80x24 instead of 3) with `NN% [████░░░░]` text gauges in `meta`. Tall detail rows are opt-in only (`notify` message bodies); any detail-bearing row restores the full upstream node. |
| Bare tabs | `Tab::bare_rows` | Provider opt-out of all chrome (tab bar, filter, hints, gauge, right column). A bare tab owns the whole frame, so the list height is the full terminal height; scoped to flex's non-mixer providers (`launch`, `theme`, `clip`, `power`, `shot`). |
| Gauge row (single volume/brightness widget) | `render.rs:draw_gauge` | Upstream renders per-node volumes instead; flex's `center` provider needs a single-line control. Muted shows the word `muted` (upstream's wording). |
| `— no matches —` / `— offline` empty states | `render.rs:draw_empty`, `OFFLINE_STATE` | Offline dimming uses the `offline` token (a flex token). |
| `-- confirm` suffix on armed danger rows | `render.rs:draw_header` | Suffix only, no colour — matches §8 of the old flex UI. |
| Frame background clear | `render.rs:fill_bg` | Upstream never paints; flex clears so stray output cannot bleed through. |
| `ACTION:TARGET` reporting | `backend.rs:target_line` | Providers are out-of-process; upstream mutates PipeWire directly. |
| Mouse support | — | **Not implemented**; see §1. |

---

## 11. Not ported (explicit gaps)

| Element | Upstream | Status |
|---|---|---|
| Mouse interaction (select, set-default, volume drag, scroll buttons, clickable `•••`) | `node_widget.rs:98-128`, `object_list.rs:459-476` | Out of scope: flex's loop ignores mouse events. |
| TOML config file (`wiremix.toml`) for themes/char-sets/keybindings | `config.rs`, `wiremix.toml` | Replaced by CLI flags (`--theme`, `--char-set`, `--peaks`) plus `pub const` token tables. |
| Device/configuration tabs (profiles, ports, per-device settings) | `device_widget.rs`, `view.rs` | Only the device *row layout* is ported (§3); flex providers have no profile/port data. |
| Help column widths from config | `help.rs:27-33`, `config/help.rs` | Flex's help is a single fixed table of lines. |
| PipeWire-specific semantics (default sink/source, routing resolution) | `view.rs` | Providers supply rows; flex renders them. |
| **Provider-supplied volume/meter/detail data** | — | Opt-in only: list providers (`wifi`, `net`, `bt`) render gauges as text in `meta` and set no `Row::volume`, `Row::peaks`, `Row::config` or `Row::detail`, so every tab stays compact. `Row::volume`/`detail`/`sublabel` are reserved for rows whose content genuinely needs several lines (the `notify` feed). |

---

## 12. Implementation map

| Area | File |
|---|---|
| Frame, list, node rows, tab bar, help, dropdown rendering | `flex-core/src/render.rs` |
| Node metrics (compact vs upstream, `NodeMetrics`) | `flex-core/src/render.rs` |
| Peak meter port | `flex-core/src/meter.rs` |
| Character sets | `flex-core/src/charset.rs` |
| Themes | `flex-core/src/theme.rs` |
| Row/target/dropdown state model | `flex-core/src/lib.rs` |
| Key handling (incl. dropdown open/move/commit) | `flex-core/src/keys.rs` |
| `ACTION:` lines | `flex-core/src/backend.rs`, `flex-core/src/run.rs` |
| CLI flags | `flex-rice/src/main.rs` |
| Rendered-detail goldens | `flex-rice/tests/golden.rs` |
| Upstream-pinned constants and geometry | `flex-core/tests/compliance.rs` |
| Dropdown replays | `flex-core/tests/dropdown.rs` |
| Unit tests for meter math, char sets, themes | `flex-core/src/meter.rs`, `flex-core/src/charset.rs`, `flex-core/src/theme.rs` |
