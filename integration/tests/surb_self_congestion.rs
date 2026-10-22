//! Does the entry's SURB balancer congest the entry's own uplink?
//!
//! # The incident these reproduce
//!
//! On 2026-09-24 a `gnosis_vpn-client` v0.96.0 → v0.96.3 user on macOS connected to the
//! Netherlands exit three times; every connect succeeded in 5–9 s and every one was ended by the
//! user after minutes of a bursty, stalling downlink (0–880 KiB/s, tunnel ping 0.17–10.6 s). Probes
//! to *every* exit slowed down at the same instants while the tunnel was up and were clean while it
//! was down, so the bottleneck sat at the entry, not at an exit. What it lined up with was the entry's
//! own SURB production, as printed by `surb balancer state`:
//!
//! 1. **Refill bursts.** The PID (`P 0.6 / I 0.7 / D 0.2`, per 100 ms tick, `pid` crate without a
//!    dt) sat at `output=0` while the level estimate was above target and jumped to the full
//!    `max_surbs_per_sec` (3797, later 5063 SURB/s) for a second or two whenever it dipped below.
//!    At two SURBs per ~1.46 kB keep-alive that is ~22–30 Mbit/s of upstream from nothing.
//! 2. **Degraded mode pinned at the budget.** Late replies read as a dead return path
//!    (`return path degraded`); with `sustain_on_return_path_loss: true` — which the client sets —
//!    the controller overwrites the level with 0 and production stays at `max_surbs_per_sec` with
//!    `consumed` flat (12:04:54 onwards: produced 47k → 204k against 40k consumed).
//! 3. **It survived the disconnect.** The client dropped the session without closing it (fixed in
//!    gnosis_vpn-client #830, unreleased at the time); the balancer kept minting ~3.3k SURB/s for the
//!    30 s until the worker stopped.
//!
//! Full analysis: `claude/2026-09-25-kika-macos-surb-flood-self-congestion.md` in the
//! "Session stability" project.
//!
//! # The scenarios
//!
//! Every scenario asserts the **desired** behaviour, as `upload_survival` does: a failure is the bug
//! reproduced, a pass means the fix reached the line.
//!
//! | scenario | needs | reproduces |
//! | -------- | ----- | ---------- |
//! | [`surb_refills_should_track_consumption`] | 3 nodes, bursty load | 1 |
//! | [`return_outage_should_not_pin_surb_production_at_max`] | 5 nodes, SIGSTOP all but one relayer | 2 |
//! | [`dropping_a_session_should_stop_its_balancer`] | 5 nodes, SIGSTOP all but one relayer | 3 |
//! | [`shaped_uplink_should_not_stall_downstream`] | 3 nodes + a shaped entry uplink (root) | the user-visible symptom |
//! | [`degraded_mode_should_not_pin_surb_production_at_max`] | as 2, planner refresh slowed to 60 s | 2, via degraded mode |
//! | [`shaped_outage_should_not_loop`] | 5 nodes, shaped entry uplink (root), relayers flapping | the loop: 1–3 feeding the symptom |
//!
//! The first three need no shaping: they read the balancer's own gauges and SURB counters (see
//! [`hoprd_integration_test::balancer`]), which show the bursts and the pinning whether or not a link
//! suffers from them. The fourth is the end-to-end symptom and needs the entry's uplink to be the
//! bottleneck, which an unshaped loopback never is — see `scripts/shape-edge-uplink.sh`.
//!
//! Every balancer here is [`gnosis_vpn_client_surb_config`] — the client's main-session config
//! including `sustain_on_return_path_loss` — not the throughput tests' `gnosis_main_surb_config`.
//!
//! **Thresholds are predicted from the incident log and a model of the control law.** The first
//! cluster run (2026-10-07) passed scenarios 1–3 vacuously, and taught three things now built in:
//!
//! - A steady 0.25 MB/s never drives the PID to the budget: each surplus winds its integral to
//!   −budget, which caps the next refill (measured: peak 1132 SURB/s at 284 SURB/s consumed). The
//!   incident's full-budget bursts followed large dips under a bursty 0.1–0.9 MB/s download, so
//!   scenario 1 now offers bursts.
//! - Degraded mode never triggers when **every** return relayer is down: hopr-transport marks a
//!   session degraded only in the refill step, which runs only after a re-plan *moved* traffic
//!   (`return_path_recovery.rs`), and with no relayer left nothing can move. Scenarios 2 and 3 now
//!   freeze all relayers but the least used one, and scenario 2 fails as inconclusive if the freeze
//!   did not take the return path down rather than passing on a balancer that was simply idle.
//! - An idle leaked balancer produces nothing, so scenario 3 also asserts the session's own
//!   `hopr_session_lifetime_state` leaves Active.
//!
//! The second run (2026-10-07) reproduced 1 and 3 and refined scenario 1:
//!
//! - Under bursts the mean mint tracks consumption (438 vs 429 SURB/s) but the 1 s peak (1856)
//!   sat within 8 % of a 4x bound, so it is now only logged. Scenario 1 asserts the windup itself —
//!   output 0 with the level > 200 below target, 17 % of samples — and the initial fill at the
//!   budget (3.5 s).
//! - The dropped session stayed Active and minted 3532 SURBs, partly at the full budget after the
//!   level estimate's periodic (~15 s) correction knocked it below target.
//! - Scenario 2 stayed inconclusive. Not for lack of relayer diversity: the planner's
//!   `diversity collapsed … distinct_relayers=0` warning is a false positive (it counts first hops
//!   on paths already moved out by `std::mem::take`, so it always reads 0). The detector needs
//!   ≥ 20 SURBs minted per second onto a dead pair for 5 s in a row, and the balancer, idling above
//!   target at 0.25 MB/s, never minted that steadily. Scenario 2 now freezes the relayers into a
//!   1 MB/s load.
//!
//! The third run (2026-10-07, 1 MB/s) reproduced scenario 2 **without** degraded mode:
//!
//! - The silence runs reached 3 of 5, then reset: the planner's graph weighting moved minting off
//!   the frozen relayers within ~4 s (from ~90 to < 20 SURB/s per dead pair, below the detector's
//!   evidence floor) while all 4 candidates were still cached. Rerouting beats detection, so
//!   degraded mode is not a precondition any more.
//! - SURBs already stored for the dead routes kept being spent (69 % arrival), so the level
//!   estimate ran ~6k high. The 15 s corrections then dropped it by 6763 and 5733 and drove output
//!   to the full budget for 4.9 s in total — the incident's flood, by another path.
//! - Scenario 5 reaches degraded mode on purpose. The planner re-weights its cached paths every
//!   5 s from the same telemetry the detector reads, so with a healthy survivor it always wins the
//!   race. Slowing its refresh to 60 s (`IntegrationEnv::setup_with_planner_refresh`) models a
//!   network whose alternatives look no better, which is when the detector is the only way out.
//!
//! `SURB_BALANCER_CSV_DIR=<dir>` writes each scenario's balancer series. The scenarios' own summary
//! lines log under the `surb_self_congestion` target, so include it in `RUST_LOG`:
//!
//! ```bash
//! RUST_LOG=warn,hoprd_integration_test=info,surb_self_congestion=info,\
//! hopr_transport_session::balancer::controller=debug,\
//! hopr_transport=info,hopr_transport::protocol::surb_telemetry=debug
//! ```
//!
//! # Varying the controller
//!
//! The PID gains are read from `HOPR_BALANCER_PID_{P,I,D}_GAIN` when each session is created, in this
//! process (the entry is in-process), so a gain experiment needs no code change:
//!
//! ```bash
//! HOPR_BALANCER_PID_I_GAIN=0.01 just surb-congestion surb_refills_should_track_consumption
//! ```
//!
//! # Running them
//!
//! ```bash
//! just surb-congestion                                       # the unshaped scenarios
//! sudo bash scripts/shape-edge-uplink.sh up 16 19005         # 5-node cluster: entry on 19005
//! just surb-congestion shaped_uplink_should_not_stall_downstream shaped_outage_should_not_loop
//! sudo bash scripts/shape-edge-uplink.sh down
//! ```
//!
//! One scenario per invocation — several of them SIGSTOP cluster nodes.
//!
//! # The loop (scenario 6)
//!
//! Scenario 4 (2026-10-07, 8 Mbit/s) stalled only once, at session start: 5.1 s to first byte
//! against 0.07 s for the capped control arm — the initial fill queued the data behind it. With a
//! healthy return path nothing re-triggered the flood afterwards. Scenario 6 adds the disturbance:
//! after a 20 s warm-up (fill done, every return pair has delivered), three of four return
//! relays are down 6 s and up 2 s, repeated, for 45 s. A stop outlasts the
//! entry's 2 s frame age, so to the session it is loss, while the relayers come back often enough
//! for the planner to keep them as candidates and the SURBs routed through them go stale. Two loops can then close through the shaped uplink: stale SURBs → level correction →
//! full-budget burst → queued replies → more stale SURBs; and late replies → degraded mode → 10 s
//! at the budget → late replies again. Each arm runs the same outage; the capped control arm is
//! the yardstick for the harm, and the count of separate full-budget episodes is the loop.
//!
//! # Changing the relay outage
//!
//! Scenarios 2, 3, 4, 5 and 6 take return relays down by pausing their `hoprd` processes. Each has
//! a default outage (S4: none); four environment variables override it without a rebuild — see
//! [`hoprd_integration_test::outage`]:
//!
//! ```bash
//! SURB_RELAY_OUTAGE=down|<down_s>/<up_s>|none   # whole outage, intermittent, or no outage
//! SURB_RELAYS_DOWN=all-but-one|all|<n>          # which return relays go down
//! SURB_LOAD_MBPS=1                              # data rate during the outage
//! SURB_OUTAGE_SECS=30                           # how long it lasts
//! ```
//!
//! With its defaults (2026-10-07) scenario 6 never reached the rate limit: at 0.25 MB/s the
//! buffer-estimate corrections stay small. Scenario 2's conditions — relays down for the whole
//! outage at 1 MB/s — are what reached it there.

