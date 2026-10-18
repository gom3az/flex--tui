# AGENTS.md — flex

Cargo workspace: `flex-core` (publishable TUI engine) + `flex-rice` (unpublished rice glue, `publish = false`).
Dependency is one-way `flex-rice` → `flex-core` (path dep). Never add machine-specific paths to `flex-core`.

## Commands (repo root)

- `cargo test` — whole workspace (~479 tests); `cargo test -p <crate> --test <file>` for one suite, e.g. `cargo test -p flex-rice --test wifi`
- `cargo test --locked --test e2e` — e2e against the installed `~/.local/bin` farm (needs `./setup.sh` first)
- `cargo fmt --all --check` (`max_width=100`); `cargo clippy --locked --all-targets -- -D warnings` — both must pass (CI gate order: fmt → clippy → test → release build → `setup.sh --check` → e2e)
- `cargo build --release` → `target/release/{flex,flex-<provider>,flex-record}` (15 binaries); `./setup.sh` links them into `~/.local/bin`, `./setup.sh --check` verifies
- `cargo tree -i crossterm` — must show a single `crossterm` major (`ratatui =0.29.0` pinned, do not bump)
- `cargo bench -p flex-core --bench rerank` — rerank regression bench

## Architecture (non-obvious)

- Every provider binary is thin: `runner::popup_guard → runner::build_menu → flex_core::run::run_capture → exec::<provider>::execute`. Side effects live **only** in `flex-rice/src/exec/*.rs`; `flex-core` only returns an `Outcome`. `--print-action` prints the `ACTION:` line and exits without executing — it is the only producer of one.
- Exit codes are the contract (`flex-core/src/backend.rs`): `0` executed, `130` cancel, `1` error.
- In `flex-rice`, build menus with `flex_rice::menu(…)` (installs the `TickHook`), not `Menu::new` — else wifi/proc/net/bt/notify silently stop refreshing.
- `runner::fail` owns the single `flex: error:` prefix — nothing below it adds its own `flex:` prefix (B-022/B-027).
- Runtime stderr/stdout writes must use `flex_core::diag::{warn,warn_inline,note}`, never `eprintln!`/`println!` — a dead popup pty (`EIO`) + `panic = "abort"` release profile turns a failed print into SIGABRT. Locked by `flex-rice/tests/diagnostics.rs`.
- Empty-store policy: `clip` with no history / `wallpaper` with no images exits `130` with a stderr note before touching the terminal; a chooser that found nothing shows a `noop` row (`providers::NOOP_ID`) and Enter is a no-op.
- Compact rows are the default: quantitative state (signal/share/battery) renders as a wifi-style text gauge in `meta` (`wifi::bar(val, max, 8)`), never `Row::volume`/`config`/`detail`/`sublabel` — those force 3-line nodes. Rows with dropdown targets set `hide_target_in_header` so the gauge-bearing `meta` owns the header right column. Tall rows are opt-in only for genuinely multi-line content (`notify` bodies).
- Space-free ids: `clip`/`wallpaper`/`launch`/`theme` hash free text into `action_id` and resolve in-process (`clip::resolve`, `wallpaper::resolve`, `launch::resolve_id`, `theme_::resolve_name`); never put raw names in ids.
- `flex` dispatcher re-execs sibling `flex-<provider>` binaries, reconstructing `-s/-t/-p/--filter-mode` in canonical order (`runner::canonical_tail`); `net`/`notify` auto-inject `-m`.
- `FLEX_TEST=1` fixes RNG/time seeds for deterministic golden/key tests.

## Gotchas

- `Cargo.lock` is committed; `target/` is gitignored. `[profile.release]` and `[workspace.lints]` (unsafe deny, clippy all+pedantic deny) live only at the workspace root.
- `Docs/Implementation.md` and `Docs/project_structure.md` contain stale claims (e.g. "no serde", "eleven binaries", "sixteen binaries") — trust `Cargo.toml`/`setup.sh`/CI over prose. Current truth: `serde`/`serde_json` are used, 15 binaries.
- `flex-mixer` is not a TUI menu — it toggles a `wiremix` floating terminal. `flex-record`/`flex-mixer` are sync-only (no tokio runtime).
- Popup toggle is keyed on window-class variant (`flex-menu` / `flex-menu-wide` / `flex-notify-center`), not provider. `$TERMINAL` unknown/empty warns once and falls back to kitty, never exits 1.
