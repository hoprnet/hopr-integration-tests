//! End-to-end traffic overhead and latency regression benchmark suite.
//!
//! Runs three isolated VPN workloads over a 1-hop `Segmentation | NoDelay` session on a 3-node
//! local cluster with `edgli` as the entry, scraping Prometheus packet counters on every cluster
//! node (`hopr_packets_count`, `hopr_protocol_ack_sent_count`, and
//! `hopr_protocol_ack_batches_sent_count` when present) and in-process entry session counters
//! before and after each workload:
//!
//! 1. `tcp_download_1hop`: asymmetric 100 MB bulk download (`1,000 pkt/s × 1,420 B` replies from
//!    an external UDP service + `500 pkt/s × 148 B` upstream TCP-ACK-sized datagrams from Entry).
//!    Measures TTFB, delivery %, throughput, Exit/Relay HOPR packet and ack-batch counts, and
//!    entry SURBs produced against configurable upper bounds.
//! 2. `loopback_1hop_full_mtu`: symmetric 4 MiB full-MTU (`1,452 B`) tagged loopback. Verifies
//!    record integrity (zero foreign/unattributed bytes; SHA-256 on complete transfer) and
//!    asserts Exit HOPR packet counts against a configurable upper bound.
//! 3. `interactive_1hop`: two 400 B echo-probe phases — sparse stop-and-wait (`100 ms` spacing,
//!    idle-gap path) and open-loop burst (`6 ms` spacing ≈ 167 pkt/s, exercising burst packing
//!    and stream coalescing windows concurrently) — verifying exact byte-for-byte echo integrity
//!    across phases (discarding any late echo from an earlier timed-out probe cleanly) and
//!    asserting RTT `p50` / `p95` bounds.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use edgli::hopr_lib::exports::transport::{SESSION_MTU, SessionTarget, SurbBalancerConfig};
use hoprd_integration_test::{
    Address, IntegrationEnv, cluster,
    origination::{self, OriginationSample},
    pump::{PumpOpts, drain_until_quiet, pump_halves, tagged_payload},
    session_metrics::{self, SessionCounters},
    step, udp_service,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, ReadHalf, WriteHalf};

const PUMP_TIMEOUT: Duration = Duration::from_secs(180);
const MIN_ARRIVAL_PCT: f64 = 99.0;
const HOPR_PACKET_BYTES: u64 = 3_669;

const DL_DATAGRAM: usize = 1420;
const DL_RATE_PPS: u64 = 1_000;
const DL_TARGET_BYTES: u64 = 100 * 1_000_000; // 100 MB
const DL_DATAGRAMS: u64 = DL_TARGET_BYTES.div_ceil(DL_DATAGRAM as u64);
const DL_TOTAL_BYTES: u64 = DL_DATAGRAMS * DL_DATAGRAM as u64; // 100,000,660 B (~100 MB)
const DL_DURATION_SECS: u64 = DL_DATAGRAMS.div_ceil(DL_RATE_PPS);

const ACK_DATAGRAM: usize = 148;
const ACK_RATE_PPS: u64 = 500;

const LOOPBACK_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

const INTERACTIVE_PROBES: usize = 30;
const INTERACTIVE_PAYLOAD_BYTES: usize = 400;
const INTERACTIVE_SPARSE_SPACING: Duration = Duration::from_millis(100);
const INTERACTIVE_BURST_SPACING: Duration = Duration::from_millis(6);

const SETTLE_BETWEEN_SCENARIOS: Duration = Duration::from_secs(2);

const ACK_BATCHES_METRIC: &str = "hopr_protocol_ack_batches_sent_count";

