//! `net` executor: high-performance network throughput monitor & process bandwidth tracker.
//!
//! Provides kernel statistics parsing for Waybar (`custom/net-speed`), Top Bandwidth Consumers
//! ("Top Talkers"), and interface details. Interface throughput comes from kernel counters
//! (`/proc/net/route`, `/sys/class/net/<iface>/statistics/`, `/proc/net/dev`); per-process
//! rates come from per-connection TCP accounting (`ss -tinp` `bytes_sent`/`bytes_received`
//! deltas attributed to pids). Socket counters exclude the local IPC (pipes, pty, Wayland)
//! that `/proc/<pid>/io` `rchar`/`wchar` counts — measured live: `waybar` alone reads
//! ~500 KB/s through pipes while the wire sat idle, and a 7 MB/s download by short-lived
//! `curl` processes was 92% unattributed in `rchar` sums because dead pids vanish from
//! `/proc` before the next sample.
//!
//! Known limits (by kernel design, documented not hidden): UDP/QUIC traffic has no
//! per-socket byte counters, so it lands in the `unattributed` remainder; connections
//! that die mid-window lose their in-window bytes (bounded by the 3 s tick); counters
//! reset on reconnect, so churned sets clamp at zero instead of going negative
//! (a naive pid-level delta summed to *negative* rates live).
//!
//! All samplers share lightweight timestamped stat caches.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};

use crate::exec::proc as proc_exec;

/// Environment variable to override the stat file path (test seam).
pub const STAT_PATH_ENV: &str = "FLEX_NET_STAT_PATH";

/// Fallback base directory for stat storage.
const DEFAULT_STAT_FILE: &str = "flex-net-speed.stat";

pub const KB: f64 = 1024.0;

pub const MB: f64 = 1_048_576.0;

pub const GB: f64 = 1_073_741_824.0;

/// Information about an active process consuming network bandwidth.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessBandwidth {
    pub pid: u32,
    pub comm: String,
    pub rx_rate: f64,
    pub tx_rate: f64,
    pub total_rate: f64,
    pub share: f32,
}

/// Information about a network interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceInfo {
    pub name: String,
    pub operstate: String,
    pub is_default: bool,
    pub ip_cidr: Option<String>,
    pub gateway: Option<String>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// One baseline sample for a single TCP connection.
/// Public only so [`attribute_tcp_rates`] can take baselines across crates.
#[derive(Debug, Clone, Copy)]
pub struct TcpSample {
    /// Baseline timestamp (millis since epoch).
    pub ts_ms: u128,
    /// Baseline cumulative `bytes_sent`.
    pub sent: u64,
    /// Baseline cumulative `bytes_received`.
    pub recv: u64,
}

const DEFAULT_TCP_STAT_FILE: &str = "flex-net-tcp.stat";

/// Static per-connection baseline cache across ticks.
fn tcp_cache() -> &'static Mutex<HashMap<ConnKey, TcpSample>> {
    static CACHE: OnceLock<Mutex<HashMap<ConnKey, TcpSample>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve the stat cache file path.
fn stat_file_path() -> PathBuf {
    if let Ok(path) = std::env::var(STAT_PATH_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime_dir.is_empty() {
            return PathBuf::from(runtime_dir).join(DEFAULT_STAT_FILE);
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    std::env::temp_dir().join(format!("{DEFAULT_STAT_FILE}-{user}"))
}

/// Resolve the per-connection TCP baseline cache file path.
fn tcp_stat_file_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime_dir.is_empty() {
            return PathBuf::from(runtime_dir).join(DEFAULT_TCP_STAT_FILE);
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    std::env::temp_dir().join(format!("{DEFAULT_TCP_STAT_FILE}-{user}"))
}

/// Read cached per-connection baselines from disk.
///
/// Line shape: `<pid|-> <ts_ms> <sent> <recv> <local> <peer>` (addresses hold
/// no spaces, IPv6 colons included). Malformed lines are skipped; a missing
/// file reads as empty (first run reports zero rates, like every sampler).
fn read_tcp_stat_cache(path: &Path) -> HashMap<ConnKey, TcpSample> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid_raw) = fields.next() else {
            continue;
        };
        let pid = if pid_raw == "-" {
            None
        } else if let Ok(pid) = pid_raw.parse::<u32>() {
            Some(pid)
        } else {
            continue;
        };
        let (Some(ts_ms), Some(sent), Some(recv), Some(local), Some(peer)) = (
            fields.next().and_then(|s| s.parse::<u128>().ok()),
            fields.next().and_then(|s| s.parse::<u64>().ok()),
            fields.next().and_then(|s| s.parse::<u64>().ok()),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        map.insert(
            ConnKey {
                local: local.to_string(),
                peer: peer.to_string(),
                pid,
            },
            TcpSample { ts_ms, sent, recv },
        );
    }
    map
}

