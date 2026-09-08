//! PIX under end-user traffic shapes.
//!
//! `tests/pix.rs` asks whether the deposit exchange happens at all, and answers it at a demo
//! geometry of 8 polynomials x (2 + 2) — a cycle of **32 packets**. That is the right size for
//! that question and the wrong size for this one: a single 64 KiB write spans two such cycles, so
//! there is no shape a cycle can be observed *inside*.
//!
//! These scenarios run at [`crate::shapes`]' profile — a cycle of 81 920 packets, ~4.5 min nominal
//! — and ask whether a Session sustains its cycles under the traffic a VPN client actually
//! produces: idle with keep-alives, bursty browsing, sustained download, sustained upload, and the
//! three in sequence on one Session.
//!
//! # Running
//!
//! ```text
//! HOPRD_SRC=../hoprd-shapes just pix <scenario>
//! ```
//!
//! Manual, like every PIX scenario here — never in CI. Each gets a fresh chain, and a full pass is
//! hours rather than minutes. See `docs/pix-traffic-shapes.md` for the measured results and the
//! configuration they pin.

#![cfg(feature = "pix")]

use std::time::Duration;

use anyhow::Context as _;
use hoprd_integration_test::{
    IntegrationEnv,
    cluster::{self, NodeInfo},
    pix::{self, PixCounters},
    pump::{self, PumpOpts},
    shapes,
};

/// Relays between Entry and Exit. At least one: PIX derives the share encryption key from the
/// first return relayer's acknowledgement, so a zero-hop path is refused outright.
const HOPS: usize = 1;

/// Bytes per write.
///
/// Chosen so a write is one HOPR packet and no SURB can piggyback on it, which is what leaves the
/// balancer as the only SURB source and therefore the only thing sizing the return pipeline. Same
/// reasoning as `session_pix_soak.rs`'s identical constant.
const CHUNK: usize = 900;

/// Send pace, one chunk per this — the profile's packet rate expressed as a delay.
const PACE: Duration = Duration::from_micros(1_000_000 / shapes::TARGET_PACKET_RATE);

/// How long a scenario waits for the sweeps it expects.
///
/// Three times the nominal cycle per cycle waited on: the achieved cycle runs 2-3x nominal because
/// the Exit's egress is shaped by its SURB buffer. A budget under that measures the buffer rather
/// than the shape.
fn sweep_budget(cycles: u64) -> Duration {
    Duration::from_secs(3 * shapes::NOMINAL_CYCLE_SECS * cycles + 120)
}

/// Poll cadence while waiting on sweeps.
const SETTLE_POLL: Duration = Duration::from_secs(5);

/// Resolve the `NodeInfo` for an address, so its Prometheus endpoint can be read.
fn node_for(env: &IntegrationEnv, address: hoprd_integration_test::Address) -> anyhow::Result<NodeInfo> {
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == address)
        .cloned()
        .with_context(|| format!("no cluster node with address {address}"))
}

/// Poll the Exit until it has swept `target` cycles, or the budget expires.
///
/// Returns the delta seen rather than erroring on timeout, deliberately: the assertion that follows
/// is what names the cause, and "timed out" on its own says nothing about whether the Exit never
/// recovered a key, never swept one, or was never paid in the first place.
async fn await_sweeps(
    exit: &NodeInfo,
    before: &PixCounters,
    target: u64,
    budget: Duration,
) -> anyhow::Result<PixCounters> {
    let deadline = std::time::Instant::now() + budget;
    let mut delta = before.delta(&pix::sample_exit(exit).await?);
    while std::time::Instant::now() < deadline {
        delta = before.delta(&pix::sample_exit(exit).await?);
        if delta.sweeps().unwrap_or(0) >= target {
            tracing::info!(summary = %delta.summary(), "reached the sweep target");
            return Ok(delta);
        }
        tracing::info!(
            elapsed_s = (budget - deadline.saturating_duration_since(std::time::Instant::now())).as_secs(),
            summary = %delta.summary(),
            "waiting on sweeps"
        );
        tokio::time::sleep(SETTLE_POLL).await;
    }
    Ok(delta)
}

/// Bytes of payload that offer `cycles` whole cycles of return traffic.
///
/// The Exit's loopback echoes every byte, so one chunk offered is one return packet — and one
/// return packet is one share. A margin on top because a cycle only completes once *its own*
/// shares have all ridden back, and the SURBs already in flight when it commits carry the
/// predecessor's.
fn payload_for(cycles: u64) -> Vec<u8> {
    let chunks = (shapes::CYCLE_PACKETS * cycles) as usize * 12 / 10;
    pump::tagged_payload(0, chunks * CHUNK)
}

/// The spike: does a cluster at this geometry admit the Session and complete a cycle at all?
///
/// Everything else in this file rests on that, and none of it had ever been run — `tests/pix.rs`
/// exercises a cycle 2 500x smaller, and hoprd's own soak a different geometry again through a
/// different harness. What this proves, in order: `--pix-config` reaches the nodes, the Entry's
/// announced quota lands inside the window the Exit was given, the deposit clears a per-deposit
/// ceiling derived from a price two orders of magnitude below the demo's, and a cycle of 81 920
/// packets recovers and sweeps within three nominal cycle lengths.
///
/// A failure here is a configuration failure, not a shape failure, which is why it is separate:
/// the shapes below cannot be read at all until this passes.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn the_profile_geometry_completes_a_cycle() -> anyhow::Result<()> {
    shapes::install_profile();
    cluster::request_cluster_size(3);

    let env = IntegrationEnv::setup_pix_with(shapes::entry_config()?).await?;
    let (session, exit_addr) = env
        .open_pix_session_with(
            HOPS,
            HOPS,
            shapes::surb_balancer(),
            hoprd_integration_test::SessionTarget::ExitNode(0),
        )
        .await?;
    let exit = node_for(&env, exit_addr)?;

    let before = pix::sample_exit(&exit).await?;
    anyhow::ensure!(
        before.observable(),
        "the Exit exposes no hopr_strategy_pix_* counters at all — it was built without \
         `hopr-strategy/telemetry`, so nothing here can be measured"
    );

    let (mut rx, mut tx) = tokio::io::split(session);
    let payload = payload_for(1);
    tracing::info!(
        bytes = payload.len(),
        cycle_packets = shapes::CYCLE_PACKETS,
        "offering one cycle of traffic"
    );

    let transfer = pump::pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "spike",
        sweep_budget(1),
        PumpOpts {
            pace: Some(PACE),
            chunk: Some(CHUNK),
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(
        arrival_pct = transfer.arrival_pct(),
        mbps = transfer.throughput_at(0.9).unwrap_or(0.0),
        outcome = ?transfer.outcome,
        "traffic finished"
    );
    anyhow::ensure!(
        transfer.arrival_pct() > 0.0,
        "not one byte came back, so the Session never carried traffic: {:?}",
        transfer.outcome
    );

    let delta = await_sweeps(&exit, &before, 1, sweep_budget(1)).await?;
    assert_eq!(
        0,
        delta.deposits_timed_out().unwrap_or(0),
        "the Exit gave up waiting for a deposit, so the geometry's deadlines and the Entry's \
         tracking window disagree: {}",
        delta.summary()
    );
    assert!(
        delta.sweeps().unwrap_or(0) >= 1,
        "no cycle was swept at the profile geometry, so nothing below can be measured: {}",
        delta.summary()
    );

    tracing::info!(summary = %delta.summary(), "profile geometry spike PASSED");
    Ok(())
}
