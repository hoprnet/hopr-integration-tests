//! A resumed PIX Session keeps its paid cycle (CI's v5 nightly tier; `just pix-resume` locally).
//!
//! # The question
//!
//! A client that is forced to reconnect, say a VPN client whose network changed, opens a new Session
//! to the same Exit. Without resumption that is a new PIX cycle: another deposit, while the one it
//! already paid sits at the old Session's stealth address. hoprnet#8711 lets the client re-open
//! under the **same pseudonym**. The Exit *detaches* the funded Session when its data plane ends and
//! holds it for a **resume window**; a re-initiation under that pseudonym *takes it over* and keeps
//! the paid cycle. After the window the Exit lets go as before, and a re-open starts a fresh cycle,
//! which is to say pays again.
//!
//! The Entry is `edgli` in-process, as in `tests/pix.rs`, because that is what ships in
//! `gnosis_vpn-client`. A clean `close()` of its session sends a terminating segment that reaches
//! the Exit; that is what makes the Exit detach.
//!
//! # The Session
//!
//! Opened with `Segmentation | NoDelay` and no reliability, i.e. the **stateless socket**
//! (`IntegrationEnv::open_pix_session_with_pseudonym`). Takeover is only eligible for Sessions on
//! the stateless socket, so a reliable Session would be refused and say nothing about resumption.
//!
//! # Stray datagrams
//!
//! The first leg sends only ~100 datagrams each way, so hoprnet's successor-stream guard cannot tell
//! the old stream from the new one, and a few datagrams in flight at the close can be delivered into
//! the resumed Session in both directions; that is by design for unreliable Sessions. Every datagram
//! therefore carries a leg tag ([`TAG_MAGIC`], leg, sequence), the reader ignores other legs' and
//! counts them, and the run logs the count and asserts it is at most [`MAX_STRAYS`], never zero.
//!
//! # What is asserted
//!
//! | scenario | the re-open | asserted |
//! | --- | --- | --- |
//! | `a_session_resumed_within_the_window_keeps_its_paid_cycle` | [`QUICK_GAP`] after the close | no second deposit for SSA 1; `taken_over == 1`; swept cycles are paid exactly once |
//! | `a_session_resumed_after_a_pause_inside_the_window_continues` | [`PAUSED_GAP`] = 0.6 x the window | as above, and the Exit's fill, which keeps running while detached, has not finished the cycle by itself |
//! | `a_session_reopened_after_the_window_starts_a_new_cycle` | [`EXPIRED_GAP`] = window + 5 s | `window_expired == 1`, `taken_over == 0`, the Entry deposits again, nothing is swept twice |
//!
//! The third is the control and documents the designed boundary: **past the window the client pays
//! again.**
//!
//! Each scenario reads `hopr_pix_resumptions_total{event}` and
//! `hopr_pix_admission_rejections_total{reason="takeover_ineligible"}` on the Exit
//! ([`pix_exit::ExitTelemetry::resumptions`]), the settlement counters on both ends
//! ([`pix::PixCounters`]) and the Exit's Safe balance, so the verdict is "was paid" and not only
//! "was counted".
//!
//! # Prerequisites
//!
//! * hoprnet carrying the resume window (hoprnet#8711), in both the Exit's `hoprd` and the `edgli`
//!   this binary links. Until that is on `master`/`main`, point `HOPRD_SRC` at a hoprd checkout on
//!   that hoprnet branch.
//! * A **hoprd/localcluster that knows `resume_window`** (`hoprd#293`). The window is stated in the
//!   `--pix-config` YAML, [`RESUME_WINDOW`], so that a run is seconds long rather than waiting out
//!   the 60 s default. Localcluster's raw PIX settings are `deny_unknown_fields`, so an older binary
//!   fails fast at start-up on the YAML; `just pix-resume` greps for the field first.
//! * The other PIX prerequisites of `tests/pix.rs`: release builds with the secp256k1 deposit pool.
//!
//! # Running
//!
//! ```text
//! HOPRD_SRC=../hoprd HOPRD_KEEP_ARTIFACTS=1 LINE=v5 just pix-resume > /tmp/pix-resume.log 2>&1
//! LINE=v5 HOPRD_SRC=../hoprd just pix-resume a_session_reopened_after_the_window_starts_a_new_cycle
//! ```
//!
//! One cluster per scenario (a fresh chain state each, the Exit's `hopr_pix_*` family has no Session
//! label). Redirect with `>`, not a pipe, so a failed run is not masked.
//!
//! # The geometry
//!
//! The demo geometry of `tests/pix.rs` (8 x (2 + 2) = 32 packets, a ~13 s cycle) is shorter than
//! the gaps below, so a reconnect could not land *mid-cycle*. This test states its own, derived the
//! way `tests/pix_shapes.rs` derives its profile: the ratios matter, not the rates.
//!
//! * `24 x (8 + 4)` = [`CYCLE_PACKETS`] = 288 packets, a cycle of 288 x [`SEND_INTERVAL`] = 115 s of
//!   nominal traffic. Recovery needs every polynomial's threshold, `24 x 8` = [`USEFUL_TARGET`] =
//!   192 useful shares; the other 96 are surplus, emitted last.
//! * The scenarios close once [`PROGRESS_SHARES`] = 64 useful shares have arrived, a third of the
//!   way: the deposit has long been confirmed (3-6 s) and recovery is a long way off. Even the
//!   2.5 shares/s of organic traffic, resumed, needs another ~50 s to finish the cycle, so the
//!   cycle is certainly still in flight at every re-open below.
//! * The cycle is long next to the gaps (10 s window) and short enough that three of them fit a
//!   nightly: [`TARGET_SWEPT`] cycles take ~6 min.
//! * Fill is **slow by design**: [`MAX_RECOVERY_TIME`] 480 s at a finish fraction of 0.6 aims an
//!   idle cycle at 288 s, about one packet/s. Detached for [`PAUSED_GAP`] it adds roughly eight
//!   shares, a few percent of 192, so "fill did not exhaust the cycle" is a statement the geometry
//!   makes possible rather than one that holds trivially. The validator's floor
//!   (`1 MiB / 1452 x 1.05 / 288 s`, ~3 packets/s) is cleared by [`FILL_MAX_RATE`].
//! * The price is set so a deposit is ~3.3 wxHOPR, as at the demo geometry: `quota x price` with
//!   the 418 kB quota is below `max_ssa_allocation` (10 wxHOPR) and comfortable for the Safe float.
//!
//! # The timings
//!
//! One named constant, [`RESUME_WINDOW`] = 10 s, and everything else derives from it. Ten seconds is
//! long enough for a clean close plus a re-open, which completes in a few seconds on a local
//! cluster (asserted under [`REOPEN_BUDGET`]), and short enough that the control waits ~15 s
//! rather than 75 s. The paused gap is 0.6 x window: a quiet gap, leaving 4 s for the client open, the
//! mixnet `StartSession` and the Exit's decision; the gap is measured from the client's own `close()`, which precedes the Exit's
//! detach, so the real gap the Exit sees is slightly shorter. The expired gap adds 5 s to the
//! window: the Exit's timer, the metrics scrape and the cluster's latency are each well under a
//! second, so 5 s is several times what is needed and cheap.
//!
//! # What a healthy run measures
//!
//! To be filled in after the first real run, on a named stack, so a later reader can tell a
//! drifted constant from a regression.
//!
//! | | measured | asserted |
//! | --- | --- | --- |
//! | resumed in window: open time | | < [`REOPEN_BUDGET`] |
//! | stray datagrams from the previous leg (any scenario) | | <= [`MAX_STRAYS`] |
//! | resumed in window: cycles swept | | >= [`TARGET_SWEPT`] |
//! | resumed in window: entry deposits / cycles requested | | deposits <= requested |
//! | resumed in window: Exit Safe gained | | exactly swept x deposit |
//! | resumed after a pause: useful shares at re-open | | < [`USEFUL_TARGET`] (not recovered) |
//! | resumed after a pause: `taken_over` | | exactly 1 |
//! | reopened after the window: `window_expired` | | exactly 1 |
//! | reopened after the window: entry deposits | | >= first-session deposits + swept |
//! | reopened after the window: cycles swept | | >= [`TARGET_SWEPT_CONTROL`], <= deposits |
//!
//! # Which build each participant runs
//!
//! As `tests/pix.rs`: the Entry is the `edgli` compiled into this binary, the Exit and relay are
//! `$HOPRD_BIN` at runtime. Both must carry the resume window or the tests mean nothing.

