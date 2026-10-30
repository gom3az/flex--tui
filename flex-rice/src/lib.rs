//! `flex-rice` — the rice-specific half of flex.
//!
//! The eight providers here adapt this machine's tools to the generic
//! [`flex_core`] menu engine: hyprpaper (wallpaper), cliphist (clip),
//! nmcli (wifi), bluetoothctl/wpctl (bt), the theme directories
//! (theme), `.desktop` entries (launch), `grim`/`slurp` (shot) and
//! systemctl (power). The `flex` dispatcher (`src/main.rs`) parses the
//! provider names and re-execs the matching `flex-<provider>` binary, which
//! renders one menu and executes the selected row in-process (`exec/*.rs`).
//!
//! Nothing in [`flex_core`] depends on this crate — the dependency runs one
//! way, so the engine stays publishable and this crate stays local.

pub mod command;
pub mod exec;
pub mod palette;
pub mod popup;
pub mod providers;
pub mod runner;
mod spawn;
pub mod terminal;
pub mod tools;
pub mod usage;

pub use providers::{menu, tick_hook};
