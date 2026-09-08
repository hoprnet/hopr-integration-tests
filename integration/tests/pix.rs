//! End-to-end PIX with **edgli as the paying Entry** (manual; NOT run in CI).
//!
//! hoprd's own `session_pix` covers hoprd-Entry ↔ hoprd-Exit over the REST API. This covers the
//! configuration that actually ships: `gnosis_vpn-client` embeds edgli, so in production the node
//! that opens a Session and pays for it is the edge client, linked in-process here.
//!
//! The happy path being asserted:
//!
//!   1. edgli opens a PIX Session to a cluster Exit through one relay.
//!   2. edgli's strategy deposits `price_per_byte × quota` to the SSA stealth address.
//!   3. The Exit observes the deposit and clears that SSA's deposit deadline, so the Session
//!      survives.
//!   4. Bidirectional traffic carries SSA shares on the return-path SURBs until the Exit
//!      reconstructs the stealth address private key.
//!   5. The Exit sweeps the deposit into its Safe.
//!   6. Repeated across several SSA cycles.
//!
//! # Prerequisites
//!
//! Both cluster binaries must be built in **release** with the secp256k1 deposit pool. PIX itself
//! is on hoprd `main` since hoprd#91, but the pool is not: it is a *build-time* choice behind a
//! non-default feature, and a binary carrying the other one bootstraps normally and then never
//! deposits — so [`just pix`](../../justfile) greps the binary for its pool marker before starting
//! anything. Release is not a preference either: debug builds slow packet processing enough to
//! distort the cycle pacing this rests on.
//!
//! ```bash
//! nix develop -c cargo build --release -p hoprd -p hoprd-localcluster \
//!   --features strategy-pix-test
//! ```
//!
//! # Running
//!
//! ```bash
//! # both scenarios, ~11 min (~270 s + ~371 s, plus a chain bootstrap each)
//! HOPRD_SRC=../hoprd HOPRD_KEEP_ARTIFACTS=1 just pix > /tmp/pix.log 2>&1
//!
//! # just one
//! HOPRD_SRC=../hoprd just pix edgli_entry_deposits_should_be_swept_into_the_exit_safe
//! ```
//!
//! The two run sequentially on a fresh chain each — they want different entry budgets, and the
//! cluster binds fixed ports, so they cannot share one. `HOPRD_KEEP_ARTIFACTS=1` (the recipe's
//! default) always, or the node logs are deleted at teardown and a failed run leaves nothing to
//! read. Redirect with `>` rather than piping into `tail`: a pipeline's exit status is the last
//! command's, so a failed run reports success.
//!
//! Cargo captures test output on a pass, so the counter summaries logged below reach the log only
//! for a failing run. Add `TEST_ARGS=--nocapture` to see them on green — at the cost of every
//! `sqlx` query the nodes make.
//!
//! # What a healthy run measures
//!
//! Recorded so a future reader can tell a drifted constant from a real regression. Measured on
//! the stack these constants were last run against — hoprnet `85e698a7`, hopr-strategy 4.0.0,
//! edgli `main` (`ced8fa64`), blokli v0.14.0 — one full run plus a second of the exhaustion
//! scenario, which reproduced to the wxHOPR:
//!
//! | | measured | asserted |
//! | --- | --- | --- |
//! | happy path: cycles swept | 6 | ≥ [`TARGET_CYCLES`] = 4 |
//! | happy path: deposits made | 7 | within [`MAX_SSAS_IN_FLIGHT`] of the swept count |
//! | happy path: Exit Safe gained | 19.9296 wxHOPR | exactly 6 × 3.3216 |
//! | happy path: entry Safe fell | 23.2512 wxHOPR | ≥ what the Exit gained |
//! | happy path: failed / over-budget | 0 / 0 | both 0 |
//! | exhaustion: deposits made | 2 | exactly [`EXHAUSTION_BUDGETED_CYCLES`] |
//! | exhaustion: refused for budget | **1** | ≥ 1 |
//! | exhaustion: Exit deposit timeouts | **1** | ≥ 1 |
//! | exhaustion: cycles swept | 2 | 1..=2 |
//! | exhaustion: Exit Safe gained | 6.6432 wxHOPR | exactly 2 × 3.3216 |
//!
//! The happy path clears its cycle target by 50 % and every other margin there is wider still, so
//! a run that lands *on* a threshold is worth investigating rather than accepting.
//!
//! **The two bolded rows have no margin, by design rather than by luck.** Both read 5 before
//! hoprnet#8237 replaced the per-SSA deadline pair with the Exit's Session supervisor. The old
//! deposit awaiter re-armed and kept timing out while the Session stayed up, so the repeats
//! accumulated; the supervisor closes on the first failed cycle instead — `max_failed_cycles`
//! defaults to 1 — and the entry, having refused one deposit for budget, never gets asked again.
//! So 1 is the whole of what this scenario can now produce, and `≥ 1` is exact rather than
//! generous. A 0 is a real failure; anything above 1 means the supervisor tolerated a failed cycle
//! it is configured not to.
//!
//! Wall-clock was 265.9 s and 371.4 / 371.5 s.
//!
//! # Which build each participant runs
//!
//! | participant | role | comes from |
//! | ----------- | ---- | ---------- |
//! | entry (`edgli`, in-process) | opens the Session, mints SURBs, **pays the deposits** | the `edgli` git dependency, compiled into this binary |
//! | relay + Exit | forward packets, reply, recover keys, **sweep** | `$HOPRD_BIN` at runtime |
//!
//! Compile-time on one side and runtime on the other, so the two can silently disagree — and both
//! must carry PIX for anything here to mean what it says. The pool marker check covers the Exit;
//! this binary not compiling without `--features pix` covers the entry.
#![cfg(feature = "pix")]

