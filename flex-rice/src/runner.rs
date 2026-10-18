//! Shared runner for the sixteen flex entry points.
//!
//! The thirteen per-provider binaries (`flex-power`, `flex-launch`, …), the
//! two sync helpers (`flex-record`, `flex-mixer`) and the `flex` compat
//! dispatcher are all thin clap shells over this module, so
//! the contracts live here exactly once:
//!
//! - [`fail`] is the single owner of the `flex: error:` prefix plus
//!   `exit(1)` (B-022/B-027: nothing below this module adds its own `flex:`
//!   prefix to an error value).
//! - [`build_menu`] owns per-provider menu construction (including the
//!   `clip`/`wallpaper` empty-store early exits to 130).
//! - [`popup_guard`] owns the outside-a-popup re-exec into
//!   [`popup::toggle_with`], mirroring the wrappers' `POPUP_KITTY` guard.
//! - [`run_select`] owns the select loop: [`run::run_capture`], then the
//!   `ACTION:` emit plus exit mapping (chosen/delete/toggle/target print
//!   one line and exit `0`; `Quit{code}` exits with `code` and no output).
//! - [`canonical_tail`] owns the canonical flag order the dispatcher uses
//!   to re-exec provider binaries.
//!
//! [`popup::toggle_with`]: crate::popup::toggle_with
//! [`run::run_capture`]: flex_core::run::run_capture

use anyhow::{Context as _, Result};
use clap::Args;
use flex_core::backend::{EXIT_CANCELLED, EXIT_ERROR, EXIT_OK};
use flex_core::{CharSet, CharSetName, Menu, Outcome, Peaks, Theme, ThemeName};

/// Initialize tracing/logging from `RUST_LOG` (fmt-only, OPT-11).
///
/// `tracing-subscriber` ships without `env-filter` (which pulled `regex`),
/// so the level comes from a small `RUST_LOG` scan instead: the first
/// `trace`/`debug`/`info`/`warn`/`error` token wins, defaulting to `WARN`
/// when unset or unrecognized (`off` silences down to `ERROR`).
pub fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(level_from_env())
        .with_writer(std::io::stderr)
        .try_init();
}

/// `RUST_LOG` level without the `env-filter`/`regex` dependency.
fn level_from_env() -> tracing::Level {
    let value = std::env::var("RUST_LOG").unwrap_or_default().to_lowercase();
    for token in value.split(|sep: char| !sep.is_ascii_alphanumeric()) {
        match token {
            "trace" => return tracing::Level::TRACE,
            "debug" => return tracing::Level::DEBUG,
            "info" => return tracing::Level::INFO,
            "warn" | "warning" => return tracing::Level::WARN,
            "error" => return tracing::Level::ERROR,
            _ => {}
        }
    }
    if value.contains("off") {
        return tracing::Level::ERROR;
    }
    tracing::Level::WARN
}

use crate::{menu, popup, providers};
use providers::{
    bt, clip, launch, net, notify, power, proc, profile, rec_opt, shot, theme_, wallpaper, wifi,
};

/// The twelve menu providers, one per `flex-<name>` binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Shutdown/reboot/logout menu.
    Power,
    /// Application launcher.
    Launch,
    /// Screenshot flow.
    Shot,
    /// Theme switcher.
    Theme,
    /// Clipboard history.
    Clip,
    /// Wallpaper picker (image previews).
    Wallpaper,
    /// Wi-Fi picker.
    Wifi,
    /// Native process manager (kill menu).
    Proc,
    /// Network monitor and top bandwidth consumers.
    Net,
    /// Native Bluetooth manager.
    Bt,
    /// Notification Center drawer.
    Notify,
    /// Power Profile switcher.
    Profile,
}

impl Provider {
    /// Every provider, in binary-name order.
    pub const ALL: [Self; 12] = [
        Self::Power,
        Self::Launch,
        Self::Shot,
        Self::Theme,
        Self::Clip,
        Self::Wallpaper,
        Self::Wifi,
        Self::Proc,
        Self::Net,
        Self::Bt,
        Self::Notify,
        Self::Profile,
    ];

