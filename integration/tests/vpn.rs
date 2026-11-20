//! gnosis_vpn-testenv's regression suite on the HOPR layer alone: gnosis_vpn-client's WireGuard
//! session ([`IntegrationEnv::open_vpn_session`]) carrying what the tunnel would, judged by testenv's
//! thresholds. Each test names the `tests/regression/test_tNN_*.py` it replaces.
#![cfg(feature = "v5")]

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use edgli::hopr_lib::exports::transport::{SessionCapabilities, SessionCapability};
use hoprd_integration_test::{
    HoprSession, IntegrationEnv,
    cluster::{NodeInfo, request_latency_profile, scrape_metrics},
    env::{VPN_MAIN_TIER, vpn_surb_tier},
    pump::{PumpOpts, Shape, Transfer, pace_for_rate_with_chunk, pump_halves},
    step,
    udp_service::{self, Mode, PROBE_MAGIC, PUSH_FILL, UdpService, push_request},
};
use rand::RngExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, ReadHalf, WriteHalf};

/// Link rules need the localcluster's latency relays, which only run with a latency profile.
const FLAT_LATENCY: &str = "default: \"1ms\"\n";
/// A full WireGuard data message at the client's 1420 B tunnel MTU.
const WG_DATAGRAM: usize = 1420 + 32;
/// testenv's probes send 1200 B of UDP payload; inside the tunnel that is 1200 + IP/UDP + WireGuard.
const PROBE_DATAGRAM: usize = 1200 + 28 + 32;
/// testenv's `SURB_RAMP_WAIT`: the idle after connect before anything is measured.
const RAMP_WAIT: Duration = Duration::from_secs(25);
/// How long the reader keeps going after the offer ends, for the last returns.
const READ_TAIL: Duration = Duration::from_secs(3);

const LOSS_MAX_PCT: f64 = 5.0;
const STALL_MAX_S: f64 = 5.0;
/// A probe that sent less than this share of its packets did not run; its loss figure is noise.
const SAMPLE_MIN_PCT: f64 = 80.0;
const CALL_BITS: u64 = 1_500_000;
const STREAM_BITS: u64 = 3_000_000;
/// A probe every 100 ms, for round-trip times next to a load.
const RTT_PROBE_BITS: u64 = PROBE_DATAGRAM as u64 * 8 * 10;

/// A T04-style transfer: 10 MB offered as WireGuard datagrams at `BULK_MBIT`.
const BULK_BYTES: usize = 10_000_000;
const BULK_MBIT: f64 = 12.0;
/// testenv's `DOWN_MIN_MBIT`: catches a halving of the rate a healthy stack reaches.
const BULK_FLOOR_MBIT: f64 = 7.0;
const BULK_TIMEOUT: Duration = Duration::from_secs(240);
const ARRIVAL_MIN_PCT: f64 = 99.0;
/// On its ping SURB tier a fresh session carries ~0.65 Mbit/s of a 12 Mbit/s blast (7 % came
/// back locally). testenv gates a cold start only on collapse: something arrives, no long stall.
const COLD_ARRIVAL_MIN_PCT: f64 = 1.0;
/// Sessions sharing the exit offer 4 × 12 Mbit/s at a fixed rate, so they judge on stream loss.
const SHARED_ARRIVAL_MIN_PCT: f64 = 100.0 - LOSS_MAX_PCT;

// ─────────────────────────────────────────────────────────────────────────────
// The client, its far end and what one window of traffic did
// ─────────────────────────────────────────────────────────────────────────────

async fn vpn_env() -> anyhow::Result<IntegrationEnv> {
    request_latency_profile(FLAT_LATENCY);
    // A test that failed mid-impairment leaves its rules behind; the next one starts clean.
    IntegrationEnv::setup_with(|cluster| {
        cluster.clear_link_rules()?;
        Ok(None)
    })
    .await
}

fn wg_capabilities() -> SessionCapabilities {
    SessionCapability::Segmentation | SessionCapability::NoDelay
}

async fn open(env: &IntegrationEnv, far_end: &UdpService) -> anyhow::Result<HoprSession> {
    env.open_vpn_session(
        far_end.target(),
        wg_capabilities(),
        vpn_surb_tier(VPN_MAIN_TIER),
    )
    .await
}

async fn open_warm(
    env: &IntegrationEnv,
    far_end: &UdpService,
) -> anyhow::Result<(ReadHalf<HoprSession>, WriteHalf<HoprSession>)> {
    let session = open(env, far_end).await?;
    tokio::time::sleep(RAMP_WAIT).await;
    Ok(tokio::io::split(session))
}

fn exit_node(env: &IntegrationEnv) -> anyhow::Result<NodeInfo> {
    let exit = env.dest_for(1)?;
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == exit)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("exit {exit} is not a cluster node"))
}

