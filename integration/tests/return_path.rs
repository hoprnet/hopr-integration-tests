//! Return-path resilience: does a reply stream survive losing one of its relays?
//!
//! # The incident these reproduce
//!
//! On 2026-08-11 a `gnosis_vpn-client` session broke. Post-mortem: ~99.6% of the exit's
//! return SURBs named just three first relayers, so when one went away the return path
//! lost 33–44% of its packets at once — past the ~20% a reliable session tolerates — and
//! the stream corrupted rather than degrading.
//!
//! Two independent weaknesses produced that:
//!
//! 1. **Concentration.** Each return path was drawn with an independent weighted pick
//!    from one cached candidate list, so the highest-scored relayers won nearly every
//!    draw. The fix tempers the weights (`w' = w^0.5`) before sampling, which compresses
//!    the ratio between candidates without reordering them, so a good relayer still wins
//!    more often but a single one no longer owns the return stream (hoprnet#8331).
//!
//!    An earlier attempt bucketed candidates and rotated between K of them. That could
//!    never work: `MAX_SURBS_IN_PACKET` is `PAYLOAD_SIZE / HoprSurb::SIZE` = 2, so K was
//!    pinned at 2 whatever the config said, and the rotation measured 2.97 imbalance --
//!    between the two pre-fix runs rather than below them.
//! 2. **Staleness.** The exit drained its SURB buffer oldest-first, so a return path
//!    that had already changed kept being used until a multi-megabyte backlog was
//!    consumed (hoprnet#8328), and SURBs whose first hop was no longer payable were
//!    used anyway (hoprnet#8329).
//!
//! # How they are measured
//!
//! Both tests open a session whose **forward** path is 0-hop and whose **return** path is
//! 1-hop. The asymmetry is the whole trick: no cluster node relays anything on the way
//! out, so `hopr_packets_count{type="forwarded"}` on each node counts exactly the replies
//! it carried. The per-node deltas are a return-relayer histogram (see
//! [`hoprd_integration_test::relayers`]).
//!
//! - [`return_paths_should_spread_across_distinct_relayers`] measures the mechanism: the
//!   histogram must be spread, not a spike.
//! - [`session_should_survive_return_relayer_loss`] reproduces the incident: kill the
//!   busiest return relayer mid-session and require the stream to keep flowing.
//!
//! Both are `#[ignore]` (they need the external binaries and a chain) and both want more
//! relayer candidates than the throughput tests, so they run their own 5-node cluster.
//!
//! # Running them
//!
//! ```bash
//! HOPRD_KEEP_ARTIFACTS=1 HOPRD_BIN=<hoprd> HOPRD_LOCALCLUSTER_BIN=<localcluster> \
//! TEST_TARGET=return_path TEST_ARGS=--nocapture SCENARIOS=<test name> \
//!   bash scripts/integration/run-binchain.sh > /tmp/run.log 2>&1
//! ```
//!
//! One at a time -- the cluster binds fixed ports. `HOPRD_KEEP_ARTIFACTS=1` always, or the node
//! logs are deleted at teardown and a failed run yields nothing. **Redirect with `>`, never pipe
//! into `tail`**: a pipe buffers the whole run, and the pipeline's exit status becomes `tail`'s, so
//! a failed test reports success. One run was read as passing for exactly that reason.
//!
//! # Which build each participant runs
//!
//! | participant | role | comes from |
//! | ----------- | ---- | ---------- |
//! | entry (`edgli`, in-process) | opens the session, mints SURBs, selects return paths | a Cargo git dependency in `integration/Cargo.toml`, compiled into this binary |
//! | relayers + exit | forward packets, reply | `$HOPRD_BIN` / `$HOPRD_LOCALCLUSTER_BIN` at runtime |
//! | chain | anvil + blokli | nix `result-*` |
//!
//! The entry is a **compile-time** dependency and the nodes are **runtime** binaries, so they can
//! silently be different builds. Before trusting a result, name the participant that owns the
//! behaviour under test and confirm *that* one carries the change -- a `strings` grep for a marker
//! unique to it is enough. Four separate runs were once invalidated this way, and the failure is
//! indistinguishable from "the fix does not work".
//!
//! For the same reason, a behavioural setting has to be stated on **both** sides: the entry
//! configures itself in [`hoprd_integration_test::env`], the nodes through their own config or
//! [`hoprd_integration_test::cluster::request_node_env`]. `SurbPopOrder` once differed between the
//! two ends for a long while, and every reading of return-path behaviour was against a mismatched
//! pair.
//!
//! # Reading a result
//!
//! - Read `outcome` first. `NeverStarted` / `SessionClosed` mean nothing was serving the session,
//!   and every other number in that run is about a stream with no counterparty.
//! - Read `foreign` next. Non-zero means traffic from an earlier phase surfaced in this one; it is
//!   excluded from the figures, but a large value means the drain failed and the phases are mixed.
//! - Check the baseline before anything else. A collapsed pre-kill pump makes every post-kill
//!   number meaningless -- and is itself the finding, since it means the change under test broke a
//!   healthy session.
//! - Healthy-run throughput varies 1-45 %. A single run cannot establish an improvement; compare
//!   against the spread, not against one prior number.
//! - Log timestamps are UTC, file mtimes are local. Compare like with like before concluding a
//!   binary predates a run.
//! - Strip ANSI before grepping structured fields: `tracing` wraps *field names* in escape codes,
//!   so `grep -c 'sessions=1'` returns zero on a line that plainly contains it. Pipe through
//!   `sed -E 's/\x1b\[[0-9;]*m//g'` first.
//! - All lines on the test's own stdout come from the in-process entry; node logs are separate
//!   files. A `hopr_transport` target in stdout is the entry, not a relayer.

use std::time::Duration;

use hoprd_integration_test::{
    HoprSession, IntegrationEnv,
    cluster::{NodeInfo, request_cluster_size, request_latency_profile},
    pump::{
        PumpOpts, PumpOutcome, Transfer, drain_until_quiet, pace_for_rate, pump_halves,
        tagged_payload,
    },
    relayers::{self, RelayerSpread},
    session_metrics,
};

/// `hoprd-localcluster` caps out here, and every extra node is one more possible return
/// relayer: with 5 nodes the exit has 4, enough for concentration and spread to look
/// nothing alike.
const NODES: usize = 5;