    /// Parse a provider subcommand/binary name (`None` for anything else).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "power" => Some(Self::Power),
            "launch" => Some(Self::Launch),
            "shot" => Some(Self::Shot),
            "theme" => Some(Self::Theme),
            "clip" => Some(Self::Clip),
            "wallpaper" => Some(Self::Wallpaper),
            "wifi" => Some(Self::Wifi),
            "proc" => Some(Self::Proc),
            "net" => Some(Self::Net),
            "bt" => Some(Self::Bt),
            "notify" => Some(Self::Notify),
            "profile" => Some(Self::Profile),
            _ => None,
        }
    }

    /// Subcommand name (also the `ACTION:` provider field).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Power => power::PROVIDER,
            Self::Launch => launch::PROVIDER,
            Self::Shot => shot::PROVIDER,
            Self::Theme => theme_::PROVIDER,
            Self::Clip => clip::PROVIDER,
            Self::Wallpaper => wallpaper::PROVIDER,
            Self::Wifi => wifi::PROVIDER,
            Self::Proc => proc::PROVIDER,
            Self::Net => net::PROVIDER,
            Self::Bt => bt::PROVIDER,
            Self::Notify => notify::PROVIDER,
            Self::Profile => profile::PROVIDER,
        }
    }

    /// Binary name (`flex-<name>`), resolved next to the running dispatcher.
    #[must_use]
    pub fn bin_name(self) -> String {
        format!("flex-{}", self.name())
    }

    /// Popup variant for this provider (`menu` for power/shot/theme/wifi/bt/profile,
    /// `menu-wide` for launch/clip/wallpaper/proc/net, `drawer` for notify — see
    /// [`popup::MENU_VARIANT`] / [`popup::WIDE_VARIANT`] / [`popup::DRAWER_VARIANT`]).
    #[must_use]
    pub fn variant(self) -> &'static str {
        match self {
            Self::Power | Self::Shot | Self::Theme | Self::Wifi | Self::Bt | Self::Profile => {
                popup::MENU_VARIANT
            }
            Self::Launch | Self::Clip | Self::Wallpaper | Self::Proc | Self::Net => {
                popup::WIDE_VARIANT
            }
            Self::Notify => popup::DRAWER_VARIANT,
        }
    }

    /// Window class for this provider's popup variant.
    #[must_use]
    pub fn variant_class(self) -> &'static str {
        match self {
            Self::Power | Self::Shot | Self::Theme | Self::Wifi | Self::Bt | Self::Profile => {
                popup::MENU_CLASS
            }
            Self::Launch | Self::Clip | Self::Wallpaper | Self::Proc | Self::Net => {
                popup::WIDE_CLASS
            }
            Self::Notify => popup::DRAWER_CLASS,
        }
    }
}

/// Hidden ranking-engine flag values (`--filter-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
enum FilterModeArg {
    /// Tiered fuzzy (default).
    #[default]
    Spec,
    /// Subsequence predicate, provider order preserved.
    Legacy,
}

impl FilterModeArg {
    /// Engine value behind the flag.
    #[must_use]
    fn to_engine(self) -> flex_core::filter::FilterMode {
        match self {
            Self::Spec => flex_core::filter::FilterMode::Spec,
            Self::Legacy => flex_core::filter::FilterMode::Legacy,
        }
    }
}

/// Parse an upstream character-set name.
fn parse_char_set(name: &str) -> Result<CharSetName, String> {
    CharSetName::parse(name).ok_or_else(|| {
        format!("'{name}' is not a built-in character set (default, compat, extracompat)")
    })
}

/// Parse an upstream theme name.
fn parse_theme(name: &str) -> Result<ThemeName, String> {
    ThemeName::parse(name)
        .ok_or_else(|| format!("'{name}' is not a built-in theme (default, nocolor, plain)"))
}