/// Echoes `bytes` of random data through an [`Mode::Echo`] far end, `chunk` bytes a datagram, at the
/// datagram rate of `BULK_MBIT` in full-size WireGuard datagrams: packets, not bytes, cost SURBs.
async fn bulk(
    rx: &mut ReadHalf<HoprSession>,
    tx: &mut WriteHalf<HoprSession>,
    bytes: usize,
    chunk: usize,
    min_arrival_pct: f64,
    label: &str,
) -> anyhow::Result<Transfer> {
    let mut payload = vec![0u8; bytes];
    rand::rng().fill(&mut payload[..]);
    let t = pump_halves(
        rx,
        tx,
        &payload,
        label,
        BULK_TIMEOUT,
        PumpOpts {
            pace: pace_for_rate_with_chunk(
                BULK_MBIT / 8.0 * chunk as f64 / WG_DATAGRAM as f64,
                chunk,
            ),
            chunk: Some(chunk),
            ..PumpOpts::default()
        },
    )
    .await?;
    anyhow::ensure!(
        t.arrival_pct() >= min_arrival_pct
            && t.longest_stall() <= STALL_MAX_S
            && (t.received_bytes < t.sent_bytes || t.sha_ok),
        "{label}: {:.2} % came back (min {min_arrival_pct} %), stalled {:.1} s (max {STALL_MAX_S}), \
         outcome {:?}, sha_ok {}",
        t.arrival_pct(),
        t.longest_stall(),
        t.outcome,
        t.sha_ok
    );
    Ok(t)
}

/// Rate to 90 % of the payload in Mbit/s, so a few late stragglers do not move it.
fn mbit(t: &Transfer) -> f64 {
    t.throughput_at(0.9).unwrap_or(0.0) * 8.0
}

#[derive(Debug, Default)]
struct Window {
    duration_s: f64,
    /// An upload alone gets nothing back, so it has no first return or gap to judge.
    expects_returns: bool,
    probes_sent: u64,
    probes_back: u64,
    rtts_ms: Vec<f64>,
    upload_bytes: u64,
    push_expected: u64,
    fill_bytes: u64,
    first_arrival_s: Option<f64>,
    /// Longest silence after the first return, the tail to the end of the offer included.
    longest_gap_s: f64,
    /// Reads that were not exactly one datagram: `NoDelay` must keep their boundaries (T31).
    split_reads: u64,
    corrupt_bytes: u64,
}

/// Zero for a direction the window did not offer.
fn loss_pct(got: u64, of: u64) -> f64 {
    if of == 0 {
        return 0.0;
    }
    100.0 * (1.0 - got as f64 / of as f64).max(0.0)
}

impl Window {
    fn call_loss_pct(&self) -> f64 {
        loss_pct(self.probes_back, self.probes_sent)
    }

    fn download_loss_pct(&self) -> f64 {
        loss_pct(self.fill_bytes, self.push_expected)
    }

    /// Probes reach a [`Mode::Sink`] too, so they are taken out of what it counted.
    fn upload_loss_pct(&self, sink_received: u64, datagram: usize) -> f64 {
        let probes = self.probes_sent * datagram as u64;
        loss_pct(sink_received.saturating_sub(probes), self.upload_bytes)
    }

    fn sampled_pct(&self, call_bits: u64, datagram: usize) -> f64 {
        let expected = call_bits as f64 / 8.0 / datagram as f64 * self.duration_s;
        100.0 * self.probes_sent as f64 / expected.max(1.0)
    }

    /// Nearest-rank, as testenv computes it.
    fn rtt_ms(&self, q: f64) -> Option<f64> {
        let mut sorted = self.rtts_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let rank = ((q * sorted.len() as f64).ceil() as usize).max(1);
        sorted.get(rank - 1).copied()
    }

    fn summary(&self) -> String {
        format!(
            "call {}/{} back ({:.2} % lost), rtt p50 {:?} / p95 {:?} ms, upload {} B, download \
             {}/{} B ({:.2} % lost), first return {:?} s, longest gap {:.1} s, {} split reads, {} \
             corrupt bytes",
            self.probes_back,
            self.probes_sent,
            self.call_loss_pct(),
            self.rtt_ms(0.5).map(|v| v.round()),
            self.rtt_ms(0.95).map(|v| v.round()),
            self.upload_bytes,
            self.fill_bytes,
            self.push_expected,
            self.download_loss_pct(),
            self.first_arrival_s.map(|v| (v * 10.0).round() / 10.0),
            self.longest_gap_s,
            self.split_reads,
            self.corrupt_bytes,
        )
    }

    /// testenv's stream verdict: the stream ran, lost under `LOSS_MAX_PCT`, never went quiet for
    /// longer than `STALL_MAX_S`, and every datagram came back whole.
    fn check(&self, label: &str, loss: f64) -> anyhow::Result<()> {
        tracing::info!("{label}: {}", self.summary());
        anyhow::ensure!(
            loss < LOSS_MAX_PCT
                && (!self.expects_returns
                    || self.first_arrival_s.is_some_and(|s| s <= STALL_MAX_S))
                && self.longest_gap_s <= STALL_MAX_S
                && self.split_reads == 0
                && self.corrupt_bytes == 0,
            "{label}: {loss:.2} % lost (max {LOSS_MAX_PCT}), gaps up to {STALL_MAX_S} s allowed; {}",
            self.summary()
        );
        Ok(())
    }
}

