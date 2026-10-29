//! Popup window classes, in-popup detection and the toggle helper.
//!
//! Rust parity with dotfiles `popup.sh`: that script toggles on the variant
//! class — `pgrep -f "kitty --class flex-<variant> "` closes an open popup
//! instead of stacking another one, else it spawns `kitty --class … -o
//! font_size=10 -e env POPUP_KITTY=1 <cmd…>` detached with stdio nulled.
//! This module keeps that keying with the classes [`MENU_CLASS`] /
//! [`WIDE_CLASS`].
//!
//! Toggle is keyed on the *variant* class, exactly like `popup.sh`: a second
//! invocation for a different provider sharing the variant closes rather
//! than stacks (opening `wifi` while the `power` popup is up closes it).
//! Spawning itself is terminal-agnostic
//! ([`crate::terminal::spawn_argv`]); only the open/close probing (`pgrep`
//! / `pkill -f "<class> "`) is shared.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use super::terminal::{self, TerminalKind};

/// Window class for the compact (`menu`) popup variant.
///
/// Providers: `power`, `shot`, `theme`, `wifi` (see
/// [`Provider::variant`](crate::runner::Provider::variant)).
pub const MENU_CLASS: &str = "flex-menu";

/// Window class for the wide (`menu-wide`) popup variant.
///
/// Providers: `launch`, `clip`, `wallpaper`.
pub const WIDE_CLASS: &str = "flex-menu-wide";

/// Window class for the notification center drawer (`drawer`) variant.
///
/// Providers: `notify`.
pub const DRAWER_CLASS: &str = "flex-notify-center";

/// Variant name for the compact popup (the `popup.sh` spelling).
pub const MENU_VARIANT: &str = "menu";

/// Variant name for the wide popup (the `popup.sh` spelling).
pub const WIDE_VARIANT: &str = "menu-wide";

/// Variant name for the notification center drawer.
pub const DRAWER_VARIANT: &str = "drawer";

/// Map a popup variant to its window class.
///
/// Accepts both the `popup.sh` variant spellings (`menu`, `menu-wide`, `drawer`) and
/// the already-resolved class names (`flex-menu`, `flex-menu-wide`, `flex-notify-center`;
/// idempotent, so callers holding either spelling converge). Returns `None`
/// for anything else — `popup.sh` rejects unknown variants with a usage
/// error, and this is the typed equivalent.
#[must_use]
pub fn class_for(variant: &str) -> Option<&'static str> {
    if variant == MENU_VARIANT || variant == MENU_CLASS {
        Some(MENU_CLASS)
    } else if variant == WIDE_VARIANT || variant == WIDE_CLASS {
        Some(WIDE_CLASS)
    } else if variant == DRAWER_VARIANT || variant == DRAWER_CLASS {
        Some(DRAWER_CLASS)
    } else {
        None
    }
}

/// Build the `pgrep`/`pkill -f` pattern for a popup class.
///
/// The trailing space is load-bearing `popup.sh` parity: `flex-menu` is a
/// string prefix of `flex-menu-wide`, and the space keeps the two patterns
/// from colliding with each other.
#[must_use]
pub fn match_pattern(class: &str) -> String {
    format!("{class} ")
}

/// Pure predicate behind [`in_popup`]: the marker test without the env read.
///
/// `Some("1")` is inside; anything else (unset, empty, other values) is
/// outside — the `[[ "${POPUP_KITTY:-}" != 1 ]]` wrapper check, inverted.
#[must_use]
pub fn in_popup_with(marker: Option<&str>) -> bool {
    marker.is_some_and(|value| value == "1")
}

/// Whether this process already runs inside a popup (`POPUP_KITTY == "1"`).
///
/// Parity with the wrappers' `[[ "${POPUP_KITTY:-}" != 1 ]]` re-exec guard:
/// outside a popup the wrappers re-exec into `popup.sh`; inside, they run
/// the menu directly.
#[must_use]
pub fn in_popup() -> bool {
    in_popup_with(std::env::var("POPUP_KITTY").ok().as_deref())
}

/// The ambient `PATH`, empty when unset (tool resolution then fails cleanly
/// instead of inheriting a surprising default).
fn ambient_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Resolve `name` against `path_env` (`:`-separated, shell-style).
///
/// Returns the first entry that names an existing file, so stub-`PATH`
/// tests can shadow the real tools without touching the process env.
fn resolve_tool(name: &str, path_env: &str) -> Option<PathBuf> {
    path_env
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file())
}

