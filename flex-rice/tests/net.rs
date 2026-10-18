use std::path::PathBuf;

use flex_rice::exec::net::{self, STAT_PATH_ENV};
use flex_rice::providers::net as net_provider;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flex-net-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn format_speed_units_and_rounding() {
    assert_eq!(net::format_speed(0.0), "0 B/s");
    assert_eq!(net::format_speed(512.0), "512 B/s");
    assert_eq!(net::format_speed(1023.9), "1024 B/s");
    assert_eq!(net::format_speed(1024.0), "1 KB/s");
    assert_eq!(net::format_speed(24_576.0), "24 KB/s");
    assert_eq!(net::format_speed(1_048_576.0), "1.0 MB/s");
    assert_eq!(net::format_speed(1_572_864.0), "1.5 MB/s");
    assert_eq!(net::format_speed(104_857_600.0), "100.0 MB/s");
}

#[test]
fn format_bytes_scales_units() {
    assert_eq!(net::format_bytes(500), "500 B");
    assert_eq!(net::format_bytes(1024), "1 KB");
    assert_eq!(net::format_bytes(1024 * 1024), "1.0 MB");
    assert_eq!(net::format_bytes(1_572_864), "1.5 MB");
    assert_eq!(net::format_bytes(1024 * 1024 * 1024 * 4), "4.0 GB");
}

#[test]
fn hex_to_ipv4_parses_little_endian() {
    assert_eq!(
        net::hex_to_ipv4("0101A8C0"),
        Some("192.168.1.1".to_string())
    );
    assert_eq!(net::hex_to_ipv4("00000000"), Some("0.0.0.0".to_string()));
    assert_eq!(net::hex_to_ipv4("0100007F"), Some("127.0.0.1".to_string()));
    assert_eq!(net::hex_to_ipv4("invalid"), None);
}

