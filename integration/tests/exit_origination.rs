//! Does the exit keep originating packets when one of its return paths can never be resolved?
//!
//! # The incident this reproduces
//!
//! On 2026-08-16 a `gnosis_vpn` exit node stopped originating packets at 20:46:18 UTC and never
//! originated another one. It kept forwarding, kept acknowledging, kept receiving, kept its twelve
//! peers, kept redeeming tickets, and reported healthy to its Docker healthcheck for the next
//! 1h44m. It established zero sessions in that time. There was no panic, no error line, no
//! restart, no OOM, and two sibling exits on the identical image digest absorbed the same traffic
//! and stayed healthy.
//!
//! The signature, sampled three times over twenty minutes on the wedged node:
//!
//! ```text
//! hopr_packets_count{type="sent"}       460497  460497  460497   <- frozen
//! hopr_packets_count{type="forwarded"} 2314830 2315762 ...       <- climbing
//! hopr_protocol_ack_sent_count         2858694 2859650 ...       <- climbing
//! ```
//!
//! # The mechanism
//!
//! Every packet a node originates passes through one routing-resolution stage before SPHINX
//! encode. When resolution finds no SURB for a return route it retries every 5 ms, on the
//! assumption that the counterparty will deliver more. Once that counterparty is gone the
//! assumption never holds again, and the retry has no bound.
//!
//! The stage preserves submission order — entry-side reassembly needs it — so it withholds
//! completed work behind an unfinished item. One packet that can never resolve therefore withholds
//! *every* packet the node would originate, for as long as the process lives. Forwarding and
//! acknowledgement run on the ingress path and never touch this stage, which is why they carry on
//! and why nothing looks wrong.
//!
//! Reproduced at unit level in `hopr_transport::path::resolve`; this scenario is the end-to-end
//! proof on a real cluster.
//!
//! # How the unresolvable packet is created here
//!
//! The exit's own keep-alive stream keeps emitting return-routed packets for a Session until the
//! Session slot is closed, each on one of the Session's SURBs. Abandoning a session at the entry —
//! without closing it, so the exit never tears the slot down — stops the SURB supply while the
//! keep-alives go on: once the last SURB is spent, the next keep-alive has nothing to carry it.
//!
//! An admitted Session's SURB ring has no idle timeout, so on v5 the SURBs are spent, not expired.
//! An exit that holds no SURBs back spends them all on the victim's warm-up echo. One that holds
//! its data back for a keep-alive reserve leaves the last few to the keep-alives, which spend one per
//! notify period (see `SURB_NOTIFY_PERIOD`).
//!
//! # On v5 the fault no longer arms, and that is what the v5 run checks
//!
//! Since hoprnet#8629 an exit sends its SURB-level notification only to an entry that has shown it
//! reads them (a keep-alive carrying SURBs), and only while its own SURB estimate is above zero, so
//! a notification never goes into an empty store. The victim here is opened without a balancer, so
//! it never sends a keep-alive: the exit never notifies it, holds its data back for the reserve, and
//! the victim's last SURBs stay unspent. No return route goes unresolvable.
//!
//! The v5 run therefore checks that guarantee instead of the starvation: after the same
//! abandonment the exit must never log a starved return route, and must keep originating for the
//! canary. An exit without the gate brings the old behaviour back (a notification every
//! `SURB_NOTIFY_PERIOD` spends the twelve reserve SURBs within a minute, and the next one starves),
//! so the run then fails on the starvation evidence. The resolver's bounded wait, which a starved
//! packet would still exercise, stays covered at unit level in `hopr_transport::path::resolve`.
//! The v4 run is unchanged: its exit has no such gate, and the fault must arm there.
//!
//! The production `pseudonyms_lifetime` is 600 s, which is longer than the Session idle timeout, so
//! the scenario shortens it to the minimum the config validator accepts. That is the only setting
//! changed, it is the same value a config file could state, and it compresses a timer rather than
//! altering an ordering.
//!
//! Run with:
//!
//! ```bash
//! cargo test --test exit_origination -- --include-ignored --test-threads=1
//! ```

use std::time::Duration;

