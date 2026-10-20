//! PIX at the **deployed geometry**, with the Exit as the only Exit and the relayers killable.
//!
//! `tests/pix.rs` asks whether the deposit exchange happens at all, and answers it at a demo
//! geometry of 8 polynomials x (2 + 2) — a cycle of 32 packets. `tests/pix_shapes.rs` asks whether
//! a Session sustains its cycles under realistic traffic, at a profile whose whole design is that
//! it is *not* the deployed one (see `crate::shapes`' argument that PIX is governed by ratios).
//!
//! This suite runs the third question, which neither of those can: **does the configuration that
//! actually ships recover its cycles, and does it still recover them when half of the return
//! relayers are killed mid-cycle?**
//!
//! # The geometry under test
//!
//! `4096 × (32 + 79)`, hoprnet#8519's deployed default. A cycle is 454 656 packets and one deposit
//! is `price_per_byte × 629.6 MiB`. See [`hoprd_integration_test::prod_pix`] for why running the
//! real dimensions is not the same experiment as running a scaled profile that agrees with them on
//! every ratio.
//!
//! # The topology, and why three nodes is not an accident
//!
//! ```text
//!   Entry (edgli, in-process)
//!      │  forward, direct
//!      ▼
//!   relay A ──────► Exit          (one of three cluster nodes)
//!      ▲              │
//!      └──────────────┘  return, 1 hop — drawn per packet from {B, C}
//! ```
//!
//! The **forward** path is direct and the **return** path one hop, which is not symmetric for two
//! independent reasons. PIX requires a relay on the return path — a share is encrypted with the
//! first return relayer's ticket-challenge solution and carried in a return-path SURB, so a direct
//! Exit → Entry SURB has no relayer to supply it and share delivery is dead — but the forward
//! direction has no such constraint, and leaving it direct is what keeps the per-node
//! `hopr_packets_count{type="forwarded"}` deltas readable as a *return*-relayer histogram. A
//! forwarded packet does not say which direction it travelled, so a relay on the way out lands in
//! the same counters. See [`FORWARD_HOPS`].
//!
//! A three-node cluster is the minimum at which the Exit has **exactly two** return-relayer
//! candidates: the full mesh gives every node a channel to the other two, and the Exit is itself
//! one of the three, so its return path can only start at one of the remaining two. That number is
//! what makes the second scenario a 50 % loss experiment rather than an approximate one — killing
//! one of two relayers removes half the return traffic by construction.
//!
//! # Running
//!
//! ```bash
//! HOPRD_SRC=../hoprd LINE=v5 just pix-prod
//! ```
//!
//! Manual, like every PIX scenario here — never in CI, and needing a hoprd built with a deposit
//! pool. The relayers are killed by the second scenario, so each runs as its own invocation with
//! its own cluster.
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
    relayers,
};
use rand::RngExt as _;

/// Nodes in the cluster: an Exit and its **exactly two** return-relayer candidates.
///
/// Not a preference. The return path is planned from the Exit, and the full mesh gives it a
/// channel only to the other cluster nodes, so `N - 1` is the candidate count. Two is what makes
/// the relayer-kill scenario a 50 % loss experiment by construction rather than by measurement.
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
/// Measured at 4 000 packets/s the achieved cycle is ~116 s against a 114 s nominal, so three
/// cycles is ~6 min of traffic — which is what sets [`TRAFFIC_BUDGET`], and the ~3 min of cluster
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
/// Batching 50 segments per sleep puts the step at 12.5 ms at the default rate, three orders of
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

