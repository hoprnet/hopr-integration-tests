//! Does a PIX Session survive losing half the relays that carry its shares home?
//!
//! # The question
//!
//! Entry, two relays, Exit, channels both ways, so return paths are drawn over both relays about
//! evenly. One relay dies mid-cycle. The Session has lost 50 % of its return relayers — can the
//! funded cycle still be reconstructed, and does the Exit still get paid?
//!
//! It is a different question from `tests/return_path.rs`, which asks whether a *stream* survives
//! the same event and is satisfied by it degrading and recovering. PIX is not satisfied by that. A
//! cycle pays only if **every** polynomial reconstructs, because the SSA is the sum of all their
//! constant terms, and the deposit's address derives from both sides' commitments — so a cycle
//! that falls one share short of one polynomial out of 512 strands money that has already been
//! paid. There is no partial credit and no refund.
//!
//! # What the answer turned out to depend on
//!
//! Not on the loss *rate* the surplus is documented against. The relevant arithmetic, derived in
//! [`relay_loss`] and summarised here:
//!
//! * the damage is one **burst**, not a rate: the shares bound to SURBs naming the dead relay, plus
//!   the ones naming the *surviving* relay that the Entry's re-plan throws away when its generation
//!   bump makes the Exit clear its whole buffer, plus whatever is spent into the void during the
//!   ~8 s the silence takes to detect;
//! * emission is round-robin over 512 polynomials, so a burst of `L` costs each of them `L / 512`,
//!   and each can afford `surplus`. **A cycle survives a burst of `512 x surplus`.** (The window was
//!   256 until hoprnet's window PR, hoprnet#8585, and the suite assumes the new one; against a
//!   hopr-lib still at 256 its tolerances are twice what a cycle survives.)
//!
//! So the parameter that decides this is `additional_shares`, and it has to be sized against the
//! Exit's *SURB buffer depth* rather than against a packet-loss percentage. The profile picks 32
//! against a buffer of 2400 and a rate of 150/s: 16 384 tolerated against ~3000 expected.
//!
//! One further parameter is not about PIX at all and is just as load-bearing: the Entry's
//! balancer has to learn that the Exit emptied. Left to `produced - consumed` it reads the silence
//! as a full counterparty buffer and stops producing (hoprnet#8490), so a cycle that survives the
//! burst starves immediately afterwards. The client's opt-in for that (GNO-713) is not named in
//! this suite; see [`relay_loss`] for what replaces it.
//!
//! # What was measured
//!
//! On the stack of 2026-10-17: four runs, four passes, every one landing on `useful = 512 × 64`
//! exactly and crediting the Exit's Safe with one whole deposit — including at the shipped surplus
//! of 16. The burst at a test cluster's buffer depth is smaller than even the shipped tolerance,
//! so 32 buys margin rather than survival *here*, and the generation bump fired in only one of the
//! four.
//!
//! On a prototype of hoprnet#8576 (2026-10-27), at the window of 256, the wipe became deterministic,
//! and so did its price: a cycle survives it iff `window x surplus` covers the Exit's stock. Surplus
//! 32 and 16 pass, surplus 8 ends in `recovery_idle` (the negative control), and at production
//! dimensions (4096 x (32 + 79)) a 7000-SURB and a 19 264-SURB stock were both wiped, swept and
//! paid, the second with three shares of margin (41 at 512). `docs/pix-relay-loss.md` has the
//! figures.
//!
//! At 512 the same stock costs half as much per polynomial, so surplus 8 (4096) now survives the
//! 2400-SURB wipe, and the negative control is surplus 4 (2048): `HOPRD_PIX_ADDITIONAL_SHARES=4`,
//! expected to end in `recovery_idle`. It has not been run at 512.
//!
//! # Running
//!
//! ```text
//! LINE=v5 HOPRD_SRC=../hoprd just pix-relay-loss                      # the three scenarios
//! HOPRD_PIX_ADDITIONAL_SHARES=16 LINE=v5 … just pix-relay-loss        # the shipped surplus
//! HOPRD_PIX_ADDITIONAL_SHARES=4  LINE=v5 … just pix-relay-loss        # the negative control
//! LINE=v5 … just pix-relay-loss a_pix_cycle_survives_a_deep_return_relay_death_and_the_generation_wipe
//! ```
//!
//! Manual, never in CI, and **one cluster per scenario**: each kills a cluster node, so the
//! cluster cannot be shared with anything after it. The recipe gives each scenario an invocation
//! of its own on one chain; name one with `SCENARIOS=` when driving `run-binchain.sh` directly.