#![cfg(feature = "v5")]

use std::{future::Future, time::Duration};

use anyhow::Context as _;
use hoprd_integration_test::{
    Address, HoprPseudonym, HoprSession, IntegrationEnv,
    cluster::{self, NodeInfo},
    pix::{self, PixCounters},
    pix_exit::{self, ExitTelemetry},
    random_pseudonym,
};
use rand::RngExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, ReadHalf, WriteHalf};

// ── Geometry ─────────────────────────────────────────────────────────────────

/// Three nodes: edgli -> relay -> Exit. PIX needs a relay on the return path.
const NODES: usize = 3;

/// Relays on each path. At least one: the share key derives from the first return relayer's ack.
const HOPS: usize = 1;

/// Polynomials per SSA. See the module docs, "The geometry".
const PIX_POLYS: usize = 24;
/// Shares needed per polynomial.
const PIX_SHARES: usize = 8;
/// Surplus shares per polynomial: a third of the threshold, as `tests/pix.rs`'s 2 + 2 is half.
const PIX_ADDITIONAL_SHARES: usize = 4;

/// Packets in one cycle, and therefore its price in packets.
const CYCLE_PACKETS: u64 = (PIX_POLYS * (PIX_SHARES + PIX_ADDITIONAL_SHARES)) as u64;

/// Useful shares that complete recovery: every polynomial's threshold.
const USEFUL_TARGET: u64 = (PIX_POLYS * PIX_SHARES) as u64;