/// Default regression ceilings for packed, SURB-piggybacked, and ack-batched transports.
/// Override via `HOPR_MAX_*` environment variables when measuring an unoptimized baseline.
const DEFAULT_MAX_EXIT_PKTS_DL: u64 = 60_000;
const DEFAULT_MAX_SURBS_PRODUCED_DL: u64 = 55_000;
const DEFAULT_MAX_EXIT_ACK_BATCHES_DL: u64 = 1_000;
const DEFAULT_MAX_EXIT_PKTS_LOOPBACK: u64 = 4_500;
const DEFAULT_MAX_INTERACTIVE_P50_MS: u64 = 120;
const DEFAULT_MAX_INTERACTIVE_P95_MS: u64 = 250;

#[derive(Debug, Clone, Copy, Default)]
struct NodeMetrics {
    orig: OriginationSample,
    ack_batches_sent: Option<u64>,
}

struct ClusterSnapshot {
    nodes: HashMap<Address, NodeMetrics>,
    entry: SessionCounters,
}

#[derive(Debug, Clone, Copy)]
struct OverheadDelta {
    exit_sent: u64,
    exit_recv: u64,
    exit_acks_sent: u64,
    exit_ack_batches_sent: Option<u64>,
    relays_forwarded: u64,
    relays_acks_sent: u64,
    relays_ack_batches_sent: Option<u64>,
    entry_packets_in: u64,
    entry_surbs_produced: u64,
}

impl OverheadDelta {
    /// Exit data + SURB keep-alive HOPR packets (`sent + received`).
    ///
    /// `exit_acks_sent` (`hopr_protocol_ack_sent_count`) counts individual acknowledgements
    /// (up to 33 per ack-batch HOPR packet), while `exit_ack_batches_sent`
    /// (`hopr_protocol_ack_batches_sent_count`, when exposed) counts actual 3,669 B ack-batch
    /// HOPR packets.
    fn exit_data_pkts(&self) -> u64 {
        self.exit_sent + self.exit_recv
    }

    fn est_exit_hopr_bytes(&self) -> u64 {
        (self.exit_data_pkts() + self.exit_ack_batches_sent.unwrap_or(0)) * HOPR_PACKET_BYTES
    }
}

/// Parses an unlabelled scalar Prometheus counter if present in `body`.
fn parse_optional_counter(body: &str, metric: &str) -> Option<u64> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix(metric) else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some(raw) = rest.split_whitespace().next()
            && let Ok(v) = raw.parse::<f64>()
        {
            return Some(v.max(0.0).round() as u64);
        }
    }
    None
}

async fn snapshot_cluster(env: &IntegrationEnv) -> anyhow::Result<ClusterSnapshot> {
    let cluster = env.cluster()?;
    let mut nodes = HashMap::with_capacity(cluster.nodes.len());
    for node in &cluster.nodes {
        let body = cluster::scrape_metrics(node).await?;
        let orig = origination::parse(&body);
        let ack_batches_sent = parse_optional_counter(&body, ACK_BATCHES_METRIC);
        nodes.insert(
            node.address,
            NodeMetrics {
                orig,
                ack_batches_sent,
            },
        );
    }
    Ok(ClusterSnapshot {
        nodes,
        entry: session_metrics::sample(),
    })
}

