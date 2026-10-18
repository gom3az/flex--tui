//! Event loop: `ratatui::init` on `/dev/tty`, poll → dispatch → render.
//!
//! Frame cadence mirrors `Docs/UI_UX_doc.md`: every frame sizes exactly once
//! (inside [`render::render`]), crossterm events dispatch through
//! [`keys::handle_key`] with an injected `Instant`, and the 1 s
//! [`backend::poll_timeout`] expiry drives [`Menu::tick`] (gauge cadence +
//! danger-arm expiry). Resizes need no explicit handling: the next frame
//! re-sizes and re-renders.
//!
//! Outcomes: `Select` prints one `ACTION:` line and exits `0`; `Delete`
//! prints one `ACTION:DELETE` line and exits `0` (launch/power/shot/theme
//! tabs leave `Tab::deletable` false so this never fires there); `Toggle`
//! prints one `ACTION:TOGGLE` line and exits `0` (clip NAVIGATE `m`, the
//! wrapper flips the pin); `Quit(code)` exits with `code` and prints
//! nothing. Diagnostics go to stderr.
//!
//! `FLEX_TEST=1` replays: the loop uses a fixed base plus [`FLEX_TEST_STEP`]
//! per event instead of the wall clock, so scripted runs are deterministic.
//! [`replay_keys`] is the test-facing form of the same dispatch (no TTY).
//!
//! Image previews (`menu.preview`): after every frame the reserved pane is
//! handed to [`preview::Preview`], which paints the focused row's image with
//! kitty graphics escapes and re-parks the caret. The pane is the only part
//! of the frame that is drawn outside the ratatui buffer.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{Event, KeyEvent};
use futures::stream::StreamExt;

use crate::backend;
use crate::keys::{self, KeyOutcome};
use crate::preview;
use crate::render;
use crate::{Menu, Outcome};

/// Deterministic per-event step for `FLEX_TEST` replays.
///
/// Derived from [`backend::FLEX_TEST_SEED`] so a seed change visibly retimes
/// replays; the value stays above [`keys::ARM_CONFIRM_DELAY`] so a scripted
/// double-`Enter` still confirms danger rows.
pub const FLEX_TEST_STEP: Duration = Duration::from_millis(backend::FLEX_TEST_SEED % 100 + 50);

/// Fixed-base clock for deterministic tests: one [`Instant`] per replay,
/// event `i` stamped at `base + i * FLEX_TEST_STEP`.
#[must_use]
pub fn test_base() -> Instant {
    Instant::now()
}

/// Feed synthetic keys through [`keys::handle_key`] deterministically.
///
/// Stamps are `base + i * FLEX_TEST_STEP` (no sleeps, no clock reads).
/// Returns the first terminal outcome (`Select`/`Delete`/`Toggle`/`Quit`);
/// returns `Consumed` when the script never terminates (caller asserts
/// state).
#[must_use]
pub fn replay_keys(menu: &mut Menu, keys: &[KeyEvent], base: Instant) -> KeyOutcome {
    for (index, key) in keys.iter().enumerate() {
        // Scripts are short; the index never approaches `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let step = index as u32;
        let outcome = keys::handle_key(menu, *key, base + FLEX_TEST_STEP * step);
        if outcome != KeyOutcome::Consumed {
            return outcome;
        }
    }
    KeyOutcome::Consumed
}