/// Distinct inbound delays per node, so the relayers do **not** all look alike.
///
/// This is load-bearing, not flavour. On an unshaped local cluster every relayer probes at
/// essentially the same latency, every path weight is equal, and a weighted-random draw is
/// indistinguishable from any other strategy over enough samples — measured: the pre-fix
/// stack produced the same 25/25/25/25 histogram as the fixed one. Tempering is monotone,
/// so with equal weights it is the identity. Concentration only appears
/// when edge scores differ, which is also the condition that produced it in the incident.
///
/// The values are chosen against RFC-0014's **step-function** latency score (summary
/// §6.3), not spread evenly in milliseconds — anything inside the same step scores
/// identically, so evenly-spaced delays waste candidates on duplicate scores:
///
/// | delay   | score |
/// | ------- | ----- |
/// | ≤75 ms  | 1.00  |
/// | ≤125 ms | 0.70  |
/// | ≤200 ms | 0.30  |
/// | >200 ms | 0.15  |
///
/// One node per step gives a 6.7× spread between best and worst, which is what tempering
/// has to compress: `6.7^0.5` is 2.58 before the two-per-packet draw flattens it further.
/// Without a spread there is nothing to compress and nothing to measure. Node 0 is the
/// fastest so that whichever node becomes the exit, the remaining relayers still span
/// several steps.
const LATENCY_PROFILE: &str = r#"
per_node:
  0: "10ms"
  1: "40ms"
  2: "110ms"
  3: "170ms"
  4: "260ms"
"#;

/// Which direction of the session is put on a relay, and therefore which one the kill removes.
///
/// The scenarios differ only in this. Everything else -- the payload, the pacing, the settle, the
/// thresholds, the assertions -- is shared, so a difference in outcome between them is a
/// difference in the protocol rather than in how they were measured.
///
/// # Why one hop and zero
///
/// The relayer histogram counts `hopr_packets_count{type="forwarded"}` per node, which does not
/// say which direction a packet was travelling. Pinning the *other* direction to 0 hops removes
/// the ambiguity: no cluster node relays anything that way, so every forwarded packet belongs to
/// the direction under test and the busiest node is unambiguously its relay.
#[derive(Clone, Copy, Debug)]
struct Topology {
    /// Names the direction in log lines and failure messages.
    direction: &'static str,
    forward_hops: usize,
    return_hops: usize,
}

/// Replies travel over one relay; the request path is direct.
///
/// The shape of the original incident, and the one the entry reassembles.
const RETURN_PATH: Topology = Topology {
    direction: "return",
    forward_hops: 0,
    return_hops: 1,
};

/// The mirror image: requests travel over one relay, replies come back directly.
///
/// Reassembly happens at the *exit* here, not the entry, so this covers the half of the fix that
/// lives in the hoprd binary rather than in the client. A session that survives return-path loss
/// says nothing about this one -- different process, different build, and in a real deployment
/// they are upgraded separately.
const FORWARD_PATH: Topology = Topology {
    direction: "forward",
    forward_hops: 1,
    return_hops: 0,
};

/// One relay in each direction: what a real deployment runs.
///
/// Both directions traverse a relay, so a forwarded packet cannot be attributed to one of them and
/// the histogram identifies only "the busiest relay". Weaker as a diagnostic, stronger as an
/// end-to-end check.
const SYMMETRIC: Topology = Topology {
    direction: "symmetric",
    forward_hops: 1,
    return_hops: 1,
};

/// Warm-up payload: enough packets to make the relayer histogram unambiguous, no more.
///
/// Its only jobs are to establish the baseline rate and to show who is carrying replies. Keeping it
/// small also keeps the session quiet sooner, so the kill lands on an idle session rather than on
/// one still draining.
const WARMUP_BYTES: usize = 2 * 1024 * 1024;

/// How long the survival phase must keep *offering* data after the kill.
///
/// This is the defect that made an earlier version of this test unmeasurable. The session is
/// unreliable -- there is no retransmission -- so bytes lost during the outage never arrive, and a
/// payload handed to the session as fast as it will take it is fully committed within seconds. The
/// entire result was therefore decided inside the pre-detection window, and everything after it was
/// a buffer draining. Recovery can only be observed if load is still being offered when it happens,
/// so the survival phase is paced to span several times the deadline.
const SURVIVAL_LOAD_DURATION: Duration = Duration::from_secs(60);

/// Fraction of the measured baseline rate at which the survival phase offers data.
///
/// Below 1.0 on purpose. `before_kill.mbps` is a *burst* rate: the warm-up drains a SURB buffer
/// that was filled before it started, so it measures what the session can do with SURBs already
/// in hand, not what it sustains. Offering that same rate back after a quarter of the return
/// capacity has gone simply backpressures the writer -- measured, a 60 s offer took 266 s, and
/// the "recovery" at ~270 s was the backlog draining, not the path returning. The pump's own
/// pacing cannot detect this, because backpressure and not [`pace_for_rate`] sets the real rate.
///
/// Must also stay **above** [`RECOVERY_FRACTION`], or the test demands exactly what it offers and
/// any loss at all puts the target out of reach.
const SURVIVAL_LOAD_FRACTION: f64 = 0.7;

const _: () = assert!(
    SURVIVAL_LOAD_FRACTION > RECOVERY_FRACTION,
    "the survival phase must offer more than the recovery target demands"
);
const _: () = assert!(
    SURVIVAL_LOAD_FRACTION < 1.0,
    "offering the full burst baseline backpressures the writer and measures the harness"
);

/// Share of the survival payload that must come back for the session to count as having survived.
///
/// Derived from [`RECOVERY_DEADLINE`] rather than chosen. Over a paced phase the session delivers
/// nothing while the return path is down and roughly the offered rate once it is back, so
/// aggregate arrival *is* the outage expressed as a fraction of the phase:
///
/// ```text
/// arrival ≈ 1 − outage / SURVIVAL_LOAD_DURATION
/// ```
///
/// Requiring the outage to stay inside the deadline is therefore the same statement as requiring
/// this arrival, and the two can no longer drift apart. The previous flat 90 % was only reachable
/// because writer backpressure stretched the phase far past its nominal duration.
///
/// Only the part of the outage that overlaps the phase costs arrival. Nothing is offered during
/// [`KILL_SETTLE`], so a session that recovers exactly on the deadline is only ever seen to be down
/// for `RECOVERY_DEADLINE - KILL_SETTLE`; charging it for the settle as well would demand less than
/// the deadline does and let a late recovery pass.
const MIN_SURVIVAL_ARRIVAL_PCT: f64 = 100.0
    * (1.0
        - ((RECOVERY_DEADLINE.as_secs() - KILL_SETTLE.as_secs()) as f64
            / SURVIVAL_LOAD_DURATION.as_secs() as f64));