fn diff_cluster(before: &ClusterSnapshot, after: &ClusterSnapshot, exit: Address) -> OverheadDelta {
    let mut delta = OverheadDelta {
        exit_sent: 0,
        exit_recv: 0,
        exit_acks_sent: 0,
        exit_ack_batches_sent: None,
        relays_forwarded: 0,
        relays_acks_sent: 0,
        relays_ack_batches_sent: None,
        entry_packets_in: after
            .entry
            .packets_in()
            .unwrap_or(0)
            .saturating_sub(before.entry.packets_in().unwrap_or(0)),
        entry_surbs_produced: after
            .entry
            .get("hopr_session_surb_produced_total")
            .unwrap_or(0)
            .saturating_sub(
                before
                    .entry
                    .get("hopr_session_surb_produced_total")
                    .unwrap_or(0),
            ),
    };

    for (addr, end) in &after.nodes {
        let start = before.nodes.get(addr).copied().unwrap_or_default();
        if *addr == exit {
            delta.exit_sent = end.orig.sent.saturating_sub(start.orig.sent);
            delta.exit_recv = end.orig.received.saturating_sub(start.orig.received);
            delta.exit_acks_sent = end.orig.acks_sent.saturating_sub(start.orig.acks_sent);
            delta.exit_ack_batches_sent = end
                .ack_batches_sent
                .map(|e| e.saturating_sub(start.ack_batches_sent.unwrap_or(0)));
        } else {
            delta.relays_forwarded += end.orig.forwarded.saturating_sub(start.orig.forwarded);
            delta.relays_acks_sent += end.orig.acks_sent.saturating_sub(start.orig.acks_sent);
            if let Some(e) = end.ack_batches_sent {
                let d = e.saturating_sub(start.ack_batches_sent.unwrap_or(0));
                *delta.relays_ack_batches_sent.get_or_insert(0) += d;
            }
        }
    }
    delta
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = (((sorted.len() - 1) as f64) * pct).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

async fn run_tcp_download(env: &IntegrationEnv) -> anyhow::Result<()> {
    let exit = env.dest_for(1)?;
    let before = snapshot_cluster(env).await?;

    let service = udp_service::spawn(udp_service::Mode::Push {
        datagram: DL_DATAGRAM,
        rate: DL_RATE_PPS,
        total_bytes: DL_TOTAL_BYTES,
    })
    .await?;

    // Use the library's default SURB balancer configuration: the initial buffer covers a fraction
    // of the 100 MB download so active SURB replenishment is exercised throughout the transfer.
    let (session, exit_addr) = env
        .open_unreliable_session_to(1, service.target(), Some(SurbBalancerConfig::default()))
        .await?;
    assert_eq!(exit, exit_addr);
    let (mut rx, mut tx) = tokio::io::split(session);

    tracing::info!(
        %exit,
        service = %service.addr(),
        dl_bytes = DL_TOTAL_BYTES,
        dl_rate_pps = DL_RATE_PPS,
        ack_rate_pps = ACK_RATE_PPS,
        "tcp_download_1hop: starting asymmetric download + upstream 148B ACKs"
    );

    // Kick off the UDP push service with one request datagram and time TTFB.
    let started = Instant::now();
    tx.write_all(b"start").await?;
    tx.flush().await?;

    let done = Arc::new(AtomicBool::new(false));
    let done_tx = done.clone();

    // Concurrent upstream TCP-ACK-like stream (1 ACK per 2 downstream data segments).
    let ack_sender = async move {
        let ack_payload = vec![0x5Au8; ACK_DATAGRAM];
        let mut interval = tokio::time::interval(Duration::from_micros(1_000_000 / ACK_RATE_PPS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut sent_acks = 0u64;
        while !done_tx.load(Ordering::Relaxed) && sent_acks < ACK_RATE_PPS * (DL_DURATION_SECS + 30)
        {
            interval.tick().await;
            tx.write_all(&ack_payload).await?;
            tx.flush().await?;
            sent_acks += 1;
        }
        Ok::<(u64, _), anyhow::Error>((sent_acks, tx))
    };

    struct DoneOnDrop(Arc<AtomicBool>);
    impl Drop for DoneOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    // Concurrent download reader: verify every received byte matches `PUSH_FILL`.
    let service_ref = &service;
    let dl_reader = async move {
        let _done_guard = DoneOnDrop(done);
        let mut buf = vec![0u8; 64 * 1024];
        let mut received: u64 = 0;
        let mut ttfb: Option<Duration> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(DL_DURATION_SECS + 30);
        while received < DL_TOTAL_BYTES {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                break;
            }
            match tokio::time::timeout(rem.min(Duration::from_secs(5)), rx.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    if ttfb.is_none() {
                        ttfb = Some(started.elapsed());
                    }
                    anyhow::ensure!(
                        buf[..n].iter().all(|&b| b == udp_service::PUSH_FILL),
                        "tcp_download_1hop: corrupted byte in download payload"
                    );
                    received += n as u64;
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    if service_ref.sent() >= DL_TOTAL_BYTES {
                        break;
                    }
                }
            }
        }
        let elapsed = started.elapsed();
        Ok::<(u64, Option<Duration>, Duration), anyhow::Error>((received, ttfb, elapsed))
    };

    let (ack_res, dl_res) = tokio::join!(ack_sender, dl_reader);
    let (acks_sent, mut tx) = ack_res?;
    let (dl_received, ttfb, dl_elapsed) = dl_res?;
    let _ = tx.shutdown().await;
    drop(tx);
    tokio::time::sleep(SETTLE_BETWEEN_SCENARIOS).await;

    let after = snapshot_cluster(env).await?;
    let delta = diff_cluster(&before, &after, exit);

    let dl_pct = (dl_received as f64 / DL_TOTAL_BYTES as f64) * 100.0;
    let dl_mb_s = dl_received as f64 / dl_elapsed.as_secs_f64().max(1e-6) / 1_000_000.0;
    let ack_bytes_sent = acks_sent * ACK_DATAGRAM as u64 + 5;
    let ack_bytes_recv = service.received();
    let ack_pct = (ack_bytes_recv as f64 / ack_bytes_sent as f64) * 100.0;
    let ttfb =
        ttfb.ok_or_else(|| anyhow::anyhow!("tcp_download_1hop: never received first byte"))?;
    let app_bytes = dl_received + ack_bytes_recv;
    let hopr_over_app = delta.est_exit_hopr_bytes() as f64 / app_bytes.max(1) as f64;

    tracing::info!(
        dl_received,
        dl_pushed = service.sent(),
        dl_arrival_pct = format!("{dl_pct:.2}"),
        dl_elapsed_s = format!("{:.2}", dl_elapsed.as_secs_f64()),
        dl_mb_s = format!("{dl_mb_s:.2}"),
        acks_sent,
        ack_bytes_recv,
        ack_arrival_pct = format!("{ack_pct:.2}"),
        ttfb_ms = ttfb.as_millis() as u64,
        exit_sent = delta.exit_sent,
        exit_recv = delta.exit_recv,
        exit_acks_sent = delta.exit_acks_sent,
        exit_ack_batches_sent = ?delta.exit_ack_batches_sent,
        exit_data_pkts = delta.exit_data_pkts(),
        relays_forwarded = delta.relays_forwarded,
        relays_ack_batches_sent = ?delta.relays_ack_batches_sent,
        entry_packets_in = delta.entry_packets_in,
        entry_surbs_produced = delta.entry_surbs_produced,
        hopr_over_app_ratio = format!("{hopr_over_app:.2}x"),
        "tcp_download_1hop: finished"
    );

    assert!(
        dl_pct >= MIN_ARRIVAL_PCT,
        "tcp_download_1hop: download arrival {dl_pct:.2}% < {MIN_ARRIVAL_PCT:.0}%"
    );
    assert!(
        ack_pct >= 95.0,
        "tcp_download_1hop: upstream ACK arrival {ack_pct:.2}% < 95%"
    );
    assert!(
        ttfb < Duration::from_secs(5),
        "tcp_download_1hop: TTFB {ttfb:?} exceeded 5 s"
    );
    assert!(
        delta.exit_sent > 0 && delta.exit_recv > 0,
        "tcp_download_1hop: expected non-zero exit packet metrics, got {delta:?}"
    );
    let max_exit_pkts = env_u64("HOPR_MAX_EXIT_PKTS_DL", DEFAULT_MAX_EXIT_PKTS_DL);
    assert!(
        delta.exit_data_pkts() <= max_exit_pkts,
        "tcp_download_1hop: exit data HOPR packets {} exceeded bound {max_exit_pkts} ({delta:?})",
        delta.exit_data_pkts()
    );
    if after
        .entry
        .get("hopr_session_surb_produced_total")
        .is_some()
    {
        let max_surbs = env_u64("HOPR_MAX_SURBS_PRODUCED_DL", DEFAULT_MAX_SURBS_PRODUCED_DL);
        assert!(
            delta.entry_surbs_produced > 0 && delta.entry_surbs_produced <= max_surbs,
            "tcp_download_1hop: entry_surbs_produced {} outside 1..={max_surbs} ({delta:?})",
            delta.entry_surbs_produced
        );
    }
    if let Some(ack_batches) = delta.exit_ack_batches_sent {
        let max_ack_batches = env_u64(
            "HOPR_MAX_EXIT_ACK_BATCHES_DL",
            DEFAULT_MAX_EXIT_ACK_BATCHES_DL,
        );
        assert!(
            ack_batches > 0 && ack_batches <= max_ack_batches,
            "tcp_download_1hop: exit_ack_batches_sent {ack_batches} outside 1..={max_ack_batches}"
        );
    }
    Ok(())
}

async fn run_loopback_full_mtu(env: &IntegrationEnv) -> anyhow::Result<()> {
    let exit = env.dest_for(1)?;
    let (session, _) = env
        .open_unreliable_session_to(
            1,
            SessionTarget::ExitNode(0),
            Some(SurbBalancerConfig::default()),
        )
        .await?;
    let (mut rx, mut tx) = tokio::io::split(session);

    let warm_up = tagged_payload(0xF0, 64 * 1024);
    pump_halves(
        &mut rx,
        &mut tx,
        &warm_up,
        "warm-up",
        PUMP_TIMEOUT,
        PumpOpts {
            phase: Some(0xF0),
            ..Default::default()
        },
    )
    .await?;
    drain_until_quiet(&mut rx, Duration::from_secs(2), "warm-up").await;

    let before = snapshot_cluster(env).await?;
    let payload = tagged_payload(0xB2, LOOPBACK_BYTES);

    let t = pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "loopback_1hop_full_mtu",
        PUMP_TIMEOUT,
        PumpOpts {
            phase: Some(0xB2),
            chunk: Some(SESSION_MTU),
            ..Default::default()
        },
    )
    .await?;

    let _ = tx.shutdown().await;
    drop(tx);
    drop(rx);
    tokio::time::sleep(SETTLE_BETWEEN_SCENARIOS).await;

    let after = snapshot_cluster(env).await?;
    let delta = diff_cluster(&before, &after, exit);

    tracing::info!(
        arrival_pct = format!("{:.2}", t.arrival_pct()),
        sha_ok = t.sha_ok,
        attributed_bytes = t.attributed_bytes,
        received_bytes = t.received_bytes,
        foreign_bytes = t.foreign_bytes,
        exit_sent = delta.exit_sent,
        exit_recv = delta.exit_recv,
        exit_acks_sent = delta.exit_acks_sent,
        exit_ack_batches_sent = ?delta.exit_ack_batches_sent,
        exit_data_pkts = delta.exit_data_pkts(),
        relays_forwarded = delta.relays_forwarded,
        "loopback_1hop_full_mtu: finished"
    );

    assert!(
        t.arrival_pct() >= MIN_ARRIVAL_PCT,
        "loopback_1hop_full_mtu: arrival {:.2}% < {MIN_ARRIVAL_PCT:.0}%",
        t.arrival_pct()
    );
    assert_eq!(
        0, t.foreign_bytes,
        "loopback_1hop_full_mtu: unexpected foreign phase bytes"
    );
    assert_eq!(
        t.received_bytes, t.attributed_bytes,
        "loopback_1hop_full_mtu: unattributed/corrupted bytes in stream"
    );
    if t.received_bytes == t.sent_bytes {
        assert!(
            t.sha_ok,
            "loopback_1hop_full_mtu: SHA-256 mismatch on complete transfer"
        );
    }
    assert!(
        delta.exit_sent > 0 && delta.exit_recv > 0,
        "loopback_1hop_full_mtu: expected non-zero exit packet metrics, got {delta:?}"
    );
    let max_exit_pkts = env_u64(
        "HOPR_MAX_EXIT_PKTS_LOOPBACK",
        DEFAULT_MAX_EXIT_PKTS_LOOPBACK,
    );
    assert!(
        delta.exit_data_pkts() <= max_exit_pkts,
        "loopback_1hop_full_mtu: exit data HOPR packets {} exceeded bound {max_exit_pkts} ({delta:?})",
        delta.exit_data_pkts()
    );
    Ok(())
}