use hoprd_integration_test::{
    Address, IntegrationEnv,
    cluster::{NodeInfo, request_cluster_size, request_node_env},
    origination,
    pump::{PumpOpts, pump_halves, tagged_payload},
};

/// Three nodes: the exit plus two peers, which is enough for the exit to keep forwarding and
/// acknowledging other nodes' probe traffic while its own origination is under observation.
const NODES: usize = 3;

/// How long the exit holds a pseudonym's SURBs after the last one arrives.
///
/// The production default is 600 s. `MINIMUM_SURB_LIFETIME` in `hopr-protocol-hopr` is 30 s, and
/// the override is floored at it, so this is the shortest the node will accept and the shortest a
/// config file could ask for.
const SURB_PSEUDONYM_LIFETIME: Duration = Duration::from_secs(30);

const SURB_LIFETIME_ENV: &str = "HOPR_INTERNAL_SURB_PSEUDONYM_LIFETIME_MS";

/// Short enough for the keep-alives to empty the victim's last SURBs inside [`WEDGE_WINDOW`]. A v5
/// exit holds its data back while a Session's SURB stock is down to the keep-alive reserve (1 % of
/// the target the entry announced, at least 32, at most a quarter of it), so the victim's warm-up no
/// longer drains its pool: twelve SURBs (a quarter of its target of 50) are left, and only the
/// keep-alives spend them, one per period. At 5 s they are gone within a minute of the abandonment.
/// An admitted Session's SURB ring has no idle timeout, so this, not the pseudonym lifetime, is how
/// its return route goes unresolvable here.
/// v5 only: on v4 the victim never holds SURBs, and the longer period starves the idle canary.
#[cfg(feature = "v5")]
const SURB_NOTIFY_PERIOD: Duration = Duration::from_secs(5);

#[cfg(feature = "v5")]
const SURB_NOTIFY_PERIOD_ENV: &str = "HOPR_SESSION_SURB_BALANCE_NOTIFY_PERIOD_MS";

/// What the exit logs when a return route had no SURB for the whole resolution wait.
///
/// Its presence is the proof that the fault this scenario builds actually reached the code under
/// test. Without it, a green run is equally consistent with a fault that never armed.
///
/// Matches `hopr_transport::path::resolve`; a substring rather than the whole line, so field order
/// and formatting can change without silently disarming the check.
const STARVATION_EVIDENCE: &str = "no SURB for its return path within the wait";

/// How long to wait for the exit to notice the SURBs are gone and emit a keep-alive into the void.
///
/// The exit notifies its SURB level on `surb_balance_notify_period`, set to 60 s on v5, so the first
/// unresolvable packet lands somewhere in the 30–90 s after the session is abandoned. Waiting the
/// full period plus the lifetime plus a margin makes the window deterministic.
const WEDGE_WINDOW: Duration = Duration::from_secs(105);

/// Payload used to establish that a session works, in both directions.
///
/// Small on purpose: its job is to prove liveness, not to measure throughput, and a small transfer
/// leaves the session quiet quickly so the abandonment lands on an idle session rather than one
/// still draining.
const LIVENESS_BYTES: usize = 256 * 1024;

const PUMP_TIMEOUT: Duration = Duration::from_secs(120);

/// Samples taken of the exit's counters, and the gap between them.
///
/// Four samples 20 s apart span 60 s. A single pair cannot tell a stall from a node that happened
/// to be quiet between two instants; a minute of wall clock covers the exit's own periodic traffic
/// several times over.
const SAMPLES: usize = 4;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(20);

const CANARY_PHASE: u8 = 1;
const VICTIM_PHASE: u8 = 2;
const RECOVERY_PHASE: u8 = 3;

/// Arrival floor for the canary transfers.
///
/// The session is unreliable — there is no retransmission — so 100 % is unreachable by
/// construction, and the question this scenario asks is "did anything come back at all" rather than
/// "how much". A wedged exit delivers nothing whatsoever, so any tolerant floor separates the two.
const MIN_CANARY_ARRIVAL_PCT: f64 = 50.0;