/// Why the Exit's PIX supervisor closed the Session, read out of its own log.
///
/// The causes this scenario can produce are opposite verdicts and look identical from the Entry's
/// counters alone, so the Exit's own log is what tells them apart:
///
/// * `CommitmentTimeout` in `AwaitingCommitment` — the Exit asked for the successor cycle's
///   commitment and never got it. Since hoprnet#8525 the Exit re-sends an unanswered request every
///   `ssa_request_resend_interval`, so reaching this deadline means the request was refused for the
///   whole window rather than merely lost once.
/// * `DepositObserverClosed` / a deposit timeout — the Entry's `SsaCommit` arrived but its deposit
///   did not land inside `max_deposit_wait`. That *is* the surplus failing.
/// * `RecoveryIdle` — the cycle in flight stopped making progress. At this geometry that means the
///   lost share of the emission took polynomials below their threshold, which is the split's own
///   break-even rather than a control-plane fault.
///
/// Naming these apart is the whole point: a control-plane failure and a split that cannot carry
/// the loss are different findings, and a test that reports them the same way is worse than none.
fn why_the_session_closed(env: &IntegrationEnv, exit: &NodeInfo) -> String {
    const NEEDLES: &[(&str, &str)] = &[
        (
            "reason=CommitmentTimeout",
            "the Exit's PIX supervisor closed the Session with CommitmentTimeout in \
             AwaitingCommitment: it asked for the successor cycle's commitment and never received \
             it. Since hoprnet#8525 the Exit re-sends an unanswered request every \
             `ssa_request_resend_interval` (3 s by default), so reaching this deadline means the \
             Entry refused it for the whole window — check the Entry's log for \
             `refused an under-served SSA request` and the `observed`/`required` figures it names",
        ),
        (
            "reason=DepositTimeout",
            "the Exit's PIX supervisor closed the Session on a deposit timeout: the Entry's \
             commitment arrived but its deposit did not land inside max_deposit_wait, which is the \
             surplus genuinely failing to carry the loss",
        ),
        (
            "gave up waiting for the deposit",
            "the Exit's deposit observer gave up on an SSA whose deposit never arrived inside \
             max_deposit_wait",
        ),
        (
            "reason=RecoveryIdle",
            "the Exit's PIX supervisor closed the Session with RecoveryIdle: the cycle in flight \
             stopped making progress and its recovery idle deadline expired. At this geometry that \
             means the lost share of the emission took polynomials below their threshold — the \
             surplus's own break-even — rather than any control-plane fault",
        ),
    ];
    // `node_log_contains` reads the node's own log, which is the only place this is recorded: the
    // closure reason is not exposed over the REST API and the `hopr_pix_*` family carries no
    // Session label, so a reason that arrived as a counter would be indistinguishable between
    // Sessions.
    for (needle, message) in NEEDLES {
        match env
            .cluster()
            .and_then(|cluster| cluster.node_log_contains(exit.address, needle))
        {
            Ok(true) => return format!("{message}."),
            Ok(false) => {}
            Err(error) => {
                return format!(
                    "(could not read the Exit's log to attribute the close: {error:#})"
                );
            }
        }
    }
    "The Exit's log names no PIX closure reason, so the Session was closed by something else."
        .to_string()
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

// ─────────────────────────────────────────────────────────────────────────────
// The resilience scenario
// ─────────────────────────────────────────────────────────────────────────────

/// How far into the first cycle the relayer is killed.
///
/// A quarter of a nominal cycle, which is inside the 25-50 % window the issue names and — more
/// importantly — early enough that the shares aimed at the victim are still unspent. A share is
/// bound to its SURB when the SURB is *minted*, so a relayer killed after the buffer has drained of
/// its SURBs costs the cycle nothing; killing at a quarter leaves most of the victim's share of the
/// emission still ahead of the kill.
///
/// Expressed in *time* rather than in packets, for the reason [`TRAFFIC_BUDGET`] gives: the share
/// rate is not the forward packet rate, so a packet count would put the kill at an unpredictable
/// point in the cycle.
fn kill_after() -> Duration {
    Duration::from_secs(prod_pix::nominal_cycle_secs() / 4)
}

/// How long traffic keeps flowing after the kill before the asserted cycles are counted.
///
/// The kill has to land while the cycle in flight is still emitting, and the survivors have to be
/// re-planned onto before the count begins — otherwise the outage is charged to a cycle that
/// recovered through it. Comfortably longer than the ~9 s detection `tests/return_path.rs`
/// measured.
const KILL_SETTLE: Duration = Duration::from_secs(20);

/// Quiet period the post-kill pump tolerates before calling the return stream idle.
///
/// Must exceed the Exit's worst egress-gate park after a kill — measured at ~40 s while it served
/// its way through the SURBs minted before the relayer died — or the reader stops during the
/// outage and the phase's arrival figure describes the stall rather than the recovery. The pump's
/// own default is 10 s, which is *inside* that window. Stated for the same reason
/// `tests/return_path.rs` states its `SURVIVAL_IDLE_BUDGET`.
const KILL_IDLE_BUDGET: Duration = Duration::from_secs(90);

/// Seconds of traffic the *resilience* scenario offers, which is longer than [`TRAFFIC_BUDGET`]
/// for a measured reason.
///
/// Losing half the return path does not halve the Exit's egress — measured, it keeps serving at
/// the full rate once its gate releases — but it does stretch the achieved cycle, because a share
/// reaches the Exit only on a reply that *arrives*, and half the replies now do not. Measured:
/// ~116 s per cycle on the lossless path against ~240 s here, so the 464 s that buys four cycles
/// there buys two.
///
/// Sized from that figure for [`TARGET_CYCLES`] plus a whole cycle of margin, so a run that lands
/// on the target is not one that only just scraped it. This is the scenario's dominant cost.
const KILL_TRAFFIC_BUDGET: Duration = Duration::from_secs(4 * 240);

/// How much of the return traffic the killed relayer may have been carrying, as a fraction.
///
/// Outside the band the run reports itself **inconclusive** rather than passing or failing.
///
/// **The upper bound is not a preference — it is the surplus's design point.** The deployed
/// `4096 x (32 + 79)` split tolerates a *sustained* 50 % share loss at 99 % of cycles, and its
/// break-even ratio is `surplus/(threshold + surplus)` = 79/111 = **71.2 %**, which is by
/// definition the loss at which a polynomial receives *exactly* its threshold on average —
/// `111 × (1 − 79/111) = 32.0` against a threshold of 32. Half of all polynomials land below that
/// average and — because the SSA is the sum of *every* polynomial's constant term — the cycle
/// cannot be recovered at all. The loss tolerance is a cliff, not a slope.
///
/// Measured: a run that killed the 72 % relayer recovered 3 584 of 4 096 polynomials and then
/// stopped, exactly as that arithmetic predicts, and failed with `reason=RecoveryIdle` at
/// `largest_useful_shares=116231` against a target of 131 072. That is the configuration working
/// as designed, not a defect — and asserting against it would make this scenario fail whenever the
/// weight draw happened to be skewed.
///
/// So the victim is the *quietest* relayer (`relayers::RelayerSpread::quietest`) and the band is where the
/// surplus actually has work to do: at least [`MIN_MEASURED_LOSS`] so the kill removes a real
/// share of the emission, and at most [`MAX_MEASURED_LOSS`] so it stays clear of break-even. With
/// two candidates the quiet one carries `1 - share` of the busier, so this is exactly the
/// "one of two relayers" scenario the issue asks for on any draw.
const MIN_MEASURED_LOSS: f64 = 0.25;

/// Upper bound — below the 71.2 % break-even ratio with margin for the draw's own variance, since
/// the loss is measured over a window rather than over the whole cycle.
const MAX_MEASURED_LOSS: f64 = 0.60;

/// The Exit still recovers every cycle when one of its two return relayers is killed mid-cycle.
///
/// # What this proves
///
/// `4096 × (32 + 79)` is sized so that a cycle still recovers at 99 % when half of all shares are
/// lost, which is what losing one of two return relayers costs. That tolerance was verified
/// manually, once, on a 4-node localcluster before hoprnet#8519's successor-gate change; nothing
/// automated checked it, and the new gate had never faced a real relayer failure. This scenario is
/// that check.
///
/// # Why the successor gate is the thing at risk
///
/// The Entry funds the Exit's next batch only once the returned packets it has seen reach a lower
/// confidence bound on an honest Exit's arrivals at the tolerated loss — 4σ below the mean, which
/// at this split is 41.0 % of a cycle. An Exit whose first relayer acknowledged a packet and then
/// dropped it is indistinguishable from an honestly lossy path, so at exactly 50 % observed loss
/// the gate clears by 1.24 % and is refused from about 50.6 %.
///
/// A refusal used to be fatal: the Exit sent its successor request once, so a refusal left it in
/// `AwaitingCommitment` until `max_ssa_delivery_time` and closed the Session (hoprnet#8523). Since
/// hoprnet#8525 the Exit re-sends the request every `ssa_request_resend_interval` while its
/// predecessor is served, so a refusal costs an interval rather than the Session — and the
/// re-send count is what shows the gate was exercised at all (`ssa_requests_resent`, logged below).
///
/// # What a failure looks like
///
/// The diagnostics the issue asks for are all logged: the measured loss (the victim's share of
/// return packets before the kill), the affected cycles' useful- and accepted-share fractions, and
/// every `hopr_pix_gate_blocks_total{reason}`.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn the_exit_should_recover_every_cycle_when_half_the_relayers_die() -> anyhow::Result<()> {
    prod_pix::install_profile();
    cluster::request_cluster_size(NODES);
    let t0 = std::time::Instant::now();

    let per_cycle = prod_pix::per_cycle()?;
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
    let candidates = env.relayer_candidates(exit_addr)?;
    // The count is the experiment. Two candidates means killing one removes exactly half the
    // return traffic, which is what the surplus is sized against; a third would make this an
    // approximate loss and the assertion below could not tell a real failure from an arithmetic
    // one.
    anyhow::ensure!(
        candidates.len() == 2,
        "this scenario needs exactly 2 return-relayer candidates so the kill is a 50 % loss by \
         construction, got {}. Raise NODES by one to add a candidate.",
        candidates.len()
    );
    tracing::info!(
        exit = %exit_addr,
        candidates = candidates.len(),
        elapsed = ?t0.elapsed(),
        "PIX session open; the Exit has exactly two return-relayer candidates"
    );

    let exit_before = pix::node_balances(&exit).await?;
    let exit_counters_before = pix::sample_exit(&exit).await?;
    let entry_counters_before = pix::sample_entry();
    let gate_before = pix_exit::sample(&exit).await?;
    let sampler = pix_exit::Sampler::start(&exit);

    let (mut rx, mut tx) = tokio::io::split(session);

    // ── Phase 1: drive into the first cycle, then kill a relayer mid-emission ──
    //
    // One payload for the whole scenario, offered in two pumps over the *same* session: splitting
    // the payload is what puts the kill at a chosen point in the cycle, while splitting the
    // session would measure cold-start path selection instead — a replacement session never minted
    // a SURB through the dead node, which is a much easier question.
    let payload = payload_for(KILL_TRAFFIC_BUDGET + KILL_SETTLE);
    let warmup_len = (datagrams_for(kill_after()) as usize * CHUNK).min(payload.len());
    let warmup = &payload[..warmup_len];

    // Who is carrying the return traffic right now, so the victim is chosen by measurement rather
    // than by position.
    let spread_before = {
        let before = relayers::sample(&candidates).await;
        let transfer = pump::pump_halves(
            &mut rx,
            &mut tx,
            warmup,
            "pix-prod-warmup",
            pump_budget(kill_after()),
            pix_pump_opts(),
        )
        .await?;
        anyhow::ensure!(
            transfer.arrival_pct() > 0.0,
            "the baseline pump returned nothing, so the path was already broken before the kill \
             and the result would say nothing about resilience (outcome {:?})",
            transfer.outcome
        );
        let after = relayers::sample(&candidates).await;
        relayers::spread(&before, &after)
    };
    tracing::info!(
        spread = %spread_before.summary(),
        total = spread_before.total,
        "return-relayer split before the kill"
    );

    // The *quietest* relayer, not the busiest — see [`MAX_MEASURED_LOSS`]. Killing the busiest
    // removes the largest share available, which on a skewed draw lands past the split's
    // break-even ratio and produces a failure that is the configuration working correctly.
    let victim_addr = spread_before
        .quietest()
        .ok_or_else(|| anyhow::anyhow!("no relayer forwarded anything; nothing to kill"))?;
    let victim = candidates
        .iter()
        .find(|n| n.address == victim_addr)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("quietest relayer {victim_addr} is not a cluster node"))?;

    // The measured loss, read *before* the kill from the baseline histogram. This is the figure
    // the issue asks be reported, and the one that decides whether the run is conclusive at all.
    let measured_loss = spread_before
        .share_of_relayer(victim_addr)
        .ok_or_else(|| anyhow::anyhow!("the chosen victim carried nothing"))?;
    tracing::info!(
        victim = %victim_addr,
        measured_loss_pct = measured_loss * 100.0,
        break_even_ratio_pct = 100.0 * prod_pix::PIX_ADDITIONAL_SHARES as f64
            / (prod_pix::PIX_SHARES + prod_pix::PIX_ADDITIONAL_SHARES) as f64,
        kill_after = ?kill_after(),
        "killing the quietest return relayer a quarter of the way into the first cycle"
    );

    // Conclusiveness is decided *here*, before the kill, and not after the budget has been spent.
    // An out-of-band kill is reported as unmeasured rather than as a pass or a failure — the issue
    // is explicit that the test only means something if the kill removes about half the traffic —
    // and there is a concrete case in which the run is knowably inconclusive from this figure
    // alone: `relayers::spread` drops zero-delta relayers, so if only one candidate forwarded
    // during warm-up `quietest()` returns it with a share of 1.0, above the upper bound. Checking
    // after the kill would burn the full budget and a `SETTLE_TIMEOUT` before saying so.
    anyhow::ensure!(
        (MIN_MEASURED_LOSS..=MAX_MEASURED_LOSS).contains(&measured_loss),
        "INCONCLUSIVE: the killed relayer was carrying {:.1}% of the return traffic, outside the \
         {:.0}-{:.0}% band this scenario needs. The upper bound is kept below the split's break-even \
         ratio — above that ratio the cycle is unrecoverable by design and a failure would say \
         nothing about the surplus. Spread: {}.",
        measured_loss * 100.0,
        MIN_MEASURED_LOSS * 100.0,
        MAX_MEASURED_LOSS * 100.0,
        spread_before.summary(),
    );
    victim.kill()?;

    // ── Phase 2: keep traffic flowing across the kill and for the asserted cycles ──
    //
    // What the Exit has to recover from. The kill has already landed, so the cycle in flight is
    // short of the shares the victim was carrying — which is exactly the loss the surplus exists to
    // absorb.
    let rest = &payload[warmup_len..];
    let transfer = pump::pump_halves(
        &mut rx,
        &mut tx,
        rest,
        "pix-prod-after-kill",
        pump_budget(KILL_TRAFFIC_BUDGET + KILL_SETTLE),
        PumpOpts {
            idle_budget: Some(KILL_IDLE_BUDGET),
            ..pix_pump_opts()
        },
    )
    .await?;

    // ── Phase 3: did the cycles that straddle the kill recover? ──
    let exit_counters =
        await_sweeps(&exit, &exit_counters_before, TARGET_CYCLES, SETTLE_TIMEOUT).await?;
    let entry_counters = entry_counters_before.delta(&pix::sample_entry());
    let trace = sampler.finish().await;
    let gate = gate_before.delta(&pix_exit::sample(&exit).await?);
    let recovered = pix::node_balances(&exit).await?.safe - exit_before.safe;

    tracing::info!(
        %recovered, measured_loss_pct = measured_loss * 100.0, elapsed = ?t0.elapsed(),
        "entry counters: {} | exit counters: {}",
        entry_counters.summary(),
        exit_counters.summary()
    );

    // The Session must not have closed. A gate that misjudged the loss shows up here rather than
    // as a wrong counter — and the Exit's own log names which failure it was.
    assert!(
        !transfer.outcome.exit_stopped_serving(),
        "the Session stopped being served after the kill (outcome {:?}). {} Entry {}; exit {}",
        transfer.outcome,
        why_the_session_closed(&env, &exit),
        entry_counters.summary(),
        exit_counters.summary(),
    );

    assert_the_deployed_geometry_recovered(
        &transfer,
        &exit_counters,
        &entry_counters,
        TARGET_CYCLES,
        "relayer kill",
    );

    let cycles = pix::completed_cycles(recovered, per_cycle).unwrap_or_else(|| {
        panic!(
            "the Exit's Safe gained {recovered}, which is not a whole multiple of the {per_cycle} \
             per-SSA deposit — something other than PIX sweeps moved the balance (exit: {})",
            exit_counters.summary()
        )
    });

    // Diagnostics the issue asks for, logged for any run that gets past the pre-kill
    // conclusiveness check — any conclusive run, pass or fail.
    tracing::info!(
        measured_loss_pct = measured_loss * 100.0,
        useful_fraction_recovered = ?gate.useful_fraction("recovered").map(|h| h.mean()),
        accepted_fraction_recovered = ?gate.accepted_fraction("recovered").map(|h| h.mean()),
        gate_blocks_share_lag = ?gate.gate_blocks("share_lag"),
        gate_blocks_predeposit = ?gate.gate_blocks("predeposit_exhausted"),
        // The direct evidence that the successor path was exercised rather than merely survived: a
        // re-send means the Entry refused or lost the first request, which before hoprnet#8525
        // ended the Session. `None` means the Exit predates the counter.
        ssa_requests_resent = ?gate.ssa_requests_resent(),
        cycles_committed = ?gate.cycles("committed"),
        cycles_failed = ?gate.cycles("failed"),
        "PIX diagnostics after the kill"
    );

    // The conclusiveness band was checked before the kill (see the `ensure!` above the `kill()`),
    // so the counters are logged here purely as diagnostics before the verdict — deliberately
    // before the assertion, so the run's own figures are readable whether it passes or fails.

    assert!(
        cycles >= TARGET_CYCLES,
        "the Exit swept {cycles} cycles across the kill, expected at least {TARGET_CYCLES} \
         ({recovered} recovered against a {per_cycle} per-cycle deposit). The killed relayer \
         carried {:.1}% of the return traffic, so a cycle in flight when it died was short of \
         about that fraction of its shares — which the deployed 79-share surplus exists to cover. \
         A shortfall here means the surplus did not absorb the loss, or the successor gate refused \
         the next batch. Entry {}; exit {}; gate {}",
        measured_loss * 100.0,
        entry_counters.summary(),
        exit_counters.summary(),
        gate.summary(),
    );

    assert_the_gate_served_the_surplus(&gate, &trace, "relayer kill");
    assert_exit_was_paid(
        &exit,
        &exit_before,
        exit_counters.sweeps().unwrap_or(0),
        TARGET_CYCLES,
    )
    .await?;

    close_session(&mut tx).await;
    tracing::info!(
        cycles,
        measured_loss_pct = measured_loss * 100.0,
        "50 %-relayer-loss PIX test PASSED in {:?}",
        t0.elapsed()
    );
    Ok(())
}