/// Quiet period the survival pump tolerates before calling the stream idle.
///
/// Must exceed [`RECOVERY_DEADLINE`]: the pump exists to observe how long the path takes to come
/// back, so a budget shorter than the deadline ends the measurement before the thing being
/// measured can happen. The pump's own default is shorter, which is why this is stated.
const SURVIVAL_IDLE_BUDGET: Duration = Duration::from_secs(30);

const _: () = assert!(
    SURVIVAL_IDLE_BUDGET.as_secs() > RECOVERY_DEADLINE.as_secs(),
    "the idle budget must outlive the recovery deadline it is measuring"
);

/// Hard cap on how long the survival phase keeps reading after the last byte has been offered.
///
/// The idle budget above cannot bound a trickle. A session returning a few bytes a second is never
/// quiet long enough to look idle and never fast enough to finish, so the phase runs until the
/// overall deadline: one run took 444 s to offer 60 s of data, and the extra seven minutes measured
/// a backlog draining at the end, not a return path recovering. Some of that traffic is not even
/// this phase's -- loopback and earlier-phase records keep the raw stream busy regardless.
///
/// Cutting at a fixed point after the offer changes the question from "how long until everything
/// eventually shows up" to "how much came back in a bounded window", which is the one a survival
/// scenario is asking. Whatever has not returned by then is loss.
///
/// Comfortably longer than [`RECOVERY_DEADLINE`], so a session that recovers within the deadline
/// still has ample room to drain everything it was offered -- the cap bounds a *failing* stream, and
/// must never truncate a healthy one.
const SURVIVAL_TAIL_GRACE: Duration = Duration::from_secs(45);

const _: () = assert!(
    SURVIVAL_TAIL_GRACE.as_secs() > RECOVERY_DEADLINE.as_secs(),
    "the tail cap must outlive the recovery deadline, or it truncates a session that recovered"
);

/// How long to wait after the kill before offering any new data.
///
/// Not zero, deliberately. At zero the survival phase's opening seconds race packets that were
/// already in flight when the relayer died, so an early arrival cannot be attributed to a recovered
/// session rather than to one that had not yet noticed. A few seconds of quiet means every byte
/// measured afterwards was offered to a network that has already lost the relayer.
///
/// This window counts against [`RECOVERY_DEADLINE`]: recovery is timed from the kill, not from the
/// first byte offered. Nothing is in flight during the settle, so the mechanism cannot demonstrate
/// itself here -- but the session is broken throughout it, and the clock a user experiences starts
/// when the relay dies, not when the test resumes sending.
///
/// Ten seconds rather than four: detection has been measured at ~9 s from the kill, so a shorter
/// settle offers the opening seconds of the survival phase into a path whose re-plan has not landed
/// yet. Those bytes are lost to an outage the session is still in, and they are charged to arrival
/// as though the recovered path had dropped them.
const KILL_SETTLE: Duration = Duration::from_secs(10);

const _: () = assert!(
    KILL_SETTLE.as_secs() < RECOVERY_DEADLINE.as_secs(),
    "the settle must leave some of the deadline for the mechanism to demonstrate itself in"
);