use std::time::Duration;

use edgli::hopr_lib::exports::transport::{SURB_SIZE, SurbBalancerConfig};
use hoprd_integration_test::{
    Address, HoprSession, IntegrationEnv,
    balancer::{STATE_ACTIVE, Sampler, Trace},
    cluster::{NodeInfo, request_cluster_size},
    env::{first_edge_p2p_port, gnosis_vpn_client_surb_config, last_edge_p2p_port},
    outage::{OutagePlan, Pattern, RelaysDown},
    pump::{
        PumpOpts, PumpOutcome, Shape, Transfer, drain_until_quiet, pace_for_rate_with_chunk,
        pump_halves, tagged_payload,
    },
    relayers,
};
use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};

/// How often the balancer is sampled. Its own loop ticks every 100 ms.
const SAMPLE_EVERY: Duration = Duration::from_millis(100);

/// Offered rate, MB/s. Near the incident's ~110–200 KiB/s average downlink, and far below what a
/// local cluster carries, so nothing here is throughput-bound except what a scenario shapes.
const OFFERED_MBPS: f64 = 0.25;

/// Bytes per paced write: a small, regular write like a tunnel's, not a 64 KiB burst per pace.
const CHUNK: usize = 4096;

/// Scenario 1's load: bursts at the incident's peak downlink (~0.9 MB/s) separated by quiet gaps,
/// averaging ~0.4 MB/s. A burst drains the exit's store faster than the balancer refills it at the
/// steady rate; the quiet gap lets the integral wind back. That dip-and-refill is what drove the
/// incident's full-budget bursts.
const BURST_MBPS: f64 = 1.0;
/// Bytes per burst: 2 s at [`BURST_MBPS`].
const BURST_BYTES: usize = 2_000_000;
/// Quiet gap after each burst.
const BURST_GAP: Duration = Duration::from_secs(3);
/// Bursts offered.
const BURSTS: usize = 15;

/// Time excluded from the start of a trace: the initial fill to target runs at the budget by
/// design, and the incident's question is what happens after it.
const FILL_GRACE: Duration = Duration::from_secs(15);

/// During an outage, production may run ahead of the pre-outage consumption, but not by more than
/// this factor in any one second.
///
/// The incident ran ~10–20x: bursts at 3797–5063 SURB/s while the session consumed a few hundred.
const MAX_BURST_OVER_CONSUMPTION: f64 = 4.0;

/// Floor on the burst allowance as a share of `max_surbs_per_sec`, so a nearly idle session is not
/// held to "4x almost nothing".
const BURST_FLOOR_FRACTION: f64 = 0.25;

/// How far below target (in SURBs) the level must be for a zero output to count as starved.
const STARVED_MARGIN: f64 = 200.0;

/// Largest share of steady-state samples allowed to sit starved below target.
///
/// Measured on the current controller (2026-10-07, bursty load): 17 %. The P term alone commands
/// ≥ 120 SURB/tick at 200 below target, so a controller without integral windup stays near 0.
const MAX_STARVED_SHARE: f64 = 0.02;

/// Largest share of steady-state samples allowed at ≥95 % of `max_surbs_per_sec`.
const MAX_SHARE_AT_BUDGET: f64 = 0.05;

/// Most seconds a fill or a degraded episode may spend producing at the full budget.
const MAX_SECONDS_AT_BUDGET: f64 = 2.0;

/// Phase tags, so a backlog from one phase cannot be counted as the next one arriving.
const WARMUP_PHASE: u8 = 1;
const OUTAGE_PHASE: u8 = 3;
const CONTROL_PHASE: u8 = 4;
const PRODUCTION_PHASE: u8 = 5;

const PUMP_TIMEOUT: Duration = Duration::from_secs(600);
const DRAIN_QUIET: Duration = Duration::from_secs(3);

/// Return-path outage detection takes ~9 s (`KILL_SETTLE` in `return_path.rs`, and
/// `RETURN_PATH_DEGRADED_GRACE` = 10 s in hopr-transport). Scenario 3 keeps the return path frozen
/// for twice this before dropping the session, so the drop lands inside a degraded episode.
const DETECTION_GRACE: Duration = Duration::from_secs(10);