use std::time::Duration;

use hoprd_integration_test::{
    Address, IntegrationEnv,
    cluster::{NodeInfo, request_cluster_size},
    pix::{self, PixCounters},
    pump::{PumpOpts, PumpOutcome, pump_halves},
};
use rand::RngExt as _;

/// Three nodes: edgli → relay → Exit. PIX needs at least one relay on each path, and a third node
/// gives the path planner an alternative rather than a single forced route.
const NODES: usize = 3;

/// Relays on each path. **Must be ≥ 1**: the share encryption key is derived from the first
/// relayer's acknowledgement, so a zero-hop path has nothing to derive it from and the Session is
/// refused outright.
const HOPS: usize = 1;

/// SSA cycles that must fully complete — deposited, recovered, swept.
const TARGET_CYCLES: u64 = 4;

/// How far the entry's deposits may legitimately run ahead of the Exit's recoveries.
///
/// The Exit requests the next SSA once the current one passes its early-recovery threshold, and the
/// entry deposits for it immediately, so at any instant one SSA is normally funded but not yet
/// recovered. Two allows for the sample landing mid-handover.
const MAX_SSAS_IN_FLIGHT: u64 = 2;

/// Shares one SSA emits: every polynomial leaves the generator's queue having emitted the threshold
/// *plus* the surplus, whether or not any share was lost.
const EMISSIONS_PER_SSA: u64 =
    (pix::PIX_POLYS * (pix::PIX_SHARES + pix::PIX_ADDITIONAL_SHARES)) as u64;

/// Payload per datagram, comfortably under `SESSION_MTU` so one write is one packet.
const SEND_CHUNK: usize = 512;

/// Delay between datagrams, matching `session_pix`'s validated value.
///
/// This paces the Exit → Entry packet rate, which is what drives share delivery: the Exit consumes
/// one return-path SURB per reply and each SURB carries one share. So a cycle takes roughly
/// `EMISSIONS_PER_SSA × SEND_INTERVAL` ≈ 13 s.
///
/// The pacing is load-bearing rather than cosmetic. Share collection and the deposit run
/// *concurrently* — the Exit serves data on credit and only its supervisor enforces payment — so a
/// cycle that finished before its deposit transaction was mined would leave the Exit recovering a
/// key against a zero balance, logging "already swept", and the funds stranded at the stealth
/// address. 400 ms keeps a cycle comfortably longer than an Anvil transaction.
const SEND_INTERVAL: Duration = Duration::from_millis(400);

/// Datagrams offered per cycle, over the emission count.
///
/// One write *should* be one packet is one reply is one SURB is one share, but nothing in the
/// session API guarantees a write is not coalesced. Offering twice the arithmetic costs wall-clock
/// and cannot cause a false pass — the assertions are all `>=` against cycles actually observed —
/// whereas offering exactly the arithmetic and being wrong reads as a broken strategy.
const REPLY_MARGIN: u64 = 2;