/// Parse an upstream peaks mode.
fn parse_peaks(name: &str) -> Result<Peaks, String> {
    match name {
        "off" => Ok(Peaks::Off),
        "mono" => Ok(Peaks::Mono),
        "auto" => Ok(Peaks::Auto),
        other => Err(format!("'{other}' is not a peaks mode (off, mono, auto)")),
    }
}

/// Canonical CLI spelling of a peaks mode.
#[must_use]
pub fn peaks_as_str(peaks: Peaks) -> &'static str {
    match peaks {
        Peaks::Off => "off",
        Peaks::Mono => "mono",
        Peaks::Auto => "auto",
    }
}

/// Canonical CLI spelling of a filter mode.
#[must_use]
pub fn filter_mode_as_str(mode: flex_core::filter::FilterMode) -> &'static str {
    match mode {
        flex_core::filter::FilterMode::Spec => "spec",
        flex_core::filter::FilterMode::Legacy => "legacy",
    }
}

/// Env kill-switch for usage ranking (mirrors `FLEX_USAGE_FILE`; set and
/// non-empty disables, like the clip provider's env seams).
pub const NO_FRECENCY_ENV: &str = "FLEX_NO_FRECENCY";

/// Whether frecency ranking/recording is enabled: on unless `--no-frecency`
/// was passed or `$FLEX_NO_FRECENCY` is set and non-empty.
#[must_use]
pub fn frecency_enabled(no_frecency: bool) -> bool {
    if no_frecency {
        return false;
    }
    std::env::var(NO_FRECENCY_ENV)
        .ok()
        .is_none_or(|value| value.is_empty())
}

/// Global presentation flags shared by every entry point (upstream
/// `-s/-t/-p/--filter-mode`).
#[derive(Debug, Args)]
pub struct GlobalStyle {
    /// Ranking engine override (R1 escape hatch; `legacy` preserves
    /// provider order instead of score-reordering).
    #[arg(long, hide = true, default_value = "spec", global = true)]
    filter_mode: FilterModeArg,

    /// Disable zoxide-style usage ranking: matches rank by fuzzy score
    /// only and nothing is recorded (`$FLEX_NO_FRECENCY` does the same).
    #[arg(long, global = true)]
    no_frecency: bool,

    /// Character set (upstream `-s/--char-set`): default, compat, extracompat.
    #[arg(
        short = 's',
        long,
        default_value = "default",
        value_parser = parse_char_set,
        global = true
    )]
    char_set: CharSetName,

    /// Theme (upstream `-t/--theme`): default, nocolor, plain.
    #[arg(
        short = 't',
        long,
        default_value = "default",
        value_parser = parse_theme,
        global = true
    )]
    theme: ThemeName,

    /// Peak meters (upstream `-p/--peaks`): off, mono, auto.
    #[arg(
        short = 'p',
        long,
        default_value = "auto",
        value_parser = parse_peaks,
        global = true
    )]
    peaks: Peaks,
}

impl GlobalStyle {
    /// Engine-ready presentation flags behind the CLI globals.
    #[must_use]
    pub fn options(self) -> StyleOptions {
        StyleOptions {
            filter_mode: self.filter_mode.to_engine(),
            char_set: self.char_set,
            theme: self.theme,
            peaks: self.peaks,
            frecency: frecency_enabled(self.no_frecency),
        }
    }
}

/// Presentation flags shared by every provider (upstream `-s/-t/-p/--filter-mode`).
#[derive(Debug, Clone, Copy)]
pub struct StyleOptions {
    /// Ranking engine for the filtered view.
    pub filter_mode: flex_core::filter::FilterMode,
    /// Character set for box drawing.
    pub char_set: CharSetName,
    /// Color theme.
    pub theme: ThemeName,
    /// Peak-meter mode.
    pub peaks: Peaks,
    /// Whether frecency reorders matches and choices are recorded.
    pub frecency: bool,
}

impl StyleOptions {
    /// Apply the flags to a menu.
    #[must_use]
    pub fn apply(self, mut menu: Menu) -> Menu {
        menu.app.filter_mode = self.filter_mode;
        menu.app.use_frecency = self.frecency;
        menu.char_set = CharSet::get(self.char_set);
        menu.theme = Theme::get(self.theme);
        menu.peaks = self.peaks;
        menu
    }
}