#![cfg(feature = "v5")]

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::Context as _;
use hoprd_integration_test::{
    Address, IntegrationEnv,
    cluster::{self, NodeInfo},
    pix::{self, PixCounters},
    pix_exit::{self, ExitTelemetry},
    relay_loss,
    relayers::{self, RelayerSpread},
};

/// Which direction of the Session is put on a relay, and therefore what the kill removes.
///
/// The two scenarios differ only in this. Everything else — the geometry, the pacing, the kill
/// point, the assertions — is shared, so a difference in outcome between them is a difference in
/// the protocol rather than in how it was measured.
#[derive(Clone, Copy, Debug)]
struct Topology {
    /// Names the shape in log lines and failure messages.
    name: &'static str,
    forward_hops: usize,
    /// At least 1: the share is encrypted under the first *return* relayer's acknowledgement
    /// solution, so a direct return path carries no share and no cycle can advance over it.
    /// Two for [`RETURN_TWO_HOP`], whose paths have a relay at hop two that the Exit cannot see.
    return_hops: usize,
    /// Relay candidates the cluster offers besides the Exit. Two for one-hop return paths, so that
    /// killing one is killing half; three for two-hop ones, so that a two-hop path survives the kill.
    relays: usize,
}

/// Both directions over a relay — what a deployment runs.
///
/// The kill takes out a relay that was carrying the Session *both* ways, so the forward path loses
/// half its capacity at the same instant. Measured consequence: every leg goes silent together for
/// a second or two, which is precisely the condition the silence detector refuses to act on — it
/// needs a sibling pair to the same destination still delivering before it will call a relay dead.
/// So this shape recovers on the path cache's own re-weighting and the generation bump never
/// fires.
const SYMMETRIC: Topology = Topology {
    name: "symmetric",
    forward_hops: 1,
    return_hops: 1,
    relays: 2,
};

/// Requests go direct, replies over a relay.
///
/// The harder shape for PIX and the reason it is here. With nothing relayed outbound the forward
/// path is untouched by the kill, so one reply leg keeps delivering while the other goes silent —
/// the detector corroborates, the Entry re-plans and bumps its SURB generation, and the Exit
/// discards its **whole** buffer rather than only the half bound for the dead relay. That is the
/// largest term in the burst and [`SYMMETRIC`] never reaches it.
///
/// It also makes the relayer histogram unambiguous: every forwarded packet is a reply, so the
/// split being measured before the kill is the split of the return stream and nothing else.
const RETURN_ONLY: Topology = Topology {
    name: "return-only",
    forward_hops: 0,
    return_hops: 1,
    relays: 2,
};

/// Two-hop return paths over three relays, forward path direct. Killing one relay leaves SURBs the
/// Exit cannot recognise as dead: it sees only a SURB's first hop, so those with the victim at hop
/// two stay in its store. This is the case the Entry's dead-verdict generation bump exists for, and
/// the bump wipes the Exit's whole stock for the Session, shares included -- so this is also the
/// scenario that measures whether a cycle survives that wipe rather than the relay death itself.
const RETURN_TWO_HOP: Topology = Topology {
    name: "return-two-hop",
    forward_hops: 0,
    return_hops: 2,
    relays: 3,
};

// Cluster size is `1 + topology.relays`: the Exit plus its relay candidates. Two candidates make
// killing one killing half of them -- with four a death would cost a quarter of the return stream
// and the planner would have three places to go, and the question asked here is the one where the
// survivor has to carry everything.

/// Bytes per write, under `SESSION_MTU` so one write is one packet.
const CHUNK: usize = 900;

/// Send pace — the profile's packet rate as a per-chunk delay.
fn pace() -> Duration {
    Duration::from_micros(1_000_000 / relay_loss::target_packet_rate())
}

