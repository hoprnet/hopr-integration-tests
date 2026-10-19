//! PIX at the **deployed geometry**.
//!
//! `tests/pix.rs` asks whether the deposit exchange happens at all, and answers it at a demo
//! geometry of 8 polynomials x (2 + 2) — a cycle of 32 packets. `tests/pix_shapes.rs` asks whether
//! a Session sustains its cycles under realistic traffic, at a profile whose whole design is that
//! it is *not* the deployed one (see `crate::shapes`' argument that PIX is governed by ratios).
//!
//! This suite runs the third question, which neither of those can: **does the configuration that
//! actually ships recover its cycles?**
//!
//! A second scenario — killing one of the Exit's two return relayers mid-cycle — is not here yet;
//! see the note at the end of this file.
//!
//! # The geometry under test
//!
//! `4096 × (32 + 79)`, hoprnet#8519's deployed default. A cycle is 454 656 packets and one deposit
//! is `price_per_byte × 629.6 MiB`. See [`hoprd_integration_test::prod_pix`] for why running the
//! real dimensions is not the same experiment as running a scaled profile that agrees with them on
//! every ratio.
//!
//! # The topology
//!
//! ```text
//!   Entry (edgli, in-process)
//!      │  forward, direct
//!      ▼
//!   relay A ──────► Exit          (one of three cluster nodes)
//!      ▲              │
//!      └──────────────┘  return, 1 hop
//! ```
//!
//! The **forward** path is direct and the **return** path is one hop. Only the return path needs a
//! relay: PIX refuses a Session whose return path has no intermediate hop, because shares are
//! encrypted with the first return relayer's ticket-challenge solution and carried in return-path
//! SURBs, so a direct Exit → Entry SURB has no relayer to supply it and the ongoing share delivery
//! is dead. The forward direction is free to be direct, and leaving it so is what keeps the
//! return-relayer histogram readable — see [`FORWARD_HOPS`].
//!
//! Three nodes is the minimum at which the Exit has exactly two return-relayer candidates: the full
//! mesh gives every node a channel to the other two, and the Exit is itself one of the three, so
//! its return path can only start at one of the remaining two.
//!
//! # Running
//!
//! ```bash
//! HOPRD_SRC=../hoprd LINE=v5 just pix-prod
//! ```
//!
//! Manual, like every PIX scenario here — never in CI, and needing a hoprd built with a deposit
//! pool.
//!
//! # Which build each participant runs
//!
//! | participant | role | comes from |
//! | ----------- | ---- | ---------- |
//! | entry (`edgli`, in-process) | opens the Session, mints SURBs, **pays the deposits** | the `edgli` git dependency, compiled into this binary |
//! | relay + Exit | forward packets, reply, recover keys, **sweep** | `$HOPRD_BIN` at runtime |
//!
//! Compile-time on one side and runtime on the other, so the two can silently disagree. Both must
//! carry PIX *and* the hoprnet#8519 split: the Exit's `PixGlobalConfig` must default to
//! `4096 × (32 + 79)` or the surplus this scenario is exercising is not the one being run. The
//! pool marker check in `scripts/integration/lib.sh` covers the Exit; the entry's dimensions are
//! asserted from this process's own config by [`hoprd_integration_test::prod_pix::install_profile`].

#![cfg(feature = "v5")]

use std::time::Duration;

use anyhow::Context as _;
use hoprd_integration_test::{
    Address, IntegrationEnv,
    cluster::{self, NodeInfo},
    pix::{self, PixCounters},
    pix_exit::{self, ExitTelemetry, Trace},
    prod_pix,
    pump::{self, PumpOpts, Transfer},
};
use rand::RngExt as _;

/// Nodes in the cluster.
///
/// Three, so the Exit has exactly two return-relayer candidates: the full mesh gives every node a
/// channel to the other two, and the Exit is itself one of the three. Nothing here depends on that
/// count, but the relayer-kill scenario this file is being extended with does — killing one of two
/// removes half the return traffic by construction, where a third candidate would make it an
/// approximate loss.
const NODES: usize = 3;