/// How long the return relayers stay frozen.
const OUTAGE_DURATION: Duration = Duration::from_secs(30);

/// Offered load around the outage: four times the warm-up, so the balancer is minting steadily
/// when the relayers freeze rather than idling above target.
///
/// The detector (`surb_telemetry.rs::degraded_destinations`) calls a return pair dead only after
/// 5 consecutive 1 s flushes (`SILENT_FLUSHES_BEFORE_DEGRADED`) in which it minted ≥ 20 SURBs and
/// got nothing back, while a sibling pair still delivers. The planner's own refresh drops
/// SIGSTOPped relayers within ~10–14 s, after which nothing is minted onto them. At 0.25 MB/s
/// (2026-10-07) the balancer minted nothing for 3 s after the freeze and then only in 1–2 s lumps,
/// so no dead pair ever strung five qualifying flushes together and degraded mode was never reached.
const OUTAGE_MBPS: f64 = 1.0;

/// The heavy load runs this long before the freeze, so the PID has left its post-warm-up idle
/// and is minting onto every cached return pair when they go silent.
const PRE_FREEZE: Duration = Duration::from_secs(5);

/// After a session is dropped, its balancer gets this long to notice before production must stop.
const DROP_GRACE: Duration = Duration::from_secs(2);

/// How long production is watched after the drop.
const AFTER_DROP: Duration = Duration::from_secs(15);

/// SURBs a dropped session may still produce after [`DROP_GRACE`] — in-flight writes, not a loop.
const MAX_SURBS_AFTER_DROP: u64 = 50;

/// Nodes for every scenario. One size for all, so `just surb-congestion` can run them on one
/// chain with frozen identities (a smaller cluster must not follow a bigger one on the same
/// chain), and 0-hop out / 1-hop back needs more than one return candidate.
const CLUSTER_NODES: usize = 5;

/// Resumes paused relayers on drop, so an early return cannot leave SIGSTOPped `hoprd`s holding
/// their ports (they ignore SIGTERM until continued). Same as `return_path.rs`'s guard.
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

fn paced(phase: u8, mbps: f64) -> PumpOpts {
    PumpOpts {
        pace: pace_for_rate_with_chunk(mbps, CHUNK),
        chunk: Some(CHUNK),
        phase: Some(phase),
        idle_budget: Some(Duration::from_secs(20)),
        tail_grace: Some(Duration::from_secs(10)),
        ..PumpOpts::default()
    }
}

fn bytes_for(mbps: f64, d: Duration) -> usize {
    (mbps * 1_000_000.0 * d.as_secs_f64()) as usize
}

/// The trace must actually contain the balancer, or every assertion below is about nothing.
fn require_observable(trace: &Trace, name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        trace.observable(),
        "{name}: no balancer gauges in the entry's registry ({}) — the build lacks telemetry or no \
         balancer ran, so this run cannot say anything about SURB production",
        trace.summary(),
    );
    Ok(())
}

/// 1. A bursty stream: does SURB production follow consumption, or swing between nothing and the
///    full budget?
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn surb_refills_should_track_consumption() -> anyhow::Result<()> {
    let cfg = gnosis_vpn_client_surb_config();
    request_cluster_size(CLUSTER_NODES);
    let env = IntegrationEnv::setup().await?;
    // Sample from before the session exists, so the initial fill is in the trace too.
    let sampler = Sampler::start(SAMPLE_EVERY);
    // 1-hop both ways: the shape the user ran.
    let (session, _exit) = env
        .open_unreliable_session_with_surbs(1, 1, Some(cfg))
        .await?;
    let session_id = AsRef::<str>::as_ref(session.id()).to_owned();
    let (mut rx, mut tx) = tokio::io::split(session);

    let sampler = sampler.for_session(session_id);
    let payload = tagged_payload(WARMUP_PHASE, BURST_BYTES * BURSTS);
    let transfer = pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "bursty",
        PUMP_TIMEOUT,
        PumpOpts {
            shape: Some(Shape::Burst {
                on: BURST_BYTES,
                off: BURST_GAP,
            }),
            ..paced(WARMUP_PHASE, BURST_MBPS)
        },
    )
    .await?;
    let trace = sampler.stop().await?;
    trace.maybe_write_csv("surb_refills_should_track_consumption");
    require_observable(&trace, "bursty")?;

    let steady = trace.after(FILL_GRACE);
    let budget = cfg.max_surbs_per_sec as f64;
    // The initial fill is the incident's stall-at-connect: the whole target requested at the
    // budget before any traffic needs it.
    let fill = trace.window(Duration::ZERO, FILL_GRACE);
    let fill_at_budget = fill.seconds_at_budget(budget);
    tracing::info!(
        seconds_at_budget = fill_at_budget,
        "initial fill: {}",
        fill.summary()
    );
    let consumption = steady.consume_rate().unwrap_or_default();
    let peak = steady
        .peak_mint_rate(Duration::from_secs(1))
        .unwrap_or_default();
    let at_budget = steady
        .share_output_at_least(0.95 * budget)
        .unwrap_or_default();
    let starved = steady
        .share_starved_below_target(STARVED_MARGIN)
        .unwrap_or_default();
    // Reported, not asserted: the 1 s peak depends on burst size and sat within 8 % of any sane
    // bound, so it flips between runs. `starved` is the threshold-free signature of the same lumps.
    tracing::info!(
        arrival_pct = transfer.arrival_pct(),
        longest_stall_s = transfer.longest_stall(),
        consumption,
        peak,
        peak_over_consumption = peak / consumption.max(1.0),
        starved,
        at_budget,
        budget,
        "bursty-load balancer: {}",
        steady.summary(),
    );

    anyhow::ensure!(
        consumption > 0.0 && transfer.arrival_pct() > 50.0,
        "the path was broken ({:.1}% back, {consumption:.0} SURB/s consumed) — refill shape cannot \
         be judged on a session that is not carrying traffic",
        transfer.arrival_pct(),
    );
    assert!(
        starved <= MAX_STARVED_SHARE,
        "integral windup: output 0 while the level was > {STARVED_MARGIN:.0} below target in {:.1}% \
         of the steady state (max {:.0}%) — the I term, wound negative by the last overshoot, holds \
         the refill back until the deficit is large, then overshoots again",
        starved * 100.0,
        MAX_STARVED_SHARE * 100.0,
    );
    assert!(
        fill_at_budget <= MAX_SECONDS_AT_BUDGET,
        "initial fill requested the full {budget:.0} SURB/s budget for {fill_at_budget:.1} s \
         (max {MAX_SECONDS_AT_BUDGET:.0} s) — the stall-at-connect",
    );
    assert!(
        at_budget <= MAX_SHARE_AT_BUDGET,
        "control output was at ≥95% of the {budget:.0} SURB/s budget for {:.1}% of the steady state \
         (max {:.0}%) — bang-bang control rather than tracking",
        at_budget * 100.0,
        MAX_SHARE_AT_BUDGET * 100.0,
    );
    Ok(())
}

/// Planner refresh period for the degraded-mode scenario: long enough that the detector's five
/// silent flushes complete before the planner re-weights away from the dead relayers.
const SLOW_PLANNER_REFRESH: Duration = Duration::from_secs(60);