/// [`KILL_SETTLE`], overridable for a one-off experiment via `HOPRD_KILL_SETTLE_SECS`.
fn kill_settle() -> Duration {
    std::env::var("HOPRD_KILL_SETTLE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(KILL_SETTLE, Duration::from_secs)
}

/// How long the return stream must be silent before the warm-up counts as finished.
const DRAIN_QUIET: Duration = Duration::from_secs(3);

/// Leftover warm-up bytes above which the phases cannot be told apart.
///
/// Anything still arriving from the warm-up would be counted as survival-phase arrival, which is
/// exactly the contamination that makes a dead stream look like a recovering one.
const MAX_LEFTOVER_BYTES: usize = 64 * 1024;

const PUMP_TIMEOUT: Duration = Duration::from_secs(600);

/// Ignore relayers below this share of the stream: a stray packet from a probe or a
/// single re-plan does not make a node a genuine participant in the return stream.
const ACTIVE_RELAYER_FLOOR: f64 = 0.05;

/// Maximum ratio between the busiest and least-busy relayer.
///
/// Selection is weighted-random with the weights tempered (`w' = w^0.5`). Tempering is
/// monotone, so it never reorders candidates — it only compresses the ratio between them.
/// The target is therefore *not* 1.0: a good relayer is still supposed to carry more than a
/// bad one, just not by the raw score ratio.
///
/// Derived for this profile rather than from what makes the test green. RFC-0014 scores the
/// per-node latencies below as 1.0 / 0.7 / 0.3 / 0.15, which as raw weights would give
/// shares of 46/33/14/7 — a ratio of 6.67. Tempered they become 36/30/20/14, a ratio of
/// 2.58. Drawing two distinct relayers per packet flattens the marginal distribution
/// further; calibrating that from the pre-fix run (ideal 6.67 measured 4.63, so ×0.69)
/// predicts **≈1.79**. The threshold allows jitter above that while staying under the
/// lowest pre-fix measurement.
///
/// **The margin is thin and the value is predicted, not yet measured.** Pre-fix runs
/// measured 2.28 and 4.63 (`docs/return-path-scenarios.md`), so the separation from the old
/// behaviour is only ~8% at the low end. With four candidates and `wanted = 2` this ratio
/// is a weak discriminator, and a run that lands near 2.2 would be ambiguous rather than
/// conclusive. Re-derive from a measured tempered run before trusting a pass.
///
/// Deliberately **not** a cap on the maximum *share*: with only four candidates the shares
/// overlap between designs, so a share cap cannot separate them and flips sign between runs.
const MAX_RELAYER_IMBALANCE: f64 = 2.1;

/// Arrival floor once one of the return relayers is dead.
///
/// Immediately after the kill the share that was aimed at it is lost — ~1/K, so ~25% with
/// 4 relayers — and stays lost until probing drops the dead node from the candidate set.
/// Anything above this floor means the session degraded proportionally instead of
/// collapsing; concentration puts this number near zero.
/// Fraction of pre-kill throughput the stream must get back to.
///
/// Losing a relayer costs real capacity, so the bar is not "as before" -- it is that the stream
/// settles at a usable rate instead of trickling or dying.
const RECOVERY_FRACTION: f64 = 0.5;

/// How long after the kill that rate must be reached, as the *test* boundary.
///
/// Measured from the kill itself, which includes [`KILL_SETTLE`] -- the session is already broken
/// during that window even though the test is offering nothing, and whoever is waiting for the
/// connection is not paused along with it.
///
/// The design target is [`RECOVERY_AIM`] -- 15s. This bar sits above it deliberately: the recovery
/// path is a sequence of independent stages (detection, graph trend, weight recompute, refill) and
/// a run that lands at 17s is a mechanism that works with a stage to tighten, not a regression to
/// bisect. Failing at 15s would spend runs on that distinction, and a cluster run is ~12 minutes.
///
/// Recovery time is logged against both, so drift toward the boundary stays visible instead of
/// only surfacing when it crosses.
const RECOVERY_DEADLINE: Duration = Duration::from_secs(20);

/// What the mechanism is designed to hit. Not asserted -- reported, so a run that passes the
/// boundary while missing the aim is still legible as such.
const RECOVERY_AIM: Duration = Duration::from_secs(15);

/// How long recovered throughput must hold before it counts as recovered.
///
/// Long enough that a single lucky burst does not read as recovery, short enough to resolve a
/// target measured in seconds.
/// How long the recovered rate has to hold before it counts as recovered rather than a burst.
///
/// Two seconds was too short: a transfer that ended after 1.6s satisfied it from its opening burst
/// alone and reported a recovery that never happened.
const RECOVERY_SUSTAIN_WINDOW: Duration = Duration::from_secs(3);

/// Phase tag for the warm-up payload.
const WARMUP_PHASE: u8 = 1;
/// Phase tag for the post-kill survival payload.
const SURVIVAL_PHASE: u8 = 2;

/// Bring up a cluster sized for return-path work and open a 0-hop-out / 1-hop-back
/// session, returning the env, the session, and the nodes that can relay replies.
async fn setup_return_path_env() -> anyhow::Result<(IntegrationEnv, HoprSession, Vec<NodeInfo>)> {
    setup_env_with_hops(0, 1).await
}

/// As above, with the forward and return hop counts named explicitly.
///
/// `(0, 1)` keeps the relayer histogram attributable — no cluster node relays anything outbound, so
/// every forwarded packet is a reply. `(1, 1)` is the realistic shape but gives up that attribution,
/// since the victim can sit on either direction.
async fn setup_env_with_hops(
    forward_hops: usize,
    return_hops: usize,
) -> anyhow::Result<(IntegrationEnv, HoprSession, Vec<NodeInfo>)> {
    let size = request_cluster_size(NODES);
    request_latency_profile(LATENCY_PROFILE);
    anyhow::ensure!(
        size >= 3,
        "return-path scenarios need ≥3 nodes to have >1 relayer candidate, got {size}"
    );

    let env = IntegrationEnv::setup().await?;
    let (session, exit) = env
        .open_unreliable_session_paths(forward_hops, return_hops)
        .await?;
    let candidates = env.relayer_candidates(exit)?;
    anyhow::ensure!(
        candidates.len() >= 2,
        "need ≥2 return-relayer candidates to tell spread from concentration, got {}",
        candidates.len()
    );
    tracing::info!(
        %exit,
        candidates = candidates.len(),
        forward_hops,
        return_hops,
        "return-path env ready"
    );
    Ok((env, session, candidates))
}

/// Pump `payload` while sampling the forwarded-packet counters either side of it.
///
/// Takes the session halves by reference so a scenario can pump the *same* session more than once.
async fn pump_and_measure(
    rx: &mut tokio::io::ReadHalf<HoprSession>,
    tx: &mut tokio::io::WriteHalf<HoprSession>,
    candidates: &[NodeInfo],
    payload: &[u8],
    label: &str,
    opts: PumpOpts,
) -> anyhow::Result<(Transfer, RelayerSpread)> {
    let before = relayers::sample(candidates).await;
    let transfer = pump_halves(rx, tx, payload, label, PUMP_TIMEOUT, opts).await?;
    let after = relayers::sample(candidates).await;
    let spread = relayers::spread(&before, &after);
    tracing::info!(
        "{label}: return relayers → {} (total {} pkts, max share {:.0}%)",
        spread.summary(),
        spread.total,
        spread.max_share() * 100.0,
    );
    Ok((transfer, spread))
}

/// The mechanism: replies must rotate over distinct first relayers, not follow edge score.
///
/// **Currently fails on both stacks under skewed scores** (fixed 2.97, pre-fix 2.28/4.63)
/// and passes on both unshaped (≈1.03) — i.e. it does not yet observe the behaviour
/// hoprnet#8331 intends. That is a finding about the fix, not a threshold to relax: the
/// suspected cause is that diversity is bounded per *packet* rather than across the SURB
/// stream, so bucket selection re-randomises weight-proportionally on every call. See
/// `docs/return-path-scenarios.md` for the measurements and the call-site analysis.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn return_paths_should_spread_across_distinct_relayers() -> anyhow::Result<()> {
    let (_env, session, candidates) = setup_return_path_env().await?;
    let payload = tagged_payload(WARMUP_PHASE, WARMUP_BYTES);

    let (mut rx, mut tx) = tokio::io::split(session);
    let (transfer, spread) = pump_and_measure(
        &mut rx,
        &mut tx,
        &candidates,
        &payload,
        "spread",
        PumpOpts {
            phase: Some(WARMUP_PHASE),
            ..PumpOpts::default()
        },
    )
    .await?;

    anyhow::ensure!(
        spread.total > 0,
        "no packets were forwarded by any cluster node — the return path was not 1-hop, \
         so this measures nothing (transfer: {:.1}% arrived)",
        transfer.arrival_pct(),
    );

    // Every candidate should appear; require all but one so a single unlucky bucket draw
    // is not a failure, while still ruling out concentration.
    let expected_distinct = (candidates.len() - 1).max(2);
    let active = spread.active_relayers(ACTIVE_RELAYER_FLOOR);
    anyhow::ensure!(
        active.len() >= expected_distinct,
        "return paths concentrated: only {} of {} candidate relayers carried ≥{:.0}% \
         of the replies, expected ≥{expected_distinct} — {}",
        active.len(),
        candidates.len(),
        ACTIVE_RELAYER_FLOOR * 100.0,
        spread.summary(),
    );
    let imbalance = spread.imbalance(ACTIVE_RELAYER_FLOOR);
    anyhow::ensure!(
        imbalance <= MAX_RELAYER_IMBALANCE,
        "return paths track the raw edge score instead of the tempered one: busiest relayer \
         carried {imbalance:.2}× the least-busy one (max {MAX_RELAYER_IMBALANCE:.2}) across \
         {} candidates. Tempering compresses the score ratio without reordering, so a value \
         up near the untempered ratio means the compression is not being applied and a \
         single relay owns more of the return traffic than intended — {}",
        candidates.len(),
        spread.summary(),
    );
    Ok(())
}