/// Byte the offered payload is filled with, so the echo can be checked for corruption.
///
/// A constant fill rather than a counter: the Session is `Segmentation`-only, so loss leaves holes
/// and reordering is legal, and a sequence would report both as damage. What this catches is the
/// failure that would matter — bytes coming back that were never sent.
const FILL: u8 = 0xCD;

/// Fraction of a cycle that must be behind the Exit before the relay is killed.
///
/// Far enough in that the cycle is unambiguously funded and under way, and far enough from the end
/// that the burst lands where it can still do damage: a cycle killed at 95 % has already emitted
/// almost every share and would survive on almost any surplus. It is also well clear of the 85 %
/// early-recovery signal, so no successor exists yet — the cycle under test is the only one the
/// Session has, and if it dies the Session dies with it rather than being carried by a sibling.
const KILL_AT_FRACTION: f64 = 0.25;

/// Smallest share of the forwarded traffic the *least-busy* relay must carry before the kill.
///
/// The non-vacuity guard, and the one this scenario cannot do without. If the Entry had already
/// concentrated its return paths on one relay, killing the busiest would remove ~100 % (nothing to
/// survive on) or ~0 % (nothing was lost) — and either way the run would say nothing about losing
/// *half* the relayers. With two candidates drawn independently the expected split is 50/50, so 30
/// % is a loose floor that still excludes a collapsed distribution.
const MIN_RELAYER_SHARE: f64 = 0.3;

/// How long the warm-up may take to put [`KILL_AT_FRACTION`] of a cycle behind the Exit.
///
/// Generous against the nominal figure, because the achieved cycle runs longer than nominal on a
/// local cluster. Exceeding it means the Session never got going, which is a setup failure and is
/// reported as one rather than being killed into.
fn warmup_budget() -> Duration {
    Duration::from_secs(2 * relay_loss::nominal_cycle_secs() + 120)
}

/// How long the cycle gets to recover and be swept, measured from the start of the traffic.
///
/// Three nominal cycles plus three minutes. The achieved cycle runs ~1.3x nominal here, and the
/// outage adds the detection window plus however long the open-loop refill takes to put the Exit's
/// buffer back — so the slack is for the failure under test, not for the cluster.
fn sweep_budget() -> Duration {
    Duration::from_secs(3 * relay_loss::nominal_cycle_secs() + 180)
}

/// Poll cadence while waiting on the Exit.
const POLL: Duration = Duration::from_secs(5);

/// How long to keep watching the Exit's Safe after its counters say it swept.
///
/// A sweep is two on-chain round trips behind the reply that completed the cycle.
const PAYMENT_SETTLE: Duration = Duration::from_secs(180);

/// Resolve the `NodeInfo` for an address, so its Prometheus endpoint can be read.
fn node_for(env: &IntegrationEnv, address: Address) -> anyhow::Result<NodeInfo> {
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == address)
        .cloned()
        .with_context(|| format!("no cluster node with address {address}"))
}

/// Shares the Exit has accepted for this scenario, useful and surplus together.
///
/// Both kinds, because the question is how far the *cycle* has got and a polynomial's surplus
/// counts toward its emission exactly as its threshold shares do. `None` from either accessor is a
/// label that has not materialised yet, which at the start of a run is the ordinary case.
fn shares_accepted(delta: &ExitTelemetry) -> u64 {
    delta.shares("useful").unwrap_or(0) + delta.shares("surplus").unwrap_or(0)
}

/// Offer `CHUNK`-sized writes at [`pace`] until `run_for` elapses or `stop` is set.
///
/// The Exit's loopback echoes every byte, so one chunk offered is one reply packet and therefore
/// one share: this is what drives the cycle. Written directly rather than through `pump::*`
/// because the pump's reader decides completion against what was sent, and under the loss this
/// scenario inflicts it would declare the transfer over while the cycle still needs traffic.
async fn offer_for(
    tx: &mut tokio::io::WriteHalf<hoprd_integration_test::HoprSession>,
    run_for: Duration,
    stop: &AtomicBool,
) -> anyhow::Result<u64> {
    use tokio::io::AsyncWriteExt as _;

    let deadline = std::time::Instant::now() + run_for;
    let payload = vec![FILL; CHUNK];
    let mut offered = 0u64;
    while std::time::Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        // A write that cannot complete must not outlive the stop flag: after the kill the forward
        // path is down to one relay too, and a blocked write would hold the join open long past
        // the point the scenario has its answer.
        match tokio::time::timeout(Duration::from_secs(10), tx.write_all(&payload)).await {
            Ok(Ok(())) => offered += CHUNK as u64,
            Ok(Err(error)) => {
                tracing::warn!(%error, offered, "the Session stopped accepting writes");
                break;
            }
            Err(_) => tracing::warn!(offered, "a write stalled for 10 s; still offering"),
        }
        tokio::time::sleep(pace()).await;
    }
    let _ = tx.flush().await;
    Ok(offered)
}

