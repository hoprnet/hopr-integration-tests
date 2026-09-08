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
    shapes, udp_service,
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
fn node_for(
    env: &IntegrationEnv,
    address: hoprd_integration_test::Address,
) -> anyhow::Result<NodeInfo> {
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

// ─────────────────────────────────────────────────────────────────────────────
// The shapes
// ─────────────────────────────────────────────────────────────────────────────

/// An idle Session completes its cycle on Exit fill alone.
///
/// This is the shape the whole fill mechanism exists for, and the one every deployed VPN client
/// spends most of its life in. A WireGuard tunnel with nothing to carry still sends a persistent
/// keep-alive every 25 s — one small datagram, which is three orders of magnitude below the
/// ~114 packets/s a cycle of this geometry needs to finish inside `max_recovery_time`. Without
/// fill the funded cycle simply expires, and because the deposit address derives from both sides'
/// commitments the money is stranded rather than refunded.
///
/// The successor being funded is the other half of the property, and not decoration: a cycle
/// completed by keep-alives asks for its successor on the strength of keep-alives, and an Entry
/// that does not credit them as service refuses the request as under-served. That is the upstream
/// fix this whole toolchain was bumped for, observed from the outside.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn an_idle_session_completes_its_cycle_on_exit_fill() -> anyhow::Result<()> {
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

    // The aim point fill plans against. A cycle that has not completed by then has not been filled;
    // one that completes long after it was filled by something else.
    let aim_point = shapes::MAX_RECOVERY_TIME.mul_f64(shapes::FILL_FINISH_FRACTION);
    let (mut rx, mut tx) = tokio::io::split(session);

    // Enough keep-alives to span the aim point, and nothing else. At 32 bytes every 25 s this is
    // ~28 packets over nine minutes against a cycle of 81 920 — the application cannot be what
    // finishes it, which is what makes the assertion below about fill.
    let keepalives = (aim_point.as_secs() / 25 + 4) as usize;
    let payload = pump::tagged_payload(0, keepalives * 32);
    tracing::info!(
        keepalives,
        ?aim_point,
        "offering keep-alives only; the cycle is fill's to finish"
    );

    let idle = pump::pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "idle",
        aim_point + Duration::from_secs(120),
        PumpOpts {
            shape: Some(pump::Shape::Keepalive {
                every: Duration::from_secs(25),
                bytes: 32,
            }),
            // A keep-alive every 25 s is quieter than any default idle budget, so the pump must be
            // told that silence is the shape rather than a stalled Session.
            idle_budget: Some(Duration::from_secs(90)),
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(outcome = ?idle.outcome, arrival_pct = idle.arrival_pct(), "keep-alive phase finished");

    let delta = await_sweeps(&exit, &before, 1, aim_point).await?;
    assert!(
        delta.keys_recovered().unwrap_or(0) >= 1,
        "an idle cycle was not completed within {aim_point:?}, so the deposit stranded — which is \
         the pre-fill behaviour: {}",
        delta.summary()
    );
    assert!(
        delta.deposits_confirmed().unwrap_or(0) >= 2,
        "the idle cycle completed but no successor was funded, so the recovery bought nothing — \
         the Entry refused the request as under-served: {}",
        delta.summary()
    );

    // And the Session is alive rather than merely accounted for.
    let echo = pump::pump_halves(
        &mut rx,
        &mut tx,
        &pump::tagged_payload(1, 8 * CHUNK),
        "liveness",
        Duration::from_secs(90),
        PumpOpts {
            pace: Some(PACE),
            chunk: Some(CHUNK),
            phase: Some(1),
            ..Default::default()
        },
    )
    .await?;
    assert!(
        echo.arrival_pct() > 50.0,
        "a Session completed by fill must still carry application traffic, but only {:.0}% came \
         back",
        echo.arrival_pct()
    );

    tracing::info!(summary = %delta.summary(), "idle shape PASSED");
    Ok(())
}

/// A browsing Session sustains its cycles across the gaps between bursts.
///
/// Casual browsing is a page load and then a pause: bursts of a few tens of kB separated by
/// seconds of silence. The same bytes spread evenly would keep a cycle advancing steadily, so the
/// duty cycle is the whole point — the gaps are where the Exit has to decide whether to make up
/// the shortfall, and a cycle that only completes because the burst average happened to clear the
/// deadline has not been tested for anything.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_browsing_session_sustains_its_cycles() -> anyhow::Result<()> {
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

    let (mut rx, mut tx) = tokio::io::split(session);
    // 40 kB pages, 3 s apart: ~13 kB/s average, well under the ~270 kB/s a cycle wants, so the
    // application covers roughly a twentieth of what the deadline needs and fill covers the rest.
    let payload = payload_for(2);
    let transfer = pump::pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "browsing",
        sweep_budget(2),
        PumpOpts {
            pace: Some(PACE),
            chunk: Some(CHUNK),
            shape: Some(pump::Shape::Burst {
                on: 40 * 1024,
                off: Duration::from_secs(3),
            }),
            idle_budget: Some(Duration::from_secs(60)),
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(
        arrival_pct = transfer.arrival_pct(),
        p95_gap = transfer.inter_arrival_quantile(0.95).unwrap_or(0.0),
        outcome = ?transfer.outcome,
        "browsing traffic finished"
    );

    let delta = await_sweeps(&exit, &before, 2, sweep_budget(1)).await?;
    assert_eq!(
        0,
        delta.deposits_timed_out().unwrap_or(0),
        "the Exit gave up on a deposit during a bursty Session: {}",
        delta.summary()
    );
    assert!(
        delta.sweeps().unwrap_or(0) >= 2,
        "a browsing Session completed fewer than two cycles, so it did not sustain them: {}",
        delta.summary()
    );

    tracing::info!(summary = %delta.summary(), "browsing shape PASSED");
    Ok(())
}

/// A sustained download completes its cycles on application traffic alone.
///
/// The direction PIX bills is the one a download saturates: every reply packet the Exit forwards
/// carries a share, so a client pulling at the profile's rate produces exactly the return traffic
/// a cycle needs and fill should have nothing to make up. That is the easy case for PIX and the
/// hard case for the *plumbing* — it is the only shape here that puts the SURB pipeline under
/// sustained pressure in the direction it is sized for, so it is where a mis-sized buffer shows up
/// as the Exit starving mid-cycle.
///
/// Genuinely asymmetric, unlike everything else in this file: the Session targets a local UDP
/// service that pushes rather than the Exit's loopback, so the return volume is what the *service*
/// decides and not an echo of what was sent.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_download_session_sustains_its_cycles() -> anyhow::Result<()> {
    shapes::install_profile();
    cluster::request_cluster_size(3);

    // Two cycles' worth of packets pushed at the profile's rate.
    let service = udp_service::spawn(udp_service::Mode::Push {
        datagram: CHUNK,
        rate: shapes::TARGET_PACKET_RATE,
        total_bytes: shapes::CYCLE_PACKETS * 2 * CHUNK as u64,
    })
    .await?;

    let env = IntegrationEnv::setup_pix_with(shapes::entry_config()?).await?;
    let (session, exit_addr) = env
        .open_pix_session_with(HOPS, HOPS, shapes::surb_balancer(), service.target())
        .await?;
    let exit = node_for(&env, exit_addr)?;
    let before = pix::sample_exit(&exit).await?;

    let (mut rx, mut tx) = tokio::io::split(session);
    // One datagram is the whole request; everything after it is the service's stream coming back.
    let transfer = pump::pump_halves(
        &mut rx,
        &mut tx,
        &pump::tagged_payload(0, 32),
        "download",
        sweep_budget(2),
        PumpOpts {
            // The reply stream is not an echo, so nothing arriving matches the phase tag that was
            // sent. Untagged attribution counts every byte, which is what a download is.
            phase: None,
            idle_budget: Some(Duration::from_secs(60)),
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(
        received_bytes = transfer.received_bytes,
        pushed_by_service = service.sent(),
        mbps = transfer.throughput_at(0.9).unwrap_or(0.0),
        outcome = ?transfer.outcome,
        "download traffic finished"
    );
    anyhow::ensure!(
        transfer.received_bytes > 0,
        "the download never started, so the Exit could not reach the UDP service at {} — check \
         `use_target_allow_list` on the generated node config: {:?}",
        service.addr(),
        transfer.outcome
    );

    let delta = await_sweeps(&exit, &before, 2, sweep_budget(1)).await?;
    assert!(
        delta.sweeps().unwrap_or(0) >= 2,
        "a saturated download completed fewer than two cycles, which is the shape PIX is best \
         suited to: {}",
        delta.summary()
    );

    tracing::info!(summary = %delta.summary(), "download shape PASSED");
    Ok(())
}

/// A sustained upload completes its cycles on fill, because almost nothing comes back.
///
/// The mirror of the download and the harder case for PIX: the client saturates the direction that
/// is *not* billed, and the service answers with nothing, so the return path carries only what the
/// protocol itself generates. A cycle cannot advance on that — which before fill meant a
/// bulk-uploading client stranded every deposit it made, while paying for the quota.
///
/// The assertion is deliberately about cycles rather than about fill packets: how the Exit makes
/// up the shortfall is its business, and pinning the mechanism here would make this a test of the
/// implementation rather than of the property.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn an_upload_session_completes_on_fill() -> anyhow::Result<()> {
    shapes::install_profile();
    cluster::request_cluster_size(3);

    let service = udp_service::spawn(udp_service::Mode::Sink).await?;

    let env = IntegrationEnv::setup_pix_with(shapes::entry_config()?).await?;
    let (session, exit_addr) = env
        .open_pix_session_with(HOPS, HOPS, shapes::surb_balancer(), service.target())
        .await?;
    let exit = node_for(&env, exit_addr)?;
    let before = pix::sample_exit(&exit).await?;

    let (mut rx, mut tx) = tokio::io::split(session);
    let aim_point = shapes::MAX_RECOVERY_TIME.mul_f64(shapes::FILL_FINISH_FRACTION);
    let payload = payload_for(1);
    tracing::info!(
        bytes = payload.len(),
        "uploading into a sink; nothing will come back"
    );

    let transfer = pump::pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "upload",
        aim_point,
        PumpOpts {
            pace: Some(PACE),
            chunk: Some(CHUNK),
            // Nothing is expected back at all, so the read side must not end the pump early —
            // `Idle` here is the shape working as intended, not a stalled Session.
            idle_budget: Some(aim_point),
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(
        sent_bytes = transfer.sent_bytes,
        absorbed_by_service = service.received(),
        received_bytes = transfer.received_bytes,
        outcome = ?transfer.outcome,
        "upload traffic finished"
    );
    anyhow::ensure!(
        service.received() > 0,
        "nothing reached the UDP service, so the upload never left the Session: {:?}",
        transfer.outcome
    );

    let delta = await_sweeps(&exit, &before, 1, aim_point).await?;
    assert!(
        delta.keys_recovered().unwrap_or(0) >= 1,
        "an upload-only Session stranded its deposit — the return path carried too little to \
         complete the cycle and nothing made up the shortfall: {}",
        delta.summary()
    );
    assert!(
        delta.deposits_confirmed().unwrap_or(0) >= 2,
        "the upload's cycle completed but no successor was funded: {}",
        delta.summary()
    );

    tracing::info!(summary = %delta.summary(), "upload shape PASSED");
    Ok(())
}

/// One Session carries a burst, then goes quiet, then transfers in bulk — and sustains its cycles
/// across all three.
///
/// A real client does not pick a shape and keep it. What this adds over the shapes run separately
/// is the *transitions*: a cycle that was being carried by the application when the application
/// stops has to be picked up mid-flight from whatever remainder is outstanding, and one being
/// carried by fill when traffic resumes has to yield rather than keep sending on top of it. Both
/// are re-planning behaviour that a steady shape never exercises.
///
/// Phases are tagged so each is attributed its own arrivals, and separated by `drain_until_quiet`
/// so a phase's tail is not counted as the next one's.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_mixed_session_sustains_its_cycles() -> anyhow::Result<()> {
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
    let (mut rx, mut tx) = tokio::io::split(session);

    // 1. Browsing: a quarter of a cycle in bursts, so the cycle is genuinely part-finished when
    //    the application goes quiet and fill has a real remainder to plan against.
    let browsing = pump::pump_halves(
        &mut rx,
        &mut tx,
        &pump::tagged_payload(0, (shapes::CYCLE_PACKETS / 4) as usize * CHUNK),
        "mixed/browsing",
        sweep_budget(1),
        PumpOpts {
            pace: Some(PACE),
            chunk: Some(CHUNK),
            phase: Some(0),
            shape: Some(pump::Shape::Burst {
                on: 40 * 1024,
                off: Duration::from_secs(3),
            }),
            idle_budget: Some(Duration::from_secs(60)),
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(
        arrival_pct = browsing.arrival_pct(),
        "mixed: browsing phase done"
    );
    let leftover = pump::drain_until_quiet(&mut rx, Duration::from_secs(5), "mixed/drain").await;
    tracing::info!(leftover, "mixed: settled before going quiet");

    // 2. Quiet: no application traffic at all for two minutes. The cycle keeps its deadline.
    tracing::info!("mixed: going quiet for 120s");
    tokio::time::sleep(Duration::from_secs(120)).await;

    // 3. Bulk: the rest of the cycle and another, at the full rate.
    let bulk = pump::pump_halves(
        &mut rx,
        &mut tx,
        &pump::tagged_payload(1, (shapes::CYCLE_PACKETS * 3 / 2) as usize * CHUNK),
        "mixed/bulk",
        sweep_budget(2),
        PumpOpts {
            pace: Some(PACE),
            chunk: Some(CHUNK),
            phase: Some(1),
            idle_budget: Some(Duration::from_secs(60)),
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(
        arrival_pct = bulk.arrival_pct(),
        foreign_bytes = bulk.foreign_bytes,
        outcome = ?bulk.outcome,
        "mixed: bulk phase done"
    );
    assert!(
        bulk.arrival_pct() > 50.0,
        "the Session did not carry traffic again after going quiet, so it did not survive the \
         transition: {:.0}% came back",
        bulk.arrival_pct()
    );

    let delta = await_sweeps(&exit, &before, 2, sweep_budget(1)).await?;
    assert_eq!(
        0,
        delta.deposits_timed_out().unwrap_or(0),
        "a deposit timed out across a change of shape: {}",
        delta.summary()
    );
    assert!(
        delta.sweeps().unwrap_or(0) >= 2,
        "a Session that browsed, went quiet and then transferred completed fewer than two cycles: \
         {}",
        delta.summary()
    );

    tracing::info!(summary = %delta.summary(), "mixed shape PASSED");
    Ok(())
}