/// What [`warmed_outage_session`] hands a scenario: the env (kept alive), the session halves, the
/// relays to take down, the ones left up, and the sampler already running since the warm-up.
type WarmedSession = (
    IntegrationEnv,
    ReadHalf<HoprSession>,
    WriteHalf<HoprSession>,
    Vec<NodeInfo>,
    Vec<Address>,
    Sampler,
);

/// Bring up the outage cluster, open a 0-hop-out / 1-hop-back session with the client's balancer,
/// warm it up with the sampler running, and pick the relays the plan takes down.
async fn warmed_outage_session(
    name: &str,
    planner_refresh: Option<Duration>,
    plan: &OutagePlan,
) -> anyhow::Result<WarmedSession> {
    let size = request_cluster_size(CLUSTER_NODES);
    anyhow::ensure!(size >= 3, "outage scenarios need ≥3 nodes, got {size}");
    let env = match planner_refresh {
        Some(period) => IntegrationEnv::setup_with_planner_refresh(period).await?,
        None => IntegrationEnv::setup().await?,
    };
    // 0-hop out / 1-hop back: freezing the relayer set takes the whole return path down while the
    // entry's keep-alives still reach the exit directly — the "return path silent, uplink fine"
    // shape the balancer reads as degraded.
    let (session, exit) = env
        .open_unreliable_session_with_surbs(0, 1, Some(gnosis_vpn_client_surb_config()))
        .await?;
    let candidates = env.relayer_candidates(exit)?;
    anyhow::ensure!(
        candidates.len() >= 2,
        "{name}: need ≥2 return relay candidates, got {}",
        candidates.len()
    );
    let session_id = AsRef::<str>::as_ref(session.id()).to_owned();
    let (mut rx, mut tx) = tokio::io::split(session);

    let sampler = Sampler::start(SAMPLE_EVERY).for_session(session_id);
    let forwarded_before = relayers::sample(&candidates).await;
    let warmup = tagged_payload(
        WARMUP_PHASE,
        bytes_for(OFFERED_MBPS, Duration::from_secs(30)),
    );
    let before = pump_halves(
        &mut rx,
        &mut tx,
        &warmup,
        &format!("{name}-warmup"),
        PUMP_TIMEOUT,
        paced(WARMUP_PHASE, OFFERED_MBPS),
    )
    .await?;
    anyhow::ensure!(
        before.arrival_pct() > 50.0,
        "{name}: warm-up only returned {:.1}% — the path was broken before the outage",
        before.arrival_pct(),
    );
    drain_until_quiet(&mut rx, DRAIN_QUIET, "warm-up").await;
    let spread = relayers::spread(&forwarded_before, &relayers::sample(&candidates).await);
    let (down, up) = plan.pick(&candidates, &spread);
    anyhow::ensure!(!down.is_empty(), "{name}: the outage takes no relay down");
    tracing::info!(
        ?up,
        down = down.len(),
        "return relays during warm-up: {}",
        spread.summary()
    );
    Ok((env, rx, tx, down, up, sampler))
}

/// Most of the outage's offered data may still arrive before the outage counts as one: above this,
/// freezing the relayers did not take the return path down and the scenario measured nothing.
const MAX_OUTAGE_ARRIVAL_PCT: f64 = 95.0;

/// 2. A return-path outage with the client's config: does production stay bounded, or pin at the
///    budget while nothing comes back?
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn return_outage_should_not_pin_surb_production_at_max() -> anyhow::Result<()> {
    outage_scenario(
        "outage",
        "return_outage_should_not_pin_surb_production_at_max",
        None,
    )
    .await
}

/// 5. The same outage with the planner's re-weighting slowed down, so the return-path detector
///    fires and the balancer enters degraded mode, as in the incident at 12:04:54: does
///    `sustain_on_return_path_loss` pin production at the budget?
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn degraded_mode_should_not_pin_surb_production_at_max() -> anyhow::Result<()> {
    outage_scenario(
        "degraded",
        "degraded_mode_should_not_pin_surb_production_at_max",
        Some(SLOW_PLANNER_REFRESH),
    )
    .await
}

/// Freeze all return relayers but one, 5 s into a 1 MB/s load, and judge SURB production during
/// the outage. With `slow_planner` set, degraded mode is a precondition (inconclusive without it).
async fn outage_scenario(
    name: &str,
    csv: &str,
    slow_planner: Option<Duration>,
) -> anyhow::Result<()> {
    let cfg = gnosis_vpn_client_surb_config();
    let default = OutagePlan {
        pattern: Pattern::Down,
        relays: RelaysDown::AllButOne,
        load_mbps: OUTAGE_MBPS,
        duration: OUTAGE_DURATION,
    };
    let plan = OutagePlan::configure(Some(default), default)?
        .ok_or_else(|| anyhow::anyhow!("{name}: SURB_RELAY_OUTAGE=none leaves nothing to test"))?;
    let (_env, mut rx, mut tx, down, up, sampler) =
        warmed_outage_session(name, slow_planner, &plan).await?;
    let pre_outage = sampler.now();

    // Start the outage *into* a running load rather than before it: the dead pairs must keep
    // receiving fresh SURBs for the detector to see them fall silent (see `OUTAGE_MBPS`).
    let thawed = Thawed(&down);
    let payload = tagged_payload(
        OUTAGE_PHASE,
        bytes_for(plan.load_mbps, PRE_FREEZE + plan.duration),
    );
    let outage = async {
        tokio::time::sleep(PRE_FREEZE).await;
        tracing::info!(?up, "outage begins: {}", plan.describe());
        let started = sampler.now();
        plan.run(&down).await?;
        anyhow::Ok(started)
    };
    // The entry keeps sending; little comes back until a re-plan moves the return path onto the
    // survivor.
    let (during, frozen_at) = tokio::join!(
        pump_halves(
            &mut rx,
            &mut tx,
            &payload,
            "outage",
            PUMP_TIMEOUT,
            PumpOpts {
                idle_budget: Some(plan.duration),
                tail_grace: Some(plan.longest_down().max(Duration::from_secs(10))),
                ..paced(OUTAGE_PHASE, plan.load_mbps)
            },
        ),
        outage,
    );
    let (during, frozen_at) = (during?, frozen_at?);
    let outage_end = sampler.now();
    drop(thawed);
    let trace = sampler.stop().await?;
    trace.maybe_write_csv(csv);
    require_observable(&trace, name)?;
    anyhow::ensure!(
        during.outcome != PumpOutcome::SessionClosed,
        "the session was torn down during the outage — that is a different failure than this \
         scenario measures"
    );

    let budget = cfg.max_surbs_per_sec as f64;
    // Consumption under the outage's own load, in the seconds before the freeze.
    let baseline = trace
        .window(pre_outage, frozen_at)
        .consume_rate()
        .unwrap_or_default();
    anyhow::ensure!(
        baseline > 0.0,
        "no consumption measured before the outage — the warm-up did not carry traffic"
    );
    let outage = trace.window(frozen_at, outage_end);
    let degraded_samples = outage.degraded_samples(budget);
    let peak = outage
        .peak_mint_rate(Duration::from_secs(1))
        .unwrap_or_default();
    let seconds_at_budget = outage.seconds_at_budget(budget);
    tracing::info!(
        baseline,
        peak,
        seconds_at_budget,
        degraded_samples,
        budget,
        "{name}: balancer during the outage: {}",
        outage.summary(),
    );
    // Inconclusive, not a pass, when the freeze did not bite. Degraded mode is no precondition:
    // the planner shifts minting off the dead relayers within ~4 s, before the detector's five
    // silent flushes, so on this build it is rarely reached (see the module docs).
    anyhow::ensure!(
        during.arrival_pct() < MAX_OUTAGE_ARRIVAL_PCT,
        "{name}: inconclusive — {:.1}% of the data still arrived, so freezing the relayers did not \
         take the return path down",
        during.arrival_pct(),
    );
    // Degraded mode is recognised by the current controller's signature (level forced to 0 at the
    // budget); a fix that bounds degraded mode changes that, and this check must then read a
    // metric or the `degraded=true` debug line instead.
    anyhow::ensure!(
        slow_planner.is_none() || degraded_samples > 0,
        "{name}: inconclusive — degraded mode was never reached even with the planner refreshing \
         every {:?}. Run with `hopr_transport=info,hopr_transport::protocol::surb_telemetry=debug` \
         and check how far `silence run climbing` got and whether `refilling behind the re-plan` \
         followed `return path silent, re-planned`.",
        slow_planner.unwrap_or_default(),
    );

    let allowed = (MAX_BURST_OVER_CONSUMPTION * baseline).max(BURST_FLOOR_FRACTION * budget);
    assert!(
        peak <= allowed && seconds_at_budget <= MAX_SECONDS_AT_BUDGET,
        "SURB production during the return-path outage peaked at {peak:.0} SURB/s (allowed \
         {allowed:.0} against {baseline:.0} SURB/s consumed before it) and spent \
         {seconds_at_budget:.1}s at the {budget:.0} SURB/s budget (max {MAX_SECONDS_AT_BUDGET}s) — \
         the level estimate ignores SURBs lost on dead relayers, and each periodic correction (or \
         degraded mode, if reached) drives production to max_surbs_per_sec",
    );
    Ok(())
}