/// Offers a call, an upload and a download (each off at 0 bit/s) of `datagram`-sized packets for
/// `duration`. The download is a push the [`Mode::Echo`] far end starts on request.
async fn drive(
    rx: &mut ReadHalf<HoprSession>,
    tx: &mut WriteHalf<HoprSession>,
    duration: Duration,
    call_bits: u64,
    upload_bits: u64,
    push_bits: u64,
    datagram: usize,
) -> anyhow::Result<Window> {
    let start = Instant::now();
    let mut window = Window {
        duration_s: duration.as_secs_f64(),
        expects_returns: call_bits > 0 || push_bits > 0,
        ..Window::default()
    };
    if push_bits > 0 {
        let rate = (push_bits / (8 * datagram as u64)).max(1);
        window.push_expected = push_bits / 8 * duration.as_secs();
        tx.write_all(&push_request(datagram, rate, window.push_expected))
            .await?;
    }
    let every = |bits: u64| {
        (bits > 0).then(|| Duration::from_secs_f64(datagram as f64 * 8.0 / bits as f64))
    };

    let send = async {
        let (call, upload) = (every(call_bits), every(upload_bits));
        let (mut next_call, mut next_upload) = (call.map(|_| start), upload.map(|_| start));
        let (mut probes, mut uploaded, mut seq) = (0u64, 0u64, 0u64);
        let upload_fill = vec![0x55u8; datagram];
        while let Some(at) = [next_call, next_upload].into_iter().flatten().min() {
            if at >= start + duration {
                break;
            }
            tokio::time::sleep_until(at.into()).await;
            if next_call == Some(at) {
                let mut probe = vec![0u8; datagram];
                probe[..4].copy_from_slice(&PROBE_MAGIC);
                probe[4..12].copy_from_slice(&seq.to_le_bytes());
                let sent_us = start.elapsed().as_micros() as u64;
                probe[12..20].copy_from_slice(&sent_us.to_le_bytes());
                tx.write_all(&probe).await?;
                seq += 1;
                probes += 1;
                next_call = call.map(|d| at + d);
            } else {
                tx.write_all(&upload_fill).await?;
                uploaded += datagram as u64;
                next_upload = upload.map(|d| at + d);
            }
        }
        tx.flush().await?;
        Ok::<_, std::io::Error>((probes, uploaded))
    };

    let recv = async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending: Vec<u8> = Vec::new();
        let mut last: Option<f64> = None;
        let end = start + duration + READ_TAIL;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Ok(read) = tokio::time::timeout(left, rx.read(&mut buf)).await else {
                break;
            };
            let n = read?;
            if n == 0 {
                tracing::warn!("the session closed under the window");
                break;
            }
            let at = start.elapsed().as_secs_f64();
            window.first_arrival_s.get_or_insert(at);
            if let Some(previous) = last {
                window.longest_gap_s = window.longest_gap_s.max(at - previous);
            }
            last = Some(at);
            window.split_reads += u64::from(n != datagram);
            pending.extend_from_slice(&buf[..n]);
            consume(&mut pending, &mut window, start, datagram);
        }
        if window.expects_returns {
            let quiet_since = last.unwrap_or(0.0);
            window.longest_gap_s = window.longest_gap_s.max(window.duration_s - quiet_since);
        }
        Ok::<_, std::io::Error>(())
    };

    let (sent, read) = tokio::join!(send, recv);
    read?;
    (window.probes_sent, window.upload_bytes) = sent?;
    Ok(window)
}

/// Takes whole `datagram`-sized probes and runs of push fill off the front of `pending`; a byte that
/// is neither is corruption.
fn consume(pending: &mut Vec<u8>, window: &mut Window, start: Instant, datagram: usize) {
    loop {
        if pending.starts_with(&PROBE_MAGIC) {
            if pending.len() < datagram {
                return;
            }
            let sent_us = u64::from_le_bytes(pending[12..20].try_into().expect("8 bytes"));
            let now_us = start.elapsed().as_micros() as u64;
            window.probes_back += 1;
            window
                .rtts_ms
                .push(now_us.saturating_sub(sent_us) as f64 / 1000.0);
            pending.drain(..datagram);
        } else if pending.first() == Some(&PUSH_FILL) {
            let run = pending.iter().take_while(|b| **b == PUSH_FILL).count();
            window.fill_bytes += run as u64;
            pending.drain(..run);
        } else if pending.is_empty() || PROBE_MAGIC.starts_with(pending) {
            return;
        } else {
            window.corrupt_bytes += 1;
            pending.remove(0);
        }
    }
}

/// Every `hopr_surb_balancer_*` series the exit holds: one per Session it shapes.
async fn shaped_sessions(exit: &NodeInfo) -> anyhow::Result<BTreeSet<String>> {
    Ok(scrape_metrics(exit)
        .await?
        .lines()
        .filter_map(|l| l.strip_prefix("hopr_surb_balancer_current_buffer_target{"))
        .filter_map(|l| l.split_once('}').map(|(labels, _)| labels.to_string()))
        .collect())
}

fn rss_kb() -> anyhow::Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no VmRSS in /proc/self/status"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Throughput
// ─────────────────────────────────────────────────────────────────────────────