/// Hops on the forward path — **zero**, deliberately.
///
/// PIX refuses a zero-hop *return* path (a share is encrypted with the first return relayer's
/// ticket-challenge solution, and a direct Exit -> Entry SURB has no relayer to supply it), but the
/// forward path is free to be direct. Leaving it direct is what makes the return-relayer histogram
/// readable: `hopr_packets_count{type="forwarded"}` does not say which direction a packet was
/// travelling, so with a relay on the way out that node's forwarded count lands in the same
/// per-node deltas and the histogram measures the forward hop as much as the return one. Measured:
/// with a 1-hop forward the "return relayer split" read 76/24, which is a forward-hop artefact
/// rather than a return-path concentration.
///
/// This is the shape `tests/return_path.rs` uses for the same reason, and the shape issue #74 asks
/// for.
const FORWARD_HOPS: usize = 0;

/// Hops on the return path. Exactly one, so the killed relayer is the first and only return hop and
/// the traffic it carried is lost shares.
const RETURN_HOPS: usize = 1;

/// Bytes per write, under `SESSION_MTU` so one write is one packet.
///
/// 1 400 leaves the MTU a little slack, which is what hoprd's own soak uses. It sets nothing but
/// how much data a funded cycle moves: a packet is billed one full segment whatever it carries.
const CHUNK: usize = 1_400;

/// Cycles the happy-path scenario drives and asserts.
///
/// Three: the first is the one the successor gate has to admit on, the second proves the gate
/// admitted it, and the third gives the run a margin so a single slow cycle is not a failure.
/// Measured at 3 000 packets/s the achieved cycle is ~145 s against a 152 s nominal, so three
/// cycles is ~7 min of traffic — which is what sets [`TRAFFIC_BUDGET`], and the ~3 min of cluster
/// bootstrap is what makes the whole run ~10 min rather than under 5.
const TARGET_CYCLES: u64 = 3;

/// How far the entry's deposits may legitimately run ahead of the Exit's recoveries.
///
/// The Exit requests the next SSA once the current one passes its early-recovery threshold, and the
/// entry deposits for it immediately, so at any instant one SSA is normally funded but not yet
/// recovered. Two allows for the sample landing mid-handover.
const MAX_SSAS_IN_FLIGHT: u64 = 2;

/// Segments written per paced step.
///
/// **This is the knob that makes the profile's rate reachable, and getting it wrong is silent.**
/// `PumpOpts::pace` sleeps once per *write*, and tokio's timer wheel floors a sleep at ~1 ms
/// whatever is asked of it — so a pace of `1 / rate` seconds below ~1 000 packets/s cannot be
/// expressed at all, and a pace derived for one segment throttles the whole run to that floor. A
/// run configured for 3 000 packets/s then delivers ~600 and reports a geometry that never
/// completes, which reads as a broken surplus rather than as a mis-paced writer. Measured: 30 s at
/// a 3 000/s pace moved ~19 000 segments, i.e. ~620/s.
///
/// Batching 50 segments per sleep puts the step at 16.7 ms at the default rate, three orders of
/// magnitude clear of the floor, and the burst within a step is 50 × 1 400 = 70 kB — well inside
/// the Entry's inbox, so the reply stream it produces is not dropped for backpressure either.
const SEGMENTS_PER_STEP: u64 = 50;

/// Bytes written per paced step.
fn step_bytes() -> usize {
    (SEGMENTS_PER_STEP * CHUNK as u64) as usize
}

/// Send pace: one *step* of [`SEGMENTS_PER_STEP`] segments per this.
fn pace() -> Duration {
    Duration::from_micros(
        1_000_000 * SEGMENTS_PER_STEP / prod_pix::packet_rate().max(SEGMENTS_PER_STEP),
    )
}