/// Build the interactive menu for `provider`.
///
/// This is the single home of per-provider menu construction (moved out of
/// the old `main.rs` `run()` match so the dispatcher and the eight binaries
/// cannot diverge). Empty `clip`/`wallpaper` stores keep their historical
/// behavior: a `flex: <provider>`: diagnostic and exit 130, no menu.
///
/// # Errors
///
/// Currently infallible (`Ok` always); the `Result` keeps the shared
/// call-site shape for providers whose construction may fail later.
pub fn build_menu(provider: Provider, style: StyleOptions) -> Result<Menu> {
    let inner: Result<Menu> = {
        match provider {
            Provider::Power => {
                let tabs = power::power_tabs();
                Ok(style.apply(menu(power::PROVIDER, tabs)))
            }
            Provider::Launch => {
                let tab = launch::launch_tab();
                Ok(style.apply(menu(launch::PROVIDER, vec![tab])))
            }
            Provider::Shot => {
                let tab = shot::shot_tab();
                Ok(style.apply(menu(shot::PROVIDER, vec![tab])))
            }
            Provider::Theme => {
                let tab = theme_::theme_tab();
                Ok(style.apply(menu(theme_::PROVIDER, vec![tab])))
            }
            Provider::Clip => {
                let tab = clip::clip_tab();
                if tab.rows.is_empty() {
                    flex_core::diag::warn("flex: clip: no history yet");
                    std::process::exit(EXIT_CANCELLED);
                }
                Ok(style.apply(menu(clip::PROVIDER, vec![tab])))
            }
            Provider::Wallpaper => {
                let tab = wallpaper::wallpaper_tab();
                if tab.rows.is_empty() {
                    flex_core::diag::warn("flex: wallpaper: no wallpapers found");
                    std::process::exit(EXIT_CANCELLED);
                }
                let mut built = style.apply(menu(wallpaper::PROVIDER, vec![tab]));
                // Image previews need a kitty-compatible terminal; everywhere
                // else the picker is a plain list (no pane reserved).
                built.preview = flex_core::preview::enabled();
                Ok(built)
            }
            Provider::Wifi => Ok(style.apply(wifi::menu())),
            Provider::Proc => Ok(style.apply(menu(proc::PROVIDER, vec![proc::proc_tab()]))),
            Provider::Net => Ok(style.apply(net::net_menu())),
            Provider::Bt => Ok(style.apply(bt::bt_menu())),
            Provider::Notify => Ok(style.apply(notify::menu())),
            Provider::Profile => {
                let tab = profile::profile_tab();
                Ok(style.apply(menu(profile::PROVIDER, vec![tab])))
            }
        }
    };
    let mut built = inner?;
    // Frecency table: loaded once per invocation and keyed by the menu's own
    // provider tag (what `Chosen` reports). Tabs that are neither
    // filterable nor learnable (fixed menus, ephemeral lists) skip
    // entirely — static options are not a learnable list.
    if style.frecency
        && built
            .app
            .tabs
            .iter()
            .any(|tab| tab.filterable && tab.learnable)
    {
        built.app.usage = crate::usage::load_one(&built.provider, None);
    }
    Ok(built)
}

/// This process's argv for a popup re-exec: the current executable plus the
/// actual arguments (mirrors the wrappers' `popup.sh <variant> "$0" "$@"`).
///
/// # Errors
///
/// When the current executable path cannot be read.
pub fn self_argv() -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("cannot read the current executable path")?;
    let mut argv = vec![exe.display().to_string()];
    argv.extend(std::env::args().skip(1));
    Ok(argv)
}