/// The exit must keep originating after one of its return paths becomes permanently unresolvable.
///
/// Fails on a stack whose routing resolution retries SURB starvation without a bound: the exit's
/// `sent` counter freezes while `forwarded`, `received` and `acks_sent` keep climbing, and the
/// canary session — established and working before the fault — goes silent without ever erroring.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn exit_should_keep_originating_when_a_return_path_becomes_unresolvable() -> anyhow::Result<()>
{
    let size = request_cluster_size(NODES);
    anyhow::ensure!(size >= 2, "need at least an exit and one peer, got {size}");
    request_node_env([
        (
            SURB_LIFETIME_ENV.to_string(),
            SURB_PSEUDONYM_LIFETIME.as_millis().to_string(),
        ),
        #[cfg(feature = "v5")]
        (
            SURB_NOTIFY_PERIOD_ENV.to_string(),
            SURB_NOTIFY_PERIOD.as_millis().to_string(),
        ),
        // The Session-layer lines that say whether the exit's keep-alive was ever spawned and
        // whether its SURBs expired are `debug`. Without them a run that does not reproduce cannot
        // be told apart from one where the emitter never existed — which is exactly how the first
        // attempt was read.
        (
            "RUST_LOG".to_string(),
            "info,hopr_transport_session=debug,hopr_protocol_hopr=debug".to_string(),
        ),
    ]);

    let env = IntegrationEnv::setup().await?;

    // The canary is opened *first* and deliberately outlives the fault. Opening it afterwards would
    // conflate two different failures: a wedged exit cannot answer a new session either, so the
    // scenario would fail in `open_unreliable_session` and never reach the assertion it is about.
    let (canary, exit_addr) = env.open_unreliable_session_paths(0, 0).await?;
    let (mut canary_rx, mut canary_tx) = tokio::io::split(canary);
    let exit = exit_node(&env, exit_addr)?;

    let baseline = pump_halves(
        &mut canary_rx,
        &mut canary_tx,
        &tagged_payload(CANARY_PHASE, LIVENESS_BYTES),
        "canary baseline",
        PUMP_TIMEOUT,
        PumpOpts {
            phase: Some(CANARY_PHASE),
            ..PumpOpts::default()
        },
    )
    .await?;
    anyhow::ensure!(
        baseline.arrival_pct() >= MIN_CANARY_ARRIVAL_PCT,
        "the canary session was not working before the fault was introduced ({:.1}% arrived), so \
         nothing this scenario measures afterwards means anything",
        baseline.arrival_pct(),
    );
    tracing::info!(arrival = baseline.arrival_pct(), %exit_addr, "canary established");

    // A second session to the same exit, pumped once so the exit holds SURBs for its pseudonym and
    // has a live Session slot with a keep-alive stream attached to it.
    //
    // Opened without an entry-side SURB balancer, because with one the abandonment does not
    // actually abandon anything: the balancer's keep-alives go on delivering SURBs to the exit
    // after the application has stopped, so the exit's pool never expires. Measured — a session
    // abandoned for 105 s with the balancer on kept originating throughout.
    let (victim, victim_exit) = env.open_unreliable_session_unbalanced(0, 0).await?;
    anyhow::ensure!(
        victim_exit == exit_addr,
        "victim and canary must share an exit for the victim's fault to be able to affect the \
         canary: canary {exit_addr}, victim {victim_exit}"
    );
    let (mut victim_rx, mut victim_tx) = tokio::io::split(victim);
    pump_halves(
        &mut victim_rx,
        &mut victim_tx,
        &tagged_payload(VICTIM_PHASE, LIVENESS_BYTES),
        "victim warm-up",
        PUMP_TIMEOUT,
        PumpOpts {
            phase: Some(VICTIM_PHASE),
            ..PumpOpts::default()
        },
    )
    .await?;

    // Abandon it. `forget` rather than `drop`: dropping the session writes a terminating segment,
    // which is exactly the clean close the exit needs to tear its slot down — and a torn-down slot
    // stops the keep-alive stream, which is the emitter this scenario depends on. An initiator that
    // crashes or is partitioned away sends no such segment.
    std::mem::forget(victim_rx);
    std::mem::forget(victim_tx);
    tracing::info!(
        wait = ?WEDGE_WINDOW,
        "victim session abandoned without a close; waiting for its SURBs to expire under the exit's \
         still-running keep-alive"
    );
    tokio::time::sleep(WEDGE_WINDOW).await;

    // Observe the exit while pumping the canary, so `sent` has something to originate for and the
    // ingress counters have something to move against.
    let recovery_payload = tagged_payload(RECOVERY_PHASE, LIVENESS_BYTES);
    let (verdict, recovery) = tokio::join!(
        origination::watch(&exit, SAMPLES, SAMPLE_INTERVAL),
        pump_halves(
            &mut canary_rx,
            &mut canary_tx,
            &recovery_payload,
            "canary after the fault",
            PUMP_TIMEOUT,
            PumpOpts {
                phase: Some(RECOVERY_PHASE),
                ..PumpOpts::default()
            },
        ),
    );
    let verdict = verdict?;
    tracing::info!("exit origination {}", verdict.summary());

    // The control first: without evidence the exit was busy, a frozen `sent` says nothing, since an
    // idle node originates nothing either.
    anyhow::ensure!(
        verdict.processed_inbound(),
        "the exit's ingress counters did not move either, so this run cannot tell a stalled node \
         from an idle one and proves nothing — {}",
        verdict.summary(),
    );

    let recovery_arrival = recovery.map(|t| t.arrival_pct()).unwrap_or(0.0);

    anyhow::ensure!(
        verdict.originated(),
        "the exit stopped originating packets: {} — it kept forwarding, receiving and acknowledging \
         throughout, so it was demonstrably alive and simply produced nothing of its own. The canary \
         session, working at {:.1}% before the fault, returned {recovery_arrival:.1}% afterwards. One \
         return path that could not be resolved took every other packet on the node with it.",
        verdict.summary(),
        baseline.arrival_pct(),
    );

    anyhow::ensure!(
        recovery_arrival >= MIN_CANARY_ARRIVAL_PCT,
        "the exit was still originating, but the canary session stopped delivering: {:.1}% before \
         the fault, {recovery_arrival:.1}% after — {}",
        baseline.arrival_pct(),
        verdict.summary(),
    );

    let starved = env
        .cluster()?
        .node_log_contains(exit_addr, STARVATION_EVIDENCE)?;

    // v4: everything above passes just as well on a run where the fault was never armed: if the
    // pseudonym-lifetime override did not reach the nodes, the victim's SURBs never expire, no
    // return route ever goes unresolvable, and a node that was never under test reports healthy.
    // That is the same class of failure as a participant silently running a build that predates the
    // change under test, and it is indistinguishable from success — so require positive evidence
    // that the exit actually hit, and survived, a starved return route.
    #[cfg(not(feature = "v5"))]
    anyhow::ensure!(
        starved,
        "the exit never reported a starved return route, so the fault was not armed and this run \
         says nothing about the stall. Check that {SURB_LIFETIME_ENV} reached the hoprd processes \
         (expected {:?}) — {}",
        SURB_PSEUDONYM_LIFETIME,
        verdict.summary(),
    );

    // v5: the exit must not have starved the abandoned victim at all (hoprnet#8629, see the module
    // docs). Starvation here means the exit sent a return-routed packet into an empty store, which
    // the notification gate exists to prevent.
    #[cfg(feature = "v5")]
    anyhow::ensure!(
        !starved,
        "the exit reported a starved return route for the abandoned session: it sent a return-routed \
         packet with no SURB left to carry it, which an exit that notifies only readers and only \
         above a zero SURB estimate never does (hoprnet#8629). It survived the starvation \
         (origination went on), but the gate that should have prevented it did not — {}",
        verdict.summary(),
    );

    Ok(())
}

/// The cluster node that terminated the session, by address.
fn exit_node(env: &IntegrationEnv, exit: Address) -> anyhow::Result<NodeInfo> {
    env.cluster()?
        .nodes
        .iter()
        .find(|node| node.address == exit)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("exit {exit} is not one of the cluster nodes"))
}