/// 3. Drop a session mid-outage without closing it, as gnosis_vpn-client ≤ v0.96.3 did on
///    disconnect: does its balancer stop?
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn dropping_a_session_should_stop_its_balancer() -> anyhow::Result<()> {
    let default = OutagePlan {
        pattern: Pattern::Down,
        relays: RelaysDown::AllButOne,
        load_mbps: OFFERED_MBPS,
        duration: DETECTION_GRACE * 2,
    };
    // The outage makes a surviving balancer loud (degraded mode at the rate limit) rather than
    // idle above target. The lifecycle-state assertion below catches an idle leak too, so
    // `SURB_RELAY_OUTAGE=none` is allowed here.
    let plan = OutagePlan::configure(Some(default), default)?;
    let (_env, mut rx, mut tx, down, _up, sampler) =
        warmed_outage_session("drop", None, &plan.unwrap_or(default)).await?;
    let down = if plan.is_some() { down } else { Vec::new() };
    let thawed = Thawed(&down);
    let (load, before_drop_for) = plan.map_or((OFFERED_MBPS, default.duration), |p| {
        (p.load_mbps, p.duration)
    });
    let outage = async {
        match plan {
            Some(plan) => {
                tracing::info!("outage before the drop: {}", plan.describe());
                plan.run(&down).await.map(|_| ())
            }
            None => Ok(()),
        }
    };
    let payload = tagged_payload(OUTAGE_PHASE, bytes_for(load, before_drop_for));
    let (pumped, outage) = tokio::join!(
        pump_halves(
            &mut rx,
            &mut tx,
            &payload,
            "drop-before",
            PUMP_TIMEOUT,
            PumpOpts {
                idle_budget: Some(before_drop_for),
                tail_grace: Some(Duration::from_secs(1)),
                ..paced(OUTAGE_PHASE, load)
            },
        ),
        outage,
    );
    pumped?;
    outage?;

    // Drop both halves without `shutdown()`: the session handle goes away and nothing tells the
    // session manager.
    let before_drop = sampler.now();
    drop(rx);
    drop(tx);
    tracing::info!("session dropped without close");
    tokio::time::sleep(AFTER_DROP).await;
    let end = sampler.now();
    drop(thawed);
    let trace = sampler.stop().await?;
    trace.maybe_write_csv("dropping_a_session_should_stop_its_balancer");
    require_observable(&trace, "drop")?;

    let before = trace.window(
        before_drop.saturating_sub(Duration::from_secs(5)),
        before_drop,
    );
    let after = trace.window(before_drop + DROP_GRACE, end);
    let produced_after = after.produced_delta().unwrap_or_default();
    let state_after = after.last_state();
    tracing::info!(
        produced_after,
        ?state_after,
        "before the drop: {} | after the drop: {}",
        before.summary(),
        after.summary(),
    );
    let state_after = state_after.ok_or_else(|| {
        anyhow::anyhow!(
            "drop: hopr_session_lifetime_state is absent, so whether the session outlived its \
             handle cannot be read"
        )
    })?;
    assert!(
        state_after != STATE_ACTIVE && produced_after <= MAX_SURBS_AFTER_DROP,
        "the dropped session is still {} with its balancer running ({produced_after} SURBs produced \
         in the {:?} after the drop, +{DROP_GRACE:?} grace) — HoprSession has no Drop that tells the \
         session manager",
        if state_after == STATE_ACTIVE {
            "Active"
        } else {
            "not Active"
        },
        AFTER_DROP - DROP_GRACE,
    );
    Ok(())
}

/// Longest acceptable silence on the downlink while the uplink has headroom for the data itself.
const MAX_SHAPED_STALL: Duration = Duration::from_secs(2);

/// Arrival both arms must reach on a link that fits the data.
const MIN_SHAPED_ARRIVAL_PCT: f64 = 95.0;

/// SURB upstream for the control arm: 2 Mb/s, a sixth of what the client ships with.
const CONTROL_SURB_UPSTREAM_BITS: u64 = 2_000_000;

/// How long each arm pumps.
const ARM_DURATION: Duration = Duration::from_secs(60);

/// When scenario 4 runs with a relay outage (`SURB_RELAY_OUTAGE` set), how far into each run the
/// outage starts: after the initial fill, so the two effects are not mixed.
const SHAPED_OUTAGE_START: Duration = Duration::from_secs(20);