/// Read and discard the echo until `run_for` elapses or `stop` is set.
async fn drain_for(
    rx: &mut tokio::io::ReadHalf<hoprd_integration_test::HoprSession>,
    run_for: Duration,
    stop: &AtomicBool,
) -> anyhow::Result<u64> {
    use tokio::io::AsyncReadExt as _;

    let deadline = std::time::Instant::now() + run_for;
    let mut buf = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    while std::time::Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(left.min(Duration::from_secs(5)), rx.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                if let Some(at) = buf[..n].iter().position(|b| *b != FILL) {
                    anyhow::bail!(
                        "the echo is corrupt {} bytes in: expected {FILL:#04x}, read {:#04x}",
                        received + at as u64,
                        buf[at]
                    );
                }
                received += n as u64;
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "drain: read failed");
                break;
            }
            // A quiet window is the outage, not a stall.
            Err(_) => continue,
        }
    }
    Ok(received)
}

/// What the outage was, as observed rather than as configured.
struct Outage {
    /// Who carried what over the warm-up — the evidence that the stream really was split.
    spread: RelayerSpread,
    /// The relay that was killed.
    victim: Address,
    /// Shares the Exit had accepted at the moment of the kill.
    shares_at_kill: u64,
    /// Seconds from the start of the traffic to the kill.
    killed_after: f64,
}

/// Wait for the cycle to get going, kill half the return relayers, then wait for the sweep.
///
/// Everything time-ordered about the scenario lives here so the traffic tasks stay dumb loops. It
/// sets `stop` on the way out, whatever happened, or the two of them would run out their full
/// budget after a setup failure.
async fn drive_outage(
    exit: &NodeInfo,
    candidates: &[NodeInfo],
    gate_before: &ExitTelemetry,
    strategy_before: &PixCounters,
) -> anyhow::Result<(Outage, PixCounters)> {
    let started = std::time::Instant::now();
    let relayers_before = relayers::sample(candidates).await;

    // ── wait until a funded cycle is a quarter of the way through ──
    let target = (relay_loss::cycle_packets() as f64 * KILL_AT_FRACTION) as u64;
    let warmup_deadline = started + warmup_budget();
    let shares_at_kill = loop {
        let now = pix_exit::sample(exit).await?;
        let delta = gate_before.delta(&now);
        let accepted = shares_accepted(&delta);
        // Absolute, not the delta: at production dimensions the deposit confirms within ~2 s of the
        // Session opening, before `gate_before` is sampled, and a delta then waits for the *successor*
        // cycle's funding at 85 % -- measured: a kill meant for 25 % landed at 83.5 %. One cluster per
        // scenario, so the Exit's counter is this Session's alone.
        let funded = now.cycles("funded").unwrap_or(0);
        if funded >= 1 && accepted >= target {
            break accepted;
        }
        anyhow::ensure!(
            std::time::Instant::now() < warmup_deadline,
            "the Session never put {target} shares of a funded cycle behind it within {:?} \
             (funded cycles {funded}, shares accepted {accepted}) — nothing was killed, so this \
             is a setup failure rather than a result about relayer loss: {}",
            warmup_budget(),
            delta.summary()
        );
        tracing::info!(
            accepted,
            target,
            funded,
            elapsed_s = started.elapsed().as_secs(),
            "warming up the cycle before the kill"
        );
        tokio::time::sleep(POLL).await;
    };

    // ── who is carrying it ──
    let spread = relayers::spread(&relayers_before, &relayers::sample(candidates).await);
    tracing::info!(
        shares_at_kill,
        cycle_packets = relay_loss::cycle_packets(),
        "return relayers over the warm-up → {} (total {} pkts)",
        spread.summary(),
        spread.total
    );
    anyhow::ensure!(
        spread.total > 0,
        "no cluster node forwarded anything, so the Session was not relayed at all and there is \
         no return relayer to kill"
    );
    let active = spread.active_relayers(MIN_RELAYER_SHARE);
    anyhow::ensure!(
        active.len() >= 2,
        "only {} of {} relayers carried at least {:.0}% of the traffic, so the return stream was \
         already concentrated and killing one does not remove half of it — {}",
        active.len(),
        candidates.len(),
        MIN_RELAYER_SHARE * 100.0,
        spread.summary()
    );

    let busiest = spread
        .busiest()
        .ok_or_else(|| anyhow::anyhow!("no relayer forwarded anything; nothing to kill"))?;
    let victim = candidates
        .iter()
        .find(|n| n.address == busiest)
        .ok_or_else(|| anyhow::anyhow!("busiest relayer {busiest} is not a cluster node"))?;

    tracing::warn!(
        victim = %busiest,
        share_pct = spread.max_share() * 100.0,
        shares_at_kill,
        progress_pct = 100.0 * shares_at_kill as f64 / relay_loss::cycle_packets() as f64,
        expected_burst = relay_loss::expected_burst(relay_loss::surb_buffer_target()),
        tolerated_burst = relay_loss::tolerated_burst(),
        "killing half the return relayers mid-cycle"
    );
    victim.kill()?;
    let killed_after = started.elapsed().as_secs_f64();

    // ── does the cycle still complete and pay ──
    let remaining = sweep_budget().saturating_sub(started.elapsed());
    let swept = await_sweeps(exit, strategy_before, remaining).await?;

    Ok((
        Outage {
            spread,
            victim: busiest,
            shares_at_kill,
            killed_after,
        },
        swept,
    ))
}