fn build_probe(phase_tag: u8, seq: u64) -> Vec<u8> {
    let mut probe = vec![(seq as u8).wrapping_mul(7).wrapping_add(1); INTERACTIVE_PAYLOAD_BYTES];
    probe[0] = phase_tag;
    probe[1..9].copy_from_slice(&seq.to_le_bytes());
    probe
}

/// Stop-and-wait sparse probes (`spacing = 100 ms` > burst window): exercises the idle-gap
/// immediate flush path and records every sent `(phase_tag, seq)` in `seen_sent` so late echoes
/// arriving in a subsequent phase are discarded cleanly instead of treated as corruption.
async fn run_sparse_echo_probes(
    rx: &mut ReadHalf<hoprd_integration_test::HoprSession>,
    tx: &mut WriteHalf<hoprd_integration_test::HoprSession>,
    phase_tag: u8,
    probes: usize,
    spacing: Duration,
    seen_sent: &mut HashSet<(u8, u64)>,
) -> anyhow::Result<(Vec<Duration>, usize)> {
    let mut rbuf = vec![0u8; 2048];
    let mut rtts = Vec::with_capacity(probes);
    let mut late_discarded = 0usize;

    for seq in 0..probes as u64 {
        let probe = build_probe(phase_tag, seq);
        seen_sent.insert((phase_tag, seq));

        let sent_at = Instant::now();
        tx.write_all(&probe).await?;
        tx.flush().await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                tracing::warn!(seq, "interactive_sparse_100ms: probe timed out");
                break;
            }
            match tokio::time::timeout(rem, rx.read(&mut rbuf)).await {
                Ok(Ok(n)) if n == probe.len() && rbuf[..n] == probe[..] => {
                    rtts.push(sent_at.elapsed());
                    break;
                }
                Ok(Ok(n)) if n == INTERACTIVE_PAYLOAD_BYTES => {
                    let r_phase = rbuf[0];
                    let r_seq = u64::from_le_bytes(rbuf[1..9].try_into().unwrap());
                    let expected = build_probe(r_phase, r_seq);
                    if seen_sent.contains(&(r_phase, r_seq)) && rbuf[..n] == expected[..] {
                        late_discarded += 1;
                        continue;
                    }
                    anyhow::bail!(
                        "interactive_sparse_100ms: probe {seq} received corrupted/unknown reply (phase={r_phase:#x}, seq={r_seq})"
                    );
                }
                Ok(Ok(n)) => {
                    anyhow::bail!(
                        "interactive_sparse_100ms: probe {seq} echo corruption (expected {} B, got {n} B)",
                        probe.len()
                    );
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    tracing::warn!(seq, "interactive_sparse_100ms: probe timed out");
                    break;
                }
            }
        }

        let spent = sent_at.elapsed();
        if let Some(rem) = spacing.checked_sub(spent) {
            tokio::time::sleep(rem).await;
        }
    }

    Ok((rtts, late_discarded))
}