/// Open one run, pump it (with the relay outage, if any), close it, and require its balancer to go
/// quiet before the next run.
async fn shaped_arm(
    env: &IntegrationEnv,
    name: &str,
    phase: u8,
    cfg: SurbBalancerConfig,
    plan: Option<OutagePlan>,
    keep_up: Option<&[Address]>,
) -> anyhow::Result<(Transfer, Trace, Vec<Address>)> {
    let (session, exit) = env
        .open_unreliable_session_with_surbs(1, 1, Some(cfg))
        .await?;
    let candidates = env.relayer_candidates(exit)?;
    let session_id = AsRef::<str>::as_ref(session.id()).to_owned();
    let (mut rx, mut tx) = tokio::io::split(session);
    let sampler = Sampler::start(SAMPLE_EVERY).for_session(session_id);
    let load = plan.map_or(OFFERED_MBPS, |p| p.load_mbps);
    let forwarded_before = relayers::sample(&candidates).await;
    let outage = async {
        let Some(plan) = plan else {
            return anyhow::Ok(Vec::new());
        };
        tokio::time::sleep(SHAPED_OUTAGE_START).await;
        let spread = relayers::spread(&forwarded_before, &relayers::sample(&candidates).await);
        let (down, up) = match keep_up {
            // Keep the same relays up as in the first run, so the two runs differ only in the
            // SURB rate limit and not in which path survives the outage.
            Some(keep) => (
                candidates
                    .iter()
                    .filter(|n| !keep.contains(&n.address))
                    .cloned()
                    .collect::<Vec<_>>(),
                keep.to_vec(),
            ),
            None => plan.pick(&candidates, &spread),
        };
        anyhow::ensure!(!down.is_empty(), "{name}: the outage takes no relay down");
        tracing::info!(?up, "{name}: outage begins: {}", plan.describe());
        // Resumes the relays even if this future is dropped half-way.
        let _thawed = Thawed(&down);
        plan.run(&down).await?;
        Ok(up)
    };
    let payload = tagged_payload(phase, bytes_for(load, ARM_DURATION));
    let (transfer, outage) = tokio::join!(
        pump_halves(
            &mut rx,
            &mut tx,
            &payload,
            name,
            PUMP_TIMEOUT,
            PumpOpts {
                // Unchanged from `paced` without an outage; long enough to outlast one with it.
                idle_budget: Some(
                    plan.map_or(Duration::ZERO, |p| p.duration)
                        .max(Duration::from_secs(20)),
                ),
                tail_grace: Some(
                    plan.map_or(Duration::ZERO, |p| p.longest_down())
                        .max(Duration::from_secs(10)),
                ),
                ..paced(phase, load)
            },
        ),
        outage,
    );
    let transfer = transfer?;
    let up = outage?;
    // Close, not drop, so the next arm is not measured against this one's leftover balancer.
    let _ = tx.shutdown().await;
    drop(rx);
    drop(tx);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let quiet_from = sampler.now().saturating_sub(Duration::from_secs(3));
    let trace = sampler.stop().await?;
    trace.maybe_write_csv(&format!("shaped_{name}"));
    let leftover = trace.after(quiet_from).produced_delta().unwrap_or_default();
    anyhow::ensure!(
        leftover <= MAX_SURBS_AFTER_DROP,
        "{name}: the closed session kept minting ({leftover} SURBs in its last 3 s), so the arms \
         cannot be isolated — see dropping_a_session_should_stop_its_balancer",
    );
    Ok((transfer, trace, up))
}

/// The entry uplink's shaped rate, after checking the shaper sits on this binary's entry port and
/// leaves headroom over the data. Call after any `request_cluster_size`: the port depends on it.
fn shaped_uplink_mbit(data_mbps: f64) -> anyhow::Result<f64> {
    let mbit: f64 = std::env::var("EDGE_UPLINK_SHAPED_MBIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "EDGE_UPLINK_SHAPED_MBIT is not set: shape the entry uplink first with \
                 `sudo bash scripts/shape-edge-uplink.sh up <mbit> {}` (the `just` recipe \
                 reads its state file). Unshaped, loopback is never the bottleneck and this scenario \
                 measures nothing.",
                first_edge_p2p_port(),
            )
        })?;
    let (first, last) = shaped_ports();
    anyhow::ensure!(
        first == first_edge_p2p_port(),
        "the shaper starts at port {first} but this binary's first edgli will listen on {} — re-run \
         the shaper with that port",
        first_edge_p2p_port(),
    );
    anyhow::ensure!(
        last > first,
        "the shaper covers only port {first}, but every entry this binary boots takes the next \
         port — re-run the shaper (it now covers a range of ports by default)",
    );
    let data_mbit = data_mbps * 8.0;
    anyhow::ensure!(
        mbit >= 2.0 * data_mbit,
        "a {mbit} Mbit/s uplink leaves no headroom over the {data_mbit} Mbit/s of data; shape it to \
         at least {} Mbit/s",
        2.0 * data_mbit,
    );
    Ok(mbit)
}

/// The shaped source ports, first and last (inclusive), from the shaper's state file.
fn shaped_ports() -> (u16, u16) {
    let read = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u16>().ok());
    let first = read("EDGE_UPLINK_PORT").unwrap_or_default();
    (first, read("EDGE_UPLINK_PORT_LAST").unwrap_or(first))
}

/// Check that the entry booted last — the one this run is about to measure — sends through the
/// shaper. Every boot takes the next port, so a run with a fresh entry is on a new port.
fn require_shaped_entry(name: &str) -> anyhow::Result<()> {
    let port = last_edge_p2p_port();
    let (first, last) = shaped_ports();
    anyhow::ensure!(
        (first..=last).contains(&port),
        "{name}: the entry listens on port {port}, outside the shaped ports {first}–{last}, so its \
         uplink is not limited — re-run the shaper with more ports",
    );
    Ok(())
}

/// The client's balancer config, and the same capped to a sixth of its SURB upstream.
fn shaped_arm_configs() -> (SurbBalancerConfig, SurbBalancerConfig) {
    let production = gnosis_vpn_client_surb_config();
    let control = SurbBalancerConfig {
        max_surbs_per_sec: CONTROL_SURB_UPSTREAM_BITS / (8 * SURB_SIZE as u64),
        ..production
    };
    (production, control)
}

