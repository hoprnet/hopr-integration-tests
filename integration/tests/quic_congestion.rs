//! Does a sustained HOPR session keep its throughput, or does the QUIC connection settle into a
//! degraded state it never leaves?
//!
//! # The incident this reproduces
//!
//! On `hoprd` v5.0.0 a `gnosis_vpn-client` session (VPN traffic over a HOPR unreliable session)
//! was observed to enter a state where its throughput collapsed to a low plateau and stayed there
//! for the life of the connection. Nothing recovered it — not idle time, not the application
//! backing off and resuming. Only tearing the connection down and establishing a fresh one brought
//! the throughput back.
//!
//! # The mechanism
//!
//! QUIC congestion control is per **connection**, not per stream. libp2p's QUIC transport uses
//! quinn, whose default controller is CUBIC. CUBIC applies its multiplicative decrease
//! (beta = 0.7) on every loss event, so on a path that loses repeatedly the connection's
//! congestion window is driven down to quinn's hardcoded RFC 9002 floor of `2 * MTU` — 2400 bytes
//! at the 1200-byte base MTU. At a 100 ms RTT that floor caps the connection at ~192 kbit/s, and
//! because CUBIC grows its window cubically in time from `w_max` it does not climb back on its
//! own. The connection is left pinned there: the write pump blocks on the stalled window, the
//! peer's replies stop arriving, and the only way out is a new connection.
//!
//! # What this scenario measures
//!
//! A sustained, paced upload whose echo is read back concurrently, sampled every
//! [`SAMPLE_EVERY`]. A healthy connection holds the offered rate; a connection that has settled
//! onto the 2400-byte floor shows a rate that never comes back to the rate it held earlier in the
//! same run, and a send-side stall that grows without bound.
//!
//! # Reading a result
//!
//! - **Pass** — the rate held through the end of the run: no collapse.
//! - **Fail** — the rate in the final windows is a small fraction of the peak, and/or the stall at
//!   the end of the run runs long: the connection degraded and did not recover. That is the bug.
//!
//! The measurements are logged with `TEST_ARGS=--nocapture`, so a failing run reports the whole
//! rate series rather than only the assertion that tripped.
//!
//! Run with:
//!
//! ```bash
//! HOPRD_KEEP_ARTIFACTS=1 HOPRD_BIN=<hoprd> HOPRD_LOCALCLUSTER_BIN=<localcluster> \
//! TEST_TARGET=quic_congestion TEST_ARGS=--nocapture \
//!   bash scripts/integration/run-binchain.sh > /tmp/run.log 2>&1
//! ```
//!
//! `LINE=v5` when the target is the v5 stack (hoprd `main`, hoprnet `master`).

use std::time::Duration;

use hoprd_integration_test::{
    IntegrationEnv,
    pump::{pace_for_rate_with_chunk, pump_sustained},
    request_edgli_egress_timeout,
};

/// Sampling interval; also the width of each rate window.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// How long the paced stream is offered. Long enough for a mid-run collapse to show and for the
/// tail windows to say whether it recovered.
const DEFAULT_RUN_DURATION: Duration = Duration::from_secs(60);
/// Offered rate on the v5 stack (`NoDelay` sessions reject chunks over 64 segments).
const TARGET_MBPS_V5: f64 = 2.0;
/// Offered rate on the v4 stack, where the session chunks at `IO_CHUNK`.
const TARGET_MBPS_V4: f64 = 4.0;
/// Write size. A whole number of records (16 B) and comfortably under the v5 64-segment ceiling.
const CHUNK: usize = 16 * 1024;
/// The worst tolerable send-side stall, as the reason a run fails rather than as a knob.
const MAX_STALL: Duration = Duration::from_secs(20);
/// Floor on the final windows relative to the peak, in percent.
///
/// A healthy loopback connection holds its offered rate, so the real figure sits near 100 and this
/// only has to catch a collapse to the 2400-byte floor, which is well over an order of magnitude
/// below it. Overridable so a lossy run can be judged without rebuilding.
const DEFAULT_HEALTHY_FLOOR_PCT: f64 = 35.0;

fn run_duration() -> Duration {
    std::env::var("HOPRD_QUIC_DURATION")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RUN_DURATION)
}

