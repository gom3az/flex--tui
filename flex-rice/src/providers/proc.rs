//! Process provider: a native, filterable process list for `flex proc`.
//!
//! Replaces the htop-based `kill-menu.sh`: rows come straight from `/proc`
//! (std-only, no crates), refresh once per engine tick with focus preserved
//! by pid, and the executor signals the selected pid.
//!
//! - Row `label` is `"<comm> <pid>"`, so the engine's label fuzzy-filter
//!   matches either the process name or the pid out of the box.
//! - Row `meta` is `"<cpu%> [bar] <mem> <user>"` with a wifi-style CPU text
//!   gauge (`12.3% [█░░░░░░░]`); `confirmable` arms the danger flow
//!   (Enter → SIGTERM, Delete → SIGKILL).
//! - CPU% is the delta of `utime+stime` over the delta of total system
//!   jiffies since the previous scan (per-core, so a busy multi-threaded
//!   process can read >100% like `top`); the first scan reports `0.0`.
//!   The printed value is always exact; only the bar input clamps to
//!   0–100 (a 312% reader saturates its gauge, never its number).
//! - Service-group rows set `hide_target_in_header` so the aggregate
//!   totals + gauge own the header (member targets stay in the dropdown),
//!   like the `bt` device rows.
//! - Kernel threads (empty `/proc/<pid>/cmdline`) are hidden unless
//!   `FLEX_PROC_KTHREADS` is set non-empty/non-`0`.
//!
//! Everything is best-effort and panic-free: a `/proc` entry that vanishes
//! mid-scan is skipped, and unparsable fields degrade to `0`/`?`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use flex_core::{Menu, Row, RowId, Tab, Target};

/// Provider name for the `ACTION:` line.
pub const PROVIDER: &str = "proc";
/// Tab title.
pub const TAB_NAME: &str = "Processes";
/// Env: also show kernel threads (empty cmdline).
const KTHREADS_ENV: &str = "FLEX_PROC_KTHREADS";
/// `procfs` mount point.
const PROC_ROOT: &str = "/proc";

/// Sort order for process rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    /// Sort by memory consumption (RSS) descending (default).
    #[default]
    Mem,
    /// Sort by CPU% delta descending.
    Cpu,
    /// Sort by PID ascending.
    Pid,
    /// Sort by process/service name ascending.
    Name,
}

impl SortBy {
    /// Parse from string (CLI or env).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "mem" | "memory" | "rss" => Some(Self::Mem),
            "cpu" => Some(Self::Cpu),
            "pid" => Some(Self::Pid),
            "name" | "comm" => Some(Self::Name),
            _ => None,
        }
    }
}

/// The active sort order (`FLEX_PROC_SORT`, defaulting to memory).
#[must_use]
pub fn sort_order() -> SortBy {
    std::env::var("FLEX_PROC_SORT")
        .ok()
        .and_then(|v| SortBy::parse(&v))
        .unwrap_or_default()
}

/// Format memory in KB to human-readable string (e.g. `512K`, `12.5M`, `1.4G`).
#[must_use]
pub fn format_memory_kb(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        #[allow(clippy::cast_precision_loss)]
        let gb = kb as f64 / (1024.0 * 1024.0);
        format!("{gb:.1}G")
    } else if kb >= 1024 {
        #[allow(clippy::cast_precision_loss)]
        let mb = kb as f64 / 1024.0;
        format!("{mb:.1}M")
    } else {
        format!("{kb}K")
    }
}

/// Expanded services state (survives refreshes).
fn expanded_services() -> &'static Mutex<HashSet<String>> {
    static EXPANDED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    EXPANDED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Toggle expansion of `service`.
pub fn toggle_service_expanded(service: &str) {
    let mut guard = match expanded_services().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.contains(service) {
        guard.remove(service);
    } else {
        guard.insert(service.to_string());
    }
}

/// Whether `service` is currently expanded.
#[must_use]
pub fn is_service_expanded(service: &str) -> bool {
    if std::env::var("FLEX_PROC_EXPAND").is_ok_and(|v| v == "all" || v == "1") {
        return true;
    }
    let guard = match expanded_services().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.contains(service)
}