/// Bytes a share is priced at; read from hopr-lib so it cannot drift.
const QUOTA_BYTES_PER_SHARE: u64 =
    edgli::hopr_lib::exports::transport::session::PIX_QUOTA_BYTES_PER_SHARE;

/// Quota of one cycle in bytes.
const QUOTA_PER_SSA: u64 = CYCLE_PACKETS * QUOTA_BYTES_PER_SHARE;

/// wxHOPR per byte of quota on both sides: ~3.3 wxHOPR a deposit at [`QUOTA_PER_SSA`].
const PRICE_PER_BYTE_WXHOPR: f64 = 8e-6;

/// Exit's per-cycle recovery deadline, and the idle tariff. See "The geometry": with
/// [`FILL_FINISH_FRACTION`] it aims an idle cycle at 288 s, slow next to the ~115 s of traffic.
const MAX_RECOVERY_TIME: Duration = Duration::from_secs(480);
/// Fraction of [`MAX_RECOVERY_TIME`] by which fill aims to be done.
const FILL_FINISH_FRACTION: f64 = 0.6;
/// Fill ceiling, packets/s; clears the validator's floor of ~3 (see "The geometry").
const FILL_MAX_RATE: u32 = 10;

// ── Timings, all derived from the window ─────────────────────────────────────

/// How long the Exit holds a detached Session (`incoming_session_pix_config.resume_window`).
///
/// Stated in the cluster's `--pix-config` rather than left to hoprnet's 60 s default, to keep runs
/// short. Ten seconds fits a clean close and a re-open (a few seconds locally, asserted below).
const RESUME_WINDOW: Duration = Duration::from_secs(10);

/// Re-open delay after the close in scenario 1: 10 % of the window. Not zero: the terminating
/// segment has to reach the Exit and be processed before an initiation can find the Session
/// detached, and a re-open that races it measures the race.
const QUICK_GAP: Duration = Duration::from_secs(1);

/// Re-open delay in scenario 2: 0.6 x the window. Still a quiet gap with the fill running, and it
/// leaves 4 s for the client open, the mixnet `StartSession` and the Exit's decision; 0.8 left ~2 s.
const PAUSED_GAP: Duration = Duration::from_millis(RESUME_WINDOW.as_millis() as u64 * 6 / 10);

/// Re-open delay in scenario 3: the window plus 5 s, which is far above the Exit's timer
/// resolution, the scrape and the cluster's latency, and so cheap insurance.
const EXPIRED_GAP: Duration = Duration::from_secs(RESUME_WINDOW.as_secs() + 5);

/// How long the re-open may take; the Entry is under the same window, so this must be < 10 s.
const REOPEN_BUDGET: Duration = Duration::from_secs(10);

/// How long after the window a `window_expired` may still be missing before the Exit is judged not
/// to have expired it.
const EXPIRY_GRACE: Duration = Duration::from_secs(10);

// ── Traffic ──────────────────────────────────────────────────────────────────

/// Payload per datagram, under `SESSION_MTU` so one write is one packet.
const SEND_CHUNK: usize = 512;

/// Delay between datagrams, `tests/pix.rs`'s validated 400 ms: one reply, one SURB, one share, so a
/// cycle takes `CYCLE_PACKETS x 0.4 s` = 115 s, comfortably more than a deposit round trip.
const SEND_INTERVAL: Duration = Duration::from_millis(400);

/// First byte of every datagram; the second is the leg, the next four a sequence number. The rest
/// is random, so ciphertexts stay unique.
const TAG_MAGIC: u8 = 0xA5;

/// Most datagrams of a previous leg tolerated in the resumed Session, both directions together.
///
/// Bounded by what can be in flight at the close: the replies the Exit has queued against the
/// Entry's SURB buffer (16 kB / 1452 B, ~11 SURBs) plus forward datagrams still crossing the mixnet
/// (2.5 per second times a few seconds, ~13), ~24, rounded up to 32. A count above it means the
/// resumed Session is replaying a stream, not leaking a tail.
const MAX_STRAYS: u64 = 32;

/// Useful shares to see before closing: a third of [`USEFUL_TARGET`].
const PROGRESS_SHARES: u64 = 64;

/// Swept cycles that end scenarios 1 and 2: the resumed SSA 1 plus two more, so the Session is
/// shown to continue past the cycle it kept.
const TARGET_SWEPT: u64 = 3;

/// Swept cycles that end the control: the fresh SSA 1 and its successor.
const TARGET_SWEPT_CONTROL: u64 = 2;

/// Budget for the first leg: ~26 s of traffic for the progress target, with generous slack.
const PROGRESS_BUDGET: Duration = Duration::from_secs(180);

/// Budget for the sweeps: three times the nominal time of every cycle waited on, as
/// `tests/pix_shapes.rs` does for its achieved-over-nominal factor, plus the deposit slack.
const SWEEP_BUDGET: Duration = Duration::from_secs(
    3 * (CYCLE_PACKETS * SEND_INTERVAL.as_millis() as u64 / 1000) * TARGET_SWEPT + 120,
);