/// Poll the Exit until it has swept a cycle, or the budget expires.
///
/// Returns the delta either way rather than erroring on timeout: "timed out" says nothing about
/// whether the cycle was never recovered, recovered and never swept, or swept against a deposit
/// that never landed, and the assertions that follow are what name it.
async fn await_sweeps(
    exit: &NodeInfo,
    before: &PixCounters,
    budget: Duration,
) -> anyhow::Result<PixCounters> {
    let deadline = std::time::Instant::now() + budget;
    let mut delta = before.delta(&pix::sample_exit(exit).await?);
    while std::time::Instant::now() < deadline {
        delta = before.delta(&pix::sample_exit(exit).await?);
        if delta.sweeps().unwrap_or(0) >= 1 {
            tracing::info!(summary = %delta.summary(), "the Exit swept a cycle after the outage");
            return Ok(delta);
        }
        tracing::info!(
            remaining_s = deadline.saturating_duration_since(std::time::Instant::now()).as_secs(),
            summary = %delta.summary(),
            "waiting on the post-outage sweep"
        );
        tokio::time::sleep(POLL).await;
    }
    Ok(delta)
}

/// Assert the Exit's Safe actually received what its counters say it swept.
///
/// The counters alone cannot establish payment: `keys_recovered` and `sweeps` both increment on
/// the "already swept" path, where a key is recovered against a stealth address the deposit never
/// reached. A floor rather than an exact multiple, for the reason `tests/pix_shapes.rs` documents
/// — these nodes also relay, and `AutoRedeeming` puts winning tickets in the same Safe.
async fn assert_exit_was_paid(
    exit: &NodeInfo,
    before: &pix::NodeBalances,
    swept: u64,
) -> anyhow::Result<()> {
    let per_cycle = relay_loss::per_cycle()?;
    let target = per_cycle * swept.max(1);
    let deadline = std::time::Instant::now() + PAYMENT_SETTLE;
    let mut delta = pix::node_balances(exit).await?.safe - before.safe;
    while std::time::Instant::now() < deadline && delta < target {
        tokio::time::sleep(POLL).await;
        delta = pix::node_balances(exit).await?.safe - before.safe;
        tracing::info!(%delta, %target, "waiting for the Exit's Safe to receive the swept cycle");
    }

    let floor = per_cycle;
    tracing::info!(
        %delta, %floor, %per_cycle, swept,
        whole_cycles = ?pix::completed_cycles(delta, per_cycle),
        "the Exit's Safe over the scenario"
    );
    assert!(
        delta >= floor,
        "the Exit's counters claim {swept} swept cycle(s) but its Safe rose by only {delta}, \
         against {floor} for one cycle. A sweep counted without funds arriving is the 'already \
         swept' path — the key was recovered against a stealth address the deposit had not reached."
    );
    Ok(())
}