/// Popup guard: inside a popup (`POPUP_KITTY == "1"`) this is a no-op; outside
/// one it toggles this provider's popup variant with this process's argv and
/// exits `0` — the `POPUP_KITTY` re-exec the wrappers used to own.
///
/// # Errors
///
/// When the current argv cannot be read or the toggle (probe/spawn) fails.
/// The error carries no `flex:` prefix; [`fail`] adds it.
pub fn popup_guard(provider: Provider) -> Result<()> {
    if popup::in_popup() {
        return Ok(());
    }
    popup::toggle_with(provider.variant_class(), &self_argv()?)?;
    std::process::exit(EXIT_OK);
}

/// Run the select loop for `provider` and emit the result.
///
/// [`flex_core::run::run_capture`] runs the interactive loop in-process;
/// terminal outcomes map exactly like the old `run::run`: chosen, deleted,
/// toggled and target rows print one `ACTION:` line and exit `0`;
/// `Quit{code}` exits with `code` and prints nothing; `Cancelled` exits
/// 130. Only TTY/emit failures return; everything else diverges.
///
/// Both the default select+execute path (whose execute phase is still a
/// stub — see the `TODO(executor)` markers at the call sites) and
/// `--print-action` share this function, so the row→action mapping under
/// probe is the mapping the menu really runs.
///
/// # Errors
///
/// When the terminal cannot initialize, frames cannot draw, events cannot
/// be polled/read, or the `ACTION:` line cannot be written. The error
/// carries no `flex:` prefix; [`fail`] adds it.
pub async fn run_select(provider: Provider, style: StyleOptions) -> Result<()> {
    let built = build_menu(provider, style)?;
    crate::spawn::set_wakeup_notifier(built.notifier());
    emit_outcome(flex_core::run::run_capture(built).await?)
}

/// Helper for the standard provider CLI execution path.
///
/// Guards the popup, optionally prints the action (dry-run), runs the UI,
/// handles the standard `Quit` and `Cancelled` outcomes natively, and passes
/// any other outcome to the provider-specific handler.
///
/// # Errors
///
/// Returns an error when popup toggle, menu building, event loop execution,
/// or the outcome handler fails.
pub async fn run_standard_cli<F>(
    provider: Provider,
    style: StyleOptions,
    print_action: bool,
    handler: F,
) -> Result<()>
where
    F: FnOnce(Outcome) -> Result<()>,
{
    popup_guard(provider)?;
    if print_action {
        return run_select(provider, style).await;
    }
    run_custom_menu(build_menu(provider, style)?, style, handler).await
}

/// Run the select loop over a caller-built [`Menu`] (same outcome contract
/// as [`run_standard_cli` minus the popup guard): frecency hook, `Quit` and
/// `Cancelled` diverge, everything else goes to `handler`. Lets one binary
/// chain several menus in-process (the recording-options submenu).
///
/// # Errors
///
/// When the event loop fails or the outcome handler fails.
pub async fn run_custom_menu<F>(menu: Menu, style: StyleOptions, handler: F) -> Result<()>
where
    F: FnOnce(Outcome) -> Result<()>,
{
    crate::spawn::set_wakeup_notifier(menu.notifier());
    // Snapshot row snapshots before the event loop consumes the menu: the
    // record hook joins choices to rows by id, skipping offline placeholders
    // and rows in fixed (non-filterable) tabs.
    let rows: Vec<crate::usage::RowSnap> = menu
        .app
        .tabs
        .iter()
        .flat_map(|tab| {
            tab.rows.iter().map(|row| crate::usage::RowSnap {
                id: row.id.as_str().to_owned(),
                offline: row.offline,
                searchable: tab.filterable && tab.learnable,
            })
        })
        .collect();
    match flex_core::run::run_capture(menu).await? {
        Outcome::Quit { code } => std::process::exit(code),
        Outcome::Cancelled => std::process::exit(EXIT_CANCELLED),
        other => {
            record_hook(style.frecency, &rows, &other);
            handler(other)
        }
    }
}