/// Open-loop burst probes (`spacing = 6 ms`, inside both the 10 ms pack window and 8 ms QUIC
/// coalesce window): sends probes on a fixed interval without waiting for replies, while a
/// concurrent reader matches replies by `(phase_tag, seq)` and computes per-probe RTTs.
async fn run_open_loop_burst_probes(
    mut rx: ReadHalf<hoprd_integration_test::HoprSession>,
    mut tx: WriteHalf<hoprd_integration_test::HoprSession>,
    phase_tag: u8,
    probes: usize,
    spacing: Duration,
    prior_sent: &HashSet<(u8, u64)>,
) -> anyhow::Result<(
    ReadHalf<hoprd_integration_test::HoprSession>,
    WriteHalf<hoprd_integration_test::HoprSession>,
    Vec<Duration>,
    usize,
)> {
    let sent_times = Arc::new(tokio::sync::Mutex::new(
        HashMap::<u64, Instant>::with_capacity(probes),
    ));
    let sent_times_tx = sent_times.clone();

    let sender = async move {
        let mut interval = tokio::time::interval(spacing);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        for seq in 0..probes as u64 {
            interval.tick().await;
            let probe = build_probe(phase_tag, seq);
            sent_times_tx.lock().await.insert(seq, Instant::now());
            tx.write_all(&probe).await?;
            tx.flush().await?;
        }
        Ok::<_, anyhow::Error>(tx)
    };

    let reader = async move {
        let mut rbuf = vec![0u8; 2048];
        let mut rtts = Vec::with_capacity(probes);
        let mut seen_replies = HashSet::<u64>::with_capacity(probes);
        let mut late_discarded = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        while seen_replies.len() < probes {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                break;
            }
            match tokio::time::timeout(rem.min(Duration::from_secs(2)), rx.read(&mut rbuf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) if n == INTERACTIVE_PAYLOAD_BYTES => {
                    let r_phase = rbuf[0];
                    let r_seq = u64::from_le_bytes(rbuf[1..9].try_into().unwrap());
                    let expected = build_probe(r_phase, r_seq);
                    if rbuf[..n] != expected[..] {
                        anyhow::bail!(
                            "interactive_burst_6ms: corrupted payload for phase={r_phase:#x} seq={r_seq}"
                        );
                    }
                    if r_phase == phase_tag && (r_seq as usize) < probes {
                        if seen_replies.insert(r_seq)
                            && let Some(sent_at) = sent_times.lock().await.get(&r_seq).copied()
                        {
                            rtts.push(sent_at.elapsed());
                        }
                    } else if prior_sent.contains(&(r_phase, r_seq)) {
                        late_discarded += 1;
                    } else {
                        anyhow::bail!(
                            "interactive_burst_6ms: unexpected reply phase={r_phase:#x} seq={r_seq}"
                        );
                    }
                }
                Ok(Ok(n)) => {
                    anyhow::bail!(
                        "interactive_burst_6ms: unexpected reply size {n} B (expected {INTERACTIVE_PAYLOAD_BYTES} B)"
                    );
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => break,
            }
        }
        Ok::<_, anyhow::Error>((rx, rtts, late_discarded))
    };

    let (tx_res, rx_res) = tokio::join!(sender, reader);
    let tx = tx_res?;
    let (rx, rtts, late_discarded) = rx_res?;
    Ok((rx, tx, rtts, late_discarded))
}