/// Datagrams to offer over `budget` at the profile's rate.
///
/// The one place the payload size comes from, so a rate override and the budget cannot disagree
/// about how long the run lasts.
fn datagrams_for(budget: Duration) -> u64 {
    prod_pix::packet_rate() * budget.as_secs()
}

/// Seconds of traffic the scenario offers, which is what bounds its wall-clock.
///
/// **Time, not a packet count, and that is the point.** A cycle is bounded by the *share* rate,
/// which is not the forward packet rate: the Exit spends one return-path SURB per reply and a share
/// rides on a reply only when the emission queue has one ready, so hoprd's own soak measures shares
/// per reply falling from ~0.7 at 1 000 packets/s to ~0.3 at 5 000. Sizing a payload from
/// `cycles × emissions` therefore under-offers by whatever that ratio turns out to be, and the
/// error is silent — the run simply sweeps fewer cycles than it asked for and reports a broken
/// surplus.
///
/// So the scenario offers traffic for a fixed span and asserts how many cycles that bought. Sized
/// from [`TARGET_CYCLES`] at the *achieved* cycle length — measured 116 s at 4 000 packets/s, i.e.
/// the configured rate — with a whole extra cycle of slack, so a run that lands on the target is
/// not one that only just scraped it.
///
/// Measured: this budget bought 5 cycles in a run that asserts 3, so the slack is real rather than
/// nominal.
const TRAFFIC_BUDGET: Duration = Duration::from_secs(4 * 116);

/// Budget for offering a payload of `traffic` seconds at the paced rate.
///
/// The pace is per write, so the payload is offered over roughly its own span; the multiplier
/// covers backpressure (a session whose sink is full stretches the offer) and the slack covers the
/// reader's tail. A budget under the offer's own span measures the harness rather than the
/// geometry.
fn pump_budget(traffic: Duration) -> Duration {
    traffic * 3 + Duration::from_secs(120)
}

/// How long to keep watching the Exit after the traffic stops.
///
/// The last cycle's recovery and sweep are two on-chain round trips behind the reply that completed
/// it, so reading balances the instant the pump returns undercounts by a cycle or more.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Poll cadence while waiting on sweeps.
const SETTLE_POLL: Duration = Duration::from_secs(5);

/// How long to keep watching the Exit's Safe after its counters say it swept.
const PAYMENT_SETTLE: Duration = Duration::from_secs(180);

/// A cluster node by address.
fn node_for(env: &IntegrationEnv, address: Address) -> anyhow::Result<NodeInfo> {
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == address)
        .cloned()
        .with_context(|| format!("no cluster node at {address}"))
}

/// Random bytes to offer over `budget` at the profile's rate.
///
/// Random rather than patterned so each run's packet ciphertexts are unique and replay tags do not
/// collide, the same reason `tests/integration.rs` randomises its payload.
fn payload_for(budget: Duration) -> Vec<u8> {
    let mut payload = vec![0u8; datagrams_for(budget) as usize * CHUNK];
    rand::rng().fill(&mut payload[..]);
    payload
}

/// `PumpOpts` for a PIX session at this geometry: a small chunk paced to the profile's rate.
fn pix_pump_opts() -> PumpOpts {
    PumpOpts {
        pace: Some(pace()),
        chunk: Some(step_bytes()),
        ..PumpOpts::default()
    }
}