/// Poll cadence on the Exit's metrics.
const POLL: Duration = Duration::from_secs(2);

/// How long to watch the Exit's Safe after its counters say it swept: a sweep is two on-chain
/// round trips behind the reply that completed the cycle.
const PAYMENT_SETTLE: Duration = Duration::from_secs(120);

/// Cycles of deposit budget the Entry carries: far above what a run consumes.
const BUDGETED_CYCLES: u64 = 20;

/// The gaps must fit the window with margin, and the control must be past it.
const _: () = {
    assert!(QUICK_GAP.as_secs() < RESUME_WINDOW.as_secs());
    assert!(PAUSED_GAP.as_millis() < RESUME_WINDOW.as_millis());
    assert!(RESUME_WINDOW.as_millis() - PAUSED_GAP.as_millis() >= 3000);
    assert!(EXPIRED_GAP.as_secs() > RESUME_WINDOW.as_secs());
    // Re-opening must be able to finish inside the window it relies on.
    assert!(REOPEN_BUDGET.as_secs() <= RESUME_WINDOW.as_secs());
    // Closing a third of the way in must leave the cycle in flight.
    assert!(PROGRESS_SHARES * 2 < USEFUL_TARGET);
    // A cycle must outlast three deposit round trips (the Entry's 30 s tracking window each).
    assert!(CYCLE_PACKETS as u128 * SEND_INTERVAL.as_millis() >= 3 * 30_000);
};

// ── The profile ──────────────────────────────────────────────────────────────

fn price() -> String {
    format!("{PRICE_PER_BYTE_WXHOPR:.10} wxHOPR")
}

/// This profile as the YAML `cluster::request_pix_settings` passes to `--pix-config`.
///
/// Only what this profile moves is named; the rest stays at the localcluster demo default. The
/// quota window is the default (0 .. 1 MiB), which admits [`QUOTA_PER_SSA`] (418 kB).
fn cluster_pix_yaml() -> String {
    format!(
        "\
num_ssa_parts: {PIX_POLYS}
ssa_part_size: {PIX_SHARES}
additional_shares: {PIX_ADDITIONAL_SHARES}
max_recovery_time: {recovery}s
fill_enabled: true
fill_max_rate: {FILL_MAX_RATE}
fill_finish_fraction: {FILL_FINISH_FRACTION}
price_per_byte: \"{price}\"
resume_window: {window}s
",
        recovery = MAX_RECOVERY_TIME.as_secs(),
        price = price(),
        window = RESUME_WINDOW.as_secs(),
    )
}

fn per_cycle() -> anyhow::Result<hoprd_integration_test::HoprBalance> {
    let price: hoprd_integration_test::HoprBalance = price().parse()?;
    Ok(price * QUOTA_PER_SSA)
}

fn entry_config() -> anyhow::Result<edgli::PixEntryConfig> {
    let per_cycle = per_cycle()?;
    Ok(edgli::PixEntryConfig {
        strategy: edgli::PixEntryStrategy {
            price_per_byte: price().parse()?,
            max_ssa_allocation: per_cycle * 2u64,
            max_spend_per_window: per_cycle * BUDGETED_CYCLES,
            spend_window: pix::SPEND_WINDOW,
            ..Default::default()
        },
        pool: edgli::PixEntryPool {
            max_deposit_tracking_time: pix::MAX_DEPOSIT_TRACKING_TIME,
            ..Default::default()
        },
        state_dir: None,
        state_password: "integration test".to_owned().into(),
    })
}

/// Point both halves of the run at this profile, before the cluster is brought up.
fn install_profile() {
    cluster::request_cluster_size(NODES);
    cluster::request_pix_settings(cluster_pix_yaml());
    pix::request_dimensions(edgli::PixGlobalConfig {
        num_ssa_parts: PIX_POLYS,
        ssa_part_size: PIX_SHARES,
        additional_shares: Some(PIX_ADDITIONAL_SHARES),
        ..Default::default()
    });
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn node_for(env: &IntegrationEnv, address: Address) -> anyhow::Result<NodeInfo> {
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == address)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no cluster node at {address}"))
}

/// Everything sampled before the first open, so every reading below is a delta over this run.
struct Baseline {
    exit: NodeInfo,
    exit_counters: PixCounters,
    exit_gate: ExitTelemetry,
    exit_safe: hoprd_integration_test::HoprBalance,
    entry_counters: PixCounters,
    entry_safe: hoprd_integration_test::HoprBalance,
    /// Datagrams of another leg read back in a later one; see [`TAG_MAGIC`].
    strays: std::sync::atomic::AtomicU64,
}

