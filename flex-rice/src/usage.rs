//! `usage` store: the zoxide-style frecency database behind usage ranking.
//!
//! One tab-separated file (`$FLEX_USAGE_FILE`, else
//! `$HOME/.cache/flex/usage.tsv`) holds every provider's learned entries:
//! `provider\trank\tlast_accessed\trow_id`. The row lists themselves stay
//! stateless (fresh `.desktop`/theme/Wi-Fi scans every invocation); this
//! file is the only persistence, joined to rows by stable row id at rank
//! time (see `flex_core::filter::rank_all_scored`).
//!
//! Best-effort throughout: a missing store loads empty, malformed lines are
//! skipped, and write failures are reported to the caller — the runner
//! demotes them to a warning, so a broken cache never fails a launch.
//! Writes are atomic (`.tmp` + rename, like the popup test scripts).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use flex_core::filter::{self, UsageTable};

/// Env override for the usage file (mirrors `CLIPHIST_FILE`; empty = default).
pub const USAGE_ENV: &str = "FLEX_USAGE_FILE";

/// Whole-store map: provider name → that provider's usage table.
pub type AllUsage = HashMap<String, UsageTable>;

/// Non-empty env override, if set (mirrors `clip::env_override`).
fn env_override(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `$HOME` (empty when unset, matching the clip provider's fallback).
fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Store path: explicit override, then `$FLEX_USAGE_FILE`, else
/// `$HOME/.cache/flex/usage.tsv`.
///
/// `path_env` is the test seam (a direct file path, so tests never touch
/// process env); pass `None` in production.
#[must_use]
pub fn usage_path(path_env: Option<&str>) -> PathBuf {
    if let Some(path) = path_env.filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    env_override(USAGE_ENV).unwrap_or_else(|| home_dir().join(".cache/flex/usage.tsv"))
}

/// Load the whole store; missing/unreadable files and malformed lines yield
/// an empty (or partial) map, never an error.
#[must_use]
pub fn load_all(path_env: Option<&str>) -> AllUsage {
    let path = usage_path(path_env);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut all: AllUsage = HashMap::new();
    for line in text.lines() {
        // No `split().collect::<Vec>()` per line: destructure the four
        // fields directly (one pass, no alloc).
        let mut fields = line.split('\t');
        let (Some(provider), Some(rank), Some(last), Some(id)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(rank), Ok(last)) = (rank.parse::<f64>(), last.parse::<u64>()) else {
            continue;
        };
        if provider.is_empty() || id.is_empty() || !rank.is_finite() {
            continue;
        }
        all.entry(provider.to_owned()).or_default().insert(
            id.to_owned(),
            filter::UsageEntry {
                rank: rank.max(0.0),
                last_accessed: last,
            },
        );
    }
    all
}

/// This provider's slice of the store (empty when unrecorded).
#[must_use]
pub fn load_provider(all: &AllUsage, provider: &str) -> UsageTable {
    all.get(provider).cloned().unwrap_or_default()
}

/// Load only `provider`'s rows in one pass (popup-open fast path).
///
/// [`load_all`] + [`load_provider`] builds every provider's table then clones
/// one; this parses the same file but keeps only matching lines — no
/// `AllUsage` map and no clone. Malformed lines are skipped identically.
#[must_use]
pub fn load_one(provider: &str, path_env: Option<&str>) -> UsageTable {
    let path = usage_path(path_env);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut table = UsageTable::new();
    for line in text.lines() {
        // Split without `collect::<Vec>`: provider is the first field, so
        // non-matching lines skip parsing rank/last/id entirely.
        let mut fields = line.split('\t');
        let (Some(row_provider), Some(rank), Some(last), Some(id)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if row_provider != provider || row_provider.is_empty() || id.is_empty() {
            continue;
        }
        let (Ok(rank), Ok(last)) = (rank.parse::<f64>(), last.parse::<u64>()) else {
            continue;
        };
        if !rank.is_finite() {
            continue;
        }
        table.insert(
            id.to_owned(),
            filter::UsageEntry {
                rank: rank.max(0.0),
                last_accessed: last,
            },
        );
    }
    table
}

/// Save the whole store atomically (`.tmp` + rename), creating parent dirs.
///
/// # Errors
///
/// When the write or rename fails. The runner demotes this to a warning.
pub fn save_all(all: &AllUsage, path_env: Option<&str>) -> anyhow::Result<()> {
    let path = usage_path(path_env);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut providers: Vec<&String> = all.keys().collect();
    providers.sort();
    let mut text = String::new();
    for provider in providers {
        let Some(table) = all.get(provider) else {
            continue;
        };
        let mut ids: Vec<&String> = table.keys().collect();
        ids.sort();
        for id in ids {
            let Some(entry) = table.get(id) else {
                continue;
            };
            if provider.contains(['\t', '\n']) || id.contains(['\t', '\n']) {
                continue;
            }
            writeln!(
                text,
                "{provider}\t{}\t{}\t{id}",
                entry.rank, entry.last_accessed
            )
            .expect("appending to a String cannot fail");
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Wall-clock epoch seconds for recording (`run_standard_cli`).
///
/// Unreachable-in-practice fallback is `0` (pre-epoch clock).
#[must_use]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |age| age.as_secs())
}

/// Snapshot of one menu row for the record hook.
///
/// The runner flattens the menu's tabs before the event loop consumes them;
/// `searchable` carries the owning tab's `filterable && learnable` flags, so
/// fixed tabs and ephemeral lists neither learn nor reorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowSnap {
    /// The row's action id.
    pub id: String,
    /// Offline placeholder rows (`noop`) never learn.
    pub offline: bool,
    /// Whether the owning tab is filterable (has a search box).
    pub searchable: bool,
}

/// Record a choice: bump (`provider`, `action_id`) by one use at `now`.
///
/// `rows` is every row snapshot across the menu's tabs. No-op (still `Ok`)
/// when `action_id` names no live row, an offline placeholder, or a row in
/// a fixed (non-filterable) tab — choosing `noop` or a static action must
/// never learn a dead row to the top. Stale entries (ids absent from `rows`)
/// are pruned and the table aged before saving.
///
/// # Errors
///
/// When the store cannot be saved. The runner demotes this to a warning.
pub fn record_choice(
    provider: &str,
    rows: &[RowSnap],
    action_id: &str,
    path_env: Option<&str>,
    now: u64,
) -> anyhow::Result<()> {
    let live: HashSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    let learnable = rows
        .iter()
        .any(|row| row.id.as_str() == action_id && !row.offline && row.searchable);
    if !learnable {
        return Ok(());
    }
    let mut all = load_all(path_env);
    let table = all.entry(provider.to_owned()).or_default();
    table.retain(|id, _| live.contains(id.as_str()));
    filter::usage_record(table, action_id, now);
    filter::usage_age(table);
    save_all(&all, path_env)
}

/// Drop one learned entry (the `Delete`-outcome port of zoxide `remove`).
///
/// Saves only when an entry existed; a missing store or id is `Ok`.
///
/// # Errors
///
/// When the store cannot be saved. The runner demotes this to a warning.
pub fn remove_choice(
    provider: &str,
    action_id: &str,
    path_env: Option<&str>,
) -> anyhow::Result<()> {
    let mut all = load_all(path_env);
    let Some(table) = all.get_mut(provider) else {
        return Ok(());
    };
    if filter::usage_remove(table, action_id) {
        save_all(&all, path_env)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str) -> RowSnap {
        RowSnap {
            id: id.to_owned(),
            offline: false,
            searchable: true,
        }
    }

    fn fixed(id: &str) -> RowSnap {
        RowSnap {
            id: id.to_owned(),
            offline: false,
            searchable: false,
        }
    }

    fn temp_path() -> String {
        let dir = std::env::temp_dir().join(format!("flex-usage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join(format!("usage-{}.tsv", next_suffix()))
            .to_string_lossy()
            .into_owned()
    }

    // Nanosecond truncation to u64 is fine: this only needs uniqueness,
    // not the full timestamp.
    #[allow(clippy::cast_possible_truncation)]
    fn next_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Nanos add cross-process uniqueness on top of the pid dir.
        NEXT.fetch_add(1, Ordering::Relaxed).wrapping_add(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |age| age.as_nanos() as u64),
        )
    }

    use std::time::SystemTime;

    #[test]
    fn missing_store_loads_empty() {
        let path = temp_path();
        let all = load_all(Some(&path));
        assert!(all.is_empty(), "no file is not an error");
    }

    // Ranks compared here are exact binary fractions (small integers,
    // halves), so `==` via `assert_eq!` is sound.
    #[allow(clippy::float_cmp)]
    #[test]
    fn round_trip_preserves_entries() {
        let path = temp_path();
        let now = now_secs();
        let rows = vec![snap("abc")];
        record_choice("launch", &rows, "abc", Some(&path), now).expect("record saves");
        let all = load_all(Some(&path));
        let entry = &all["launch"]["abc"];
        assert_eq!(entry.rank, 1.0);
        assert_eq!(entry.last_accessed, now);
        // Second use accumulates rather than duplicating the line.
        record_choice("launch", &rows, "abc", Some(&path), now + 10).expect("record saves");
        let text = std::fs::read_to_string(&path).expect("store readable");
        assert_eq!(text.lines().count(), 1, "one line per entry");
        let all = load_all(Some(&path));
        assert_eq!(all["launch"]["abc"].rank, 2.0);
    }

    // Ranks compared here are exact binary fractions (small integers,
    // halves), so `==` via `assert_eq!` is sound.
    #[allow(clippy::float_cmp)]
    #[test]
    fn malformed_lines_are_skipped() {
        let path = temp_path();
        std::fs::write(
            &path,
            "launch\t1.5\t100\tabc\nbroken-line\nlaunch\tnan\t100\tbad\nlaunch\t1.0\tnotnum\tbad2\n\t1.0\t100\tnoid\nlaunch\t1.0\t100\t\nlaunch\t-3.0\t100\tneg\n",
        )
        .expect("fixture");
        let all = load_all(Some(&path));
        let table = load_provider(&all, "launch");
        assert_eq!(table.len(), 2, "only the two well-formed lines survive");
        assert_eq!(table["abc"].rank, 1.5);
        assert_eq!(table["neg"].rank, 0.0, "negative ranks clamp to zero");
    }

    #[test]
    fn record_ignores_unknown_offline_and_fixed_ids() {
        let path = temp_path();
        let now = now_secs();
        let rows = vec![
            snap("abc"),
            RowSnap {
                id: "noop".to_owned(),
                offline: true,
                searchable: true,
            },
            fixed("start"),
        ];
        record_choice("launch", &rows, "ghost", Some(&path), now).expect("unknown is Ok");
        record_choice("launch", &rows, "noop", Some(&path), now).expect("offline is Ok");
        record_choice("launch", &rows, "start", Some(&path), now).expect("fixed tab is Ok");
        assert!(
            !std::path::Path::new(&path).exists(),
            "no-ops write nothing"
        );
    }

    // Ranks compared here are exact binary fractions (small integers,
    // halves), so `==` via `assert_eq!` is sound.
    #[allow(clippy::float_cmp)]
    #[test]
    fn record_prunes_stale_entries() {
        let path = temp_path();
        let now = now_secs();
        std::fs::write(&path, "launch\t9.0\t100\tgone\nlaunch\t2.0\t100\tabc\n").expect("fixture");
        let rows = vec![snap("abc")];
        record_choice("launch", &rows, "abc", Some(&path), now).expect("record saves");
        let all = load_all(Some(&path));
        assert!(!all["launch"].contains_key("gone"), "stale ids are pruned");
        assert_eq!(all["launch"]["abc"].rank, 3.0);
    }

    #[test]
    fn remove_drops_only_the_named_entry() {
        let path = temp_path();
        let now = now_secs();
        let rows = vec![snap("abc"), snap("def")];
        record_choice("launch", &rows, "abc", Some(&path), now).expect("record");
        record_choice("launch", &rows, "def", Some(&path), now).expect("record");
        remove_choice("launch", "abc", Some(&path)).expect("remove saves");
        let all = load_all(Some(&path));
        assert!(!all["launch"].contains_key("abc"));
        assert!(all["launch"].contains_key("def"));
        remove_choice("launch", "ghost", Some(&path)).expect("missing id is Ok");
    }

    #[test]
    fn explicit_path_beats_env_override() {
        let direct = temp_path();
        let via_env = temp_path();
        std::env::set_var(USAGE_ENV, &via_env);
        let now = now_secs();
        let rows = vec![snap("abc")];
        record_choice("launch", &rows, "abc", Some(&direct), now).expect("record");
        assert!(std::path::Path::new(&direct).exists());
        assert!(
            !std::path::Path::new(&via_env).exists(),
            "env ignored when explicit"
        );
        std::env::remove_var(USAGE_ENV);
        // Env alone is honored when no explicit path is given.
        std::env::set_var(USAGE_ENV, &via_env);
        assert_eq!(usage_path(None), std::path::PathBuf::from(&via_env));
        std::env::remove_var(USAGE_ENV);
    }
}