/// Poll the Exit until it has swept `target` cycles, or the deadline expires.
///
/// Returns the counters as of the last poll either way — a timeout here is not itself the verdict,
/// since the assertions downstream say more about *why* than "it did not happen".
async fn await_sweeps(
    exit: &NodeInfo,
    before: &PixCounters,
    target: u64,
    timeout: Duration,
) -> anyhow::Result<PixCounters> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let delta = before.delta(&pix::sample_exit(exit).await?);
        tracing::info!("exit PIX counters: {}", delta.summary());
        if delta.sweeps().unwrap_or(0) >= target || tokio::time::Instant::now() >= deadline {
            return Ok(delta);
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

/// Assert the Exit's Safe actually **received** what its counters say it swept.
///
/// Every other assertion reads `hopr_strategy_pix_*`, which is the Exit's own bookkeeping:
/// `keys_recovered` and `sweeps` say it got as far as recovering a key and submitting a sweep.
/// Neither says money moved, and there is a documented path where it does not — a cycle that
/// completes before its deposit transaction is mined leaves the Exit recovering a key against a
/// zero balance, logging "already swept", and the funds stranded at the stealth address. Both
/// counters increment on that path. Only the Safe balance tells them apart, and being paid is the
/// entire point of PIX.
async fn assert_exit_was_paid(
    exit: &NodeInfo,
    before: &pix::NodeBalances,
    swept: u64,
    required: u64,
) -> anyhow::Result<()> {
    let per_cycle = prod_pix::per_cycle()?;
    let target = per_cycle * swept.max(required);
    let deadline = tokio::time::Instant::now() + PAYMENT_SETTLE;
    let mut delta = pix::node_balances(exit).await?.safe - before.safe;
    while tokio::time::Instant::now() < deadline && delta < target {
        tokio::time::sleep(SETTLE_POLL).await;
        delta = pix::node_balances(exit).await?.safe - before.safe;
        tracing::info!(%delta, %target, "waiting for the Exit's Safe to receive the swept cycles");
    }

    let floor = per_cycle * required;
    tracing::info!(
        %delta, %floor, %per_cycle, swept,
        whole_cycles = ?pix::completed_cycles(delta, per_cycle),
        "the Exit's Safe over the scenario"
    );
    assert!(
        delta >= floor,
        "the Exit's counters claim {swept} swept cycle(s) but its Safe rose by only {delta}, \
         against {floor} for the {required} this scenario requires. A sweep counted without funds \
         arriving is the 'already swept' path — the key was recovered against a stealth address \
         the deposit had not reached. {}",
        pix::sample_exit(exit).await?.summary()
    );
    Ok(())
}

/// A one-line reading of how fast the Session actually carried data, for a shortfall's message.
///
/// A cycle count below target has two very different causes — the geometry not recovering, and the
/// Session simply not moving enough packets to have finished the cycles in the time offered — and
/// the raw arrival figure cannot tell them apart. Naming the achieved packet rate next to the
/// configured one does, in the run's own numbers rather than in a reader's inference.
fn geometry_rate_hint(transfer: &Transfer, offered: Duration) -> String {
    let seconds = transfer.wall_seconds.max(1.0);
    let segments = transfer.attributed_bytes as f64 / CHUNK as f64;
    format!(
        "The Session carried {segments:.0} segments in {seconds:.0}s of wall clock ({:.0}/s \
         against a configured {} packets/s), offering {offered:?} of traffic at that pace; a cycle \
         needs {} packets, so this run had time for ~{:.1} cycles.",
        segments / seconds,
        prod_pix::packet_rate(),
        prod_pix::CYCLE_PACKETS,
        seconds * (segments / seconds) / prod_pix::CYCLE_PACKETS as f64,
    )
}

/// The Exit served traffic and was paid for it, with no cycle lost and the Session never closing.
///
/// Shared by both scenarios: the relayer-kill one differs only in *when* a relayer dies, so
/// anything that separates them is the protocol behaving differently rather than the measurement.
#[allow(clippy::too_many_arguments)]
fn assert_the_deployed_geometry_recovered(
    transfer: &Transfer,
    exit_counters: &PixCounters,
    entry_counters: &PixCounters,
    required: u64,
    label: &str,
) {
    // Nothing below means anything about a session with no counterparty.
    assert!(
        transfer.arrival_pct() > 0.0,
        "{label}: no byte completed the entry -> exit -> loopback -> entry round trip, so the PIX \
         session never carried traffic (outcome {:?}; entry {}; exit {})",
        transfer.outcome,
        entry_counters.summary(),
        exit_counters.summary(),
    );

    // The counters are what every assertion after this reads. Absent means the Exit was built
    // without `hopr-strategy/telemetry`, and reporting that as zero would blame the entry.
    assert!(
        exit_counters.observable(),
        "{label}: the Exit exposed no hopr_strategy_pix_* series at all — it was built without PIX \
         strategy telemetry, so nothing here was measured"
    );

    // "The Exit sees the deposit and does not close the session": its deposit pool counts one
    // confirmation per SSA when the deposit lands inside the deadline, and a timeout when it does
    // not — the latter being what the Exit's supervisor closes the Session on.
    assert_eq!(
        exit_counters.deposits_timed_out().unwrap_or(0),
        0,
        "{label}: the Exit gave up waiting for {:?} deposit(s) and closed the session on its PIX \
         deposit deadline (it confirmed {:?}). Either the entry never deposited — check its \
         `deposits` and `over_budget` counters — or the deposit landed outside the \
         max_deposit_wait + max_ssa_delivery_time window.",
        exit_counters.deposits_timed_out(),
        exit_counters.deposits_confirmed(),
    );
    assert!(
        exit_counters.deposits_confirmed().unwrap_or(0) >= required,
        "{label}: the Exit confirmed only {:?} deposits, expected at least {required} (entry: {})",
        exit_counters.deposits_confirmed(),
        entry_counters.summary(),
    );
    assert!(
        exit_counters.keys_recovered().unwrap_or(0) >= required
            && exit_counters.sweeps().unwrap_or(0) >= required,
        "{label}: the Exit recovered {:?} keys and swept {:?} of them, expected at least {required} \
         each. Recoveries without sweeps means the funds are reachable and were not collected; \
         neither means shares never completed an SSA.",
        exit_counters.keys_recovered(),
        exit_counters.sweeps(),
    );

    // A failed deposit means one was attempted and did not land. The budget is 10x the target, and
    // a refusal for budget is a separate counter, so neither explains a non-zero here.
    assert_eq!(
        entry_counters.deposits_failed().unwrap_or(0),
        0,
        "{label}: the entry failed {:?} deposit(s) despite being budgeted for {} cycles ({})",
        entry_counters.deposits_failed(),
        prod_pix::BUDGETED_CYCLES,
        entry_counters.summary(),
    );
    assert_eq!(
        entry_counters.deposits_over_budget().unwrap_or(0),
        0,
        "{label}: the entry hit its {}-cycle deposit budget, so it stopped paying part-way through \
         a scenario that is not supposed to reach it ({})",
        prod_pix::BUDGETED_CYCLES,
        entry_counters.summary(),
    );
}

/// Close the scenario's Session and wait for the Exit to stop supervising it.
///
/// The Exit's `hopr_pix_*` family has no Session label, so a Session left behind becomes part of
/// whichever scenario lands on that Exit next. Dropping one does not close it: the Entry is gone,
/// but the Exit keeps supervising its funded successor cycle until `max_recovery_time`.
async fn close_session(tx: &mut tokio::io::WriteHalf<hoprd_integration_test::HoprSession>) {
    use tokio::io::AsyncWriteExt as _;
    if let Err(error) = tx.shutdown().await {
        tracing::warn!(%error, "closing the PIX Session failed; the Exit may keep supervising it");
    }
}

/// Assert the Exit's egress gate never parked for `share_lag` and the surplus was served.
///
/// A gate that parks part-way through a surplus run stops the very SURB spending that was draining
/// it, and what a scenario then sees is `sweeps` failing to move — indistinguishable from a
/// starved buffer or an unmined deposit. This is the assertion that names the cause.
fn assert_the_gate_served_the_surplus(gate: &ExitTelemetry, trace: &Trace, label: &str) {
    let ceiling = prod_pix::max_served_without_progress();
    tracing::info!(
        gate = %gate.summary(),
        trace = %trace.summary(),
        ceiling,
        "{label}: the Exit's egress gate over the scenario"
    );

    assert!(
        gate.observable(),
        "{label}: the Exit exposes no hopr_pix_* series at all, so none of what follows was \
         measured — either it was built without `hopr-transport-session/telemetry` or it predates \
         hoprnet#8411. These are not zeroes."
    );
    assert_eq!(
        0,
        gate.gate_blocks("share_lag").unwrap_or(0),
        "{label}: the Exit's egress gate parked on share lag. The free credit (parts x surplus) \
         covers the SURB queue several times over, so the drain after a recovery is credited as \
         liveness and a block here is the false stall the surplus run exists to catch. {}",
        gate.summary()
    );
    assert!(
        !trace.saw_share_lag_block(),
        "{label}: the gate parked on share lag at some point mid-scenario and had resumed by the \
         end, so the before/after counters look clean: {}",
        trace.summary()
    );

    let surplus = gate.shares("surplus").unwrap_or(0);
    assert!(
        surplus >= ceiling,
        "{label}: the Exit accepted only {surplus} surplus shares against a gate ceiling of \
         {ceiling}, so the scenario never reached a surplus run long enough to test the gate at \
         all — the assertion above passed vacuously. {}",
        gate.summary()
    );

    // A recovered cycle's accepted-share fraction sits above 1.0 when the Exit took the negotiated
    // surplus: a complete cycle lands at `(threshold + surplus) / threshold` = 3.47 at this split.
    // Independent of the trace's sampling entirely.
    match gate.accepted_fraction("recovered") {
        Some(hist) => {
            let above_one = hist.above(1.0).unwrap_or(0);
            assert!(
                above_one > 0,
                "{label}: no finalized cycle recorded an accepted-share fraction above 1.0, so the \
                 Exit never took a conforming surplus — a cycle at this split lands at {:.2}. {}",
                (prod_pix::PIX_SHARES + prod_pix::PIX_ADDITIONAL_SHARES) as f64
                    / prod_pix::PIX_SHARES as f64,
                gate.summary()
            );
        }
        None => panic!(
            "{label}: the Exit recorded no `hopr_pix_cycle_accepted_share_fraction{{outcome=\"recovered\"}}` \
             at all, so nothing says the surplus was served ({})",
            gate.summary()
        ),
    }
}

/// The happy path at the deployed geometry: three cycles complete, are swept, and are paid for.
///
/// This is the scenario that answers "can the shipped configuration run at all on a machine that
/// is not the deployed network". The relayer-kill scenario below is the one that answers the
/// resilience question; this is its control, and a failure here makes that one unreadable.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn the_deployed_geometry_should_complete_and_sweep_its_cycles() -> anyhow::Result<()> {
    prod_pix::install_profile();
    cluster::request_cluster_size(NODES);
    let t0 = std::time::Instant::now();

    let per_cycle = prod_pix::per_cycle()?;
    tracing::info!(
        %per_cycle,
        quota = prod_pix::QUOTA_PER_SSA,
        cycle_packets = prod_pix::CYCLE_PACKETS,
        packet_rate = prod_pix::packet_rate(),
        nominal_cycle_secs = prod_pix::nominal_cycle_secs(),
        target_cycles = TARGET_CYCLES,
        "PIX accounting at the DEPLOYED geometry: one cycle costs price_per_byte x 629.6 MiB"
    );

    let env = IntegrationEnv::setup_pix_with(prod_pix::entry_config()?).await?;
    let (session, exit_addr) = env
        .open_pix_session_with(
            FORWARD_HOPS,
            RETURN_HOPS,
            prod_pix::surb_balancer(),
            hoprd_integration_test::SessionTarget::ExitNode(0),
        )
        .await?;
    let exit = node_for(&env, exit_addr)?;
    tracing::info!(exit = %exit_addr, elapsed = ?t0.elapsed(), "PIX session open at the deployed geometry");

    let exit_before = pix::node_balances(&exit).await?;
    let exit_counters_before = pix::sample_exit(&exit).await?;
    let entry_counters_before = pix::sample_entry();
    let entry_before = env.entry_safe_balance().await?;
    let gate_before = pix_exit::sample(&exit).await?;
    let sampler = pix_exit::Sampler::start(&exit);
    tracing::info!(entry_safe = %entry_before, exit_safe = %exit_before.safe, "balances before the session");

    let payload = payload_for(TRAFFIC_BUDGET);
    tracing::info!(
        bytes = payload.len(),
        budget = ?TRAFFIC_BUDGET,
        packet_rate = prod_pix::packet_rate(),
        "offering traffic at {} packets/s for {TRAFFIC_BUDGET:?}",
        prod_pix::packet_rate()
    );

    let (mut rx, mut tx) = tokio::io::split(session);
    let transfer = pump::pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "pix-prod",
        pump_budget(TRAFFIC_BUDGET),
        pix_pump_opts(),
    )
    .await?;

    let exit_counters =
        await_sweeps(&exit, &exit_counters_before, TARGET_CYCLES, SETTLE_TIMEOUT).await?;
    let entry_counters = entry_counters_before.delta(&pix::sample_entry());
    let trace = sampler.finish().await;
    let gate = gate_before.delta(&pix_exit::sample(&exit).await?);
    let recovered = pix::node_balances(&exit).await?.safe - exit_before.safe;

    tracing::info!(
        %recovered, elapsed = ?t0.elapsed(),
        "entry counters: {} | exit counters: {}",
        entry_counters.summary(),
        exit_counters.summary()
    );

    assert_the_deployed_geometry_recovered(
        &transfer,
        &exit_counters,
        &entry_counters,
        TARGET_CYCLES,
        "deployed geometry",
    );

    // The real verdict: an exact whole multiple of the per-SSA deposit, which with auto-redeeming
    // off is the only thing that credits the Exit's Safe in wxHOPR.
    let cycles = pix::completed_cycles(recovered, per_cycle).unwrap_or_else(|| {
        panic!(
            "the Exit's Safe gained {recovered}, which is not a whole multiple of the {per_cycle} \
             per-SSA deposit — something other than PIX sweeps moved the balance (exit: {})",
            exit_counters.summary()
        )
    });
    assert!(
        cycles >= TARGET_CYCLES,
        "expected at least {TARGET_CYCLES} completed SSA cycles, got {cycles} ({recovered} of the \
         {} target) after {:?}. A recovered-key count above the cycle count means keys were \
         reconstructed before their deposits were mined and the funds are stranded at the stealth \
         addresses. {} (exit: {})",
        per_cycle * TARGET_CYCLES,
        t0.elapsed(),
        geometry_rate_hint(&transfer, TRAFFIC_BUDGET),
        exit_counters.summary(),
    );

    // The entry paid for every one of those, and for hardly any more.
    let deposited = entry_counters.deposits().unwrap_or_else(|| {
        panic!(
            "the entry exposed no hopr_strategy_pix_deposits_total series — it was built without \
             `hopr-strategy/telemetry` ({})",
            entry_counters.summary()
        )
    });
    assert!(
        (cycles..=cycles + MAX_SSAS_IN_FLIGHT).contains(&deposited),
        "the entry deposited for {deposited} SSAs but only {cycles} were recovered and swept. Up \
         to {MAX_SSAS_IN_FLIGHT} may legitimately be in flight thanks to early-recovery \
         pipelining; more than that means deposits are being made for SSAs that never complete."
    );

    assert_the_gate_served_the_surplus(&gate, &trace, "deployed geometry");
    assert_exit_was_paid(
        &exit,
        &exit_before,
        exit_counters.sweeps().unwrap_or(0),
        TARGET_CYCLES,
    )
    .await?;

    close_session(&mut tx).await;
    tracing::info!(
        cycles, deposited, %recovered,
        "deployed-geometry PIX test PASSED in {:?}", t0.elapsed()
    );
    Ok(())
}