/// 4. The user-visible symptom: with the entry's uplink shaped to a rate that comfortably fits the
///    data, does the client's SURB budget stall the downlink? A capped-budget control arm on the
///    same link proves the link itself is not the cause.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires a shaped entry uplink (scripts/shape-edge-uplink.sh) + a chain"]
async fn shaped_uplink_should_not_stall_downstream() -> anyhow::Result<()> {
    // Before `shaped_uplink_mbit`: the entry's port, which the shaper must match, depends on it.
    request_cluster_size(CLUSTER_NODES);
    let template = OutagePlan {
        pattern: Pattern::Down,
        relays: RelaysDown::AllButOne,
        load_mbps: OFFERED_MBPS,
        duration: Duration::from_secs(20),
    };
    // No outage unless asked for: by default the scenario measures the uplink alone.
    let plan = OutagePlan::configure(None, template)?;
    let mbit = shaped_uplink_mbit(plan.map_or(OFFERED_MBPS, |p| p.load_mbps))?;
    let env = IntegrationEnv::setup().await?;
    require_shaped_entry("shaped uplink")?;
    let (production, control) = shaped_arm_configs();
    if let Some(plan) = plan {
        tracing::info!("shaped uplink with a relay outage: {}", plan.describe());
    }

    // Control first: if it leaks, it leaks at a sixth of the rate into the production arm.
    let (ctl, ctl_trace, ctl_up) =
        shaped_arm(&env, "control", CONTROL_PHASE, control, plan, None).await?;
    let (prod, prod_trace, _) = shaped_arm(
        &env,
        "production",
        PRODUCTION_PHASE,
        production,
        plan,
        Some(&ctl_up),
    )
    .await?;
    for (name, t, trace) in [
        ("control", &ctl, &ctl_trace),
        ("production", &prod, &prod_trace),
    ] {
        tracing::info!(
            arm = name,
            uplink_mbit = mbit,
            arrival_pct = t.arrival_pct(),
            ttfb_s = t.time_to_first_byte(),
            longest_stall_s = t.longest_stall(),
            p95_gap_s = t.inter_arrival_quantile(0.95),
            "{}",
            trace.after(FILL_GRACE).summary(),
        );
    }

    // With an outage, fixed limits cannot be met by any run: compare the run with the full SURB
    // rate limit against the control run under the same outage instead.
    if let Some(plan) = plan {
        anyhow::ensure!(
            ctl.arrival_pct() > 50.0,
            "inconclusive: the outage itself broke the path in the control run ({:.1}% arrived)",
            ctl.arrival_pct(),
        );
        // A stall as long as the outage comes from the outage itself, not from the SURB rate.
        let allowed_stall = ctl.longest_stall().max(plan.longest_down().as_secs_f64())
            + MAX_SHAPED_STALL.as_secs_f64();
        assert!(
            prod.longest_stall() <= allowed_stall
                && prod.arrival_pct() >= ctl.arrival_pct() - MAX_ARRIVAL_DEFICIT_PCT,
            "on the same {mbit} Mbit/s uplink and the same outage, the session with the full SURB \
             rate limit lost more data than the one with the lower limit: {:.1}% arrived, longest \
             stall {:.1}s (allowed {allowed_stall:.1}s; lower limit: {:.1}%, {:.1}s)",
            prod.arrival_pct(),
            prod.longest_stall(),
            ctl.arrival_pct(),
            ctl.longest_stall(),
        );
        return Ok(());
    }

    anyhow::ensure!(
        ctl.arrival_pct() >= MIN_SHAPED_ARRIVAL_PCT
            && ctl.longest_stall() <= MAX_SHAPED_STALL.as_secs_f64(),
        "the capped-budget control arm was unhealthy too ({:.1}% back, {:.1}s stall) — the shaped \
         link itself is too tight to attribute anything to the SURB budget",
        ctl.arrival_pct(),
        ctl.longest_stall(),
    );
    // A stall that starts the transfer is the initial fill (2026-10-07: 5.1 s to first byte against
    // 0.07 s for the control arm); one later on is a refill.
    let prod_ttfb = prod.time_to_first_byte().unwrap_or_default();
    let cause = if prod_ttfb >= prod.longest_stall() - 0.1 {
        "the initial fill to target runs at the budget and queues the first data behind it"
    } else {
        "SURB refills congest the entry's own uplink"
    };
    assert!(
        prod.arrival_pct() >= MIN_SHAPED_ARRIVAL_PCT
            && prod.longest_stall() <= MAX_SHAPED_STALL.as_secs_f64(),
        "with the client's SURB budget the downlink stalled on a {mbit} Mbit/s uplink: {:.1}% back, \
         longest stall {:.1}s, first byte after {prod_ttfb:.1}s (control arm: {:.1}%, {:.1}s) — \
         {cause}",
        prod.arrival_pct(),
        prod.longest_stall(),
        ctl.arrival_pct(),
        ctl.longest_stall(),
    );
    Ok(())
}

/// Warm-up before the relayers start flapping: long enough for every return pair to deliver once,
/// so the detector treats their silence as "stopped working", and for the initial fill to finish.
const FLAP_WARMUP: Duration = Duration::from_secs(20);

/// Default outage length for scenario 6.
const FLAP_DURATION: Duration = Duration::from_secs(45);

/// Default for scenario 6: relays down longer than the entry's 2 s `max_frame_age` and the
/// detector's five silent 1 s flushes, so each down period is loss to the session and silence to
/// the detector, while the relays come back often enough to stay path candidates. 2 s / 1 s
/// (2026-10-07) was only delay: both runs got everything back.
const FLAP_DOWN: Duration = Duration::from_secs(6);
const FLAP_UP: Duration = Duration::from_secs(2);

/// How much longer the production arm may stall than the control arm before the budget is blamed.
const MAX_EXTRA_STALL: Duration = Duration::from_secs(2);

/// How many percentage points of arrival the production arm may lose against the control arm.
const MAX_ARRIVAL_DEFICIT_PCT: f64 = 5.0;

/// Full-budget episodes allowed in one disturbance: one burst is a reaction, more is a loop.
const MAX_BUDGET_EPISODES: usize = 1;

/// One run of scenario 6: warm a 0-hop-out / 1-hop-back session, then run the relay outage while
/// pumping. Returns the transfer during the outage and the balancer trace of that window.
async fn flapping_arm(
    env: &IntegrationEnv,
    name: &str,
    phase: u8,
    cfg: SurbBalancerConfig,
    plan: OutagePlan,
    keep_up: Option<&[Address]>,
) -> anyhow::Result<(Transfer, Trace, Vec<Address>)> {
    // Each run gets a fresh entry on the next port: make sure the shaper covers it too.
    require_shaped_entry(name)?;
    let (session, exit) = env
        .open_unreliable_session_with_surbs(0, 1, Some(cfg))
        .await?;
    let candidates = env.relayer_candidates(exit)?;
    anyhow::ensure!(
        candidates.len() >= 2,
        "{name}: need ≥2 return relayer candidates, got {}",
        candidates.len()
    );
    let session_id = AsRef::<str>::as_ref(session.id()).to_owned();
    let (mut rx, mut tx) = tokio::io::split(session);
    let sampler = Sampler::start(SAMPLE_EVERY).for_session(session_id);

    let forwarded_before = relayers::sample(&candidates).await;
    let warmup = pump_halves(
        &mut rx,
        &mut tx,
        &tagged_payload(WARMUP_PHASE, bytes_for(OFFERED_MBPS, FLAP_WARMUP)),
        &format!("{name}-warmup"),
        PUMP_TIMEOUT,
        paced(WARMUP_PHASE, OFFERED_MBPS),
    )
    .await?;
    anyhow::ensure!(
        warmup.arrival_pct() > 50.0,
        "{name}: warm-up only returned {:.1}% — the path was broken before the flapping",
        warmup.arrival_pct(),
    );
    drain_until_quiet(&mut rx, DRAIN_QUIET, &format!("{name}-warmup")).await;
    let spread = relayers::spread(&forwarded_before, &relayers::sample(&candidates).await);
    let (down, up) = match keep_up {
        // Keep the same relays up as in the first run, so the two runs differ only in the SURB
        // rate limit and not in which return path survives the outage.
        Some(keep) => (
            candidates
                .iter()
                .filter(|n| !keep.contains(&n.address))
                .cloned()
                .collect::<Vec<_>>(),
            keep.to_vec(),
        ),
        None => plan.pick(&candidates, &spread),
    };
    anyhow::ensure!(!down.is_empty(), "{name}: the outage takes no relay down");

    let thawed = Thawed(&down);
    let payload = tagged_payload(phase, bytes_for(plan.load_mbps, plan.duration));
    let outage_from = sampler.now();
    let (transfer, periods) = tokio::join!(
        pump_halves(
            &mut rx,
            &mut tx,
            &payload,
            name,
            PUMP_TIMEOUT,
            PumpOpts {
                idle_budget: Some(plan.duration),
                tail_grace: Some(plan.longest_down().max(Duration::from_secs(10))),
                ..paced(phase, plan.load_mbps)
            },
        ),
        plan.run(&down),
    );
    let (transfer, periods) = (transfer?, periods?);
    drop(thawed);
    let outage_to = sampler.now();

    // Close, not drop, so the next arm is not measured against this one's leftover balancer.
    let _ = tx.shutdown().await;
    drop(rx);
    drop(tx);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let trace = sampler.stop().await?;
    trace.maybe_write_csv(&format!("shaped_outage_{name}"));
    require_observable(&trace, name)?;
    tracing::info!(
        arm = name,
        ?up,
        down = down.len(),
        down_periods = periods,
        "{name}: {}",
        plan.describe(),
    );
    Ok((transfer, trace.window(outage_from, outage_to), up))
}