/// T04-fixed-throughput: a warm session carries 1, 10 and 50 MB whole and without a stall, and 10 MB
/// at no less than the floor. The 50 MB size is where testenv's download stalled after 25-30 MB.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn transfers_of_every_size_should_complete_without_a_stall() -> anyhow::Result<()> {
    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
    for megabytes in [1, 10, 50] {
        let label = format!("{megabytes} MB");
        step(&label, async {
            let t = bulk(
                &mut rx,
                &mut tx,
                megabytes * 1_000_000,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                &label,
            )
            .await?;
            if megabytes == 10 {
                anyhow::ensure!(
                    mbit(&t) >= BULK_FLOOR_MBIT,
                    "{label}: {:.1} Mbit/s, under the {BULK_FLOOR_MBIT} floor",
                    mbit(&t)
                );
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// T13-mtu-sweep: the datagram the tunnel emits is a mechanism switch (how many SURBs ride along),
/// so each WireGuard size at MTU 1420, 1280 and 940 must carry a transfer and a 3 Mbit/s upload.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn every_tunnel_mtu_should_carry_transfers_and_uploads() -> anyhow::Result<()> {
    const UPLOAD_FOR: Duration = Duration::from_secs(60);
    /// testenv's `MTU940_DOWN_MIN_MBIT` against `DOWN_MIN_MBIT`: a smaller MTU spends more packets.
    const SMALL_MTU_FLOOR_MBIT: f64 = BULK_FLOOR_MBIT * 6.0 / 7.0;

    let env = step("setup", vpn_env()).await?;
    for mtu in [1420, 1280, 940] {
        let datagram = mtu + 32;
        step(&format!("MTU {mtu}"), async {
            let echo = udp_service::spawn(Mode::Echo).await?;
            let (mut rx, mut tx) = open_warm(&env, &echo).await?;
            let t = bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                datagram,
                ARRIVAL_MIN_PCT,
                &format!("MTU {mtu}"),
            )
            .await?;
            if mtu == 940 {
                anyhow::ensure!(
                    mbit(&t) >= SMALL_MTU_FLOOR_MBIT,
                    "MTU {mtu}: {:.1} Mbit/s, under {SMALL_MTU_FLOOR_MBIT:.1}",
                    mbit(&t)
                );
            }

            let sink = udp_service::spawn(Mode::Sink).await?;
            let (mut rx, mut tx) = open_warm(&env, &sink).await?;
            let w = drive(&mut rx, &mut tx, UPLOAD_FOR, 0, STREAM_BITS, 0, datagram).await?;
            let loss = w.upload_loss_pct(sink.received(), datagram);
            tracing::info!("MTU {mtu} upload: {}", w.summary());
            anyhow::ensure!(
                loss < LOSS_MAX_PCT,
                "MTU {mtu}: the upload lost {loss:.2} %"
            );
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// T07-cold-start and T15-warmup-knee: a fresh session carries a transfer at any idle after connect,
/// and reaches 0.8 of its best rate by 30 s. The knee sat at ~240 s on the broken 0.96.1 ramp.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_fresh_session_should_reach_full_rate_within_its_ramp() -> anyhow::Result<()> {
    const KNEE_FRACTION: f64 = 0.8;
    const KNEE_MAX_S: u64 = 30;

    let env = step("setup", vpn_env()).await?;
    let mut rates = Vec::new();
    for idle in [0, 5, 15, 30, 60] {
        let rate = step(&format!("{idle} s after connect"), async {
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let (mut rx, mut tx) = tokio::io::split(open(&env, &far_end).await?);
            tokio::time::sleep(Duration::from_secs(idle)).await;
            let floor = if Duration::from_secs(idle) < RAMP_WAIT {
                COLD_ARRIVAL_MIN_PCT
            } else {
                ARRIVAL_MIN_PCT
            };
            let t = bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                floor,
                &format!("idle {idle} s"),
            )
            .await?;
            Ok(mbit(&t))
        })
        .await?;
        rates.push((idle, rate));
    }
    let best = rates.iter().map(|(_, r)| *r).fold(0.0, f64::max);
    let knee = rates.iter().find(|(_, r)| *r >= KNEE_FRACTION * best);
    tracing::info!(?rates, ?knee, "warm-up knee");
    anyhow::ensure!(
        knee.is_some_and(|(idle, _)| *idle <= KNEE_MAX_S),
        "no idle up to {KNEE_MAX_S} s reached {KNEE_FRACTION} of the best rate: {rates:?} (Mbit/s)"
    );
    Ok(())
}

/// T12-balancer-sweep: the client's main SURB upstream, swept over a known cliff (96 Mb/s collapsed
/// under a discard storm while 48 held): every cell completes and the worst warm rate stays within
/// 0.3 of the best.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn every_surb_upstream_should_carry_downloads() -> anyhow::Result<()> {
    const WORST_TO_BEST: f64 = 0.3;

    let env = step("setup", vpn_env()).await?;
    let mut warm = Vec::new();
    for upstream_mbit in [12, 16, 48, 96] {
        let rate = step(&format!("{upstream_mbit} Mb/s"), async {
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let main = vpn_surb_tier((VPN_MAIN_TIER.0, upstream_mbit * 1_000_000));
            let session = env
                .open_vpn_session(far_end.target(), wg_capabilities(), main)
                .await?;
            let (mut rx, mut tx) = tokio::io::split(session);
            bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                COLD_ARRIVAL_MIN_PCT,
                "cold",
            )
            .await?;
            tokio::time::sleep(RAMP_WAIT).await;
            let t = bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                "warm",
            )
            .await?;
            Ok(mbit(&t))
        })
        .await?;
        warm.push((upstream_mbit, rate));
    }
    let best = warm.iter().map(|(_, r)| *r).fold(0.0, f64::max);
    let worst = warm.iter().map(|(_, r)| *r).fold(f64::INFINITY, f64::min);
    tracing::info!(?warm, "warm download per SURB upstream (Mbit/s)");
    anyhow::ensure!(
        best > 0.0 && worst >= WORST_TO_BEST * best,
        "warm downloads spread from {worst:.1} to {best:.1} Mbit/s: {warm:?}"
    );
    Ok(())
}

/// T18-capacity-ceiling: one session's download stream holds under 5 % loss up to the knee. The
/// fleet's exit topped out near 2000 packets/s; every other rate in this file is set under it.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_download_should_hold_up_to_the_capacity_knee() -> anyhow::Result<()> {
    const STEP: Duration = Duration::from_secs(30);
    const KNEE_MIN_MBIT: u64 = 8;

    let env = step("setup", vpn_env()).await?;
    let mut knee = 0;
    for rung_mbit in [1, 2, 4, 8, 12, 16] {
        let loss = step(&format!("{rung_mbit} Mbit/s"), async {
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
            let w = drive(
                &mut rx,
                &mut tx,
                STEP,
                0,
                0,
                rung_mbit * 1_000_000,
                PROBE_DATAGRAM,
            )
            .await?;
            tracing::info!("{rung_mbit} Mbit/s: {}", w.summary());
            Ok(w.download_loss_pct())
        })
        .await?;
        if loss >= LOSS_MAX_PCT {
            break;
        }
        knee = rung_mbit;
    }
    anyhow::ensure!(
        knee >= KNEE_MIN_MBIT,
        "the download held under {LOSS_MAX_PCT} % loss only up to {knee} Mbit/s (min {KNEE_MIN_MBIT})"
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Real-time traffic
// ─────────────────────────────────────────────────────────────────────────────

/// T06-realtime-udp, echo arm: a 1.5 Mbit/s call, which shows the return-path loss a bulk TCP
/// transfer hides behind retransmission (the fleet lost 54-96 % while TCP looked fine).
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_call_should_not_lose_packets() -> anyhow::Result<()> {
    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
    step("call", async {
        let w = drive(
            &mut rx,
            &mut tx,
            Duration::from_secs(120),
            CALL_BITS,
            0,
            0,
            PROBE_DATAGRAM,
        )
        .await?;
        anyhow::ensure!(
            w.sampled_pct(CALL_BITS, PROBE_DATAGRAM) >= SAMPLE_MIN_PCT,
            "the call sent only {:.0} % of its packets",
            w.sampled_pct(CALL_BITS, PROBE_DATAGRAM)
        );
        w.check("call", w.call_loss_pct())
    })
    .await
}

/// T06-realtime-udp, upload arm: a 3 Mbit/s paced upload, the forward path alone.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn an_upload_stream_should_not_lose_packets() -> anyhow::Result<()> {
    let env = step("setup", vpn_env()).await?;
    let sink = udp_service::spawn(Mode::Sink).await?;
    let (mut rx, mut tx) = open_warm(&env, &sink).await?;
    step("upload", async {
        let w = drive(
            &mut rx,
            &mut tx,
            Duration::from_secs(90),
            0,
            STREAM_BITS,
            0,
            PROBE_DATAGRAM,
        )
        .await?;
        w.check("upload", w.upload_loss_pct(sink.received(), PROBE_DATAGRAM))
    })
    .await
}

/// T06-realtime-udp, download arm: a 3 Mbit/s paced download, the SURB-metered return path alone.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_download_stream_should_not_lose_packets() -> anyhow::Result<()> {
    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
    step("download", async {
        let w = drive(
            &mut rx,
            &mut tx,
            Duration::from_secs(90),
            0,
            0,
            STREAM_BITS,
            PROBE_DATAGRAM,
        )
        .await?;
        w.check("download", w.download_loss_pct())
    })
    .await
}