/// Cycles the happy path budgets the entry for.
///
/// Far above what the traffic can consume, deliberately: an entry that exhausts its budget mid-run
/// trips the Exit's deposit deadline, and this scenario would then be measuring the *other* one.
/// Five times [`TARGET_CYCLES`] leaves no doubt which one bound.
const BUDGETED_CYCLES: u64 = 20;

/// Cycles the exhaustion scenario budgets — few, since the run is over once they are committed.
const EXHAUSTION_BUDGETED_CYCLES: u64 = 2;

/// Cycles the exhaustion scenario offers traffic for.
///
/// Must outlast the close, which lands about `EXHAUSTION_BUDGETED_CYCLES` cycles of traffic plus the
/// Exit's `max_deposit_wait + max_ssa_delivery_time` deadline (80 s) later — call it 130 s against
/// the ~205 s this offers. Sized in cycles rather than seconds so it tracks the pacing constants.
const EXHAUSTION_PAYLOAD_CYCLES: u64 = 8;

/// Budget for offering the whole payload. Bounds the writer, which is otherwise capped only by the
/// session accepting writes.
const PUMP_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to keep watching the Exit after the traffic stops.
///
/// The last cycle's recovery and sweep are two on-chain round trips behind the reply that completed
/// it, so reading balances the instant the pump returns undercounts by a cycle or more.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);
const SETTLE_POLL: Duration = Duration::from_secs(3);

/// A cluster node by address.
fn node_for(env: &IntegrationEnv, address: Address) -> anyhow::Result<NodeInfo> {
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == address)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no cluster node at {address}"))
}

/// Random bytes sized to drive `cycles` SSA cycles.
///
/// Random rather than patterned so each run's packet ciphertexts are unique and replay tags do not
/// collide, the same reason `tests/integration.rs` randomises its payload.
fn payload_for(cycles: u64) -> Vec<u8> {
    let datagrams = cycles * EMISSIONS_PER_SSA * REPLY_MARGIN;
    let mut payload = vec![0u8; datagrams as usize * SEND_CHUNK];
    rand::rng().fill(&mut payload[..]);
    payload
}