/// The incident, in whichever direction [`Topology`] puts on a relay: with the busiest relay for
/// that direction killed mid-session, the stream must degrade proportionally rather than stop.
///
/// Fails on the pre-fix stack — the killed relayer was carrying essentially the whole stream for
/// its direction, so the second pump returns almost nothing.
///
/// Shared by both scenarios deliberately. The two differ only in the topology, so anything that
/// separates them is the protocol behaving differently, not the measurement.
async fn session_should_survive_relayer_loss(topology: Topology) -> anyhow::Result<()> {
    tracing::info!(
        direction = topology.direction,
        forward_hops = topology.forward_hops,
        return_hops = topology.return_hops,
        "relayer-loss scenario"
    );
    // Held for the cluster lifetime; the scenario itself never opens a second session.
    let (_env, session, candidates) =
        setup_env_with_hops(topology.forward_hops, topology.return_hops).await?;

    // Split once and keep the halves: both phases run over this one session, so what is measured
    // is the *established* stream surviving. A replacement session opened after the kill would
    // instead measure cold-start path selection -- an easier problem, since a fresh session never
    // minted a SURB through the dead relayer.
    let (mut rx, mut tx) = tokio::io::split(session);

    // Phase 1 -- warm up: establish the baseline rate and who is actually carrying replies.
    let warmup_payload = tagged_payload(WARMUP_PHASE, WARMUP_BYTES);
    let (before_kill, spread) = pump_and_measure(
        &mut rx,
        &mut tx,
        &candidates,
        &warmup_payload,
        &format!("{}-before-kill", topology.direction),
        PumpOpts {
            phase: Some(WARMUP_PHASE),
            ..PumpOpts::default()
        },
    )
    .await?;
    anyhow::ensure!(
        before_kill.arrival_pct() > 50.0,
        "baseline pump only returned {:.1}% — the path was already broken before the \
         kill, so the result would say nothing about resilience",
        before_kill.arrival_pct(),
    );

    // Phase 2 -- quiesce: everything still in flight from the warm-up has to land before the kill,
    // or it is counted as survival-phase arrival and a dead stream reads as a recovering one.
    let leftover = drain_until_quiet(&mut rx, DRAIN_QUIET, "warm-up").await;
    anyhow::ensure!(
        leftover <= MAX_LEFTOVER_BYTES,
        "{leftover} B of warm-up traffic was still arriving after the warm-up was declared \
         complete (limit {MAX_LEFTOVER_BYTES} B); the two phases cannot be told apart, so no \
         survival measurement taken here would mean anything",
    );

    let busiest = spread
        .busiest()
        .ok_or_else(|| anyhow::anyhow!("no relayer forwarded anything; nothing to kill"))?;
    let victim = candidates
        .iter()
        .find(|n| n.address == busiest)
        .ok_or_else(|| anyhow::anyhow!("busiest relayer {busiest} is not a cluster node"))?;
    tracing::info!(
        victim = %busiest,
        share_pct = spread.max_share() * 100.0,
        direction = topology.direction,
        "killing the busiest relayer for the direction under test"
    );
    victim.kill()?;

    // Phase 3 -- settle: let the kill land before any new data is offered.
    //
    // Starting the survival phase at the instant of the kill measures something else entirely: its
    // first seconds race packets that were already in flight when the relayer died, so a byte that
    // arrives cannot be attributed to a session that recovered rather than to one that had not yet
    // noticed. Waiting a few seconds means every byte of the survival phase is offered to a network
    // that has *already* lost the relayer, which is the question being asked.
    let settle = kill_settle();
    tracing::info!(?settle, "letting the kill land before offering new data");
    tokio::time::sleep(settle).await;

    // Phase 4 -- survive: fresh bytes, offered at a steady rate for several times the deadline so
    // that load is still arriving when recovery happens. Sized from the measured baseline rather
    // than fixed, so the offered rate stays a constant fraction of what this cluster can do.
    let offered_mbps = before_kill.mbps * SURVIVAL_LOAD_FRACTION;
    let survival_bytes =
        (offered_mbps * 1_000_000.0 * SURVIVAL_LOAD_DURATION.as_secs_f64()) as usize;
    tracing::info!(
        "survival phase: offering {:.2} MB at {offered_mbps:.2} MB/s for {SURVIVAL_LOAD_DURATION:?} \
         (baseline {:.2} MB/s)",
        survival_bytes as f64 / 1_000_000.0,
        before_kill.mbps,
    );
    // Counted at the entry's own session, not from the relayers' node-wide `forwarded` counter.
    // That counter includes probes, keep-alives and every other session those nodes carry, so
    // comparing it against this phase's payload is not a like-for-like measurement -- doing so once
    // produced a two-orders-of-magnitude "delivery gap" that was mostly other traffic. These
    // counters carry a `session_id`, so they say what actually reached this session and what became
    // of it.
    let counters_before = session_metrics::sample();
    // Absolute, not a delta: a gauge that never moves differences to zero. Logged so an experiment
    // on HOPR_SESSION_FRAME_TIMEOUT_MS can tell "the timeout changed nothing" from "the timeout
    // never changed" -- the override is floored at 100 ms and read once at manager construction.
    tracing::info!(
        frame_timeout_ms = ?counters_before.frame_timeout_ms(),
        "sequencer frame timeout in effect for this session"
    );
    let survival_payload = tagged_payload(SURVIVAL_PHASE, survival_bytes);
    let (after_kill, spread_after) = pump_and_measure(
        &mut rx,
        &mut tx,
        &candidates,
        &survival_payload,
        &format!("{}-after-kill", topology.direction),
        PumpOpts {
            pace: pace_for_rate(offered_mbps),
            phase: Some(SURVIVAL_PHASE),
            idle_budget: Some(SURVIVAL_IDLE_BUDGET),
            tail_grace: Some(SURVIVAL_TAIL_GRACE),
        },
    )
    .await?;
    // Logged whatever the outcome: on a healthy run it confirms the counters track the payload, so
    // the one time they disagree the reading is already trusted.
    let counters = counters_before.delta(&session_metrics::sample());
    tracing::info!("after-kill session counters: {}", counters.summary());
    tracing::info!(
        "after-kill session families that moved: {}",
        counters.nonzero()
    );

    assert_recovered(&before_kill, &after_kill, &spread, &spread_after, settle)
}

