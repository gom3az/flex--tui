//! Executors: one module per provider, porting the retired wrapper logic.
//!
//! The binaries stay thin (argv → shared runner → select → `exec::<provider>`)
//! while each `exec` module owns the side effects the matching shell wrapper
//! used to own. Integration tests reach the library
//! (`src/bin/*.rs` is not importable), so executors must live here for the
//! dispatch coverage to become Rust tests. Each module documents its
//! deliberate departures from the wrapper it ported.

pub mod bt;
pub mod clip;
pub mod launch;
pub mod mixer;
pub mod net;
pub mod notify;
pub mod power;
pub mod proc;
pub mod profile;
pub mod record;
pub mod shot;
pub mod speedtest;
pub mod theme;
pub mod toast;
pub mod wallpaper;
pub mod wifi;

/// When a notify step runs (the wrapper's `if connect; then …; else …`
/// verbatim: the open-connect plan notifies only on success, the
/// disconnect plan always, and the secure plan lists both the `Connected`
/// (success) and the `Failed` (failure) notifies of which exactly one runs).
///
/// Shared by the `wifi` and `bt` executors, so the enum lives here rather
/// than in either provider module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyWhen {
    /// Always (disconnect + `|| true` arms).
    Always,
    /// Only when the previous tool step exited `0`.
    Success,
    /// Only when the previous tool step failed.
    Failure,
}