/// `PumpOpts` for a PIX session: a small chunk paced slowly.
///
/// The chunk size is the point. `pace` alone fixes the average rate while leaving the shape a
/// 64 KiB burst followed by silence — fine when the average is the measurement, wrong here, where
/// what is being paced is the reply rate that advances a cycle.
fn pix_pump_opts() -> PumpOpts {
    PumpOpts {
        pace: Some(SEND_INTERVAL),
        chunk: Some(SEND_CHUNK),
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

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn edgli_entry_deposits_should_be_swept_into_the_exit_safe() -> anyhow::Result<()> {
    request_cluster_size(NODES);
    let t0 = std::time::Instant::now();

    let per_cycle = pix::per_cycle()?;
    let quota = pix::quota_per_ssa()?;
    let budget = per_cycle * BUDGETED_CYCLES;
    tracing::info!(
        %per_cycle, quota, %budget, TARGET_CYCLES,
        "PIX accounting: one SSA cycle costs price_per_byte x quota"
    );

    let env = IntegrationEnv::setup_pix(budget).await?;
    let (session, exit_addr) = env.open_pix_session(HOPS, HOPS).await?;
    let exit = node_for(&env, exit_addr)?;
    tracing::info!(exit = %exit_addr, elapsed = ?t0.elapsed(), "PIX session open");

    // Both sides move wxHOPR through their Safe: a deposit debits the entry's, a sweep credits the
    // Exit's. Sampled after the channels are funded, so the stakes are already out.
    let exit_before = pix::node_balances(&exit).await?;
    let exit_counters_before = pix::sample_exit(&exit).await?;
    let entry_before = env.entry_safe_balance().await?;
    let entry_counters_before = pix::sample_entry();
    tracing::info!(
        entry_safe = %entry_before, exit_safe = %exit_before.safe,
        "balances before the session"
    );

    // The one precondition nothing else checks. The localcluster funds each extra identity with
    // 1000 wxHOPR and `deploy_safe` sweeps it into the Safe, so the budget is comfortably covered —
    // but if that ever stops being true the entry runs dry before the budget binds, and every
    // assertion below would be reporting the wrong cause.
    assert!(
        entry_before >= budget,
        "the entry's Safe holds {entry_before} against a {budget} deposit budget, so it would run \
         dry before the budget bound and this run would end for a reason it does not test"
    );

    let payload = payload_for(TARGET_CYCLES);
    let (mut rx, mut tx) = tokio::io::split(session);
    let transfer = pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "pix",
        PUMP_TIMEOUT,
        pix_pump_opts(),
    )
    .await?;

    let exit_counters =
        await_sweeps(&exit, &exit_counters_before, TARGET_CYCLES, SETTLE_TIMEOUT).await?;
    let entry_counters = entry_counters_before.delta(&pix::sample_entry());
    let exit_after = pix::node_balances(&exit).await?;
    let entry_after = env.entry_safe_balance().await?;
    let recovered = exit_after.safe - exit_before.safe;
    // An upper bound on PIX spend, not a measurement of it: the channel-lifecycle strategy stakes
    // and tops up from the same Safe, and it keeps ticking through the run. Only the Exit's side
    // has a clean enough account for an exact-multiple reading — see the assertions below.
    let safe_drop = entry_before - entry_after;

    tracing::info!(
        %recovered, %safe_drop, elapsed = ?t0.elapsed(),
        "entry counters: {} | exit counters: {}",
        entry_counters.summary(), exit_counters.summary()
    );

    // ── Assertions, ordered so a failure names its own cause ─────────────────

    // Nothing below means anything about a session with no counterparty.
    assert!(
        transfer.arrival_pct() > 0.0,
        "no byte completed the entry -> exit -> loopback -> entry round trip, so the PIX session \
         never carried traffic (outcome {:?}; entry {}; exit {})",
        transfer.outcome,
        entry_counters.summary(),
        exit_counters.summary(),
    );

    // The counters are what every assertion after this reads. Absent means the Exit was built
    // without `hopr-strategy/telemetry`, and reporting that as zero would blame the entry.
    assert!(
        exit_counters.observable(),
        "the Exit exposed no hopr_strategy_pix_* series at all — it was built without PIX \
         strategy telemetry, so nothing here was measured"
    );

    // "The Exit sees the deposit and does not close the session": its deposit pool counts one
    // confirmation per SSA when the deposit lands inside the deadline, and a timeout when it does
    // not — the latter being what the Exit's supervisor closes the Session on.
    //
    // `unwrap_or(0)` and not `== Some(0)`, which is the opposite of how the unlabelled families are
    // read here. `deposit_tracking` is a *labelled* counter and a label set materialises only once
    // it is first incremented, so a run where nothing timed out has no `{outcome="timeout"}` series
    // at all. Absent therefore means the event never happened — the build-has-no-telemetry reading
    // of absent is already ruled out by `observable()` above.
    assert_eq!(
        exit_counters.deposits_timed_out().unwrap_or(0),
        0,
        "the Exit gave up waiting for {:?} deposit(s) and closed the session on its PIX deposit \
         deadline (it confirmed {:?}). Either the entry never deposited — check its `deposits` and \
         `over_budget` counters below — or the deposit landed outside the \
         max_deposit_wait + max_ssa_delivery_time window.",
        exit_counters.deposits_timed_out(),
        exit_counters.deposits_confirmed(),
    );
    assert!(
        exit_counters.deposits_confirmed().unwrap_or(0) >= TARGET_CYCLES,
        "the Exit confirmed only {:?} deposits, expected at least {TARGET_CYCLES} (entry: {})",
        exit_counters.deposits_confirmed(),
        entry_counters.summary(),
    );
    assert!(
        exit_counters.keys_recovered().unwrap_or(0) >= TARGET_CYCLES
            && exit_counters.sweeps().unwrap_or(0) >= TARGET_CYCLES,
        "the Exit recovered {:?} keys and swept {:?} of them, expected at least {TARGET_CYCLES} \
         each. Recoveries without sweeps means the funds are reachable and were not collected; \
         neither means shares never completed an SSA.",
        exit_counters.keys_recovered(),
        exit_counters.sweeps(),
    );

    // The real verdict. With auto-redeeming off, PIX sweeps are the only thing that credits the
    // Exit's Safe in wxHOPR, so an exact whole multiple says every wxHOPR that arrived did so as a
    // complete SSA deposit — which is the statement that recovered funds correspond to the data
    // quota delivered back to the entry.
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
         addresses — slow SEND_INTERVAL down. (exit: {})",
        per_cycle * TARGET_CYCLES,
        t0.elapsed(),
        exit_counters.summary(),
    );

    // The entry paid for every one of those, and for hardly any more.
    //
    // Counted from the entry's own `deposits` counter rather than divided out of its Safe balance,
    // which is what this used to do. Both were exact while the deposit left the node's own account
    // — nothing else touched it — but the Safe also stakes and tops up channels, and the
    // channel-lifecycle strategy keeps ticking through the run. A whole-multiple assertion on that
    // balance now fails on a *healthy* run as soon as one channel is topped up, and the counter is
    // the more direct statement anyway: it counts deposits rather than inferring them from money.
    //
    // Not an equality against `cycles`: early-recovery pipelining legitimately funds SSAs still in
    // flight when the sample is taken.
    let deposited = entry_counters.deposits().unwrap_or_else(|| {
        panic!(
            "the entry exposed no hopr_strategy_pix_deposits_total series — it was built without \
             `hopr-strategy/telemetry`, so nothing counted its deposits ({})",
            entry_counters.summary()
        )
    });
    assert!(
        (cycles..=cycles + MAX_SSAS_IN_FLIGHT).contains(&deposited),
        "the entry deposited for {deposited} SSAs but only {cycles} were recovered and swept. Up \
         to {MAX_SSAS_IN_FLIGHT} may legitimately be in flight thanks to early-recovery \
         pipelining; more than that means deposits are being made for SSAs that never complete."
    );

    // ...and the money came out of the Safe. Conservation, across the two accounts: the entry's
    // Safe fell by at least what the Exit's Safe gained.
    //
    // This is the half the counter cannot state. It says deposits were *made*, not which account
    // was debited — and the payer moving from the node account to the Safe is exactly what changed.
    //
    // Measured against `recovered` rather than against `deposited`, which is the tighter-looking
    // bound and the wrong one. `deposited` includes SSAs still in flight, whose transfers the
    // entry has counted but whose balance effect blokli may not have indexed when the read below
    // happens; `recovered` is money the Exit has already swept, so every debit behind it is
    // settled by construction. An equality is out for a separate reason: the channel-lifecycle
    // strategy spends the same Safe, and in this scenario only ever downwards — it opens toward
    // its channel target while the run proceeds, nothing closes, and the entry has no tickets to
    // redeem.
    assert!(
        safe_drop >= recovered,
        "the entry's Safe fell by {safe_drop} while the Exit's gained {recovered}. Deposits settle \
         through the Safe module, so the entry's must have paid at least what arrived. A drop of \
         exactly 0 means it *grew* instead — a channel closed mid-run and returned its stake, which \
         swamps the deposit signal rather than contradicting it. Anything else means the deposits \
         were debited somewhere other than the Safe. ({})",
        entry_counters.summary(),
    );

    // A failed deposit means one was attempted and did not land. The budget is 5x the target, and
    // a refusal for budget is a separate counter, so neither explains a non-zero here — it is a
    // broken transfer, and the cycle counts above understate what the run should have achieved.
    assert_eq!(
        entry_counters.deposits_failed().unwrap_or(0),
        0,
        "the entry failed {:?} deposit(s) despite being budgeted for {BUDGETED_CYCLES} cycles and \
         holding {} in its Safe ({})",
        entry_counters.deposits_failed(),
        entry_after,
        entry_counters.summary(),
    );

    // The budget is deliberately far above what this traffic can consume. Reaching it means the
    // pricing or the pacing constants have drifted, and this run measured the exhaustion scenario
    // instead of this one.
    assert_eq!(
        entry_counters.deposits_over_budget().unwrap_or(0),
        0,
        "the entry hit its {BUDGETED_CYCLES}-cycle deposit budget, so it stopped paying part-way \
         through a scenario that is not supposed to reach it ({})",
        entry_counters.summary(),
    );

    tracing::info!(
        cycles, deposited, %recovered, %safe_drop,
        "edgli PIX entry test PASSED in {:?}", t0.elapsed()
    );
    Ok(())
}