/// The original incident: the entry reassembles the reply stream, and a return relay dies under it.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn session_should_survive_return_relayer_loss() -> anyhow::Result<()> {
    session_should_survive_relayer_loss(RETURN_PATH).await
}

/// The other half of the same fix: the **exit** reassembles the request stream, so this exercises
/// the sequencer inside `hoprd` rather than the one linked into the client.
///
/// Worth its own scenario because the two are deployed independently -- shipping the client alone
/// leaves this path on whatever the exit is running -- and because nothing in the return-path
/// result predicts it: different process, different binary, different build.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn session_should_survive_forward_relayer_loss() -> anyhow::Result<()> {
    session_should_survive_relayer_loss(FORWARD_PATH).await
}

/// Phase tag for the payload offered *while the outage is in effect*.
///
/// A third tag, distinct from [`SURVIVAL_PHASE`], so the bytes offered into the frozen return path
/// cannot be counted as recovery-phase arrival — the same phase-separation the warm-up drain gives
/// the other scenarios, applied to the outage/recovery boundary here.
const OUTAGE_PHASE: u8 = 3;

/// How long every return relayer stays frozen before being thawed.
///
/// Long enough to outlast detection (~9 s, per [`KILL_SETTLE`]) and a re-plan attempt, so the
/// session is demonstrably *in* the outage — not merely mid-detection — when recovery is triggered.
/// A re-plan cannot help here (every candidate is down at once), so this only has to be long enough
/// that the degradation is unambiguous.
const OUTAGE_DURATION: Duration = Duration::from_secs(20);

/// Resumes every paused relayer when it drops, so an early return between the freeze and the
/// explicit thaw — a pump error or a failed outage assertion — cannot leave `SIGSTOP`ped `hoprd`
/// processes behind. A stopped process ignores `SIGTERM` until it is continued, so orphans would
/// hold their ports and block the next run's cluster.
struct Thawed<'a>(&'a [NodeInfo]);

impl Drop for Thawed<'_> {
    fn drop(&mut self) {
        for node in self.0 {
            if let Err(e) = node.resume() {
                tracing::warn!(%e, "failed to resume relayer during cleanup");
            }
        }
    }
}

/// The 2026-07-28 incident: a *common-mode* return-path outage.
///
/// Every return relayer freezes at once (SIGSTOP) — the shape of a client whose own uplink degrades,
/// where all exits lose their return path together while SURB supply stays healthy — and then thaws
/// (SIGCONT). The established session must **degrade gracefully** through the outage (stay alive, no
/// per-packet ERROR storm, no flap-thrash) and **recover** once the return path is restored.
///
/// This is deliberately *not* [`session_should_survive_return_relayer_loss`]. That kills one relayer
/// permanently and the session re-plans around it; here there is nothing to re-plan to — every
/// candidate is down simultaneously — so the only recovery is the outage lifting. Killing all
/// relayers would be a permanent outage with no recovery to measure, which is why this freezes and
/// thaws them instead.
///
/// Thresholds are **predicted, not yet measured** (as with the sibling scenarios): SIGSTOP/SIGCONT
/// leaves TCP sessions open but unserviced, so recovery may need a reconnect on thaw and could run
/// closer to [`RECOVERY_DEADLINE`] than the single-relayer case. Re-derive from a measured run
/// before trusting a pass.
///
/// # ERROR-storm check (manual, per the run checklist)
///
/// The "no per-packet ERROR storm" half of the incident is asserted by the `hopr-transport-session`
/// unit tests (`dispatching_to_a_session_whose_sink_closed_should_be_a_quiet_counted_drop` et al.).
/// End-to-end here it requires the entry **and** the nodes to run the dispatch-drop fix; with the
/// entry pinned to that build, `sending on a disconnected channel` / `received data for an
/// unregistered session` must not recur once per packet during the outage. Grep the kept artifacts:
/// `sed -E 's/\x1b\[[0-9;]*m//g' <run.log> | grep -cE 'disconnected channel|unregistered session'`.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn session_should_survive_common_mode_return_outage() -> anyhow::Result<()> {
    // 0-hop out / 1-hop back: every reply traverses a cluster relayer, so freezing the relayer set
    // takes the whole return path down while requests still reach the exit directly.
    let (_env, session, candidates) = setup_env_with_hops(0, 1).await?;
    let (mut rx, mut tx) = tokio::io::split(session);

    // Phase 1 — warm up: baseline rate, and confirm replies actually traverse the relayers we are
    // about to freeze (otherwise the outage would be a no-op and the test would measure nothing).
    let warmup = tagged_payload(WARMUP_PHASE, WARMUP_BYTES);
    let (before, spread) = pump_and_measure(
        &mut rx,
        &mut tx,
        &candidates,
        &warmup,
        "common-mode-before",
        PumpOpts {
            phase: Some(WARMUP_PHASE),
            ..PumpOpts::default()
        },
    )
    .await?;
    anyhow::ensure!(
        before.arrival_pct() > 50.0,
        "baseline pump only returned {:.1}% — the path was already broken before the outage",
        before.arrival_pct(),
    );
    anyhow::ensure!(
        spread.total > 0,
        "no replies traversed the cluster relayers — the return path was not 1-hop, so freezing \
         the relayer set would take nothing down and the test would measure nothing",
    );

    // Phase 2 — quiesce: warm-up traffic must land before the outage, or it is counted against a
    // later phase and a backlog draining reads as the session working through the outage.
    let leftover = drain_until_quiet(&mut rx, DRAIN_QUIET, "warm-up").await;
    anyhow::ensure!(
        leftover <= MAX_LEFTOVER_BYTES,
        "{leftover} B of warm-up traffic still arriving after drain (limit {MAX_LEFTOVER_BYTES} B); \
         the phases cannot be told apart",
    );

    // Phase 3 — freeze EVERY return relayer at once: the common-mode outage.
    tracing::info!(
        frozen = candidates.len(),
        max_share_pct = spread.max_share() * 100.0,
        "freezing all return relayers — common-mode outage begins",
    );
    // Bind the cleanup guard BEFORE pausing anything, so a failure partway through the pause loop
    // still resumes whatever was already frozen. Its Drop also covers an early return from the
    // outage phase — a stopped process would otherwise ignore cluster teardown's SIGTERM and orphan
    // itself on its ports.
    let thawed = Thawed(&candidates);
    for node in &candidates {
        node.pause()?;
    }

    // Phase 4 — offer load into the outage. The return path is fully down, so an unreliable session
    // legitimately delivers little; the point is that it degrades *cleanly* — the pump completes
    // rather than the session panicking, thrashing, or wedging the writer permanently.
    let offered_mbps = before.mbps * SURVIVAL_LOAD_FRACTION;
    let outage_bytes = (offered_mbps * 1_000_000.0 * OUTAGE_DURATION.as_secs_f64()) as usize;
    let counters_before = session_metrics::sample();
    let outage_payload = tagged_payload(OUTAGE_PHASE, outage_bytes);
    // Pump directly, without `pump_and_measure`: the relayer histogram is sampled over each node's
    // REST API, and those nodes are SIGSTOPped right now, so sampling them would hang. The entry's
    // own in-process counters (`session_metrics`, below) are the readable signal during the outage.
    //
    // `idle_budget = OUTAGE_DURATION` is what actually bounds the freeze: with the return path down
    // nothing comes back, so the pump is idle from the first byte and stops one budget later. That
    // keeps the real outage ~`OUTAGE_DURATION` instead of running to the default idle budget or the
    // pump timeout, so recovery figures stay comparable with the relayer-loss scenario.
    let during = pump_halves(
        &mut rx,
        &mut tx,
        &outage_payload,
        "common-mode-during-outage",
        PUMP_TIMEOUT,
        PumpOpts {
            pace: pace_for_rate(offered_mbps),
            phase: Some(OUTAGE_PHASE),
            idle_budget: Some(OUTAGE_DURATION),
            tail_grace: Some(OUTAGE_DURATION),
        },
    )
    .await?;
    tracing::info!(
        arrival_pct = during.arrival_pct(),
        outcome = ?during.outcome,
        "offered load through the outage (low arrival expected; the assertion is clean degradation)",
    );
    // Clean degradation, not collapse: the session must not have been *torn down* while the relayers
    // were merely frozen. Only `SessionClosed` means that — `NeverStarted` is the expected outcome of
    // a full outage (zero bytes attributed), so `exit_stopped_serving()` cannot be used here; it
    // conflates the two and would fail a healthy degrade.
    anyhow::ensure!(
        during.outcome != PumpOutcome::SessionClosed,
        "the session was torn down during the outage ({:?}) instead of degrading — this is the \
         ungraceful collapse the fix must prevent",
        during.outcome,
    );
    let during_counters = counters_before.delta(&session_metrics::sample());
    tracing::info!(
        "during-outage session families that moved: {}",
        during_counters.nonzero()
    );

    // Phase 5 — thaw every relayer: the outage lifts and the return path is restored. Dropping the
    // guard resumes them exactly once here on the normal path (and is the safety net on early exit).
    drop(thawed);
    let settle = kill_settle();
    tracing::info!(
        ?settle,
        frozen = candidates.len(),
        "thawed all return relayers — letting recovery settle"
    );
    drain_until_quiet(&mut rx, DRAIN_QUIET, "outage").await;
    tokio::time::sleep(settle).await;

    // Phase 6 — recovery: with the path restored, a fresh survival load must come back to a usable
    // rate within the deadline. Reuses the shared recovery assertion so this scenario is held to the
    // same bar as the single-relayer one.
    let survival_bytes =
        (offered_mbps * 1_000_000.0 * SURVIVAL_LOAD_DURATION.as_secs_f64()) as usize;
    let recovery_payload = tagged_payload(SURVIVAL_PHASE, survival_bytes);
    let (after, spread_after) = pump_and_measure(
        &mut rx,
        &mut tx,
        &candidates,
        &recovery_payload,
        "common-mode-after-recovery",
        PumpOpts {
            pace: pace_for_rate(offered_mbps),
            phase: Some(SURVIVAL_PHASE),
            idle_budget: Some(SURVIVAL_IDLE_BUDGET),
            tail_grace: Some(SURVIVAL_TAIL_GRACE),
        },
    )
    .await?;

    assert_recovered(&before, &after, &spread, &spread_after, settle)
}