/// PIDs belonging to `service`.
#[must_use]
pub fn service_pids(service: &str) -> Vec<u32> {
    let mut pids_out = Vec::new();
    for pid in pids() {
        if read_service(pid).as_deref() == Some(service) {
            pids_out.push(pid);
        }
    }
    pids_out
}

/// Previous jiffy counters for one pid (for the CPU delta).
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// `utime + stime` at the previous scan.
    jiffies: u64,
    /// Total system jiffies at the previous scan.
    total: u64,
}

/// Per-pid CPU sampling state (survives ticks; guarded against poisoning).
fn samples() -> &'static Mutex<HashMap<u32, Sample>> {
    static STATE: OnceLock<Mutex<HashMap<u32, Sample>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// uid → username, read once from `/etc/passwd`.
fn users() -> &'static HashMap<u32, String> {
    static USERS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    USERS.get_or_init(load_passwd)
}

/// Build the `Processes` tab: visible search bar, deletable (Delete = SIGKILL).
#[must_use]
pub fn proc_tab() -> Tab {
    let mut tab = Tab::with_rows(TAB_NAME, scan());
    tab.bare_rows = false;
    tab.filterable = true;
    tab.deletable = true;
    tab
}

#[derive(Debug, Clone)]
struct RawProc {
    pid: u32,
    comm: String,
    cpu: f32,
    rss_kb: u64,
    user: String,
    service: Option<String>,
}

enum ItemGroup {
    Single(RawProc),
    Service {
        name: String,
        procs: Vec<RawProc>,
        total_cpu: f32,
        total_rss_kb: u64,
        user: String,
    },
}

impl ItemGroup {
    fn total_rss_kb(&self) -> u64 {
        match self {
            Self::Single(p) => p.rss_kb,
            Self::Service { total_rss_kb, .. } => *total_rss_kb,
        }
    }

    fn total_cpu(&self) -> f32 {
        match self {
            Self::Single(p) => p.cpu,
            Self::Service { total_cpu, .. } => *total_cpu,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Single(p) => &p.comm,
            Self::Service { name, .. } => name,
        }
    }

    fn primary_pid(&self) -> u32 {
        match self {
            Self::Single(p) => p.pid,
            Self::Service { procs, .. } => procs.first().map_or(0, |p| p.pid),
        }
    }
}

fn sort_procs(procs: &mut [RawProc], sort: SortBy) {
    // OPT-8: fold each name once instead of `to_lowercase` per compare.
    if sort == SortBy::Name {
        procs.sort_by_cached_key(|p| (p.comm.to_lowercase(), std::cmp::Reverse(p.rss_kb)));
        return;
    }
    procs.sort_by(|a, b| match sort {
        SortBy::Mem => b
            .rss_kb
            .cmp(&a.rss_kb)
            .then_with(|| {
                b.cpu
                    .partial_cmp(&a.cpu)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.pid.cmp(&b.pid)),
        SortBy::Cpu => b
            .cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.rss_kb.cmp(&a.rss_kb))
            .then_with(|| a.pid.cmp(&b.pid)),
        SortBy::Pid => a.pid.cmp(&b.pid),
        // `Name` returns early via `sort_by_cached_key` above.
        SortBy::Name => std::cmp::Ordering::Equal,
    });
}

/// Collect raw process entries from `/proc`.
fn collect_processes(
    samples: &HashMap<u32, Sample>,
    total: u64,
    show_kthreads: bool,
    users: &HashMap<u32, String>,
) -> (Vec<RawProc>, HashMap<u32, Sample>) {
    let mut fresh = HashMap::with_capacity(samples.len());
    let mut collected = Vec::new();
    for pid in pids() {
        let Some((comm, utime, stime)) = read_stat(pid) else {
            continue;
        };
        let jiffies = utime.saturating_add(stime);
        let cpu = samples
            .get(&pid)
            .map_or(0.0, |prev| cpu_percent(prev, jiffies, total));
        fresh.insert(pid, Sample { jiffies, total });
        if !show_kthreads && !is_userspace(pid) {
            continue;
        }
        let (uid, rss_kb) = read_status(pid).unwrap_or((0, 0));
        let user = users.get(&uid).cloned().unwrap_or_else(|| uid.to_string());
        let service = read_service(pid);
        collected.push(RawProc {
            pid,
            comm,
            cpu,
            rss_kb,
            user,
            service,
        });
    }
    (collected, fresh)
}