/// 6. The feedback loop: on a shaped uplink, with return relays going down intermittently, does the
///    client's SURB budget keep returning to the maximum and hurt the data more than a capped
///    budget does on the same link and the same outage?
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires a shaped entry uplink (scripts/shape-edge-uplink.sh) + a chain"]
async fn shaped_outage_should_not_loop() -> anyhow::Result<()> {
    let size = request_cluster_size(CLUSTER_NODES);
    anyhow::ensure!(size >= 3, "the shaped outage needs ≥3 nodes, got {size}");
    let default = OutagePlan {
        pattern: Pattern::Intermittent {
            down: FLAP_DOWN,
            up: FLAP_UP,
        },
        relays: RelaysDown::AllButOne,
        load_mbps: OFFERED_MBPS,
        duration: FLAP_DURATION,
    };
    let plan = OutagePlan::configure(Some(default), default)?.ok_or_else(|| {
        anyhow::anyhow!("shaped outage: SURB_RELAY_OUTAGE=none leaves nothing to test")
    })?;
    let mbit = shaped_uplink_mbit(plan.load_mbps)?;
    let (production, control) = shaped_arm_configs();
    tracing::info!(uplink_mbit = mbit, "shaped outage: {}", plan.describe());

    // A fresh entry per arm: the planner's weights and re-plan cooldowns live in the entry, and the
    // 2026-10-07 run showed the control arm's re-plan leaving the production arm's re-plans with
    // nothing to move (`entries=0`), so it never reached degraded mode at all.
    // `SURB_RUN_ORDER=full-first` runs the full rate limit first, to tell an effect of the rate
    // limit apart from an effect of running second on relays that were just paused and resumed.
    let full_first = match std::env::var("SURB_RUN_ORDER").as_deref() {
        Err(_) | Ok("") | Ok("control-first") => false,
        Ok("full-first") => true,
        Ok(other) => anyhow::bail!(
            "SURB_RUN_ORDER={other}: expected `control-first` (default) or `full-first`"
        ),
    };
    tracing::info!(full_first, "shaped outage: run order");
    let ((ctl, ctl_trace), (prod, prod_trace)) = if full_first {
        let (prod, prod_trace, prod_up) = {
            let env = IntegrationEnv::setup().await?;
            flapping_arm(&env, "production", PRODUCTION_PHASE, production, plan, None).await?
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (ctl, ctl_trace, _) = {
            let env = IntegrationEnv::setup().await?;
            flapping_arm(
                &env,
                "control",
                CONTROL_PHASE,
                control,
                plan,
                Some(&prod_up),
            )
            .await?
        };
        ((ctl, ctl_trace), (prod, prod_trace))
    } else {
        let (ctl, ctl_trace, ctl_up) = {
            let env = IntegrationEnv::setup().await?;
            flapping_arm(&env, "control", CONTROL_PHASE, control, plan, None).await?
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (prod, prod_trace, _) = {
            let env = IntegrationEnv::setup().await?;
            flapping_arm(
                &env,
                "production",
                PRODUCTION_PHASE,
                production,
                plan,
                Some(&ctl_up),
            )
            .await?
        };
        ((ctl, ctl_trace), (prod, prod_trace))
    };

    let ctl_budget = control.max_surbs_per_sec as f64;
    let budget = production.max_surbs_per_sec as f64;
    for (name, t, trace, b) in [
        ("control", &ctl, &ctl_trace, ctl_budget),
        ("production", &prod, &prod_trace, budget),
    ] {
        tracing::info!(
            arm = name,
            uplink_mbit = mbit,
            arrival_pct = t.arrival_pct(),
            longest_stall_s = t.longest_stall(),
            p95_gap_s = t.inter_arrival_quantile(0.95),
            seconds_at_budget = trace.seconds_at_budget(b),
            budget_episodes = trace.episodes_at_budget(b),
            degraded_episodes = trace.degraded_episodes(b),
            "{}",
            trace.summary(),
        );
    }

    let episodes = prod_trace.episodes_at_budget(budget);
    let degraded = prod_trace.degraded_episodes(budget);
    let seconds = prod_trace.seconds_at_budget(budget);
    // The user-visible harm, judged against the same outage at a capped budget — only when the
    // control arm still carried data; an outage that takes everything down leaves only the loop
    // check below.
    if ctl.arrival_pct() <= 50.0 {
        tracing::warn!(
            control_arrival_pct = ctl.arrival_pct(),
            "shaped outage: the outage itself broke the path; harm is not compared, only the loop"
        );
    } else {
        // A stall as long as one down period comes from the outage itself, in both runs, so it
        // is not counted as harm; only a stall longer than both is.
        let allowed_stall = ctl.longest_stall().max(plan.longest_down().as_secs_f64())
            + MAX_EXTRA_STALL.as_secs_f64();
        assert!(
            prod.longest_stall() <= allowed_stall
                && prod.arrival_pct() >= ctl.arrival_pct() - MAX_ARRIVAL_DEFICIT_PCT,
            "on the same {mbit} Mbit/s uplink and the same outage, the session with the full SURB \
             rate limit lost more data than the one with the lower limit: {:.1}% arrived, longest \
             stall {:.1}s (allowed {allowed_stall:.1}s; lower limit: {:.1}%, {:.1}s). SURB output \
             reached the {budget:.0} SURB/s rate limit {episodes} times ({seconds:.1}s, \
             {degraded} of them in degraded mode); the extra SURB packets delay the data on the uplink",
            prod.arrival_pct(),
            prod.longest_stall(),
            ctl.arrival_pct(),
            ctl.longest_stall(),
        );
    }
    // The loop, even when the data survives it.
    assert!(
        episodes <= MAX_BUDGET_EPISODES,
        "SURB production returned to the {budget:.0} SURB/s budget {episodes} times during one \
         {}s outage ({seconds:.1}s in total, {degraded} of them degraded mode) — each correction or \
         degraded episode feeds the congestion that causes the next",
        plan.duration.as_secs(),
    );
    Ok(())
}