async fn run_interactive(env: &IntegrationEnv) -> anyhow::Result<()> {
    let exit = env.dest_for(1)?;
    let (session, _) = env
        .open_unreliable_session_to(
            1,
            SessionTarget::ExitNode(0),
            Some(SurbBalancerConfig::default()),
        )
        .await?;
    let (mut rx, mut tx) = tokio::io::split(session);

    // Warm up one probe so session establishment / initial SURB pre-fill does not distort RTT.
    let warmup = vec![0xEEu8; INTERACTIVE_PAYLOAD_BYTES];
    tx.write_all(&warmup).await?;
    tx.flush().await?;
    let mut rbuf = vec![0u8; 2048];
    let n = tokio::time::timeout(Duration::from_secs(10), rx.read(&mut rbuf))
        .await
        .map_err(|_| anyhow::anyhow!("interactive_1hop: warmup echo timed out"))??;
    anyhow::ensure!(
        n == warmup.len() && rbuf[..n] == warmup[..],
        "interactive_1hop: warmup mismatch"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    let before = snapshot_cluster(env).await?;
    let mut prior_sent = HashSet::new();

    let (mut sparse_rtts, sparse_late) = run_sparse_echo_probes(
        &mut rx,
        &mut tx,
        0xA1,
        INTERACTIVE_PROBES,
        INTERACTIVE_SPARSE_SPACING,
        &mut prior_sent,
    )
    .await?;
    drain_until_quiet(&mut rx, Duration::from_millis(300), "interactive-gap").await;

    let (rx, mut tx, mut burst_rtts, burst_late) = run_open_loop_burst_probes(
        rx,
        tx,
        0xA2,
        INTERACTIVE_PROBES,
        INTERACTIVE_BURST_SPACING,
        &prior_sent,
    )
    .await?;

    let _ = tx.shutdown().await;
    drop(tx);
    drop(rx);
    tokio::time::sleep(SETTLE_BETWEEN_SCENARIOS).await;

    let after = snapshot_cluster(env).await?;
    let delta = diff_cluster(&before, &after, exit);

    sparse_rtts.sort_unstable();
    burst_rtts.sort_unstable();
    let sparse_p50 = percentile(&sparse_rtts, 0.50);
    let sparse_p95 = percentile(&sparse_rtts, 0.95);
    let burst_p50 = percentile(&burst_rtts, 0.50);
    let burst_p95 = percentile(&burst_rtts, 0.95);

    tracing::info!(
        sparse_received = sparse_rtts.len(),
        sparse_late,
        sparse_p50_ms = sparse_p50.as_millis() as u64,
        sparse_p95_ms = sparse_p95.as_millis() as u64,
        burst_received = burst_rtts.len(),
        burst_late,
        burst_p50_ms = burst_p50.as_millis() as u64,
        burst_p95_ms = burst_p95.as_millis() as u64,
        exit_sent = delta.exit_sent,
        exit_recv = delta.exit_recv,
        exit_acks_sent = delta.exit_acks_sent,
        exit_ack_batches_sent = ?delta.exit_ack_batches_sent,
        relays_forwarded = delta.relays_forwarded,
        "interactive_1hop: finished"
    );

    let max_p50 = Duration::from_millis(env_u64(
        "HOPR_MAX_INTERACTIVE_P50_MS",
        DEFAULT_MAX_INTERACTIVE_P50_MS,
    ));
    let max_p95 = Duration::from_millis(env_u64(
        "HOPR_MAX_INTERACTIVE_P95_MS",
        DEFAULT_MAX_INTERACTIVE_P95_MS,
    ));

    for (label, rtts, p50, p95) in [
        ("sparse_100ms", &sparse_rtts, sparse_p50, sparse_p95),
        ("burst_6ms", &burst_rtts, burst_p50, burst_p95),
    ] {
        assert!(
            rtts.len() >= INTERACTIVE_PROBES - 1,
            "interactive_1hop ({label}): only {}/{INTERACTIVE_PROBES} echo probes returned",
            rtts.len()
        );
        assert!(
            p50 <= max_p50,
            "interactive_1hop ({label}): RTT p50 {p50:?} exceeded {max_p50:?}"
        );
        assert!(
            p95 <= max_p95,
            "interactive_1hop ({label}): RTT p95 {p95:?} exceeded {max_p95:?}"
        );
    }
    assert!(
        delta.exit_sent > 0 && delta.exit_recv > 0,
        "interactive_1hop: expected non-zero exit packet metrics, got {delta:?}"
    );
    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a running chain"]
async fn measure_traffic_overhead() -> anyhow::Result<()> {
    let env = step("setup", IntegrationEnv::setup()).await?;
    step("tcp_download_1hop", run_tcp_download(&env)).await?;
    step("loopback_1hop_full_mtu", run_loopback_full_mtu(&env)).await?;
    step("interactive_1hop", run_interactive(&env)).await
}