/// Group processes by systemd service when they have multiple members.
fn group_processes(collected: Vec<RawProc>) -> Vec<ItemGroup> {
    let mut by_service: HashMap<String, Vec<RawProc>> = HashMap::new();
    let mut singles: Vec<RawProc> = Vec::new();

    for mut proc_item in collected {
        if let Some(svc) = proc_item.service.take() {
            by_service.entry(svc).or_default().push(proc_item);
        } else {
            singles.push(proc_item);
        }
    }

    let mut groups: Vec<ItemGroup> = Vec::new();
    for (name, procs) in by_service {
        if procs.len() > 1 {
            let total_cpu: f32 = procs.iter().map(|p| p.cpu).sum();
            let total_rss_kb: u64 = procs.iter().map(|p| p.rss_kb).sum();
            let user = procs
                .first()
                .map_or_else(|| String::from("?"), |p| p.user.clone());
            groups.push(ItemGroup::Service {
                name,
                procs,
                total_cpu,
                total_rss_kb,
                user,
            });
        } else {
            for p in procs {
                groups.push(ItemGroup::Single(p));
            }
        }
    }
    for p in singles {
        groups.push(ItemGroup::Single(p));
    }
    groups
}

use std::fmt::Write as _;

fn format_comm_pid(comm: &str, pid: u32) -> String {
    let mut s = String::with_capacity(comm.len() + 1 + 10);
    s.push_str(comm);
    s.push(' ');
    let _ = write!(s, "{pid}");
    s
}

fn format_target_label(comm: &str, pid: u32, mem_str: &str) -> String {
    let mut s = String::with_capacity(comm.len() + 1 + 10 + 3 + mem_str.len());
    s.push_str(comm);
    s.push(' ');
    let _ = write!(s, "{pid} ({mem_str})");
    s
}

fn format_child_label(prefix: &str, comm: &str, pid: u32) -> String {
    let mut s = String::with_capacity(prefix.len() + comm.len() + 1 + 10);
    s.push_str(prefix);
    s.push_str(comm);
    s.push(' ');
    let _ = write!(s, "{pid}");
    s
}

/// CPU text gauge for a compact row meta (`12.3% [█░░░░░░░]`), wifi-style.
///
/// The printed percentage is always the exact `cpu` value; only the bar
/// input clamps to 0–100, since per-core CPU can read >100% like `top`.
fn cpu_gauge(cpu: f32) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let pct = cpu.round().clamp(0.0, 100.0) as i64;
    format!("{cpu:>5.1}% {}", super::wifi::bar(pct, 100, 8))
}

/// Build rows for a single item group.
fn push_group_rows(group: ItemGroup, sort: SortBy, rows: &mut Vec<Row>) {
    match group {
        ItemGroup::Single(p) => {
            let mem_str = format_memory_kb(p.rss_kb);
            let label = format_comm_pid(&p.comm, p.pid);
            let meta = format!("{} {mem_str:>7} {}", cpu_gauge(p.cpu), p.user);
            let mut row = Row::with_meta(RowId::new(p.pid.to_string()), label, meta);
            row.confirmable = true;
            rows.push(row);
        }
        ItemGroup::Service {
            name,
            mut procs,
            total_cpu,
            total_rss_kb,
            user,
        } => {
            let count = procs.len();
            let mem_str = format_memory_kb(total_rss_kb);
            let meta = format!("{} {mem_str:>7} {user}", cpu_gauge(total_cpu));
            let targets: Vec<Target> = procs
                .iter()
                .map(|p| {
                    let p_mem = format_memory_kb(p.rss_kb);
                    Target::new(
                        RowId::new(p.pid.to_string()),
                        format_target_label(&p.comm, p.pid, &p_mem),
                    )
                })
                .collect();

            let expanded = is_service_expanded(&name);
            let label = if expanded {
                format!("▼ {name} ({count})")
            } else {
                format!("▶ {name} ({count})")
            };
            let mut row =
                Row::with_targets(RowId::new(format!("service:{name}")), label, targets, 0);
            row.meta = Some(meta);
            // Aggregate totals + gauge own the header (member targets stay
            // in the dropdown), like the `bt` device rows.
            row.hide_target_in_header = true;
            row.confirmable = true;
            rows.push(row);

            if expanded {
                sort_procs(&mut procs, sort);
                for (i, p) in procs.iter().enumerate() {
                    let p_mem = format_memory_kb(p.rss_kb);
                    let p_meta = format!("{} {p_mem:>7} {}", cpu_gauge(p.cpu), p.user);
                    let prefix = if i + 1 == count {
                        "  └─ "
                    } else {
                        "  ├─ "
                    };
                    let p_label = format_child_label(prefix, &p.comm, p.pid);
                    let mut child = Row::with_meta(RowId::new(p.pid.to_string()), p_label, p_meta);
                    child.confirmable = true;
                    rows.push(child);
                }
            }
        }
    }
}