/// Write per-connection baselines to disk.
fn write_tcp_stat_cache(path: &Path, samples: &HashMap<ConnKey, TcpSample>) {
    let mut buf = String::with_capacity(samples.len() * 80);
    for (key, s) in samples {
        let pid = key
            .pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string());
        let _ = writeln!(
            buf,
            "{} {} {} {} {} {}",
            pid, s.ts_ms, s.sent, s.recv, key.local, key.peer
        );
    }
    let _ = std::fs::write(path, buf);
}

/// Find the default network interface name from `/proc/net/route`.
#[must_use]
pub fn default_interface() -> Option<String> {
    default_interface_from_route(Path::new("/proc/net/route"))
}

/// Parse default interface name from a route table file.
#[must_use]
pub fn default_interface_from_route(route_file: &Path) -> Option<String> {
    let content = std::fs::read_to_string(route_file).ok()?;
    for line in content.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let iface = fields.next()?;
        let dest = fields.next()?;
        if dest == "00000000" {
            return Some(iface.to_string());
        }
    }
    None
}

/// Convert a little-endian 8-hex-digit string into an IPv4 address (e.g. `0101A8C0` -> `192.168.1.1`).
#[must_use]
pub fn hex_to_ipv4(hex: &str) -> Option<String> {
    if hex.len() != 8 {
        return None;
    }
    let val = u32::from_str_radix(hex, 16).ok()?;
    let b1 = u8::try_from(val & 0xFF).ok()?;
    let b2 = u8::try_from((val >> 8) & 0xFF).ok()?;
    let b3 = u8::try_from((val >> 16) & 0xFF).ok()?;
    let b4 = u8::try_from((val >> 24) & 0xFF).ok()?;
    Some(format!("{b1}.{b2}.{b3}.{b4}"))
}

/// Find default gateway and subnet info for interfaces from `/proc/net/route`.
#[must_use]
pub fn route_info(route_file: &Path) -> HashMap<String, (Option<String>, Option<String>)> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(route_file) else {
        return map;
    };
    for line in content.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(iface) = fields.next() else { continue };
        let Some(dest) = fields.next() else { continue };
        let Some(gw_hex) = fields.next() else {
            continue;
        };
        let _flags = fields.next();
        let _refcnt = fields.next();
        let _use = fields.next();
        let _metric = fields.next();
        let mask_hex = fields.next();

        let gw = if dest == "00000000" {
            hex_to_ipv4(gw_hex)
        } else {
            None
        };

        let mask = mask_hex.and_then(hex_to_ipv4);
        let entry = map.entry(iface.to_string()).or_insert((None, None));
        if gw.is_some() {
            entry.0 = gw;
        }
        if mask.is_some() && entry.1.is_none() {
            entry.1 = mask;
        }
    }
    map
}

/// Read `(rx_bytes, tx_bytes)` for a network interface.
#[must_use]
pub fn interface_bytes(iface: &str) -> Option<(u64, u64)> {
    let rx_path = format!("/sys/class/net/{iface}/statistics/rx_bytes");
    let tx_path = format!("/sys/class/net/{iface}/statistics/tx_bytes");
    if let (Ok(rx_str), Ok(tx_str)) = (
        std::fs::read_to_string(rx_path),
        std::fs::read_to_string(tx_path),
    ) {
        if let (Ok(rx), Ok(tx)) = (rx_str.trim().parse::<u64>(), tx_str.trim().parse::<u64>()) {
            return Some((rx, tx));
        }
    }
    parse_proc_net_dev(Path::new("/proc/net/dev"), iface)
}