fn healthy_floor_pct() -> f64 {
    std::env::var("HOPRD_QUIC_HEALTHY_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HEALTHY_FLOOR_PCT)
}

/// Offered rate for this line, from `HOPRD_QUIC_MBPS` when set.
fn target_mbps() -> f64 {
    std::env::var("HOPRD_QUIC_MBPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(feature = "v5") {
            TARGET_MBPS_V5
        } else {
            TARGET_MBPS_V4
        })
}

/// A sustained transfer must not collapse to a fraction of the rate it held earlier in the run.
///
/// The assertion is on the **final** windows against the peak, which is the shape of the reported
/// fault: fast at the start, low ever after, no recovery within the connection.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a bloklid-anvil container"]
async fn sustained_transfer_keeps_its_throughput() -> anyhow::Result<()> {
    // Mirror production 2 s egress stall timeout, which triggers stream eviction under backpressure.
    request_edgli_egress_timeout(Duration::from_secs(2));

    // Route QUIC traffic between Edgli and cluster nodes through the userspace shaper with 50ms delay
    // (100ms RTT) so CUBIC's minimum window calculation (2 * MTU / 100ms RTT = ~192 kbit/s) applies.
    let (env, shaper) = IntegrationEnv::setup_shaped().await?;
    // 0-hop: the fault is on the entry↔exit connection itself, not in relaying, so a direct path
    // keeps the scenario about the transport.
    let session = env.open_unreliable_session(0).await?;
    let (mut rx, mut tx) = tokio::io::split(session);

    let mbps = target_mbps();
    let pace = pace_for_rate_with_chunk(mbps, CHUNK);
    let floor_pct = healthy_floor_pct();
    let duration = run_duration();

    tracing::info!(
        offered_mbps = mbps,
        run = ?duration,
        window = ?SAMPLE_EVERY,
        floor_pct,
        "sustained session; a collapse in the final windows is the fault under test"
    );

    // Schedule the impairment event to reproduce #8524:
    // 1. Warm-up at 100ms RTT for 15s (healthy peak throughput established).
    // 2. Blackhole for 4s (exceeds 2s egress timeout, triggering StallGuardSink eviction and Quinn loss).
    // 3. Resume with light recurring loss (1 in 20 packets) to keep CUBIC pinned at quinn's 2400-byte floor.
    let shaper_ctrl = shaper.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        tracing::warn!(
            ">>> QUIC IMPAIRMENT: inducing blackhole (4s) to trigger write stall and congestion collapse"
        );
        shaper_ctrl.set_blackhole(true);
        tokio::time::sleep(Duration::from_secs(4)).await;
        shaper_ctrl.set_blackhole(false);
        tracing::warn!(
            ">>> QUIC IMPAIRMENT: blackhole ended, activating light recurring loss (1 in 20)"
        );
        shaper_ctrl.set_drop_every(Some(20));
    });

    let run = pump_sustained(
        &mut rx,
        &mut tx,
        duration,
        pace,
        SAMPLE_EVERY,
        CHUNK,
        "quic-sustained",
    )
    .await?;

    // The whole series, so a failing run reports the shape rather than one number.
    let mut series = String::new();
    for (i, &(at, _)) in run.write_samples.iter().enumerate() {
        let at = Duration::from_secs_f64(at);
        series.push_str(&format!(
            "\n  t={at:>5.1?}  send {:>8.0} kB/s  recv {:>8.0} kB/s",
            run.write_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0) / 1e3,
            run.read_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0) / 1e3,
        ));
        if i > 40 {
            series.push_str("\n  …");
            break;
        }
    }
    tracing::info!("rate series:{series}");

    assert!(
        run.written > 0,
        "nothing was offered over {duration:?}: the session never accepted a write"
    );

    let peak_write = run
        .peak_write_rate(SAMPLE_EVERY)
        .filter(|p| *p > 0.0)
        .ok_or_else(|| anyhow::anyhow!("no measurable send rate; see the rate series above"))?;
    let peak_read = run
        .peak_read_rate(SAMPLE_EVERY)
        .filter(|p| *p > 0.0)
        .ok_or_else(|| anyhow::anyhow!("no measurable recv rate; see the rate series above"))?;

    // The last two windows, not the very last alone: the final row is a partial window when the
    // run ends off a tick boundary, and one short window is not evidence of a collapse.
    let tail = [duration, duration.saturating_sub(SAMPLE_EVERY)];
    for at in tail {
        let write_rate = run.write_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0);
        let read_rate = run.read_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0);
        assert!(
            write_rate >= peak_write * floor_pct / 100.0
                && read_rate >= peak_read * floor_pct / 100.0,
            "throughput collapsed and did not recover: send {:.0} kB/s, recv {:.0} kB/s in window \
             ending at {at:?}, against peaks of send {:.0} kB/s, recv {:.0} kB/s ({floor_pct:.0}% floor). \
             Offloaded {:.1} MB, echoed {:.1} MB, stall at end {:.1}s. Series:{series}",
            write_rate / 1e3,
            read_rate / 1e3,
            peak_write / 1e3,
            peak_read / 1e3,
            run.written as f64 / 1e6,
            run.received as f64 / 1e6,
            run.stall_since_last_progress,
        );
    }

    assert!(
        run.stall_since_last_progress <= MAX_STALL.as_secs_f64(),
        "the connection made no progress for {:.1}s at the end of the run (max {MAX_STALL:?}): the \
         write pump is blocked on a stalled connection. Offloaded {:.1} MB, echoed {:.1} MB. \
         Series:{series}",
        run.stall_since_last_progress,
        run.written as f64 / 1e6,
        run.received as f64 / 1e6,
    );

    Ok(())
}