/// One `/proc` sweep into sorted rows.
#[must_use]
pub fn scan() -> Vec<Row> {
    let total = read_total_jiffies().unwrap_or(0);
    let show_kthreads = kthreads_enabled();
    let users = users();
    let mut samples = match samples().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    let (collected, fresh) = collect_processes(&samples, total, show_kthreads, users);
    *samples = fresh;

    let mut groups = group_processes(collected);

    let sort = sort_order();
    // OPT-8: fold group names once for the `Name` order instead of
    // `to_lowercase` per comparison.
    if sort == SortBy::Name {
        groups
            .sort_by_cached_key(|g| (g.name().to_lowercase(), std::cmp::Reverse(g.total_rss_kb())));
    } else {
        groups.sort_by(|a, b| match sort {
            SortBy::Mem => b
                .total_rss_kb()
                .cmp(&a.total_rss_kb())
                .then_with(|| {
                    b.total_cpu()
                        .partial_cmp(&a.total_cpu())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.primary_pid().cmp(&b.primary_pid())),
            SortBy::Cpu => b
                .total_cpu()
                .partial_cmp(&a.total_cpu())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.total_rss_kb().cmp(&a.total_rss_kb()))
                .then_with(|| a.primary_pid().cmp(&b.primary_pid())),
            SortBy::Pid => a.primary_pid().cmp(&b.primary_pid()),
            SortBy::Name => std::cmp::Ordering::Equal,
        });
    }

    let mut rows = Vec::new();
    for group in groups {
        push_group_rows(group, sort, &mut rows);
    }

    rows
}

/// Per-tick refresh: rebuild the rows and keep the cursor on the same pid
/// (matched by row id through the filter, like the Wi-Fi scan).
///
/// OPT-6: skips unless the `Processes` tab is active. OPT-9: reuses existing
/// row allocations via [`super::sync_rows_in_place`] and restores focus in a
/// single pass, so filter/scroll survive the tick.
pub fn refresh(menu: &mut Menu) {
    let active = menu
        .app
        .active_tab()
        .is_some_and(|tab| tab.name == TAB_NAME);
    if !active {
        return;
    }
    let previous = menu.app.focused_row().map(|row| row.id.clone());
    let fresh = scan();
    let Some(tab) = menu.app.tabs.iter_mut().find(|tab| tab.name == TAB_NAME) else {
        return;
    };
    super::sync_rows_in_place(&mut tab.rows, fresh);
    super::restore_focus(menu, previous);
}

/// The process state character (`R`/`S`/`T`/`Z`/…) for `pid`, if readable.
#[must_use]
pub fn process_state(pid: u32) -> Option<char> {
    let data = std::fs::read_to_string(proc_path(pid, "stat")).ok()?;
    let rest = stat_after_comm(&data)?;
    rest.split_whitespace().next()?.chars().next()
}

/// CPU% from the jiffy deltas; `0.0` when there is no baseline.
///
/// `#[allow]`: a display percentage does not need `u64`-exact precision.
#[allow(clippy::cast_precision_loss)]
fn cpu_percent(prev: &Sample, jiffies: u64, total: u64) -> f32 {
    let delta_proc = jiffies.saturating_sub(prev.jiffies);
    let delta_total = total.saturating_sub(prev.total);
    if delta_total == 0 {
        0.0
    } else {
        100.0 * delta_proc as f32 / delta_total as f32
    }
}