/// Run the interactive menu in-process until it selects, deletes, toggles,
/// chooses a target, quits, or errors.
///
/// Shares the [`run`] event loop but stays execute-in-process: on terminal
/// outcomes it calls [`backend::restore`], prints nothing, exits nowhere, and
/// returns the corresponding [`Outcome`]. Focused-row reads are identical to
/// [`run`] (a missing row continues the loop).
///
/// # Errors
///
/// Returns an error when the terminal cannot initialize, frames cannot
/// draw, or events cannot be polled/read.
#[allow(clippy::too_many_lines)]
pub async fn run_capture(mut menu: Menu) -> Result<Outcome> {
    let provider = menu.provider.clone();
    let mut terminal = backend::init()?;
    let base = Instant::now();
    let seeded = backend::is_flex_test();
    let mut step: u32 = 0;
    let mut images = menu.preview.then(preview::Preview::new);
    let mut preview_tty = if menu.preview {
        backend::open_tty().ok()
    } else {
        None
    };

    let notify = menu.notifier();
    let mut reader = crossterm::event::EventStream::new();
    // Reused tick timer: the old `sleep(poll_timeout())` built a fresh
    // future every frame; an `Interval` reuses the timer registration.
    let mut ticker = tokio::time::interval(backend::poll_timeout());
    // The first tick fires immediately on `interval`; skip it so cadence
    // matches the old sleep-per-loop behavior (tick after 1s, not at open).
    ticker.tick().await;
    loop {
        let mut area = ratatui::layout::Rect::default();
        terminal
            .draw(|frame| {
                area = frame.area();
                render::render(frame, &mut menu);
            })
            .context("failed to draw frame")?;
        if let (Some(images), Some(tty)) = (images.as_mut(), preview_tty.as_mut()) {
            let pane = render::preview_area(area, &menu);
            // Borrow the preview path: the old `.clone()` allocated a
            // `String` every frame even when focus never moved.
            let source = menu
                .app
                .focused_row()
                .and_then(|row| row.preview_image.as_deref());
            let park = render::cursor_position(area, &menu);
            let _ = images.sync(tty, pane, source.map(std::path::Path::new), park);
        }

        tokio::select! {
            event = reader.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        let now = loop_now(base, &mut step, seeded);
                        match keys::handle_key(&mut menu, key, now) {
                            KeyOutcome::Consumed => {}
                            KeyOutcome::Select => {
                                let Some(row) = menu.app.focused_row() else {
                                    continue;
                                };
                                let (id, label) = (row.id.as_str().to_string(), row.label.clone());
                                backend::restore();
                                return Ok(Outcome::Chosen {
                                    provider: provider.clone(),
                                    action_id: id,
                                    label,
                                });
                            }
                            KeyOutcome::Delete => {
                                let Some(row) = menu.app.focused_row() else {
                                    continue;
                                };
                                let (id, label) = (row.id.as_str().to_string(), row.label.clone());
                                backend::restore();
                                return Ok(Outcome::Delete {
                                    provider: provider.clone(),
                                    action_id: id,
                                    label,
                                });
                            }
                            KeyOutcome::Toggle => {
                                let Some(row) = menu.app.focused_row() else {
                                    continue;
                                };
                                let (id, label) = (row.id.as_str().to_string(), row.label.clone());
                                backend::restore();
                                return Ok(Outcome::Toggle {
                                    provider: provider.clone(),
                                    action_id: id,
                                    label,
                                });
                            }
                            KeyOutcome::ChooseTarget { row, target, title } => {
                                backend::restore();
                                return Ok(Outcome::Target {
                                    provider: provider.clone(),
                                    row: row.as_str().to_string(),
                                    target: target.as_str().to_string(),
                                    title,
                                });
                            }
                            KeyOutcome::Quit(code) => {
                                backend::restore();
                                return Ok(Outcome::Quit { code });
                            }
                        }
                    }
                    Some(Ok(Event::Resize(_, _)
                    | Event::FocusGained
                    | Event::FocusLost
                    | Event::Mouse(_)
                    | Event::Paste(_))) => {}
                    Some(Err(err)) => return Err(err).context("event stream failed"),
                    None => break,
                }
            }
            () = notify.notified() => {
                menu.tick(loop_now(base, &mut step, seeded));
            }
            _ = ticker.tick() => {
                menu.tick(loop_now(base, &mut step, seeded));
            }
        }
    }

    Ok(Outcome::Cancelled)
}

/// Loop clock: seeded fixed-step in `FLEX_TEST` mode, wall clock otherwise.
fn loop_now(base: Instant, step: &mut u32, seeded: bool) -> Instant {
    if seeded {
        let now = base + FLEX_TEST_STEP * *step;
        *step = step.saturating_add(1);
        now
    } else {
        Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flex_test_step_confirms_confirmable_in_replays() {
        assert!(FLEX_TEST_STEP >= keys::ARM_CONFIRM_DELAY);
        assert!(FLEX_TEST_STEP < keys::ARM_EXPIRE);
    }

    #[test]
    fn chosen_outcome_holds_select_fields() {
        let outcome = Outcome::Chosen {
            provider: String::from("power"),
            action_id: String::from("id-1"),
            label: String::from("Shutdown"),
        };
        assert_eq!(
            outcome,
            Outcome::Chosen {
                provider: String::from("power"),
                action_id: String::from("id-1"),
                label: String::from("Shutdown"),
            }
        );
    }

    #[test]
    fn delete_outcome_holds_delete_fields() {
        let outcome = Outcome::Delete {
            provider: String::from("clip"),
            action_id: String::from("abc123"),
            label: String::from("paste me"),
        };
        let Outcome::Delete {
            provider,
            action_id,
            label,
        } = outcome
        else {
            panic!("expected Outcome::Delete");
        };
        assert_eq!(provider, "clip");
        assert_eq!(action_id, "abc123");
        assert_eq!(label, "paste me");
    }

    #[test]
    fn toggle_outcome_holds_toggle_fields() {
        let outcome = Outcome::Toggle {
            provider: String::from("clip"),
            action_id: String::from("abc123"),
            label: String::from("paste me"),
        };
        let Outcome::Toggle {
            provider,
            action_id,
            label,
        } = outcome
        else {
            panic!("expected Outcome::Toggle");
        };
        assert_eq!(provider, "clip");
        assert_eq!(action_id, "abc123");
        assert_eq!(label, "paste me");
    }

    #[test]
    fn target_outcome_holds_target_fields() {
        let outcome = Outcome::Target {
            provider: String::from("audio"),
            row: String::from("vol"),
            target: String::from("t-hdmi"),
            title: String::from("HDMI"),
        };
        let Outcome::Target {
            provider,
            row,
            target,
            title,
        } = outcome
        else {
            panic!("expected Outcome::Target");
        };
        assert_eq!(provider, "audio");
        assert_eq!(row, "vol");
        assert_eq!(target, "t-hdmi");
        assert_eq!(title, "HDMI");
    }

    #[test]
    fn quit_outcome_holds_exit_code() {
        assert_eq!(Outcome::Quit { code: 130 }, Outcome::Quit { code: 130 });
        assert_ne!(Outcome::Quit { code: 0 }, Outcome::Quit { code: 1 });
        assert_ne!(Outcome::Quit { code: 130 }, Outcome::Cancelled);
    }
}