/// Parse `(rx_bytes, tx_bytes)` from `/proc/net/dev` for `target_iface`.
#[must_use]
pub fn parse_proc_net_dev(dev_file: &Path, target_iface: &str) -> Option<(u64, u64)> {
    let content = std::fs::read_to_string(dev_file).ok()?;
    for line in content.lines().skip(2) {
        let Some((iface_part, stats_part)) = line.split_once(':') else {
            continue;
        };
        if iface_part.trim() == target_iface {
            let mut fields = stats_part.split_whitespace();
            let rx_bytes = fields.next()?.parse::<u64>().ok()?;
            for _ in 0..7 {
                fields.next()?;
            }
            let tx_bytes = fields.next()?.parse::<u64>().ok()?;
            return Some((rx_bytes, tx_bytes));
        }
    }
    None
}

/// Format bytes-per-second into human-readable rate matching `network-speed.sh`.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn format_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= MB {
        format!("{:.1} MB/s", bytes_per_sec / MB)
    } else if bytes_per_sec >= KB {
        let rounded = (bytes_per_sec / KB).round() as u64;
        format!("{rounded} KB/s")
    } else {
        let rounded = bytes_per_sec.round() as u64;
        format!("{rounded} B/s")
    }
}

/// Format raw byte totals into human-readable unit (e.g. `4.8 GB`, `120 MB`, `12 KB`).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Read process `comm` (name) from `/proc/<pid>/stat`.
#[must_use]
pub fn read_proc_comm(stat_file: &Path) -> Option<String> {
    let content = std::fs::read_to_string(stat_file).ok()?;
    let open = content.find('(')?;
    let close = content.rfind(')')?;
    if close > open {
        Some(content[open + 1..close].to_string())
    } else {
        None
    }
}

/// One TCP connection's cumulative counters from `ss -tinp`.
///
/// `pid` is the first owning process (`None` = kernel-owned or a foreign
/// user's socket, which unprivileged `ss -p` omits); `sent`/`recv` are
/// `bytes_sent`/`bytes_received`. Counters reset whenever the connection
/// dies, so attribution must clamp per-connection deltas at zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsConn {
    /// Local `addr:port` token as printed.
    pub local: String,
    /// Peer `addr:port` token as printed.
    pub peer: String,
    /// Owning pid, if visible.
    pub pid: Option<u32>,
    /// Cumulative `bytes_sent`.
    pub sent: u64,
    /// Cumulative `bytes_received`.
    pub recv: u64,
}

/// Identity of one tracked connection: endpoints plus owner. A socket that
/// changes owner (fd passing on fork) tracks as a fresh connection.
/// Public only so [`attribute_tcp_rates`] can take baselines across crates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConnKey {
    /// Local `addr:port` token as printed by `ss`.
    pub local: String,
    /// Peer `addr:port` token as printed by `ss`.
    pub peer: String,
    /// Owning pid, if visible.
    pub pid: Option<u32>,
}

/// Parse the decimal after `key:` in an `ss` info fragment (`None` when the
/// key is absent or non-numeric — ss versions vary, counters degrade to 0,
/// never to an error).
fn ss_counter(fragment: &str, key: &str) -> u64 {
    fragment
        .find(key)
        .and_then(|at| {
            fragment[at + key.len()..]
                .chars()
                .take_while(|c| c.is_numeric())
                .collect::<String>()
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(0)
}

/// Parse the first `pid=<n>` in an `ss -p` process fragment.
fn ss_pid(fragment: &str) -> Option<u32> {
    fragment.find("pid=").and_then(|at| {
        fragment[at + 4..]
            .chars()
            .take_while(|c| c.is_numeric())
            .collect::<String>()
            .parse::<u32>()
            .ok()
    })
}

/// Parse `ss -tinp` output into one [`SsConn`] per connection header.
///
/// Header lines start at column 0 (`State … Local Peer [users:(…)]`; the
/// trailing address pair is taken as local/peer so column-count drift cannot
/// misalign them); indented continuation lines carry the counters. A header
/// without counters yields a zero record (e.g. `LISTEN` sockets).
#[must_use]
pub fn parse_ss_tcp(text: &str) -> Vec<SsConn> {
    let mut conns = Vec::new();
    let mut current: Option<SsConn> = None;
    let flush = |current: &mut Option<SsConn>, conns: &mut Vec<SsConn>| {
        if let Some(conn) = current.take() {
            conns.push(conn);
        }
    };
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(conn) = current.as_mut() {
                conn.sent = conn.sent.max(ss_counter(line, "bytes_sent:"));
                conn.recv = conn.recv.max(ss_counter(line, "bytes_received:"));
            }
            continue;
        }
        let mut current_slot = current.take();
        flush(&mut current_slot, &mut conns);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Netid") || trimmed.starts_with("State") {
            continue;
        }
        let (addrs, proc) = match line.find("users:(") {
            Some(at) => (&line[..at], Some(&line[at..])),
            None => (line, None),
        };
        let tokens: Vec<&str> = addrs.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let peer = tokens[tokens.len() - 1].to_string();
        let local = tokens[tokens.len() - 2].to_string();
        current = Some(SsConn {
            local,
            peer,
            pid: proc.and_then(ss_pid),
            sent: 0,
            recv: 0,
        });
    }
    let mut tail = current.take();
    flush(&mut tail, &mut conns);
    conns
}