/// Retry a process call while it fails with a transient `ETXTBSY`
/// (`ExecutableFileBusy`).
///
/// See [`crate::spawn`] for the rationale; this is the shared wrapper.
fn retrying<T>(run: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    crate::spawn::retrying(run)
}

/// Whether a popup of `class` is currently open (`pgrep -f "<class> "`).
///
/// Shared with the toast host, which must not stack banners over the open
/// notification center.
#[must_use]
pub fn class_is_open(class: &str) -> bool {
    class_is_open_with(class, &ambient_path())
}

/// [`class_is_open`] with an explicit `PATH` (test seam).
#[must_use]
pub fn class_is_open_with(class: &str, path_env: &str) -> bool {
    pgrep_open(class, path_env).unwrap_or(false)
}

/// Whether a popup of `class` is currently open (`pgrep -f "<class> "`).
///
/// `# Errors`: when `pgrep` is missing from `path_env` or cannot be
/// executed. A non-zero exit (no match) is `Ok(false)`, not an error.
fn pgrep_open(class: &str, path_env: &str) -> anyhow::Result<bool> {
    let pattern = match_pattern(class);
    let Some(bin) = resolve_tool("pgrep", path_env) else {
        return Err(anyhow::anyhow!("popup: pgrep not found on PATH"));
    };
    let output = retrying(|| {
        std::process::Command::new(&bin)
            .arg("-f")
            .arg(&pattern)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
    })
    .context("popup: failed to run pgrep")?;
    Ok(output.status.success())
}

/// Close popups of `class` (`pkill -f "<class> "`).
///
/// `# Errors`: when `pkill` is missing from `path_env` or cannot be
/// executed. The `pkill` exit status itself is ignored — a match may vanish
/// between the `pgrep` probe and this call, and that race is harmless.
fn pkill_class(class: &str, path_env: &str) -> anyhow::Result<()> {
    let pattern = match_pattern(class);
    let Some(bin) = resolve_tool("pkill", path_env) else {
        return Err(anyhow::anyhow!("popup: pkill not found on PATH"));
    };
    retrying(|| {
        std::process::Command::new(&bin)
            .arg("-f")
            .arg(&pattern)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    })
    .context("popup: failed to run pkill")?;
    Ok(())
}