/// A PIX Session must survive losing half the relays carrying its shares home, and still be paid.
///
/// The headline scenario. Everything it asserts is about the *cycle*, not about the stream: a
/// return path that degrades and recovers is what `tests/return_path.rs` measures, and a PIX cycle
/// can be lost while the stream around it looks healthy — a starved polynomial never fails a
/// check, it simply never completes, and the Session then dies on a timeout naming the timer
/// rather than the cause.
///
/// # What each assertion rules out
///
/// * **The relayers were both carrying traffic before the kill.** Without it the scenario could
///   pass by removing nothing. Checked inside [`drive_outage`], before the kill, so a concentrated
///   stream fails as a setup error rather than as a result.
/// * **A cycle was swept.** The cycle in flight when the relay died reconstructed — every one of
///   its 512 polynomials reached threshold despite the burst.
/// * **The cycle in flight recovered.** One cycle recovered, one successor was requested behind it, and
///   none failed over the scenario. Without it a dead cycle passes: the Entry opens a replacement, the
///   replacement sweeps, and neither the Session's closures nor the sweep counter show that the cycle
///   that was in flight at the kill was lost.
/// * **The Safe rose.** The sweep moved money, rather than recovering a key against an address the
///   deposit never reached.
/// * **No supervisor closure.** A Session that was torn down and reopened could sweep a *later*
///   cycle and satisfy the two above; `recovery_idle` and `recovery_deadline` are what a starved
///   cycle actually produces, and both must be absent.
/// * **No deposit timed out.** Distinguishes a cycle lost to the outage from one lost because the
///   chain was slow, which at this geometry would otherwise look the same.
async fn a_pix_session_survives_losing_half_its_return_relayers(
    topology: Topology,
) -> anyhow::Result<()> {
    relay_loss::install_profile();
    cluster::request_cluster_size(1 + topology.relays);
    tracing::info!(
        topology = topology.name,
        forward_hops = topology.forward_hops,
        return_hops = topology.return_hops,
        "relay-loss scenario"
    );

    let env = IntegrationEnv::setup_pix_with(relay_loss::entry_config()?).await?;
    let (session, exit_addr) = env
        .open_pix_session_with(
            topology.forward_hops,
            topology.return_hops,
            relay_loss::surb_balancer(),
            hoprd_integration_test::SessionTarget::ExitNode(0),
        )
        .await?;
    let exit = node_for(&env, exit_addr)?;
    let candidates = env.relayer_candidates(exit_addr)?;
    anyhow::ensure!(
        candidates.len() == topology.relays,
        "this topology is about losing one of exactly {} return relayers, but the cluster offers {}",
        topology.relays,
        candidates.len()
    );
    tracing::info!(
        %exit_addr,
        relays = ?candidates.iter().map(|n| n.address).collect::<Vec<_>>(),
        surplus = relay_loss::additional_shares(),
        cycle_packets = relay_loss::cycle_packets(),
        expected_burst = relay_loss::expected_burst(relay_loss::surb_buffer_target()),
        tolerated_burst = relay_loss::tolerated_burst(),
        burst_margin = format!("{:.2}", relay_loss::burst_margin()),
        "relay-loss scenario ready"
    );

    let strategy_before = pix::sample_exit(&exit).await?;
    anyhow::ensure!(
        strategy_before.observable(),
        "the Exit exposes no hopr_strategy_pix_* counters — it was built without \
         `hopr-strategy/telemetry`, so nothing here can be measured"
    );
    let gate_before = pix_exit::sample(&exit).await?;
    anyhow::ensure!(
        gate_before.observable(),
        "the Exit exposes no hopr_pix_* series, so the cycle's progress cannot be followed and \
         the kill could not be timed against it"
    );
    let paid_before = pix::node_balances(&exit).await?;
    let sampler = pix_exit::Sampler::start(&exit);

    let (mut rx, mut tx) = tokio::io::split(session);
    let stop = AtomicBool::new(false);
    let budget = sweep_budget();

    let (offered, received, driven) = tokio::join!(
        offer_for(&mut tx, budget, &stop),
        drain_for(&mut rx, budget, &stop),
        async {
            let result = drive_outage(&exit, &candidates, &gate_before, &strategy_before).await;
            // Whatever happened, release the traffic loops rather than leaving them to run out a
            // twenty-minute budget behind a setup failure.
            stop.store(true, Ordering::Relaxed);
            result
        }
    );

    let offered = offered?;
    // The drain's error is a corrupt echo, which makes every reading below meaningless rather than
    // merely disappointing — so it propagates ahead of the counters.
    let received = received?;
    let (outage, swept) = driven?;

    let trace = sampler.finish().await;
    let gate = gate_before.delta(&pix_exit::sample(&exit).await?);
    tracing::info!(
        offered,
        received,
        arrival_pct = 100.0 * received as f64 / offered.max(1) as f64,
        victim = %outage.victim,
        killed_after_s = outage.killed_after,
        shares_at_kill = outage.shares_at_kill,
        shares_total = shares_accepted(&gate),
        spread_before_kill = %outage.spread.summary(),
        gate = %gate.summary(),
        trace = %trace.summary(),
        closures = ?gate.closure_reasons(),
        ring_share_evictions = gate.ring_share_evictions(),
        cycles_requested = ?gate.cycles("requested"),
        cycles_recovered = ?gate.cycles("recovered"),
        cycles_failed = ?gate.cycles("failed"),
        "relay-loss scenario finished"
    );

    // The supervisor's own verdict first: it names the cause, where the sweep counter only reports
    // the symptom. A starved cycle ends here, as `recovery_idle` once the Entry has emitted the
    // last share it has for a cycle it can no longer complete.
    assert_eq!(
        Vec::<(String, u64)>::new(),
        gate.closure_reasons(),
        "the PIX supervisor closed the Session during the outage. `recovery_idle` is what a \
         starved cycle produces: polynomials that lost more than their surplus never reach \
         threshold, the Entry eventually runs out of shares to emit, and the Exit gives up {:?} \
         later. At surplus {} the profile tolerates a burst of {} against an expected {}.",
        relay_loss::MAX_RECOVERY_IDLE,
        relay_loss::additional_shares(),
        relay_loss::tolerated_burst(),
        relay_loss::expected_burst(relay_loss::surb_buffer_target()),
    );

    // A share-bearing SURB the Exit's store dropped for want of capacity is a share lost for good,
    // whatever the outage did: the store gives up share-less SURBs first, so a `ring_share`
    // eviction means it held nothing else. The cycle can sweep with a few of those, which is why a
    // sweep alone does not rule them out. A generation wipe is *not* an eviction and is not counted
    // here (the buffer is cleared, not overflowed); hoprnet#8576's `hopr_surb_generation_bumps_total`
    // is the signal for that once the lock carries it.
    assert_eq!(
        0,
        gate.ring_share_evictions(),
        "the Exit's SURB store evicted share-bearing SURBs for capacity during the scenario, each \
         a PIX share lost for good: {}",
        gate.summary()
    );

    assert_eq!(
        0,
        swept.deposits_timed_out().unwrap_or(0),
        "the Exit gave up waiting for a deposit, so the cycle was lost to the chain rather than \
         to the outage: {}",
        swept.summary()
    );

    assert!(
        swept.sweeps().unwrap_or(0) >= 1,
        "no cycle was swept after half the return relayers died. The cycle that was {:.0}% \
         complete at the kill never reconstructed: at surplus {} a polynomial absorbs {} lost \
         shares and the emission window spreads a burst over {} of them, so the cycle tolerates \
         {} contiguous lost shares against the ~{} a death costs at a {}-SURB buffer. {}",
        100.0 * outage.shares_at_kill as f64 / relay_loss::cycle_packets() as f64,
        relay_loss::additional_shares(),
        relay_loss::additional_shares(),
        relay_loss::SHARE_EMISSION_WINDOW,
        relay_loss::tolerated_burst(),
        relay_loss::expected_burst(relay_loss::surb_buffer_target()),
        relay_loss::surb_buffer_target(),
        swept.summary()
    );

    // The cycle in flight at the kill has to be the one that recovers. A sweep and an empty `closures`
    // gate the Session, not that cycle: when it dies (`RecoveryIdle`, polynomials short of their
    // threshold) the Entry opens a successor, the successor sweeps, and the Session never closes. Two
    // runs passed that way: a production-dimension run at a 19 264-SURB stock, where the Exit closed
    // ssa#1 on `RecoveryIdle` 141 polynomials short and ssa#2 swept (RP19264-4), and the surplus-8
    // run (RLF2 s8), where the first closure counted was ssa#2's `commitment_timeout`.
    //
    // The window opens after the Session's first cycle was requested and funded, and the kill falls at
    // a quarter of it, so the one `requested` inside the window is the successor the Entry asks for at
    // the early-recovery threshold, and the one `recovered` is the cycle in flight. `failed` is the
    // discriminator: the supervisor books a cycle that did not recover there, whatever the reason,
    // and a replacement for a dead cycle reads as one request and one recovery just the same.
    let (requested, recovered, failed) = (
        gate.cycles("requested").unwrap_or(0),
        gate.cycles("recovered").unwrap_or(0),
        gate.cycles("failed").unwrap_or(0),
    );
    assert_eq!(
        (1, 1, 0),
        (requested, recovered, failed),
        "(requested, recovered, failed) cycles over the scenario: the cycle that was {:.0}% complete \
         at the kill must be the one that recovered, with one successor requested behind it. A failed \
         cycle means it died and a successor took over, which the Session-level counters do not show: \
         at surplus {} the cycle tolerates {} contiguous lost shares against the ~{} a death costs at \
         a {}-SURB buffer, and a late verdict lets the wipe come after more of the stock has gone. {}",
        100.0 * outage.shares_at_kill as f64 / relay_loss::cycle_packets() as f64,
        relay_loss::additional_shares(),
        relay_loss::tolerated_burst(),
        relay_loss::expected_burst(relay_loss::surb_buffer_target()),
        relay_loss::surb_buffer_target(),
        gate.summary()
    );

    assert_exit_was_paid(&exit, &paid_before, swept.sweeps().unwrap_or(0)).await?;

    tracing::info!(
        topology = topology.name,
        summary = %swept.summary(),
        "PIX survived losing half its return relayers, and was paid"
    );
    Ok(())
}