impl Baseline {
    async fn take(env: &IntegrationEnv) -> anyhow::Result<Self> {
        let exit = node_for(env, env.dest_for(HOPS)?)?;
        let entry_safe = env.entry_safe_balance().await?;
        let entry_counters = pix::sample_entry();
        let exit_gate = pix_exit::sample(&exit).await?;
        let exit_counters = pix::sample_exit(&exit).await?;
        let exit_safe = pix::node_balances(&exit).await?.safe;
        anyhow::ensure!(
            exit_gate.sessions_active().unwrap_or(0) == 0,
            "the Exit already supervises a PIX Session, so its Exit-wide counters would not be this \
             scenario's own"
        );
        Ok(Self {
            exit,
            exit_counters,
            exit_gate,
            exit_safe,
            entry_counters,
            entry_safe,
            strays: Default::default(),
        })
    }

    async fn exit_pix(&self) -> anyhow::Result<PixCounters> {
        Ok(self
            .exit_counters
            .delta(&pix::sample_exit(&self.exit).await?))
    }

    async fn exit_gate(&self) -> anyhow::Result<ExitTelemetry> {
        Ok(self.exit_gate.delta(&pix_exit::sample(&self.exit).await?))
    }

    fn entry_pix(&self) -> PixCounters {
        self.entry_counters.delta(&pix::sample_entry())
    }
}

fn resumption_summary(gate: &ExitTelemetry) -> String {
    let show = |event| format!("{event}={:?}", gate.resumptions(event));
    format!(
        "{} {} {} {} takeover_ineligible={:?}",
        show("detached"),
        show("taken_over"),
        show("window_expired"),
        show("refused"),
        gate.admission_rejections("takeover_ineligible"),
    )
}

type Halves = (ReadHalf<HoprSession>, WriteHalf<HoprSession>);