/// The failure mode edge-client documents as its known limitation, made falsifiable.
///
/// An entry that stops being able to pay stops depositing, and the Exit closes the Session on its
/// deposit deadline — "with nothing logged as an error at this end", as edge-client's README puts
/// it. This budgets exactly [`EXHAUSTION_BUDGETED_CYCLES`], offers several times that much traffic,
/// and pins what actually happens.
///
/// # Budget, not an empty Safe
///
/// The entry used to be starved by funding its account with an exact number of cycles' worth. That
/// account no longer pays: deposits settle through the Safe module, and the Safe also holds the
/// channel stakes — so "the money ran out" would now mean "the stakes' leftovers ran out too", and
/// the cycle count would turn on stake arithmetic that has nothing to do with PIX.
///
/// `max_spend_per_window` states the number outright instead. The strategy refuses the deposit that
/// would cross it and drops the event, which starves the Session in exactly the way an empty
/// account did — the Exit cannot tell the two apart, and its deposit deadline is what both scenarios
/// exercise. The refusal lands in its own counter (`over_budget`), separate from a deposit that was
/// attempted and failed, so the two endings stay distinguishable.
///
/// # What the entry does *not* learn
///
/// Measured, edge-client's "nothing logged" is stronger than it sounds: the entry gets no event at
/// all. An unreliable session carries no end-of-stream, so the Exit's closure arrives as replies
/// ceasing, and the pump ends `Idle` rather than `SessionClosed`. The only thing distinguishing "the
/// counterparty stopped paying attention" from "the network went quiet" is on the *Exit's* side, in
/// a counter the entry cannot see. An embedder that wants to react has to watch its own budget and
/// balance, which is why the `hopr_strategy_pix_*` counters and
/// [`IntegrationEnv::entry_safe_balance`](hoprd_integration_test::IntegrationEnv::entry_safe_balance)
/// exist.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_session_should_close_when_the_entry_can_no_longer_deposit() -> anyhow::Result<()> {
    request_cluster_size(NODES);
    let t0 = std::time::Instant::now();

    let per_cycle = pix::per_cycle()?;
    let budget = per_cycle * EXHAUSTION_BUDGETED_CYCLES;
    tracing::info!(
        %per_cycle, %budget, EXHAUSTION_BUDGETED_CYCLES, EXHAUSTION_PAYLOAD_CYCLES,
        "entry budgeted for a fixed number of cycles, then offered several times that much traffic"
    );

    let env = IntegrationEnv::setup_pix(budget).await?;
    let (session, exit_addr) = env.open_pix_session(HOPS, HOPS).await?;
    let exit = node_for(&env, exit_addr)?;

    let exit_before = pix::node_balances(&exit).await?;
    let exit_counters_before = pix::sample_exit(&exit).await?;
    let entry_counters_before = pix::sample_entry();
    let entry_safe_before = env.entry_safe_balance().await?;

    // The budget has to be what binds, so the Safe must be able to cover it — otherwise the entry
    // runs dry first and the run ends for a reason with no assertion behind it.
    assert!(
        entry_safe_before >= budget,
        "the entry's Safe holds {entry_safe_before} against a {budget} deposit budget, so the \
         balance would bind before the budget and this scenario would not be testing the budget"
    );

    let payload = payload_for(EXHAUSTION_PAYLOAD_CYCLES);
    let (mut rx, mut tx) = tokio::io::split(session);
    let transfer = pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "pix-exhaustion",
        PUMP_TIMEOUT,
        pix_pump_opts(),
    )
    .await?;

    // The budgeted cycles are the most the Exit can ever sweep here, so this settles rather than
    // waits — it returns on the deadline if fewer completed, which is a legitimate outcome.
    let exit_counters = await_sweeps(
        &exit,
        &exit_counters_before,
        EXHAUSTION_BUDGETED_CYCLES,
        SETTLE_TIMEOUT,
    )
    .await?;
    let entry_counters = entry_counters_before.delta(&pix::sample_entry());
    let recovered = pix::node_balances(&exit).await?.safe - exit_before.safe;
    let entry_safe_after = env.entry_safe_balance().await?;

    tracing::info!(
        %recovered, entry_safe = %entry_safe_after, outcome = ?transfer.outcome,
        elapsed = ?t0.elapsed(),
        "entry counters: {} | exit counters: {}",
        entry_counters.summary(), exit_counters.summary()
    );

    // ── Assertions ───────────────────────────────────────────────────────────

    // The session has to have worked before it stopped, or "it closed" says nothing about running
    // out of money — `NeverStarted` also satisfies `exit_stopped_serving`.
    assert!(
        transfer.arrival_pct() > 0.0,
        "the session never carried traffic at all, so its closing says nothing about the entry's \
         budget (outcome {:?}; entry {})",
        transfer.outcome,
        entry_counters.summary(),
    );

    // The entry committed its whole budget and no more. This is where an off-by-one in the budget
    // arithmetic would show: the ceiling is crossed by the deposit that *would* exceed it, so
    // exactly the budgeted number are made.
    assert_eq!(
        entry_counters.deposits().unwrap_or(0),
        EXHAUSTION_BUDGETED_CYCLES,
        "the entry made {:?} deposits against a {EXHAUSTION_BUDGETED_CYCLES}-cycle budget ({})",
        entry_counters.deposits(),
        entry_counters.summary(),
    );

    // And then it refused, for budget rather than for anything else. Separate counters, because a
    // refusal is the designed ending here and a failure is a broken transfer — an assertion that
    // accepted either would pass on an entry whose deposits simply do not work.
    assert!(
        entry_counters.deposits_over_budget().unwrap_or(0) >= 1,
        "the entry refused no deposit for budget ({}), so it never reached its \
         {EXHAUSTION_BUDGETED_CYCLES}-cycle ceiling — the payload was too short to outrun it",
        entry_counters.summary(),
    );
    assert_eq!(
        entry_counters.deposits_failed().unwrap_or(0),
        0,
        "the entry failed {:?} deposit(s). The run is supposed to end by refusing one for budget, \
         not by attempting one that does not land; its Safe still holds {} ({})",
        entry_counters.deposits_failed(),
        entry_safe_after,
        entry_counters.summary(),
    );

    // The stream stopped carrying the payload. Note what this does *not* assert.
    //
    // `SessionClosed` would be the natural expectation and it does not happen: measured, this ends
    // as `Idle` — the reply stream simply goes quiet. An unreliable session has no end-of-stream to
    // deliver, so a read never returns 0 and the Exit's closure reaches the entry as the absence of
    // replies rather than as an event. That sharpens what edge-client documents ("with nothing
    // logged as an error at this end") instead of contradicting it: there is no signal, only its
    // absence, and an embedder learns about it by noticing the silence.
    //
    // So the honest assertion is that delivery stopped, with the two counters either side of it
    // saying *why* — the entry would not pay, and the Exit's deposit deadline expired.
    assert!(
        transfer.outcome != PumpOutcome::Complete,
        "the entry reached its deposit budget and refused to pay, yet the whole payload still came \
         back ({:.1}% arrival, outcome {:?}). The Exit served traffic it was never paid for, so its \
         supervisor is not enforcing payment. (exit: {})",
        transfer.arrival_pct(),
        transfer.outcome,
        exit_counters.summary(),
    );
    assert!(
        exit_counters.deposits_timed_out().unwrap_or(0) >= 1,
        "the Exit recorded no deposit timeout ({}), so the session was closed by something other \
         than its PIX deposit deadline",
        exit_counters.summary(),
    );

    // And it collected exactly what it was paid for — no more, since nothing beyond the budget was
    // ever deposited, and at least one, since the first cycles did complete.
    let cycles = pix::completed_cycles(recovered, per_cycle).unwrap_or_else(|| {
        panic!(
            "the Exit's Safe gained {recovered}, which is not a whole multiple of the {per_cycle} \
             per-SSA deposit — something other than PIX sweeps moved the balance"
        )
    });
    assert!(
        (1..=EXHAUSTION_BUDGETED_CYCLES).contains(&cycles),
        "the Exit swept {cycles} cycles against an entry budgeted for exactly \
         {EXHAUSTION_BUDGETED_CYCLES}. Above that it collected more than was ever deposited; zero \
         means the session died before a single cycle completed, so the close was not the budget. \
         (exit: {})",
        exit_counters.summary(),
    );

    tracing::info!(
        cycles, %recovered, entry_safe = %entry_safe_after,
        "PIX exhaustion test PASSED in {:?}", t0.elapsed()
    );
    Ok(())
}