/// Replies travel over a relay and so do requests — the shape a deployment runs.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_pix_cycle_survives_a_relay_death_on_both_paths() -> anyhow::Result<()> {
    a_pix_session_survives_losing_half_its_return_relayers(SYMMETRIC).await
}

/// Requests travel direct and only the replies are relayed, which is the harder case.
///
/// Worth its own scenario rather than being a variant nobody runs: it is the only one of the two
/// that reaches the generation-bump wipe, because that needs the detector to corroborate a dead
/// relay against a sibling that is still delivering — and when the kill silences the forward path
/// too, there is no such sibling. The burst this shape inflicts is therefore the larger one, and
/// it is the one `relay_loss`'s surplus is sized against.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_pix_cycle_survives_a_return_relay_death_with_the_forward_path_intact()
-> anyhow::Result<()> {
    a_pix_session_survives_losing_half_its_return_relayers(RETURN_ONLY).await
}

/// A relay dies in the middle of two-hop return paths. The Exit drops the SURBs that start with it
/// on its own; the ones that pass through it second are invisible to the Exit and only the Entry's
/// dead-verdict generation bump removes them -- by wiping the Exit's whole stock for the Session.
/// Passing means the cycle absorbed that wipe: no supervisor closure, the cycle swept, the Exit paid.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_pix_cycle_survives_a_deep_return_relay_death_and_the_generation_wipe()
-> anyhow::Result<()> {
    a_pix_session_survives_losing_half_its_return_relayers(RETURN_TWO_HOP).await
}