#[test]
fn default_interface_from_route_parses_default_gateway() {
    let dir = scratch("route-parse");
    let route_file = dir.join("route");
    let content =
        "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                   eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
                   wlp2s0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n";
    std::fs::write(&route_file, content).expect("write mock route");
    let iface = net::default_interface_from_route(&route_file);
    assert_eq!(iface.as_deref(), Some("wlp2s0"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parse_proc_net_dev_extracts_bytes() {
    let dir = scratch("dev-parse");
    let dev_file = dir.join("dev");
    let content = "Inter-|   Receive                                                |  Transmit\n\
                   face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
                     lo: 100000      10    0    0    0     0          0         0   100000      10    0    0    0     0       0          0\n\
                 wlp2s0: 50000000   1000    0    0    0     0          0         0  20000000    500    0    0    0     0       0          0\n";
    std::fs::write(&dev_file, content).expect("write mock dev");
    let stats = net::parse_proc_net_dev(&dev_file, "wlp2s0");
    assert_eq!(stats, Some((50_000_000, 20_000_000)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_proc_comm_parses_mock_stat() {
    let dir = scratch("proc-io");
    let stat_file = dir.join("stat");

    std::fs::write(
        &stat_file,
        "1234 (firefox) S 1 1234 1234 0 -1 4194304 ...\n",
    )
    .expect("write stat");

    // `read_proc_io` is gone (the rchar source was replaced by ss-TCP
    // attribution); `read_proc_comm` still resolves talker names.
    let comm = net::read_proc_comm(&stat_file);
    assert_eq!(comm.as_deref(), Some("firefox"));

    let _ = std::fs::remove_dir_all(&dir);
}

const SS_FIXTURE: &str = "Netid State  Recv-Q Send-Q Local Address:Port  Peer Address:Port Process\n\
tcp   ESTAB  0      0      192.168.1.108:40270  159.69.246.219:https users:((\"brave\",pid=2237,fd=45))\n \
\tcubic wscale:7,10 rto:274 rtt:73.5 cwnd:10 bytes_sent:1999 bytes_acked:2000 bytes_received:11395\n\
tcp   ESTAB  0      0      192.168.1.108:58602  2.20.33.213:https\n \
\tcubic wscale:7,10 rto:251 rtt:50.3 cwnd:10 bytes_sent:84986 bytes_received:342746\n\
tcp   LISTEN 0      128    127.0.0.1:631        0.0.0.0:*     users:((\"cupsd\",pid=900,fd=3),(\"cups-browsed\",pid=901,fd=4))\n";

#[test]
fn ss_parser_captures_pids_and_counters() {
    let conns = net::parse_ss_tcp(SS_FIXTURE);
    assert_eq!(
        conns.len(),
        3,
        "header line skipped, 3 connections: {conns:?}"
    );
    assert_eq!(
        conns[0],
        net::SsConn {
            local: "192.168.1.108:40270".to_string(),
            peer: "159.69.246.219:https".to_string(),
            pid: Some(2237),
            sent: 1999,
            recv: 11395,
        }
    );
    // Kernel/foreign sockets carry no users: attributed to nobody, never lost.
    assert_eq!(conns[1].pid, None);
    assert_eq!((conns[1].sent, conns[1].recv), (84_986, 342_746));
    // Shared sockets attribute to the first owner; no counters means zero.
    assert_eq!(conns[2].pid, Some(900));
    assert_eq!((conns[2].sent, conns[2].recv), (0, 0));
}

#[test]
fn tcp_attribution_clamps_churn_and_ignores_new_conns() {
    use std::collections::HashMap;

    let key = |local: &str, peer: &str, pid: Option<u32>| net::ConnKey {
        local: local.to_string(),
        peer: peer.to_string(),
        pid,
    };
    let now = 1_000_000_u128;
    let mut prev = HashMap::new();
    // Surviving connection: 10 s baseline.
    prev.insert(
        key("a:1", "b:2", Some(100)),
        net::TcpSample {
            ts_ms: now - 10_000,
            sent: 1000,
            recv: 2000,
        },
    );
    // Reconnected socket: counters reset below the baseline.
    prev.insert(
        key("c:3", "d:4", Some(100)),
        net::TcpSample {
            ts_ms: now - 10_000,
            sent: 90_000,
            recv: 90_000,
        },
    );
    // Dead connection: baseline only, absent from the current set.
    prev.insert(
        key("gone:5", "away:6", Some(200)),
        net::TcpSample {
            ts_ms: now - 10_000,
            sent: 50,
            recv: 60,
        },
    );
    let cur = vec![
        net::SsConn {
            local: "a:1".to_string(),
            peer: "b:2".to_string(),
            pid: Some(100),
            sent: 2000,
            recv: 4000,
        },
        net::SsConn {
            local: "c:3".to_string(),
            peer: "d:4".to_string(),
            pid: Some(100),
            sent: 500,
            recv: 700,
        },
        net::SsConn {
            local: "new:7".to_string(),
            peer: "srv:8".to_string(),
            pid: Some(300),
            sent: 999,
            recv: 999,
        },
    ];
    let (rates, fresh) = net::attribute_tcp_rates(now, &prev, &cur);
    // (2000-1000)/10 s sent, (4000-2000)/10 s recv; reset clamps at zero.
    assert_eq!(rates.get(&100), Some(&(200.0, 100.0)), "{rates:?}");
    // New connections appear immediately at zero rates (list shows what's
    // connected) while riding the baseline for next tick.
    assert_eq!(rates.get(&300), Some(&(0.0, 0.0)), "{rates:?}");
    assert!(fresh.contains_key(&key("new:7", "srv:8", Some(300))));
    // Vanished baselines are dropped, never carried forward.
    assert!(!fresh.contains_key(&key("gone:5", "away:6", Some(200))));
    // Stale baselines (older than 60 s) hold the pid at zero rates rather
    // than attributing ancient bytes.
    let mut old = HashMap::new();
    old.insert(
        key("a:1", "b:2", Some(100)),
        net::TcpSample {
            ts_ms: now - 600_001,
            sent: 1,
            recv: 1,
        },
    );
    let (rates, _) = net::attribute_tcp_rates(now, &old, &cur[..1]);
    assert_eq!(rates.get(&100), Some(&(0.0, 0.0)), "{rates:?}");
}

#[test]
fn sample_json_emits_valid_waybar_payload() {
    let dir = scratch("sample-json");
    let stat_file = dir.join("speed.stat");
    std::env::set_var(STAT_PATH_ENV, stat_file.display().to_string());

    let json1 = net::sample_json();
    assert!(
        json1.starts_with('{') && json1.ends_with('}'),
        "valid json brackets: {json1}"
    );
    assert!(json1.contains("\"text\":"), "has text property: {json1}");
    assert!(json1.contains("\"class\":"), "has class property: {json1}");

    // Second sample immediately follows: stat file now exists
    let json2 = net::sample_json();
    assert!(json2.starts_with('{') && json2.ends_with('}'));

    std::env::remove_var(STAT_PATH_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn net_menu_constructs_all_tabs() {
    let menu = net_provider::net_menu();
    assert_eq!(menu.provider, net_provider::PROVIDER);
    assert_eq!(menu.app.tabs.len(), 3);
    assert_eq!(menu.app.tabs[0].name, net_provider::TAB_BANDWIDTH);
    assert_eq!(menu.app.tabs[1].name, net_provider::TAB_INTERFACES);
    assert_eq!(menu.app.tabs[2].name, net_provider::TAB_SPEEDTEST);
}

#[test]
fn bandwidth_tab_rows_carry_volume_metrics() {
    let tab = net_provider::bandwidth_tab();
    assert_eq!(tab.name, net_provider::TAB_BANDWIDTH);
    assert!(tab.filterable);
    assert!(tab.deletable);
    for row in &tab.rows {
        // Compact rows: the wire share lives in `meta` as a wifi-style text
        // gauge, never in `Row::volume` (which would force 3-line nodes).
        assert_eq!(row.volume, None, "compact text gauge: {row:?}");
        let id = row.id.as_str();
        if id == "noop" || id.starts_with("iface:") {
            continue;
        }
        assert!(row.confirmable, "row is confirmable: {row:?}");
        let meta = row.meta.as_deref().expect("talker meta");
        assert!(meta.contains('%'), "share pct: {row:?}");
        assert!(
            meta.contains('[') && meta.contains(']'),
            "share bar: {row:?}"
        );
    }
}

#[test]
fn bandwidth_header_matches_waybar_sampler_and_is_inert() {
    // The header reuses `sample_interface_rates` (waybar's sampler), so it
    // can never disagree with the bar by construction.
    if let Some((iface, rx_rate, tx_rate)) = net::sample_interface_rates() {
        assert_ne!(iface, "");
        assert!(rx_rate >= 0.0 && tx_rate >= 0.0);
        assert!(rx_rate.is_finite() && tx_rate.is_finite());
        let tab = net_provider::bandwidth_tab();
        let head = &tab.rows[0];
        assert_eq!(head.id.as_str(), format!("iface:{iface}"));
        assert!(
            head.meta
                .as_deref()
                .is_some_and(|m| m.contains("(interface total)")),
            "header says what it is: {head:?}"
        );
        assert!(head.volume.is_none(), "header carries no share: {head:?}");
        assert!(!head.confirmable, "header is not killable: {head:?}");
        assert!(
            net::execute(head.id.as_str()).is_ok(),
            "Enter on the header is a safe no-op"
        );
        // Snapshot invariants: shares are wire fractions in [0,1], talkers
        // arrive sorted, and any remainder is an inert noop row, never a
        // fake process.
        let snapshot = net::snapshot_bandwidth();
        let mut previous = f64::INFINITY;
        for talker in &snapshot.talkers {
            assert!(
                (0.0..=1.0).contains(&f64::from(talker.share)),
                "wire share in range: {talker:?}"
            );
            assert!(talker.total_rate <= previous, "sorted desc: {talker:?}");
            previous = talker.total_rate;
        }
        let (unattributed_rx, unattributed_tx) = snapshot.unattributed;
        assert!(unattributed_rx >= 0.0 && unattributed_tx >= 0.0);
    }
}

#[test]
fn bandwidth_rows_from_fabricated_snapshot() {
    // Deterministic row-shape pinning: no live `ss`, no stat-file races.
    let snapshot = net::BandwidthSnapshot {
        iface: Some(("wlp15s0".to_string(), 44_000.0, 1_100_000.0)),
        talkers: vec![net::ProcessBandwidth {
            pid: 2237,
            comm: "brave".to_string(),
            rx_rate: 1000.0,
            tx_rate: 523_000.0,
            total_rate: 524_000.0,
            share: 0.46,
        }],
        unattributed: (43_000.0, 577_000.0),
    };
    let rows = net_provider::build_bandwidth_rows(&snapshot);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0].id.as_str(), "iface:wlp15s0");
    assert!(
        rows[0]
            .meta
            .as_deref()
            .is_some_and(|m| m.contains("(interface total)")),
        "{:?}",
        rows[0]
    );
    assert!(!rows[0].confirmable);
    assert!(rows[0].volume.is_none());
    assert_eq!(rows[1].id.as_str(), "proc:2237");
    assert!(rows[1].confirmable, "kill actions still armed");
    assert_eq!(
        rows[1].volume, None,
        "share is a text gauge, row stays compact"
    );
    let talker_meta = rows[1].meta.as_deref().expect("talker meta");
    assert!(
        talker_meta.contains("46%") && talker_meta.contains("[████░░░░]"),
        "wire-share gauge in meta: {talker_meta:?}"
    );
    assert_eq!(rows[2].id.as_str(), "noop");
    assert!(rows[2].label.contains("unattributed"), "{:?}", rows[2]);
    assert!(!rows[2].confirmable);
    assert!(
        net::execute(rows[0].id.as_str()).is_ok() && net::execute(rows[2].id.as_str()).is_ok(),
        "header and remainder are safe no-ops"
    );

    // Fully attributed: remainder row disappears.
    let clean = net::BandwidthSnapshot {
        unattributed: (0.0, 0.0),
        ..snapshot.clone()
    };
    let rows = net_provider::build_bandwidth_rows(&clean);
    assert_eq!(rows.len(), 2, "{rows:?}");

    // Offline with nothing: the placeholder, as before.
    let empty = net::BandwidthSnapshot::default();
    let rows = net_provider::build_bandwidth_rows(&empty);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_str(), "noop");
}

#[test]
fn interfaces_tab_rows_have_metadata_and_targets() {
    let tab = net_provider::interfaces_tab();
    assert_eq!(tab.name, net_provider::TAB_INTERFACES);
    assert!(tab.filterable);
    assert!(!tab.deletable);
    for row in &tab.rows {
        if row.id.as_str() != "noop" {
            assert!(row.meta.is_some(), "row has meta: {row:?}");
        }
    }
}

#[test]
fn execute_noop_is_ok() {
    assert!(net::execute("noop").is_ok());
}