/// Recording-options submenu: confirm-with-live-summary first, drill-in
/// rows reopen their dimension tab, picks return here.
///
/// `Quit`/`Cancelled` diverge (exit code / 130, like the main menu), so a
/// submenu escape cancels the whole capture; confirm returns the effective
/// settings for the caller to execute.
///
/// # Errors
///
/// When a menu cannot build or an outcome is not a row choice.
pub async fn run_recording_menu(
    style: StyleOptions,
    initial: rec_opt::RecOptions,
) -> Result<rec_opt::RecOptions> {
    use rec_opt::{apply_choice, Choice, Dimension};
    let mut opts = initial;
    loop {
        let mut pick: Option<String> = None;
        run_custom_menu(
            style.apply(menu(rec_opt::PROVIDER, vec![rec_opt::options_tab(&opts)])),
            style,
            |outcome| match outcome {
                Outcome::Chosen { action_id, .. } => {
                    pick = Some(action_id);
                    Ok(())
                }
                Outcome::Delete { action_id, .. } => {
                    anyhow::bail!("recording options: unexpected delete outcome for '{action_id}'")
                }
                Outcome::Toggle { action_id, .. } => {
                    anyhow::bail!("recording options: unexpected toggle outcome for '{action_id}'")
                }
                Outcome::Target { row, .. } => {
                    anyhow::bail!("recording options: unexpected target outcome for '{row}'")
                }
                _ => unreachable!(),
            },
        )
        .await?;
        let Some(action_id) = pick else {
            anyhow::bail!("recording options: menu returned no choice");
        };
        match apply_choice(&mut opts, &action_id) {
            Choice::Confirm => return Ok(opts),
            Choice::Open(dimension) => {
                let tab = match dimension {
                    Dimension::Audio => rec_opt::audio_tab(),
                    Dimension::Quality => rec_opt::quality_tab(),
                    Dimension::Fps => rec_opt::fps_tab(),
                };
                let mut value: Option<String> = None;
                run_custom_menu(
                    style.apply(menu(rec_opt::PROVIDER, vec![tab])),
                    style,
                    |outcome| match outcome {
                        Outcome::Chosen { action_id, .. } => {
                            value = Some(action_id);
                            Ok(())
                        }
                        Outcome::Delete { action_id, .. } => {
                            anyhow::bail!(
                                "recording options: unexpected delete outcome for '{action_id}'"
                            )
                        }
                        Outcome::Toggle { action_id, .. } => {
                            anyhow::bail!(
                                "recording options: unexpected toggle outcome for '{action_id}'"
                            )
                        }
                        Outcome::Target { row, .. } => {
                            anyhow::bail!(
                                "recording options: unexpected target outcome for '{row}'"
                            )
                        }
                        _ => unreachable!(),
                    },
                )
                .await?;
                if let Some(action_id) = value {
                    let _ = apply_choice(&mut opts, &action_id);
                }
            }
            Choice::Updated | Choice::Unknown => {}
        }
    }
}

/// Frecency record hook for the interactive path.
///
/// `--print-action` returns through [`run_select`], never here, so dry runs
/// don't pollute ranking. `Chosen` bumps the row's rank, `Delete` drops its
/// entry, everything else is ignored. Store failures only warn — a broken
/// cache never fails a launch.
fn record_hook(enabled: bool, rows: &[crate::usage::RowSnap], outcome: &Outcome) {
    if !enabled {
        return;
    }
    let now = crate::usage::now_secs();
    let result = match outcome {
        Outcome::Chosen {
            provider,
            action_id,
            ..
        } => crate::usage::record_choice(provider, rows, action_id, None, now),
        Outcome::Delete {
            provider,
            action_id,
            ..
        } => crate::usage::remove_choice(provider, action_id, None),
        _ => return,
    };
    if let Err(err) = result {
        flex_core::diag::warn(&format!("flex: usage store: {err:#}"));
    }
}