/// Run `ss -tinp` once and capture stdout (`None` when `ss` is missing or
/// fails — the caller degrades to unattributed rows, never to an error).
fn ss_tcp_output() -> Option<String> {
    let output = Command::new("ss")
        .args(["-t", "-i", "-n", "-p"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Attribute current connections against per-connection baselines.
///
/// Pure (no I/O): `prev` maps [`ConnKey`] to its last sample; returns
/// `(per-pid rates, fresh baselines)`. Every owned connection appears in the
/// rates (new ones at zero, so the list shows what's connected immediately);
/// surviving connections add `delta / own elapsed`; deltas clamp at zero
/// (counters reset on reconnect — a naive pid-level sum measured *negative*
/// live under churn); vanished baselines are dropped (their in-window bytes
/// are unrecoverable — bounded by the tick interval, surfaced as
/// unattributed by [`snapshot_bandwidth`]). Baselines older than 60 s or
/// newer than 100 ms contribute zero, mirroring every other sampler here.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn attribute_tcp_rates<S: std::hash::BuildHasher>(
    now_ms: u128,
    prev: &HashMap<ConnKey, TcpSample, S>,
    cur: &[SsConn],
) -> (HashMap<u32, (f64, f64)>, HashMap<ConnKey, TcpSample>) {
    let mut per_pid: HashMap<u32, (f64, f64)> = HashMap::new();
    let mut fresh = HashMap::with_capacity(cur.len());
    for conn in cur {
        let key = ConnKey {
            local: conn.local.clone(),
            peer: conn.peer.clone(),
            pid: conn.pid,
        };
        fresh.insert(
            key.clone(),
            TcpSample {
                ts_ms: now_ms,
                sent: conn.sent,
                recv: conn.recv,
            },
        );
        let Some(pid) = conn.pid else {
            continue;
        };
        let entry = per_pid.entry(pid).or_insert((0.0, 0.0));
        let Some(baseline) = prev.get(&key) else {
            continue;
        };
        let elapsed_ms = now_ms.saturating_sub(baseline.ts_ms);
        if !(100..=60_000).contains(&elapsed_ms) {
            continue;
        }
        let delta_sec = elapsed_ms as f64 / 1000.0;
        let rx_rate = conn.recv.saturating_sub(baseline.recv) as f64 / delta_sec;
        let tx_rate = conn.sent.saturating_sub(baseline.sent) as f64 / delta_sec;
        entry.0 += rx_rate;
        entry.1 += tx_rate;
    }
    (per_pid, fresh)
}

/// Scan TCP connections and calculate per-process socket throughput.
///
/// One `ss -tinp` spawn per call (callers throttle to the 3 s tick):
/// surviving connections contribute clamped counter deltas, new ones ride
/// the baseline for next tick, and a missing `ss` degrades to an empty set.
/// `share` stays `0.0` here — [`snapshot_bandwidth`] rebases it on the
/// interface total. Sorted by total rate, ties by pid.
#[must_use]
pub fn scan_tcp_talkers() -> Vec<ProcessBandwidth> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let mut guard = match tcp_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    let tcp_stat_path = tcp_stat_file_path();
    if guard.is_empty() {
        *guard = read_tcp_stat_cache(&tcp_stat_path);
    }

    let Some(output) = ss_tcp_output() else {
        return Vec::new();
    };
    let conns = parse_ss_tcp(&output);
    let (per_pid, fresh) = attribute_tcp_rates(now_ms, &guard, &conns);
    // Skip the disk write when the socket set is empty (idle ticks cost no
    // I/O); the in-memory cache is always updated.
    if !conns.is_empty() {
        write_tcp_stat_cache(&tcp_stat_path, &fresh);
    }
    *guard = fresh;

    let mut results: Vec<ProcessBandwidth> = per_pid
        .into_iter()
        .map(|(pid, (rx_rate, tx_rate))| {
            let comm = read_proc_comm(&PathBuf::from(format!("/proc/{pid}/stat")))
                .unwrap_or_else(|| format!("pid-{pid}"));
            ProcessBandwidth {
                total_rate: rx_rate + tx_rate,
                rx_rate,
                tx_rate,
                pid,
                comm,
                share: 0.0,
            }
        })
        .collect();

    results.sort_by(|a, b| {
        b.total_rate
            .partial_cmp(&a.total_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.pid.cmp(&b.pid))
    });

    results
}

/// One consistent bandwidth sample: interface totals plus TCP attribution.
///
/// Built from a single [`sample_interface_rates`] call, so header, rows and
/// remainder describe the same window.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BandwidthSnapshot {
    /// `(iface, rx_rate, tx_rate)` wire throughput; `None` when offline.
    pub iface: Option<(String, f64, f64)>,
    /// TCP-attributed processes, `share` rebased on the interface total.
    pub talkers: Vec<ProcessBandwidth>,
    /// `(rx, tx)` wire bytes no pid claims: UDP/QUIC, churned or
    /// short-lived connections, kernel traffic. Clamped at zero.
    pub unattributed: (f64, f64),
}