/// The same sustained transfer, but handed to the session as fast as it will take it.
///
/// A paced stream under-drives the connection and can hide a floor that is well above the offered
/// rate; an unpaced one keeps the window at the edge, which is where the reported degradation was
/// seen. Kept as a separate scenario because it exercises a different regime, not a different
/// assertion.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a bloklid-anvil container"]
async fn unpaced_transfer_keeps_its_throughput() -> anyhow::Result<()> {
    request_edgli_egress_timeout(Duration::from_secs(2));

    let (env, shaper) = IntegrationEnv::setup_shaped().await?;
    let session = env.open_unreliable_session(0).await?;
    let (mut rx, mut tx) = tokio::io::split(session);

    let floor_pct = healthy_floor_pct();
    let duration = run_duration();

    let shaper_ctrl = shaper.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        shaper_ctrl.set_blackhole(true);
        tokio::time::sleep(Duration::from_secs(4)).await;
        shaper_ctrl.set_blackhole(false);
        shaper_ctrl.set_drop_every(Some(20));
    });

    let run = pump_sustained(
        &mut rx,
        &mut tx,
        duration,
        None,
        SAMPLE_EVERY,
        CHUNK,
        "quic-unpaced",
    )
    .await?;

    // The whole series, so a failing run reports the shape rather than one number.
    let mut series = String::new();
    for (i, &(at, _)) in run.write_samples.iter().enumerate() {
        let at = Duration::from_secs_f64(at);
        series.push_str(&format!(
            "\n  t={at:>5.1?}  send {:>8.0} kB/s  recv {:>8.0} kB/s",
            run.write_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0) / 1e3,
            run.read_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0) / 1e3,
        ));
        if i > 40 {
            series.push_str("\n  …");
            break;
        }
    }
    tracing::info!("rate series:{series}");

    let peak_write = run
        .peak_write_rate(SAMPLE_EVERY)
        .filter(|p| *p > 0.0)
        .ok_or_else(|| anyhow::anyhow!("no measurable send rate; the session accepted nothing"))?;
    let peak_read = run
        .peak_read_rate(SAMPLE_EVERY)
        .filter(|p| *p > 0.0)
        .ok_or_else(|| anyhow::anyhow!("no measurable recv rate; the session accepted nothing"))?;

    for at in [duration, duration.saturating_sub(SAMPLE_EVERY)] {
        let write_rate = run.write_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0);
        let read_rate = run.read_rate_at(at, SAMPLE_EVERY).unwrap_or(0.0);
        assert!(
            write_rate >= peak_write * floor_pct / 100.0
                && read_rate >= peak_read * floor_pct / 100.0,
            "unpaced throughput collapsed and did not recover: send {:.0} kB/s, recv {:.0} kB/s at {at:?} \
             against peaks of send {:.0} kB/s, recv {:.0} kB/s ({floor_pct:.0}% floor). \
             Offloaded {:.1} MB, echoed {:.1} MB, stall at end {:.1}s. Series:{series}",
            write_rate / 1e3,
            read_rate / 1e3,
            peak_write / 1e3,
            peak_read / 1e3,
            run.written as f64 / 1e6,
            run.received as f64 / 1e6,
            run.stall_since_last_progress,
        );
    }

    Ok(())
}