/// Emit one terminal outcome and exit (the diverging half of [`run_select`]).
///
/// # Errors
///
/// When the `ACTION:` line cannot be written. Every success path diverges
/// via `process::exit`, so `Ok` is unreachable in practice but keeps the
/// signature honest for the emit-failure case.
fn emit_outcome(outcome: Outcome) -> Result<()> {
    match outcome {
        Outcome::Chosen {
            provider,
            action_id,
            label,
        } => {
            flex_core::backend::emit_action(&provider, &action_id, &label)?;
            std::process::exit(EXIT_OK);
        }
        Outcome::Delete {
            provider,
            action_id,
            label,
        } => {
            flex_core::backend::emit_delete(&provider, &action_id, &label)?;
            std::process::exit(EXIT_OK);
        }
        Outcome::Toggle {
            provider,
            action_id,
            label,
        } => {
            flex_core::backend::emit_toggle(&provider, &action_id, &label)?;
            std::process::exit(EXIT_OK);
        }
        Outcome::Target {
            provider,
            row,
            target,
            title,
        } => {
            flex_core::backend::emit_target(&provider, &row, &target, &title)?;
            std::process::exit(EXIT_OK);
        }
        Outcome::Quit { code } => {
            std::process::exit(code);
        }
        Outcome::Cancelled => {
            std::process::exit(EXIT_CANCELLED);
        }
    }
}

/// Canonical flag tail for re-execing a provider binary: `-s/-t/-p` plus
/// `--filter-mode` in fixed order, `--no-frecency` when disabled, with
/// `--print-action` last when set.
///
/// Pure (no I/O, no spawn), so the dispatcher's flag-ordering is unit
/// testable without opening a TUI.
#[must_use]
pub fn canonical_tail(style: StyleOptions, print_action: bool) -> Vec<String> {
    let mut tail = vec![
        "-s".to_string(),
        style.char_set.as_str().to_string(),
        "-t".to_string(),
        style.theme.as_str().to_string(),
        "-p".to_string(),
        peaks_as_str(style.peaks).to_string(),
        "--filter-mode".to_string(),
        filter_mode_as_str(style.filter_mode).to_string(),
    ];
    if !style.frecency {
        tail.push("--no-frecency".to_string());
    }
    if print_action {
        tail.push("--print-action".to_string());
    }
    tail
}