/// Sample interface throughput plus per-process TCP attribution.
///
/// `share` is each talker's fraction of the *interface* total (so the bars
/// read as wire share); anything the sockets don't explain lands in
/// [`BandwidthSnapshot::unattributed`] instead of inflating a row.
#[must_use]
pub fn snapshot_bandwidth() -> BandwidthSnapshot {
    let iface = sample_interface_rates();
    let mut talkers = scan_tcp_talkers();
    let unattributed = match &iface {
        Some((_, rx_rate, tx_rate)) => {
            let total = rx_rate + tx_rate;
            if total > 0.0 {
                for process in &mut talkers {
                    #[allow(clippy::cast_possible_truncation)]
                    let share = (process.total_rate / total).clamp(0.0, 1.0) as f32;
                    process.share = share;
                }
            }
            let sum_rx: f64 = talkers.iter().map(|process| process.rx_rate).sum();
            let sum_tx: f64 = talkers.iter().map(|process| process.tx_rate).sum();
            ((rx_rate - sum_rx).max(0.0), (tx_rate - sum_tx).max(0.0))
        }
        None => (0.0, 0.0),
    };
    BandwidthSnapshot {
        iface,
        talkers,
        unattributed,
    }
}

/// Parse local IP address from `/proc/net/fib_trie`.
#[must_use]
pub fn local_ip_from_fib_trie(trie_file: &Path) -> Option<String> {
    let content = std::fs::read_to_string(trie_file).ok()?;
    let mut current_cidr = None;
    let mut pending_ip = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("+--") {
            if let Some(subnet) = trimmed.strip_prefix("+-- ") {
                if let Some((cidr, _)) = subnet.split_once(' ') {
                    if !cidr.starts_with("0.0.0.0") && !cidr.starts_with("127.") {
                        current_cidr = Some(cidr.to_string());
                    }
                }
            }
        } else if trimmed.starts_with("|--") {
            if let Some(ip) = trimmed.strip_prefix("|-- ") {
                if !ip.starts_with("0.0.0.0") && !ip.starts_with("127.") && !ip.ends_with(".255") {
                    pending_ip = Some(ip.to_string());
                }
            }
        } else if trimmed.contains("host LOCAL") {
            if let (Some(ip), Some(ref cidr)) = (pending_ip.take(), &current_cidr) {
                let mask = cidr.split_once('/').map_or("24", |(_, m)| m);
                return Some(format!("{ip}/{mask}"));
            }
        }
    }
    None
}