/// Report every measurement, then assert on them in combination.
///
/// Each statistic alone has a way of being satisfied by a stream that is dead. `time_to_sustain`
/// once reported "recovered after 381.3s" for one, because an opening burst filled the window and
/// nothing was required of the stream afterwards. Aggregate arrival folds recovery latency,
/// steady-state rate and transfer length into one number whose run-to-run spread (measured: 19-45%
/// on identical builds) is wider than the effect being detected. The conjunction is what has no
/// such hole: the stream must reach the rate in time, *still* be carrying it when the pump ends,
/// and never have gone quiet for longer than the deadline on the way.
fn assert_recovered(
    before_kill: &Transfer,
    after_kill: &Transfer,
    spread: &RelayerSpread,
    spread_after: &RelayerSpread,
    settle: Duration,
) -> anyhow::Result<()> {
    let target_mbps = before_kill.mbps * RECOVERY_FRACTION;
    // Timed from the kill, so the post-kill settle counts against the deadline. The session is
    // already broken during that window even though no load is being offered, and a user waiting
    // for their connection to come back is not paused with it.
    let recovered_after = after_kill
        .time_to_sustain(target_mbps, RECOVERY_SUSTAIN_WINDOW)
        .map(|since_offer| since_offer + settle.as_secs_f64());
    let steady_state = after_kill.steady_state_mbps(RECOVERY_SUSTAIN_WINDOW);
    let longest_stall = after_kill.longest_stall();

    // Nothing on the other end is serving the session, so no amount of further waiting can change
    // the answer. Report that as its own finding rather than as a recovery that ran out of time --
    // "the exit stopped echoing" and "the return path is slow" call for entirely different work.
    // Only when the close actually prevented the measurement. A session that delivered almost
    // everything and then closed has answered the question; failing it as "never got the chance to
    // recover" would be plainly false, and was -- a run reporting exactly that had already returned
    // 93.6% of its payload.
    anyhow::ensure!(
        !after_kill.outcome.exit_stopped_serving()
            || after_kill.arrival_pct() >= MIN_SURVIVAL_ARRIVAL_PCT,
        "the exit stopped serving the session before it could recover ({:?} after {:.1}s, only \
         {:.1}% of {} B returned) — {}",
        after_kill.outcome,
        after_kill.wall_seconds,
        after_kill.arrival_pct(),
        after_kill.sent_bytes,
        spread_after.summary(),
    );

    // Any warm-up bytes that surfaced during the survival phase are a backlog releasing, not this
    // phase recovering. They are excluded from every figure above by construction; report them, and
    // refuse the run if there are enough of them to mean the drain did not work.
    if after_kill.foreign_bytes > 0 {
        tracing::warn!(
            "{} B of earlier-phase traffic arrived during the survival phase and was excluded",
            after_kill.foreign_bytes,
        );
    }
    anyhow::ensure!(
        after_kill.foreign_bytes <= MAX_LEFTOVER_BYTES,
        "{} B of warm-up traffic arrived during the survival phase (limit {MAX_LEFTOVER_BYTES} B); \
         the drain did not separate the phases, so the session was still working off a backlog \
         rather than carrying new load",
        after_kill.foreign_bytes,
    );

    // A pump shorter than the sustain window cannot answer the question at all. `time_to_sustain`
    // already refuses to guess, but say so explicitly rather than let it read as "never recovered".
    anyhow::ensure!(
        after_kill.wall_seconds > RECOVERY_SUSTAIN_WINDOW.as_secs_f64(),
        "after-kill pump ran only {:.2}s, shorter than the {RECOVERY_SUSTAIN_WINDOW:?} sustain \
         window, so recovery cannot be measured at all ({:.1}% arrived)",
        after_kill.wall_seconds,
        after_kill.arrival_pct(),
    );

    tracing::info!(
        "after-kill measurements: outcome {:?} | recovery {} (aim {}s, boundary {}s){} | steady state over the \
         final {RECOVERY_SUSTAIN_WINDOW:?} {steady_state:.2} MB/s vs target {target_mbps:.2} | \
         ttfb {} | longest stall {longest_stall:.1}s | inter-arrival p50 {} / p95 {} | \
         arrival {:.1}% ({} B of {} B) | wall {:.1}s",
        after_kill.outcome,
        recovered_after.map_or("never reached".to_string(), |s| format!("took {s:.1}s")),
        RECOVERY_AIM.as_secs(),
        RECOVERY_DEADLINE.as_secs(),
        // Both thresholds, in order. The earlier version only compared against the aim and then
        // claimed the boundary had been met regardless, so a 270s recovery against a 20s boundary
        // printed "PASSES the boundary" -- the opposite of the truth, in the one line a reader
        // scans first. Neither threshold is asserted, which is exactly why the wording has to be
        // right: this string is the only place the run says whether it hit them.
        match recovered_after {
            None => " (MISSES the boundary: never reached)",
            Some(s) if s > RECOVERY_DEADLINE.as_secs_f64() => " (MISSES the boundary, and the aim)",
            Some(s) if s > RECOVERY_AIM.as_secs_f64() => " (passes the boundary, MISSES the aim)",
            Some(_) => " (passes the boundary and the aim)",
        },
        after_kill
            .time_to_first_byte()
            .map_or("never".to_string(), |s| format!("{s:.1}s")),
        after_kill
            .inter_arrival_quantile(0.5)
            .map_or("n/a".to_string(), |s| format!("{s:.3}s")),
        after_kill
            .inter_arrival_quantile(0.95)
            .map_or("n/a".to_string(), |s| format!("{s:.3}s")),
        after_kill.arrival_pct(),
        after_kill.received_bytes,
        after_kill.sent_bytes,
        after_kill.wall_seconds,
    );
    // The rate the session actually delivered, measured to the point where the bulk had returned
    // rather than to whenever the last straggler landed. `mbps` above spans first arrival to last,
    // which a single late packet can move by a third.
    tracing::info!(
        "delivered throughput: {} to 95% of the payload, {} to 50% (offered {:.2} MB/s)",
        after_kill
            .throughput_at(0.95)
            .map_or("never reached 95%".to_string(), |r| format!("{r:.2} MB/s")),
        after_kill
            .throughput_at(0.50)
            .map_or("never reached 50%".to_string(), |r| format!("{r:.2} MB/s")),
        before_kill.mbps * SURVIVAL_LOAD_FRACTION,
    );
    tracing::info!(
        "recovery is timed from the kill: the {settle:?} settle before any load was offered is \
         included in the figure above",
    );

    // 1. Nearly all of the data offered after the fault came back. This is the bar: recovery time,
    //    steady state and stalls are reported above as diagnostics, but a session that delivers its
    //    payload has survived losing a relayer whatever shape the curve took getting there.
    anyhow::ensure!(
        after_kill.arrival_pct() >= MIN_SURVIVAL_ARRIVAL_PCT,
        "session did not survive the relayer loss: only {:.1}% of {} B came back (need \
         {MIN_SURVIVAL_ARRIVAL_PCT:.0}%); recovery {}, the lost relayer had carried {:.0}% of \
         replies — {}",
        after_kill.arrival_pct(),
        after_kill.sent_bytes,
        recovered_after.map_or("never reached the target rate".to_string(), |s| format!(
            "took {s:.1}s"
        )),
        spread.max_share() * 100.0,
        spread_after.summary(),
    );

    // 3. It never went quiet for longer than the deadline. A stream that stalls for a minute
    //    mid-transfer can still satisfy both of the above, and is not a recovered stream.
    anyhow::ensure!(
        longest_stall <= RECOVERY_DEADLINE.as_secs_f64(),
        "return path went quiet for {longest_stall:.1}s, longer than the {RECOVERY_DEADLINE:?} \
         deadline, despite recovering at {} — {}",
        recovered_after.map_or("never".to_string(), |s| format!("{s:.1}s")),
        spread_after.summary(),
    );

    // Both sides must speak in attributed bytes. `sha_ok` for a phased pump is computed over
    // this phase's records alone, so comparing it against the *raw* `received_bytes` -- which
    // includes the earlier phase's backlog -- lets foreign traffic push the count past the
    // budget while some of this phase's records are still missing, and reports a merely lossy
    // stream as corrupted.
    anyhow::ensure!(
        after_kill.attributed_bytes < after_kill.sent_bytes || after_kill.sha_ok,
        "full payload returned after the kill but corrupted (SHA-256 mismatch)",
    );
    tracing::info!(
        "survived relayer loss: {:.1}% → {:.1}% arrival, replies rerouted to {}",
        before_kill.arrival_pct(),
        after_kill.arrival_pct(),
        spread_after.summary(),
    );
    Ok(())
}

/// The shape a real deployment runs: one relay in each direction.
///
/// Kept as its own scenario because it is the only one where the histogram cannot attribute a
/// packet to a direction -- both directions traverse a relay, so the victim is "the busiest relay"
/// without saying which stream it was carrying. That makes it a weaker diagnostic than the two
/// single-sided scenarios and a better end-to-end check than either.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn a_symmetric_session_should_survive_relayer_loss() -> anyhow::Result<()> {
    session_should_survive_relayer_loss(SYMMETRIC).await
}