/// Sole owner of the `flex: error:` prefix: print it once and exit 1.
///
/// Every entry point's `main` funnels fallible work through this, so error
/// values below the runner must never carry their own `flex:` prefix
/// (B-022/B-027) — otherwise the line would read
/// `flex: error: flex: …`. Exit code is [`EXIT_ERROR`].
pub fn fail(err: &anyhow::Error) -> ! {
    flex_core::diag::warn(&format!("flex: error: {err:#}"));
    std::process::exit(EXIT_ERROR);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> StyleOptions {
        StyleOptions {
            filter_mode: flex_core::filter::FilterMode::Spec,
            char_set: CharSetName::Default,
            theme: ThemeName::Default,
            peaks: Peaks::Auto,
            frecency: true,
        }
    }

    #[test]
    fn providers_parse_by_subcommand_name() {
        for provider in Provider::ALL {
            assert_eq!(Provider::parse(provider.name()), Some(provider));
        }
        assert_eq!(Provider::parse("bogus"), None);
        assert_eq!(Provider::parse(""), None);
        assert_eq!(Provider::parse("Power"), None, "match is lowercase-only");
    }

    #[test]
    fn providers_map_to_the_wrappers_popup_table() {
        for provider in Provider::ALL {
            let (variant, class) = match provider.name() {
                "power" | "shot" | "theme" | "wifi" | "bt" | "profile" => {
                    ("menu", popup::MENU_CLASS)
                }
                "launch" | "clip" | "wallpaper" | "proc" | "net" => {
                    ("menu-wide", popup::WIDE_CLASS)
                }
                "notify" => ("drawer", popup::DRAWER_CLASS),
                other => panic!("unexpected provider {other}"),
            };
            assert_eq!(provider.variant(), variant);
            assert_eq!(provider.variant_class(), class);
            assert_eq!(popup::class_for(provider.variant()), Some(class));
        }
    }

    #[test]
    fn bin_names_are_the_flex_dash_spelling() {
        assert_eq!(Provider::Power.bin_name(), "flex-power");
        assert_eq!(Provider::Wallpaper.bin_name(), "flex-wallpaper");
        assert_eq!(Provider::Net.bin_name(), "flex-net");
    }

    #[test]
    fn canonical_tail_is_ordered_and_complete() {
        assert_eq!(
            canonical_tail(style(), false),
            vec![
                "-s",
                "default",
                "-t",
                "default",
                "-p",
                "auto",
                "--filter-mode",
                "spec",
            ]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn canonical_tail_appends_print_action_last() {
        let tail = canonical_tail(style(), true);
        assert_eq!(
            tail.last().map(String::as_str),
            Some("--print-action"),
            "probe flag trails the style flags"
        );
    }

    #[test]
    fn canonical_tail_omits_no_frecency_when_enabled() {
        let tail = canonical_tail(style(), false);
        assert!(
            !tail.iter().any(|flag| flag == "--no-frecency"),
            "default-on keeps existing argv byte-identical: {tail:?}"
        );
    }

    #[test]
    fn canonical_tail_emits_no_frecency_before_print_action() {
        let disabled = StyleOptions {
            frecency: false,
            ..style()
        };
        let tail = canonical_tail(disabled, true);
        let no_frecency = tail
            .iter()
            .position(|flag| flag == "--no-frecency")
            .expect("flag present");
        let print_action = tail
            .iter()
            .position(|flag| flag == "--print-action")
            .expect("probe present");
        assert!(
            no_frecency < print_action,
            "style flags precede the probe flag"
        );
    }

    #[test]
    fn learnable_flag_marks_ephemeral_lists() {
        // Wifi rows are scan snapshots: searchable, not learnable.
        let wifi = build_menu(Provider::Wifi, style()).expect("menu builds");
        assert!(!wifi.app.tabs.is_empty());
        assert!(
            wifi.app.tabs.iter().all(|tab| !tab.learnable),
            "wifi opts out of usage ranking"
        );
        assert!(wifi.app.usage.is_empty(), "nothing loads for wifi");
        // Launch rows are stable desktop ids: learnable.
        let launch = build_menu(Provider::Launch, style()).expect("menu builds");
        assert!(!launch.app.tabs.is_empty());
        assert!(
            launch.app.tabs.iter().all(|tab| tab.learnable),
            "launch keeps usage ranking"
        );
    }

    #[test]
    fn frecency_enabled_unless_flag_or_env() {
        assert!(frecency_enabled(false), "default on");
        assert!(!frecency_enabled(true), "flag opts out");
        std::env::set_var(NO_FRECENCY_ENV, "1");
        assert!(!frecency_enabled(false), "env opts out");
        std::env::set_var(NO_FRECENCY_ENV, "");
        assert!(frecency_enabled(false), "empty env means unset");
        std::env::remove_var(NO_FRECENCY_ENV);
    }

    #[test]
    fn style_apply_carries_the_frecency_switch() {
        use flex_core::{Row, RowId, Tab};
        let menu = crate::providers::menu(
            "launch",
            vec![Tab::with_rows("apps", vec![Row::new(RowId::new("a"), "A")])],
        );
        assert!(style().apply(menu).app.use_frecency, "default on");
        let menu = crate::providers::menu(
            "launch",
            vec![Tab::with_rows("apps", vec![Row::new(RowId::new("a"), "A")])],
        );
        let disabled = StyleOptions {
            frecency: false,
            ..style()
        };
        assert!(!disabled.apply(menu).app.use_frecency);
    }

    #[test]
    fn build_menu_tags_every_provider() {
        // Non-empty providers build a menu tagged with their own name; the
        // two empty-store providers (`clip`, `wallpaper`) exit 130 instead
        // of returning, so they are covered by the integration prefix test.
        for provider in [
            Provider::Power,
            Provider::Launch,
            Provider::Shot,
            Provider::Theme,
            Provider::Wifi,
            Provider::Proc,
            Provider::Net,
            Provider::Bt,
        ] {
            let built = build_menu(provider, style()).expect("menu builds");
            assert_eq!(built.provider, provider.name());
        }
    }
}