/// Scan system network interfaces.
#[must_use]
pub fn scan_interfaces() -> Vec<InterfaceInfo> {
    let default_iface = default_interface();
    let routes = route_info(Path::new("/proc/net/route"));
    let local_ip = local_ip_from_fib_trie(Path::new("/proc/net/fib_trie"));

    let mut list = Vec::new();
    let net_dir = Path::new("/sys/class/net");
    let Ok(entries) = std::fs::read_dir(net_dir) else {
        return list;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let operstate = std::fs::read_to_string(entry.path().join("operstate"))
            .map_or_else(|_| "unknown".to_string(), |s| s.trim().to_string());

        let is_default = default_iface.as_deref() == Some(&name);
        let (rx_bytes, tx_bytes) = interface_bytes(&name).unwrap_or((0, 0));

        let (gw, _) = routes.get(&name).cloned().unwrap_or((None, None));
        let ip_cidr = if is_default {
            local_ip.clone()
        } else if name == "lo" {
            Some("127.0.0.1/8".to_string())
        } else {
            None
        };

        list.push(InterfaceInfo {
            name,
            operstate,
            is_default,
            ip_cidr,
            gateway: gw,
            rx_bytes,
            tx_bytes,
        });
    }

    list.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });

    list
}

/// Read the previous sample `(timestamp_millis, rx_bytes, tx_bytes)` from cache.
fn read_stat(path: &Path) -> Option<(u128, u64, u64)> {
    let data = std::fs::read_to_string(path).ok()?;
    let mut fields = data.split_whitespace();
    let ts = fields.next()?.parse::<u128>().ok()?;
    let rx = fields.next()?.parse::<u64>().ok()?;
    let tx = fields.next()?.parse::<u64>().ok()?;
    Some((ts, rx, tx))
}

/// Write the current sample `(timestamp_millis, rx_bytes, tx_bytes)` to cache.
fn write_stat(path: &Path, ts: u128, rx: u64, tx: u64) {
    let content = format!("{ts} {rx} {tx}\n");
    let _ = std::fs::write(path, content);
}

/// Sample default-interface throughput from kernel counters.
///
/// Returns `(iface, rx_rate, tx_rate)` in bytes/sec, sharing the same
/// on-disk baseline as [`sample_json`] so every consumer (waybar text,
/// TUI header, `--top`) reports identical rates for the same window.
/// `None` when there is no default interface or its counters are unreadable.
/// First call after boot (no baseline yet) records the baseline and reports
/// `Some` with zero rates.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn sample_interface_rates() -> Option<(String, f64, f64)> {
    let iface = default_interface()?;
    let (rx, tx) = interface_bytes(&iface)?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let stat_path = stat_file_path();
    let (rx_rate, tx_rate) = if let Some((prev_ms, prev_rx, prev_tx)) = read_stat(&stat_path) {
        let elapsed_ms = now_ms.saturating_sub(prev_ms);
        if (50..=60_000).contains(&elapsed_ms) {
            let delta_sec = elapsed_ms as f64 / 1000.0;
            let rx_diff = rx.saturating_sub(prev_rx);
            let tx_diff = tx.saturating_sub(prev_tx);
            (rx_diff as f64 / delta_sec, tx_diff as f64 / delta_sec)
        } else {
            (0.0, 0.0)
        }
    } else {
        (0.0, 0.0)
    };

    write_stat(&stat_path, now_ms, rx, tx);
    Some((iface, rx_rate, tx_rate))
}