/// T06-realtime-udp, after-bulk arm: the same download on a session that has just carried three
/// transfers. On hoprd 4.1.3 that state showed 9-54 reassembly failures and up to 71 % loss.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_download_stream_after_transfers_should_not_lose_packets() -> anyhow::Result<()> {
    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
    step("transfers", async {
        for rep in 1..=3 {
            bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                &format!("transfer {rep}"),
            )
            .await?;
        }
        Ok(())
    })
    .await?;
    step("download", async {
        let w = drive(
            &mut rx,
            &mut tx,
            Duration::from_secs(90),
            0,
            0,
            STREAM_BITS,
            PROBE_DATAGRAM,
        )
        .await?;
        w.check("download after transfers", w.download_loss_pct())
    })
    .await
}

/// T06-realtime-udp idle arm, and T07's finding that a 75 s idle let the return SURBs expire: a
/// session kept alive only by WireGuard's 25 s keep-alive carries a call when it resumes.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn an_idle_session_should_carry_a_call_when_it_resumes() -> anyhow::Result<()> {
    const IDLE: Duration = Duration::from_secs(120);
    const KEEPALIVE_EVERY: Duration = Duration::from_secs(25);
    const WG_KEEPALIVE_BYTES: usize = 32;

    let env = step("setup", vpn_env()).await?;
    let sink = udp_service::spawn(Mode::Sink).await?;
    let (mut rx, mut tx) = tokio::io::split(open(&env, &sink).await?);
    step("idle", async {
        let until = Instant::now() + IDLE;
        while Instant::now() < until {
            tx.write_all(&[0x55; WG_KEEPALIVE_BYTES]).await?;
            tokio::time::sleep(
                KEEPALIVE_EVERY.min(until.saturating_duration_since(Instant::now())),
            )
            .await;
        }
        Ok(())
    })
    .await?;
    step("call", async {
        let w = drive(
            &mut rx,
            &mut tx,
            Duration::from_secs(30),
            CALL_BITS,
            0,
            0,
            PROBE_DATAGRAM,
        )
        .await?;
        w.check("call after idle", w.call_loss_pct())
    })
    .await
}