/// Spawn `argv` detached: stdio nulled, never waited on.
///
/// This is the `</dev/null >/dev/null 2>&1 & disown` half of `popup.sh` —
/// the child outlives the toggler and no pipe stays open. Like `popup.sh`
/// the default keeps the child in the caller's process group. `session` puts
/// it in a new session through `setsid -f` instead, for callers that run
/// *inside* a pty: the notify drawer executes an action and exits, closing
/// its kitty pty, and a same-group child would take the hangup `SIGHUP`
/// before its window ever appeared.
///
/// `# Errors`: when `argv` is empty, the terminal (or `setsid`, for
/// `session`) binary is missing from `path_env`, or the spawn itself fails.
fn spawn_detached(argv: &[String], path_env: &str, session: bool) -> anyhow::Result<()> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(anyhow::anyhow!("popup: empty spawn argv"));
    };
    let bin = if program.contains('/') {
        PathBuf::from(program)
    } else {
        match resolve_tool(program, path_env) {
            Some(path) => path,
            None => {
                return Err(anyhow::anyhow!("popup: {program} not found on PATH"));
            }
        }
    };
    if session {
        let Some(setsid) = resolve_tool("setsid", path_env) else {
            return Err(anyhow::anyhow!("popup: setsid not found on PATH"));
        };
        let trailing: Vec<&str> = rest.iter().map(String::as_str).collect();
        crate::spawn::spawn_detached(&setsid, &bin, &trailing)
            .context("popup: failed to spawn terminal")?;
        return Ok(());
    }
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(rest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Reaped on a waiter thread (uniform with `spawn::spawn_and_reap`
    // callers): the toggler is short-lived so init would collect the child
    // anyway, but an explicit wait leaves no window for a stray zombie.
    crate::spawn::spawn_and_reap(&mut cmd).context("popup: failed to spawn terminal")?;
    Ok(())
}

/// Toggle implementation with an explicit `PATH` override (the test seam).
///
/// `None` inherits the ambient `PATH`; `Some` shadows it, so spawn-path
/// tests run stub `pgrep`/`pkill`/terminal scripts without touching the
/// process env or any live tool. Crate-visible so `exec::notify` can route
/// its own notification labels to a sibling flex popup with the same
/// toggle keying.
pub(crate) fn toggle_impl(
    kind: TerminalKind,
    class: &str,
    cmd: &[String],
    path_override: Option<&str>,
) -> anyhow::Result<()> {
    toggle_inner(kind, class, cmd, path_override, false)
}

/// [`toggle_impl`] that puts the new popup in its own session (`setsid -f`).
///
/// For callers running inside a pty that is about to close: the notify drawer
/// executes an "Open App" action and exits, and a popup left in its process
/// group would be killed by the hangup `SIGHUP` before its window appears.
pub(crate) fn toggle_detached(
    kind: TerminalKind,
    class: &str,
    cmd: &[String],
    path_override: Option<&str>,
) -> anyhow::Result<()> {
    toggle_inner(kind, class, cmd, path_override, true)
}

/// Shared toggle body; `session` selects the spawn mode (see
/// [`spawn_detached`]).
fn toggle_inner(
    kind: TerminalKind,
    class: &str,
    cmd: &[String],
    path_override: Option<&str>,
    session: bool,
) -> anyhow::Result<()> {
    let path_env = match path_override {
        Some(path) => path.to_string(),
        None => ambient_path(),
    };
    if class == DRAWER_CLASS {
        // The drawer supersedes the banner stack: close it and drop anything
        // still queued, so a reopen cannot replay a stale toast.
        crate::exec::toast::dismiss_all(&path_env);
    }
    if pgrep_open(class, &path_env)? {
        return pkill_class(class, &path_env);
    }
    spawn_detached(&terminal::spawn_argv(kind, class, cmd), &path_env, session)
}

/// Toggle the popup for `variant_class`, running `cmd` when opening.
///
/// `variant_class` accepts a `popup.sh` variant (`menu`, `menu-wide`) or an
/// already-resolved class (`flex-menu`, `flex-menu-wide`); anything else is
/// an error. When a popup of the variant class is open it is closed
/// (`pkill`, shared-variant keying: a `wifi` open closes on a `power`
/// toggle); otherwise `cmd` is spawned detached inside a new popup via the
/// detected terminal template (stdio nulled, never waited on). An empty
/// `cmd` is an error: `popup.sh` exits 1 on empty args, and parity must be
/// exact — there is no bare no-command constructor.
///
/// # Errors
///
/// When `cmd` is empty, the variant is unknown, `pgrep`/`pkill` or the
/// terminal binary is missing, or any of those spawns fails. Messages carry
/// no `flex:` prefix of their own; the [`runner`] reports them.
///
/// [`runner`]: crate::runner
pub fn toggle_with(variant_class: &str, cmd: &[String]) -> anyhow::Result<()> {
    if cmd.is_empty() {
        return Err(anyhow::anyhow!(
            "popup: expected a command to run inside the popup"
        ));
    }
    let Some(class) = class_for(variant_class) else {
        return Err(anyhow::anyhow!("popup: unknown variant {variant_class:?}"));
    };
    toggle_impl(terminal::detect(), class, cmd, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Providers per variant, mirroring
    /// [`Provider::variant`](crate::runner::Provider::variant): four share
    /// `menu`, three share `menu-wide`.
    const PROVIDER_VARIANTS: &[(&str, &str)] = &[
        ("power", "menu"),
        ("shot", "menu"),
        ("theme", "menu"),
        ("wifi", "menu"),
        ("launch", "menu-wide"),
        ("clip", "menu-wide"),
        ("wallpaper", "menu-wide"),
    ];

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flex-popup-{name}-{}", std::process::id()))
    }

    fn write_exe(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
    }

    /// Stub `PATH` dir: `pgrep` exits `pgrep_status`, `pkill` logs its args
    /// to `pkill.log`, `kitty` logs its args to `kitty.log`. Returns the dir
    /// plus a `PATH` value (stub dir prepended to ambient, so `env`/`bash`
    /// shebangs keep working) for the `path_override` seam.
    struct Stubs {
        dir: PathBuf,
        path_env: String,
        kitty_log: PathBuf,
        pkill_log: PathBuf,
    }

    fn install_stubs(name: &str, pgrep_status: u8) -> Stubs {
        let dir = scratch(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("stub dir");
        let kitty_log = dir.join("kitty.log");
        let pkill_log = dir.join("pkill.log");
        write_exe(
            &dir.join("pgrep"),
            &format!("#!/usr/bin/env bash\nexit {pgrep_status}\n"),
        );
        // Both logging stubs write to a temp file and `mv` it over the log,
        // so a reader never observes a half-written file: the spawn under
        // test is detached, and the test polls for the renamed file.
        write_exe(
            &dir.join("pkill"),
            &format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}.tmp'\nmv '{}.tmp' '{}'\n",
                pkill_log.display(),
                pkill_log.display(),
                pkill_log.display(),
            ),
        );
        write_exe(
            &dir.join("kitty"),
            &format!(
                "#!/usr/bin/env bash\nif [ \"$#\" -gt 0 ]; then\nprintf '%s\\n' \"$0\" \"$@\" > '{}.tmp'\nelse\nprintf '%s\\n' \"$0\" > '{}.tmp'\nfi\nmv '{}.tmp' '{}'\n",
                kitty_log.display(),
                kitty_log.display(),
                kitty_log.display(),
                kitty_log.display(),
            ),
        );
        let path_env = format!("{}:{}", dir.display(), ambient_path());
        Stubs {
            dir,
            path_env,
            kitty_log,
            pkill_log,
        }
    }

    fn wait_for_log(path: &Path) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(body) = std::fs::read_to_string(path) {
                return body.lines().map(ToString::to_string).collect();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stub log never appeared: {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn class_constants_are_the_new_names() {
        assert_eq!(MENU_CLASS, "flex-menu");
        assert_eq!(WIDE_CLASS, "flex-menu-wide");
        assert_eq!(DRAWER_CLASS, "flex-notify-center");
    }

    #[test]
    fn class_for_maps_variants_and_passes_classes_through() {
        assert_eq!(class_for("menu"), Some(MENU_CLASS));
        assert_eq!(class_for("menu-wide"), Some(WIDE_CLASS));
        assert_eq!(class_for("drawer"), Some(DRAWER_CLASS));
        assert_eq!(class_for(MENU_CLASS), Some(MENU_CLASS), "idempotent");
        assert_eq!(class_for(WIDE_CLASS), Some(WIDE_CLASS), "idempotent");
        assert_eq!(class_for(DRAWER_CLASS), Some(DRAWER_CLASS), "idempotent");
        assert_eq!(class_for(""), None);
        assert_eq!(class_for("kitty-menu"), None, "old names are gone");
        assert_eq!(class_for("bogus"), None);
    }

    #[test]
    fn providers_sharing_a_variant_share_one_toggle_class() {
        for (provider, variant) in PROVIDER_VARIANTS {
            let expected = if *variant == "menu" {
                MENU_CLASS
            } else {
                WIDE_CLASS
            };
            assert_eq!(
                class_for(variant),
                Some(expected),
                "{provider} shares its variant class",
            );
        }
        assert_ne!(MENU_CLASS, WIDE_CLASS, "variants stay distinct");
    }

    #[test]
    fn match_pattern_has_a_trailing_space_and_no_cross_collision() {
        let narrow = match_pattern(MENU_CLASS);
        let wide = match_pattern(WIDE_CLASS);
        assert_eq!(narrow, "flex-menu ");
        assert_eq!(wide, "flex-menu-wide ");
        assert!(
            !wide.contains(narrow.as_str()),
            "trailing space keeps menu/menu-wide apart: {wide:?} vs {narrow:?}",
        );
    }

    #[test]
    fn in_popup_predicate_matches_the_wrapper_guard() {
        assert!(in_popup_with(Some("1")));
        assert!(!in_popup_with(None), "unset is outside");
        assert!(!in_popup_with(Some("")), "empty is outside");
        assert!(!in_popup_with(Some("0")));
        assert!(!in_popup_with(Some("true")));
        assert!(!in_popup_with(Some(" 1")), "no trimming");
        assert!(!in_popup_with(Some("1 ")), "no trimming");
    }

    #[test]
    fn toggle_closes_the_shared_variant_without_spawning() {
        let stubs = install_stubs("close", 0);
        let cmd = vec!["flex".to_string(), "power".to_string()];
        toggle_impl(TerminalKind::Kitty, MENU_CLASS, &cmd, Some(&stubs.path_env))
            .expect("toggle close succeeds");
        assert_eq!(
            wait_for_log(&stubs.pkill_log),
            vec!["-f".to_string(), "flex-menu ".to_string()],
            "pkill probes the variant class with the trailing space",
        );
        assert!(
            !stubs.kitty_log.exists(),
            "an open popup closes instead of stacking",
        );
        let _ = std::fs::remove_dir_all(&stubs.dir);
    }

    #[test]
    fn toggle_closes_the_menu_variant_for_a_different_menu_provider() {
        let stubs = install_stubs("close-menu-other", 0);
        // `wifi` shares `menu` with the `power` popup that opened it; the
        // toggle keys on the variant, so it closes rather than stacks.
        let cmd = vec!["flex".to_string(), "wifi".to_string()];
        toggle_impl(TerminalKind::Kitty, MENU_CLASS, &cmd, Some(&stubs.path_env))
            .expect("toggle close succeeds");
        assert_eq!(
            wait_for_log(&stubs.pkill_log),
            vec!["-f".to_string(), "flex-menu ".to_string()],
            "pkill probes the shared menu class with the trailing space",
        );
        assert!(
            !stubs.kitty_log.exists(),
            "a different menu provider closes instead of stacking",
        );
        let _ = std::fs::remove_dir_all(&stubs.dir);
    }

    #[test]
    fn toggle_closes_the_wide_variant_for_a_different_wide_provider() {
        let stubs = install_stubs("close-wide-other", 0);
        // `wallpaper` shares `menu-wide` with the `launch` popup that opened it.
        let cmd = vec!["flex".to_string(), "wallpaper".to_string()];
        toggle_impl(TerminalKind::Kitty, WIDE_CLASS, &cmd, Some(&stubs.path_env))
            .expect("toggle close succeeds");
        assert_eq!(
            wait_for_log(&stubs.pkill_log),
            vec!["-f".to_string(), "flex-menu-wide ".to_string()],
            "pkill probes the shared wide class with the trailing space",
        );
        assert!(
            !stubs.kitty_log.exists(),
            "a different wide provider closes instead of stacking",
        );
        let _ = std::fs::remove_dir_all(&stubs.dir);
    }

    #[test]
    fn toggle_spawns_the_command_when_nothing_is_open() {
        let stubs = install_stubs("spawn", 1);
        let cmd = vec!["flex".to_string(), "power".to_string()];
        toggle_impl(TerminalKind::Kitty, MENU_CLASS, &cmd, Some(&stubs.path_env))
            .expect("toggle spawn succeeds");
        let kitty_bin = stubs.dir.join("kitty").display().to_string();
        let expected = [
            kitty_bin.as_str(),
            "--class",
            "flex-menu",
            "-o",
            "font_size=10",
            "-e",
            "env",
            "POPUP_KITTY=1",
            "flex",
            "power",
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            wait_for_log(&stubs.kitty_log),
            expected,
            "spawn argv is the kitty popup.sh template",
        );
        assert!(!stubs.pkill_log.exists(), "no close without an open popup");
        let _ = std::fs::remove_dir_all(&stubs.dir);
    }

    #[test]
    fn toggle_with_rejects_an_empty_cmd_like_popup_sh() {
        let result = toggle_with(MENU_CLASS, &[]);
        assert!(
            result.is_err(),
            "empty cmd errors like popup.sh (exits 1 on empty args)"
        );
    }

    #[test]
    fn toggle_fails_cleanly_when_tools_are_missing() {
        let dir = scratch("missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("empty stub dir");
        let path_env = dir.display().to_string();
        let result = toggle_impl(
            TerminalKind::Kitty,
            MENU_CLASS,
            &["flex".to_string()],
            Some(&path_env),
        );
        assert!(result.is_err(), "no pgrep on PATH must error, not spawn");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_with_rejects_unknown_variants_before_spawning() {
        let result = toggle_with("bogus", &["flex".to_string()]);
        assert!(
            result.is_err(),
            "unknown variant errors like popup.sh usage"
        );
    }
}