/// Numeric `/proc` entries (unreadable/unparseable names skipped).
fn pids() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(PROC_ROOT) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        // Borrow the file name: the old `into_string()` allocated an owned
        // `String` per entry. Numeric names are short ASCII, so `to_str` +
        // a first-byte digit guard skips `dot`/`sys`/`self` without parsing.
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            continue;
        }
        if let Ok(pid) = name.parse::<u32>() {
            out.push(pid);
        }
    }
    out
}

/// `/proc/<pid>/<file>`.
fn proc_path(pid: u32, file: &str) -> PathBuf {
    PathBuf::from(PROC_ROOT).join(pid.to_string()).join(file)
}

/// `(comm, utime, stime)` from `/proc/<pid>/stat`, parsing `comm` between the
/// first `(` and the **last** `)` (comm may contain spaces/parens).
fn read_stat(pid: u32) -> Option<(String, u64, u64)> {
    let data = std::fs::read_to_string(proc_path(pid, "stat")).ok()?;
    parse_stat(&data)
}

/// [`read_stat`] over raw `stat` text.
fn parse_stat(data: &str) -> Option<(String, u64, u64)> {
    let open = data.find('(')?;
    let close = data.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = data.get(open + 1..close)?.to_string();
    // Reuse `close`: the old path re-ran `rfind(')')` inside
    // `stat_after_comm`. The numeric tail starts after `") "`.
    let rest = data.get(close + 2..).or_else(|| data.get(close + 1..))?;
    let mut fields = rest.split_whitespace();
    // state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    for _ in 0..11 {
        fields.next()?;
    }
    let utime = fields.next()?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    Some((comm, utime, stime))
}

/// The `stat` text after `") "` (the comm terminator), for field splitting.
fn stat_after_comm(data: &str) -> Option<&str> {
    let close = data.rfind(')')?;
    data.get(close + 1..)
}

/// `(uid, rss_kb)` from `/proc/<pid>/status` (missing fields → `0`).
fn read_status(pid: u32) -> Option<(u32, u64)> {
    let data = std::fs::read_to_string(proc_path(pid, "status")).ok()?;
    Some(parse_status(&data))
}

/// [`read_status`] over raw `status` text.
fn parse_status(data: &str) -> (u32, u64) {
    let mut uid = 0_u32;
    let mut rss_kb = 0_u64;
    let mut have_uid = false;
    let mut have_rss = false;
    for line in data.lines() {
        if let Some(value) = line.strip_prefix("Uid:") {
            if let Some(first) = value.split_whitespace().next() {
                uid = first.parse().unwrap_or(0);
                have_uid = true;
            }
        } else if let Some(value) = line.strip_prefix("VmRSS:") {
            if let Some(first) = value.split_whitespace().next() {
                rss_kb = first.parse().unwrap_or(0);
                have_rss = true;
            }
        }
        // `status` files are ~50 lines; both fields sit near the top, so
        // stop scanning once both are found instead of walking the tail.
        if have_uid && have_rss {
            break;
        }
    }
    (uid, rss_kb)
}

/// Whether `pid` has a non-empty `cmdline` (a user-space process; kernel
/// threads have none).
fn is_userspace(pid: u32) -> bool {
    std::fs::read(proc_path(pid, "cmdline")).is_ok_and(|bytes| !bytes.is_empty())
}

/// Read the systemd `.service` name from `/proc/<pid>/cgroup` if present.
fn read_service(pid: u32) -> Option<String> {
    let data = std::fs::read_to_string(proc_path(pid, "cgroup")).ok()?;
    parse_service(&data)
}

/// Parse systemd `.service` name from cgroup data (excluding user session root).
fn parse_service(data: &str) -> Option<String> {
    for line in data.lines() {
        let trimmed = line.trim();
        let path = trimmed.split("::").nth(1).unwrap_or(trimmed);
        let mut svc = None;
        for part in path.split('/') {
            if part.ends_with(".service") && !part.starts_with("user@") {
                svc = Some(part.to_string());
            }
        }
        if let Some(s) = svc {
            return Some(s);
        }
    }
    None
}