/// T05-loaded-latency: round-trip time next to a saturating download and upload. The fleet's
/// bufferbloat was 2-4 s; testenv allows a p95 of 1.5 s and 2.5 s.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn latency_next_to_a_saturating_load_should_stay_bounded() -> anyhow::Result<()> {
    const PHASE: Duration = Duration::from_secs(30);
    const SATURATING_BITS: u64 = 16_000_000;
    const DOWN_P95_MAX_MS: f64 = 1500.0;
    const UP_P95_MAX_MS: f64 = 2500.0;

    let env = step("setup", vpn_env()).await?;
    step("download", async {
        let far_end = udp_service::spawn(Mode::Echo).await?;
        let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
        let w = drive(
            &mut rx,
            &mut tx,
            PHASE,
            RTT_PROBE_BITS,
            0,
            SATURATING_BITS,
            PROBE_DATAGRAM,
        )
        .await?;
        tracing::info!("loaded download: {}", w.summary());
        let p95 = w.rtt_ms(0.95);
        anyhow::ensure!(
            p95.is_some_and(|ms| ms <= DOWN_P95_MAX_MS),
            "rtt p95 {p95:?} ms (max {DOWN_P95_MAX_MS}) under a saturating download"
        );
        Ok(())
    })
    .await?;
    step("upload", async {
        let sink = udp_service::spawn(Mode::Sink).await?;
        let (mut rx, mut tx) = open_warm(&env, &sink).await?;
        let w = drive(
            &mut rx,
            &mut tx,
            PHASE,
            RTT_PROBE_BITS,
            SATURATING_BITS,
            0,
            PROBE_DATAGRAM,
        )
        .await?;
        tracing::info!("loaded upload: {}", w.summary());
        let p95 = w.rtt_ms(0.95);
        anyhow::ensure!(
            p95.is_some_and(|ms| ms <= UP_P95_MAX_MS),
            "rtt p95 {p95:?} ms (max {UP_P95_MAX_MS}) under a saturating upload"
        );
        Ok(())
    })
    .await
}

// ─────────────────────────────────────────────────────────────────────────────
// Configuration, contention and faults
// ─────────────────────────────────────────────────────────────────────────────