/// Sample throughput and format Waybar JSON with multiline Top Bandwidth Consumers tooltip.
///
/// Talkers come from the same [`snapshot_bandwidth`] the TUI rows use, so
/// the tooltip agrees with the monitor by construction.
#[must_use]
pub fn sample_json() -> String {
    let snapshot = snapshot_bandwidth();
    let Some((iface, rx_rate, tx_rate)) = snapshot.iface else {
        return r#"{"text":"⬇ ?  ⬆ ?","class":"idle"}"#.to_string();
    };
    let (down_str, up_str) = (format_speed(rx_rate), format_speed(tx_rate));
    let (rx, tx) = interface_bytes(&iface).unwrap_or((0, 0));

    let total_rate = rx_rate + tx_rate;
    let class = if total_rate >= MB {
        "heavy"
    } else if total_rate > 0.0 {
        "up"
    } else {
        "idle"
    };

    let top_talkers = snapshot.talkers;
    let local_ip = local_ip_from_fib_trie(Path::new("/proc/net/fib_trie"))
        .unwrap_or_else(|| "127.0.0.1/8".to_string());
    let routes = route_info(Path::new("/proc/net/route"));
    let gw = routes
        .get(&iface)
        .and_then(|(g, _)| g.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let iface_kind = if iface.starts_with("wl") {
        "Wi-Fi"
    } else if iface.starts_with('e') {
        "Ethernet"
    } else if iface.starts_with("wg") {
        "WireGuard"
    } else {
        "Network"
    };

    let mut tooltip = format!(
        "Interface: {iface} ({iface_kind})\nLocal IP:  {local_ip}\nGateway:   {gw}\nSession:   ⬇ {}  |  ⬆ {}",
        format_bytes(rx),
        format_bytes(tx)
    );

    if !top_talkers.is_empty() {
        tooltip.push_str("\n\nTop Bandwidth Consumers:");
        for (i, p) in top_talkers.iter().take(3).enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let share_pct = (p.share * 100.0).round() as u32;
            let rate_str = format_speed(p.rx_rate);
            let idx = i + 1;
            let _ = write!(
                tooltip,
                "\n  {idx}. {:<10} (PID {:<5})   ⬇ {:<9} ({share_pct}%)",
                p.comm, p.pid, rate_str
            );
        }
    }

    // Single-pass JSON escape: the old `.replace().replace().replace()`
    // walked the tooltip three times; this reserves once and escapes inline.
    let mut escaped_tooltip = String::with_capacity(tooltip.len() + 16);
    for c in tooltip.chars() {
        match c {
            '\\' => escaped_tooltip.push_str("\\\\"),
            '\n' => escaped_tooltip.push_str("\\n"),
            '"' => escaped_tooltip.push_str("\\\""),
            _ => escaped_tooltip.push(c),
        }
    }

    format!(
        r#"{{"text":" {down_str}   {up_str}","tooltip":"{escaped_tooltip}","class":"{class}"}}"#
    )
}

/// Stream Waybar JSON continuously at the specified interval.
///
/// # Errors
///
/// When writing to stdout fails.
pub fn stream(interval: Duration) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    loop {
        let json = sample_json();
        writeln!(stdout, "{json}")?;
        stdout.flush()?;
        std::thread::sleep(interval);
    }
}

/// Execute a net action (`signal:<pid>:<sig>`, `copy:<text>`, `wifi`).
///
/// # Errors
///
/// When subprocess execution fails.
pub fn execute(action_id: &str) -> Result<()> {
    if action_id == "noop" {
        return Ok(());
    }
    if let Some(rest) = action_id.strip_prefix("signal:") {
        let mut parts = rest.split(':');
        let pid_str = parts.next().context("missing pid in signal action")?;
        let sig_str = parts.next().context("missing sig in signal action")?;
        let sig = match sig_str {
            "SIGTERM" | "TERM" => proc_exec::Signal::Term,
            "SIGKILL" | "KILL" => proc_exec::Signal::Kill,
            "SIGSTOP" | "STOP" => proc_exec::Signal::Stop,
            "SIGCONT" | "CONT" => proc_exec::Signal::Cont,
            _ => anyhow::bail!("unknown signal {sig_str}"),
        };
        proc_exec::signal(pid_str, sig, None)?;
    } else if let Some(copy_text) = action_id.strip_prefix("copy:") {
        let mut cmd = std::process::Command::new("wl-copy");
        cmd.arg(copy_text);
        let _ = crate::spawn::status(&mut cmd)?;
    } else if action_id == "wifi" {
        let mut cmd = std::process::Command::new("flex");
        cmd.args(["popup", "menu", "flex-wifi"]);
        let _ = crate::spawn::spawn(&mut cmd)?;
    } else if action_id == "speedtest:run" || action_id == "speedtest" {
        crate::exec::speedtest::trigger_background();
    } else if action_id.starts_with("speedtest:") {
        return Ok(());
    }
    Ok(())
}