/// Total system jiffies from `/proc/stat` (`cpu …` line, all fields summed).
fn read_total_jiffies() -> Option<u64> {
    let data = std::fs::read_to_string(PathBuf::from(PROC_ROOT).join("stat")).ok()?;
    let line = data.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    Some(fields.filter_map(|field| field.parse::<u64>().ok()).sum())
}

/// Whether kernel threads should be shown (`FLEX_PROC_KTHREADS` non-empty and
/// not `0`).
fn kthreads_enabled() -> bool {
    std::env::var(KTHREADS_ENV).is_ok_and(|value| !value.is_empty() && value != "0")
}

/// uid → username from `/etc/passwd` (malformed lines skipped).
fn load_passwd() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let Ok(data) = std::fs::read_to_string("/etc/passwd") else {
        return map;
    };
    for line in data.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        let _passwd = fields.next();
        let Some(uid) = fields.next().and_then(|uid| uid.parse::<u32>().ok()) else {
            continue;
        };
        map.insert(uid, name.to_string());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parses_comm_with_parens_and_spaces() {
        // `(a ) b)` as comm, then the numeric tail.
        let line = "42 (weird ) name) S 1 42 42 0 -1 4194560 0 0 0 0 7 9 0 0 20 0 1 0 0 0 0";
        let (comm, utime, stime) = parse_stat(line).expect("stat parses");
        assert_eq!(comm, "weird ) name");
        assert_eq!(utime, 7);
        assert_eq!(stime, 9);
    }

    #[test]
    fn stat_rejects_malformed_lines() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("no parens").is_none());
        assert!(parse_stat("1 (x)").is_none(), "too few fields");
    }

    #[test]
    fn status_reads_uid_and_rss() {
        let data = "Name:\tfirefox\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t  2048 kB\n";
        assert_eq!(parse_status(data), (1000, 2048));
    }

    #[test]
    fn status_missing_fields_degrade_to_zero() {
        assert_eq!(parse_status("Name:\tfoo\n"), (0, 0));
    }

    #[test]
    fn cpu_percent_uses_the_jiffy_delta() {
        let prev = Sample {
            jiffies: 100,
            total: 1_000,
        };
        // 25 of 500 total jiffies -> 5%.
        assert!((cpu_percent(&prev, 125, 1_500) - 5.0).abs() < f32::EPSILON);
        assert!(
            cpu_percent(&prev, 100, 1_000).abs() < f32::EPSILON,
            "no total delta"
        );
    }

    #[test]
    fn format_memory_kb_formats_units() {
        assert_eq!(format_memory_kb(512), "512K");
        assert_eq!(format_memory_kb(2048), "2.0M");
        assert_eq!(format_memory_kb(150 * 1024), "150.0M");
        assert_eq!(format_memory_kb(2 * 1024 * 1024), "2.0G");
    }

    #[test]
    fn cpu_gauge_prints_exact_value_with_clamped_bar() {
        assert_eq!(cpu_gauge(0.0), "  0.0% [░░░░░░░░]");
        assert_eq!(cpu_gauge(12.3), " 12.3% [█░░░░░░░]");
        let hot = cpu_gauge(312.4);
        assert!(
            hot.starts_with("312.4%"),
            "per-core overflow prints exact: {hot}"
        );
        assert!(hot.contains("[████████]"), "only the bar saturates: {hot}");
    }

    #[test]
    fn parse_service_extracts_service_units() {
        assert_eq!(
            parse_service("0::/system.slice/bluetooth.service\n"),
            Some(String::from("bluetooth.service"))
        );
        assert_eq!(
            parse_service(
                "0::/user.slice/user-1001.slice/user@1001.service/app.slice/docker.service\n"
            ),
            Some(String::from("docker.service"))
        );
        assert_eq!(parse_service("0::/init.scope\n"), None);
        assert_eq!(parse_service("0::/\n"), None);
    }

    #[test]
    fn scan_finds_at_least_this_process() {
        let pid = std::process::id().to_string();
        let rows = scan();
        assert!(
            rows.iter().any(|row| {
                row.id.as_str() == pid || row.targets.iter().any(|t| t.id.as_str() == pid)
            }),
            "the scanning process appears in /proc"
        );
    }
}