/// T11-capability-matrix: the exit shapes a session (runs a SURB balancer for it) exactly when the
/// client leaves rate control on, and every capability set carries a call.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn the_exit_should_shape_exactly_the_rate_controlled_sessions() -> anyhow::Result<()> {
    use SessionCapability::{NoDelay, NoRateControl, Segmentation};
    const POLLS: usize = 10;

    let env = step("setup", vpn_env()).await?;
    let exit = exit_node(&env)?;
    let cells: [(&str, SessionCapabilities, bool); 3] = [
        ("segmentation+no_delay", Segmentation | NoDelay, true),
        ("segmentation", Segmentation.into(), true),
        (
            "+no_rate_control",
            Segmentation | NoDelay | NoRateControl,
            false,
        ),
    ];
    for (name, capabilities, shaped) in cells {
        step(name, async {
            let before = shaped_sessions(&exit).await?;
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let session = env
                .open_vpn_session(far_end.target(), capabilities, vpn_surb_tier(VPN_MAIN_TIER))
                .await?;
            let (mut rx, mut tx) = tokio::io::split(session);
            let w = drive(
                &mut rx,
                &mut tx,
                Duration::from_secs(30),
                CALL_BITS,
                0,
                0,
                PROBE_DATAGRAM,
            )
            .await?;
            tracing::info!("{name}: {}", w.summary());
            anyhow::ensure!(
                w.call_loss_pct() < LOSS_MAX_PCT && w.corrupt_bytes == 0,
                "{name}: {}",
                w.summary()
            );
            let mut rose = false;
            for _ in 0..POLLS {
                rose = shaped_sessions(&exit)
                    .await?
                    .difference(&before)
                    .next()
                    .is_some();
                if rose {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            anyhow::ensure!(
                rose == shaped,
                "{name}: the exit {} a SURB balancer for the session",
                if rose { "runs" } else { "runs no" }
            );
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// T19-background-load: a 10 or 100 kB request every 2 s on another session to the same exit must
/// not halve a transfer. On the fleet a 10 kB trickle cut a download from 4.4-6.6 to 1.4-2.0 Mbit/s.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_trickle_on_another_session_should_not_starve_a_transfer() -> anyhow::Result<()> {
    const STEP_FRACTION: f64 = 0.5;
    const TRICKLE_EVERY: Duration = Duration::from_secs(2);

    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
    let (mut trx, mut ttx) = open_warm(&env, &far_end).await?;
    let control = step("control", async {
        Ok(mbit(
            &bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                "control",
            )
            .await?,
        ))
    })
    .await?;
    for trickle in [10_000, 100_000] {
        step(&format!("{} kB trickle", trickle / 1000), async {
            let mut requests = vec![0u8; trickle * 10];
            rand::rng().fill(&mut requests[..]);
            let trickling = pump_halves(
                &mut trx,
                &mut ttx,
                &requests,
                "trickle",
                BULK_TIMEOUT,
                PumpOpts {
                    pace: pace_for_rate_with_chunk(BULK_MBIT / 8.0, WG_DATAGRAM),
                    chunk: Some(WG_DATAGRAM),
                    shape: Some(Shape::Burst {
                        on: trickle,
                        off: TRICKLE_EVERY,
                    }),
                    ..PumpOpts::default()
                },
            );
            let transfer = bulk(
                &mut rx,
                &mut tx,
                BULK_BYTES,
                WG_DATAGRAM,
                ARRIVAL_MIN_PCT,
                "under trickle",
            );
            let (_, t) = tokio::join!(trickling, transfer);
            let rate = mbit(&t?);
            anyhow::ensure!(
                rate >= STEP_FRACTION * control,
                "{rate:.1} Mbit/s under the trickle against {control:.1} without"
            );
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// T22-concurrent-clients: 1, 2 and 4 sessions on one exit transfer at once; all complete, the
/// aggregate never falls by more than 25 % from a lower rung, and the top one carries at least 8.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn concurrent_sessions_should_share_the_exit() -> anyhow::Result<()> {
    const WARMUP_BYTES: usize = 500_000;
    const TOLERANCE: f64 = 0.25;
    const AGGREGATE_MIN_MBIT: f64 = 8.0;

    let env = step("setup", vpn_env()).await?;
    let far_end = udp_service::spawn(Mode::Echo).await?;
    let mut aggregates: Vec<(usize, f64)> = Vec::new();
    for sessions in [1, 2, 4] {
        let aggregate = step(&format!("{sessions} sessions"), async {
            let mut halves = Vec::new();
            for _ in 0..sessions {
                let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
                bulk(
                    &mut rx,
                    &mut tx,
                    WARMUP_BYTES,
                    WG_DATAGRAM,
                    ARRIVAL_MIN_PCT,
                    "warm-up",
                )
                .await?;
                halves.push((rx, tx));
            }
            let transfers = futures::future::try_join_all(halves.iter_mut().enumerate().map(
                |(i, (rx, tx))| async move {
                    bulk(
                        rx,
                        tx,
                        BULK_BYTES,
                        WG_DATAGRAM,
                        SHARED_ARRIVAL_MIN_PCT,
                        &format!("session {i}"),
                    )
                    .await
                },
            ))
            .await?;
            let bytes: usize = transfers.iter().map(|t| t.attributed_bytes).sum();
            // Not wall time: an incomplete transfer waits out the pump's idle budget after its last byte.
            let busy_s = transfers.iter().map(|t| t.seconds).fold(0.0, f64::max);
            Ok(bytes as f64 * 8.0 / 1e6 / busy_s)
        })
        .await?;
        tracing::info!(sessions, aggregate, "aggregate Mbit/s");
        if let Some((lower, rate)) = aggregates
            .iter()
            .find(|(_, rate)| aggregate < (1.0 - TOLERANCE) * rate)
        {
            anyhow::bail!(
                "{sessions} sessions carried {aggregate:.1} Mbit/s, {lower} carried {rate:.1}"
            );
        }
        aggregates.push((sessions, aggregate));
    }
    let top = aggregates.last().map_or(0.0, |(_, rate)| *rate);
    anyhow::ensure!(
        top >= AGGREGATE_MIN_MBIT,
        "the top rung carried {top:.1} Mbit/s (min {AGGREGATE_MIN_MBIT})"
    );
    Ok(())
}

/// T09-impairment-ladder: return relays of equal latency carry a session at every rung, and so do a
/// near and a far one together. On the first self-hosted exit that RTT mismatch was the defect: a
/// 20-340 ms relay mix gave 0.2 Mbit/s where one near relay gave 4.6-6.3.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn return_relays_of_unequal_latency_should_carry_the_session() -> anyhow::Result<()> {
    const STREAM_FOR: Duration = Duration::from_secs(60);
    /// Time for the entry's probes to see the new latencies before the session opens.
    const LEARN: Duration = Duration::from_secs(15);
    const CELLS: [(&str, u64, u64); 5] = [
        ("equal-0", 0, 0),
        ("equal-25", 25, 25),
        ("equal-50", 50, 50),
        ("gap-50", 50, 0),
        ("far-100", 0, 100),
    ];

    let env = step("setup", vpn_env()).await?;
    let cluster = env.cluster()?;
    let exit = env.dest_for(1)?;
    let relays = env.relayer_candidates(exit)?;
    anyhow::ensure!(
        relays.len() >= 2,
        "needs two return relays, have {}",
        relays.len()
    );
    for (name, first_ms, second_ms) in CELLS {
        let cell = step(name, async {
            cluster.clear_link_rules()?;
            for (relay, ms) in [(&relays[0], first_ms), (&relays[1], second_ms)] {
                if ms > 0 {
                    cluster.delay_link(exit, relay.address, Duration::from_millis(ms))?;
                }
            }
            tokio::time::sleep(LEARN).await;
            let far_end = udp_service::spawn(Mode::Echo).await?;
            let (mut rx, mut tx) = open_warm(&env, &far_end).await?;
            let w = drive(
                &mut rx,
                &mut tx,
                STREAM_FOR,
                0,
                0,
                STREAM_BITS,
                PROBE_DATAGRAM,
            )
            .await?;
            w.check(name, w.download_loss_pct())?;
            for rep in 1..=2 {
                bulk(
                    &mut rx,
                    &mut tx,
                    BULK_BYTES,
                    WG_DATAGRAM,
                    ARRIVAL_MIN_PCT,
                    &format!("{name} transfer {rep}"),
                )
                .await?;
            }
            Ok(())
        })
        .await;
        cluster.clear_link_rules()?;
        cell?;
    }
    Ok(())
}

/// T10-forced-reconnect: the client drops its session while the far end keeps streaming into it and
/// opens a fresh one at once. On 2026-09-14 every such fresh session died 3-7 s after it was ready.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_session_reopened_under_inbound_traffic_should_carry_it() -> anyhow::Result<()> {
    const BEFORE_DROP: Duration = Duration::from_secs(15);
    const AFTER: Duration = Duration::from_secs(60);
    const FIRST_PACKET_MAX_S: f64 = 10.0;

    let env = step("setup", vpn_env()).await?;
    let old_far_end = udp_service::spawn(Mode::Echo).await?;
    step("drop under a stream", async {
        let (rx, mut tx) = open_warm(&env, &old_far_end).await?;
        let rate = STREAM_BITS / (8 * PROBE_DATAGRAM as u64);
        let total = STREAM_BITS / 8 * (BEFORE_DROP + AFTER).as_secs();
        tx.write_all(&push_request(PROBE_DATAGRAM, rate, total))
            .await?;
        tokio::time::sleep(BEFORE_DROP).await;
        drop(rx.unsplit(tx));
        Ok(())
    })
    .await?;
    step("reopen", async {
        let far_end = udp_service::spawn(Mode::Echo).await?;
        let (mut rx, mut tx) = tokio::io::split(open(&env, &far_end).await?);
        let w = drive(&mut rx, &mut tx, AFTER, 0, 0, STREAM_BITS, PROBE_DATAGRAM).await?;
        tracing::info!("reopened: {}", w.summary());
        anyhow::ensure!(
            w.first_arrival_s.is_some_and(|s| s <= FIRST_PACKET_MAX_S)
                && w.longest_gap_s <= STALL_MAX_S
                && w.corrupt_bytes == 0,
            "the reopened session: {}",
            w.summary()
        );
        Ok(())
    })
    .await
}

/// T23-sustained-soak: a call for ten minutes next to a transfer every minute keeps its loss, and
/// the client's memory stays bounded. The defect behind testenv appeared ~30 min into a clean call.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries and a chain"]
async fn a_long_call_should_hold_its_loss_and_memory() -> anyhow::Result<()> {
    const SOAK: Duration = Duration::from_secs(600);
    const TRANSFER_EVERY: Duration = Duration::from_secs(60);
    const RSS_SLACK_KB: u64 = 200_000;

    let env = step("setup", vpn_env()).await?;
    let call_end = udp_service::spawn(Mode::Sink).await?;
    let bulk_end = udp_service::spawn(Mode::Echo).await?;
    let (mut crx, mut ctx) = open_warm(&env, &call_end).await?;
    let (mut brx, mut btx) = open_warm(&env, &bulk_end).await?;
    let rss_before = rss_kb()?;
    let (call, transfers) = tokio::join!(
        drive(&mut crx, &mut ctx, SOAK, CALL_BITS, 0, 0, PROBE_DATAGRAM),
        async {
            let until = Instant::now() + SOAK;
            let mut n = 0;
            while Instant::now() + TRANSFER_EVERY <= until {
                tokio::time::sleep(TRANSFER_EVERY).await;
                n += 1;
                bulk(
                    &mut brx,
                    &mut btx,
                    BULK_BYTES,
                    WG_DATAGRAM,
                    ARRIVAL_MIN_PCT,
                    &format!("soak transfer {n}"),
                )
                .await?;
            }
            anyhow::Ok(())
        }
    );
    let rss_after = rss_kb()?;
    step("transfers", async { transfers }).await?;
    step("call", async {
        let w = call?;
        anyhow::ensure!(
            w.sampled_pct(CALL_BITS, PROBE_DATAGRAM) >= SAMPLE_MIN_PCT,
            "the call sent only {:.0} % of its packets",
            w.sampled_pct(CALL_BITS, PROBE_DATAGRAM)
        );
        w.check("soak call", w.call_loss_pct())
    })
    .await?;
    step("memory", async {
        anyhow::ensure!(
            rss_after < 2 * rss_before + RSS_SLACK_KB,
            "RSS grew from {rss_before} kB to {rss_after} kB"
        );
        Ok(())
    })
    .await
}