/// Offer paced datagrams and drain the echoes until `done` says so, the Session ends or `budget`
/// runs out. `done` is polled every [`POLL`].
///
/// Not `pump_halves`: that returns when a payload has been offered, and these scenarios are bounded
/// by what the Exit reports, not by a byte count.
async fn drive_until<F, Fut>(
    halves: &mut Halves,
    leg: u8,
    strays: &std::sync::atomic::AtomicU64,
    budget: Duration,
    mut done: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<bool>>,
{
    let (rx, tx) = (&mut halves.0, &mut halves.1);
    let write = async {
        let mut datagram = [0u8; SEND_CHUNK];
        let mut seq = 0u32;
        loop {
            rand::rng().fill(&mut datagram[..]);
            datagram[0] = TAG_MAGIC;
            datagram[1] = leg;
            datagram[2..6].copy_from_slice(&seq.to_be_bytes());
            seq = seq.wrapping_add(1);
            tx.write_all(&datagram)
                .await
                .context("writing to the PIX Session")?;
            tokio::time::sleep(SEND_INTERVAL).await;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let read = async {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = rx.read(&mut buf).await.context("reading the PIX Session")?;
            anyhow::ensure!(n > 0, "the Session ended while the scenario was driving it");
            // One read is one datagram on this Session; stepping by the datagram size also copes
            // with a read that coalesced several.
            for datagram in buf[..n].chunks(SEND_CHUNK) {
                if datagram.len() < 2 || datagram[0] != TAG_MAGIC || datagram[1] != leg {
                    let total = strays.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    tracing::info!(
                        leg, tag = ?datagram.get(..2), total,
                        "ignoring a datagram from another leg"
                    );
                }
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let poll = async {
        loop {
            if done().await? {
                return anyhow::Ok(());
            }
            tokio::time::sleep(POLL).await;
        }
    };
    tokio::select! {
        r = write => r,
        r = read => r,
        r = poll => r,
        _ = tokio::time::sleep(budget) => anyhow::bail!("not done within {budget:?}"),
    }
}

async fn open(env: &IntegrationEnv, pseudonym: HoprPseudonym) -> anyhow::Result<(Halves, Address)> {
    let (session, exit) = env
        .open_pix_session_with_pseudonym(HOPS, HOPS, Some(pseudonym))
        .await?;
    Ok((tokio::io::split(session), exit))
}

/// Close cleanly: shutting the write half sends the terminating segment the Exit detaches on.
/// Consumes the halves, so the client side is gone when this returns.
async fn close(mut halves: Halves) -> anyhow::Result<()> {
    halves.1.shutdown().await.context("closing the PIX Session")
}

/// Poll `pred` over the Exit's resume-window metrics until it holds or `budget` passes.
async fn await_gate(
    base: &Baseline,
    budget: Duration,
    what: &str,
    pred: impl Fn(&ExitTelemetry) -> bool,
) -> anyhow::Result<ExitTelemetry> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let gate = base.exit_gate().await?;
        if pred(&gate) {
            return Ok(gate);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "{what} did not happen within {budget:?} ({})",
            resumption_summary(&gate)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// First leg, shared by every scenario: open under `pseudonym`, drive until SSA 1 is deposited and
/// confirmed and the Exit shows share progress short of recovery, then close cleanly.
///
/// Returns when the client has closed; the instant is what the gaps are measured from. Also the
/// Entry's deposit count at the close.
async fn first_leg(
    env: &IntegrationEnv,
    base: &Baseline,
    pseudonym: HoprPseudonym,
) -> anyhow::Result<(tokio::time::Instant, u64)> {
    let (mut halves, exit_addr) = open(env, pseudonym).await?;
    anyhow::ensure!(
        exit_addr == base.exit.address,
        "opened to an unexpected Exit"
    );

    drive_until(&mut halves, 1, &base.strays, PROGRESS_BUDGET, || async {
        let gate = base.exit_gate().await?;
        let pix = base.exit_pix().await?;
        anyhow::ensure!(
            pix.keys_recovered().unwrap_or(0) == 0 && gate.cycles("recovered").unwrap_or(0) == 0,
            "SSA 1 recovered before the reconnect: the geometry no longer leaves the cycle in \
             flight at the close ({})",
            gate.summary()
        );
        Ok(gate.shares("useful").unwrap_or(0) >= PROGRESS_SHARES
            && pix.deposits_confirmed().unwrap_or(0) >= 1)
    })
    .await
    .context("first leg: waiting for SSA 1 to be deposited and the Exit to show progress")?;

    let useful = base.exit_gate().await?.shares("useful");
    let deposits = base.entry_pix().deposits().unwrap_or(0);
    tracing::info!(?useful, deposits, "closing the Session mid-cycle");
    close(halves).await?;
    Ok((tokio::time::Instant::now(), deposits))
}

/// Re-open under `pseudonym` and return within [`REOPEN_BUDGET`].
async fn reopen(
    env: &IntegrationEnv,
    pseudonym: HoprPseudonym,
) -> anyhow::Result<(Halves, Duration)> {
    let started = std::time::Instant::now();
    let (halves, _) = tokio::time::timeout(REOPEN_BUDGET, open(env, pseudonym))
        .await
        .map_err(|_| anyhow::anyhow!("re-opening did not complete within {REOPEN_BUDGET:?}"))??;
    let took = started.elapsed();
    tracing::info!(?took, "re-opened under the same pseudonym");
    Ok((halves, took))
}

/// Drive until the Exit has swept `target` cycles, then let its Safe catch up. Returns the cycles
/// the Safe gained, as a whole number, once the balance settles.
async fn sweep_and_settle(
    halves: &mut Halves,
    base: &Baseline,
    target: u64,
) -> anyhow::Result<u64> {
    drive_until(halves, 2, &base.strays, SWEEP_BUDGET, || async {
        let pix = base.exit_pix().await?;
        tracing::info!("exit PIX counters: {}", pix.summary());
        Ok(pix.sweeps().unwrap_or(0) >= target)
    })
    .await
    .with_context(|| format!("waiting for {target} swept cycles"))?;

    let per_cycle = per_cycle()?;
    let deadline = tokio::time::Instant::now() + PAYMENT_SETTLE;
    loop {
        let gained = pix::node_balances(&base.exit).await?.safe - base.exit_safe;
        let cycles = pix::completed_cycles(gained, per_cycle).with_context(|| {
            format!("the Exit's Safe gained {gained}, not a whole multiple of {per_cycle}")
        })?;
        if cycles >= target || tokio::time::Instant::now() >= deadline {
            return Ok(cycles);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Log how many datagrams of the first leg leaked into the second, and bound it by [`MAX_STRAYS`].
fn assert_strays_small(base: &Baseline) {
    let strays = base.strays.load(std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        strays,
        MAX_STRAYS,
        "datagrams of the previous leg read in the resumed one"
    );
    assert!(
        strays <= MAX_STRAYS,
        "{strays} datagrams of the first leg were delivered into the second, above the {MAX_STRAYS} \
         that can be in flight at the close"
    );
}

/// What both resumed scenarios assert at the end, with the resumed Session still open.
async fn assert_resumed(
    env: &IntegrationEnv,
    base: &Baseline,
    swept_cycles: u64,
    open_time: Duration,
) -> anyhow::Result<()> {
    // Entry before Exit: the Exit's `requested` can only have grown since the deposits were counted.
    let entry = base.entry_pix();
    let entry_safe_after = env.entry_safe_balance().await?;
    let exit = base.exit_pix().await?;
    let gate = base.exit_gate().await?;
    let recovered = pix::node_balances(&base.exit).await?.safe - base.exit_safe;
    let per_cycle = per_cycle()?;
    let deposits = entry.deposits().unwrap_or(0);
    let requested = gate.cycles("requested").unwrap_or(0);
    tracing::info!(
        %recovered, open_time = ?open_time,
        "entry: {} | exit: {} | gate: {} | resumption: {}",
        entry.summary(), exit.summary(), gate.summary(), resumption_summary(&gate),
    );

    assert!(open_time < REOPEN_BUDGET, "the re-open took {open_time:?}");
    assert_strays_small(base);
    assert!(
        exit.observable() && gate.observable(),
        "the Exit exported no PIX series"
    );

    // The headline: one deposit per cycle the Exit asked for. A second deposit for SSA 1 would put
    // the Entry one over the requests.
    assert!(
        deposits <= requested,
        "the Entry deposited {deposits} times against {requested} cycles the Exit requested: SSA 1 \
         was paid twice, so the resumed Session did not keep its cycle ({})",
        entry.summary()
    );
    assert!(
        deposits >= exit.sweeps().unwrap_or(0).min(swept_cycles),
        "fewer deposits ({deposits}) than swept cycles ({swept_cycles})"
    );

    assert!(
        gate.resumptions("detached").unwrap_or(0) >= 1,
        "the Exit never detached the closed Session ({}): the terminating segment did not reach it",
        resumption_summary(&gate)
    );
    assert_eq!(
        gate.resumptions("taken_over"),
        Some(1),
        "expected exactly one takeover ({})",
        resumption_summary(&gate)
    );
    assert_eq!(
        gate.resumptions("window_expired").unwrap_or(0),
        0,
        "the window expired though the client re-opened inside it ({})",
        resumption_summary(&gate)
    );
    assert_eq!(
        gate.resumptions("refused").unwrap_or(0),
        0,
        "{}",
        resumption_summary(&gate)
    );
    assert_eq!(
        gate.admission_rejections("takeover_ineligible")
            .unwrap_or(0),
        0,
        "the Exit judged the re-open ineligible for takeover ({})",
        resumption_summary(&gate)
    );

    assert_eq!(
        exit.deposits_timed_out().unwrap_or(0),
        0,
        "the Exit timed out waiting for a deposit ({})",
        exit.summary()
    );
    assert!(
        gate.closure_reasons().is_empty(),
        "the Exit closed a Session on a PIX failure: {:?}",
        gate.closure_reasons()
    );
    assert_eq!(
        gate.sessions_active(),
        Some(1),
        "expected exactly one Session supervised after the takeover (the old one is the new one)"
    );

    // Paid exactly once per swept cycle, and the Entry's Safe paid at least that.
    assert!(
        swept_cycles >= TARGET_SWEPT,
        "swept only {swept_cycles} cycles"
    );
    assert_eq!(
        recovered,
        per_cycle * swept_cycles,
        "the Exit's Safe gained {recovered}, not {swept_cycles} x {per_cycle}"
    );
    assert!(
        exit.sweeps().unwrap_or(0) >= swept_cycles,
        "the Safe gained more cycles ({swept_cycles}) than the Exit counted sweeps ({:?})",
        exit.sweeps()
    );
    let safe_drop = base.entry_safe - entry_safe_after;
    assert!(
        safe_drop >= recovered,
        "the Entry's Safe fell by {safe_drop} while the Exit's gained {recovered}"
    );
    Ok(())
}

/// Scenarios 1 and 2 differ in the gap only.
async fn resumed_scenario(gap: Duration) -> anyhow::Result<()> {
    install_profile();
    let per_cycle = per_cycle()?;
    let env = IntegrationEnv::setup_pix_with(entry_config()?).await?;
    let base = Baseline::take(&env).await?;
    anyhow::ensure!(
        base.entry_safe >= per_cycle * BUDGETED_CYCLES,
        "the Entry's Safe holds {} against a {BUDGETED_CYCLES}-cycle budget",
        base.entry_safe
    );

    let pseudonym = random_pseudonym();
    let (closed_at, deposits_at_close) = first_leg(&env, &base, pseudonym).await?;
    tokio::time::sleep_until(closed_at + gap).await;

    // After the pause: the cycle must still be in flight. Fill ran meanwhile, and this is the check
    // that it did not finish the cycle on its own while no client was there.
    let useful = base.exit_gate().await?.shares("useful").unwrap_or(0);
    let recovered_early = base.exit_gate().await?.cycles("recovered").unwrap_or(0);
    tracing::info!(
        useful,
        USEFUL_TARGET,
        ?gap,
        "the Exit's progress when the client returns"
    );
    assert!(
        recovered_early == 0 && useful < USEFUL_TARGET,
        "the Exit's fill finished the cycle during the {gap:?} pause ({useful} of \
         {USEFUL_TARGET} useful shares), so the re-open did not land mid-cycle"
    );

    let (mut halves, open_time) = reopen(&env, pseudonym).await?;
    let swept = sweep_and_settle(&mut halves, &base, TARGET_SWEPT).await?;
    assert!(
        base.entry_pix().deposits().unwrap_or(0) >= deposits_at_close,
        "the Entry's deposit count went backwards"
    );
    assert_resumed(&env, &base, swept, open_time).await?;
    let _ = close(halves).await;
    Ok(())
}

// ── The scenarios ────────────────────────────────────────────────────────────

/// Re-open at once under the same pseudonym: the funded cycle is kept, the client pays no second
/// deposit for SSA 1, and the Session goes on to sweep further cycles.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX binaries with the resume window, a localcluster with `resume_window`, and a chain"]
async fn a_session_resumed_within_the_window_keeps_its_paid_cycle() -> anyhow::Result<()> {
    resumed_scenario(QUICK_GAP).await
}

/// As above, after a pause of 0.6 x the window: the funded cycle survives a quiet gap, and the
/// Exit's fill, which keeps running while the Session is detached, does not exhaust it meanwhile.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX binaries with the resume window, a localcluster with `resume_window`, and a chain"]
async fn a_session_resumed_after_a_pause_inside_the_window_continues() -> anyhow::Result<()> {
    resumed_scenario(PAUSED_GAP).await
}

/// The control. Past the window the Exit lets the held Session go, the re-open starts a fresh
/// cycle, and **the client pays again**: this is the designed boundary, not a defect.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX binaries with the resume window, a localcluster with `resume_window`, and a chain"]
async fn a_session_reopened_after_the_window_starts_a_new_cycle() -> anyhow::Result<()> {
    install_profile();
    let per_cycle = per_cycle()?;
    let env = IntegrationEnv::setup_pix_with(entry_config()?).await?;
    let base = Baseline::take(&env).await?;

    let pseudonym = random_pseudonym();
    let (closed_at, deposits_first) = first_leg(&env, &base, pseudonym).await?;
    anyhow::ensure!(deposits_first >= 1, "the first Session never deposited");

    tokio::time::sleep_until(closed_at + EXPIRED_GAP).await;
    let expired = await_gate(&base, EXPIRY_GRACE, "the window expiring", |g| {
        g.resumptions("window_expired").unwrap_or(0) >= 1
    })
    .await?;
    tracing::info!("before the re-open: {}", resumption_summary(&expired));

    let (mut halves, open_time) = reopen(&env, pseudonym).await?;
    assert!(open_time < REOPEN_BUDGET);
    let swept = sweep_and_settle(&mut halves, &base, TARGET_SWEPT_CONTROL).await?;

    let entry = base.entry_pix();
    let entry_safe_after = env.entry_safe_balance().await?;
    let exit = base.exit_pix().await?;
    let gate = base.exit_gate().await?;
    let recovered = pix::node_balances(&base.exit).await?.safe - base.exit_safe;
    let deposits = entry.deposits().unwrap_or(0);
    tracing::info!(
        %recovered, deposits_first, ?open_time,
        "entry: {} | exit: {} | resumption: {} | closures: {:?}",
        entry.summary(), exit.summary(), resumption_summary(&gate), gate.closure_reasons(),
    );

    assert_strays_small(&base);
    assert_eq!(
        gate.resumptions("window_expired"),
        Some(1),
        "expected the held Session to expire exactly once ({})",
        resumption_summary(&gate)
    );
    assert_eq!(
        gate.resumptions("taken_over").unwrap_or(0),
        0,
        "the Exit took over a Session whose window had expired ({})",
        resumption_summary(&gate)
    );

    // The client paid again: the first Session's deposit, plus one per cycle the fresh one swept.
    assert!(
        deposits >= deposits_first + TARGET_SWEPT_CONTROL,
        "the Entry made {deposits} deposits; {deposits_first} were for the first Session and the \
         fresh one swept {TARGET_SWEPT_CONTROL}, so SSA 1 was not paid for again"
    );

    // Nothing is swept twice, and the Safe gains only for cycles actually recovered.
    assert!(swept >= TARGET_SWEPT_CONTROL, "swept only {swept} cycles");
    assert_eq!(
        recovered,
        per_cycle * swept,
        "the Exit's Safe gained {recovered}"
    );
    assert!(
        swept <= deposits,
        "the Exit's Safe gained {swept} cycles from {deposits} deposits: a deposit was swept twice"
    );
    assert!(
        swept <= exit.sweeps().unwrap_or(0),
        "more cycles gained ({swept}) than the Exit swept ({:?})",
        exit.sweeps()
    );
    assert!(
        base.entry_safe - entry_safe_after >= recovered,
        "the Entry's Safe fell by less than the Exit's gained"
    );
    let _ = close(halves).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_yaml_states_the_window_and_the_geometry() {
        let yaml = cluster_pix_yaml();
        for key in [
            "num_ssa_parts: 24",
            "ssa_part_size: 8",
            "additional_shares: 4",
            "resume_window: 10s",
            "max_recovery_time: 480s",
        ] {
            assert!(yaml.contains(key), "missing `{key}` in:\n{yaml}");
        }
    }

    /// The deposit has to sit under the Exit's per-deposit ceiling (10 wxHOPR) and well above zero.
    #[test]
    fn a_deposit_is_a_few_wxhopr() {
        let cost = QUOTA_PER_SSA as f64 * PRICE_PER_BYTE_WXHOPR;
        assert!((1.0..10.0).contains(&cost), "{cost}");
    }

    /// The fill ceiling clears the validator's floor for the default 1 MiB quota window.
    #[test]
    fn the_fill_ceiling_clears_the_validator_floor() {
        let widest = (1024 * 1024 / QUOTA_BYTES_PER_SHARE) as f64;
        let horizon = MAX_RECOVERY_TIME.as_secs_f64() * FILL_FINISH_FRACTION;
        assert!(FILL_MAX_RATE as f64 >= (widest * 1.25 / horizon).ceil());
    }
}
