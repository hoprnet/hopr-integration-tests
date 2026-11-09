//! Does a Session avoid relays that acknowledge everything and deliver nothing? (GNO-1183)
//!
//! # The question
//!
//! A relay that rejects the packets it should forward still answers them with a signed random
//! acknowledgement, so the per-peer ack rate stays near 1.0 and the entry keeps choosing it. This
//! scenario builds that relay on a cluster and asks whether the entry's graph, probing and
//! SURB evidence together learn to route around it.
//!
//! # The fault
//!
//! Entry, an Exit and three relays. Two relays lose their link to the Exit
//! ([`ClusterSummary::cut_link`]), in both directions, before the entry boots. Everything else stays up:
//!
//! * the entry still reaches both of them, so their pings and their acknowledgements to the entry
//!   are healthy: the ack rate gate cannot tell them from the third relay;
//! * a packet sent through either of them is acknowledged and then goes nowhere;
//! * their channels stay open and funded, so the graph still holds every edge.
//!
//! Chosen over killing the relay on purpose: a killed relay stops answering pings and is dropped
//! by the ordinary liveness checks, which is not this failure.
//!
//! # What is asserted
//!
//! After a learning period in which the entry probes with nothing else going on, a 1-hop session
//! (1 hop forward, 1 back) opens and carries a payload at the same arrival floor as the healthy
//! throughput test. The period is part of the contract under test: a client that has just
//! started cannot know, so it gets time to find out, but not the whole session.
//!
//! A control follows the pump: the healthy relay must carry nearly all of the forwarded packets.
//! Without it a pass could mean the cut never bit, for instance when the probing happened to pick
//! only the healthy relay.
//!
//! # Running
//!
//! ```text
//! LINE=v5 just dead-last-hop
//! LINE=v5 EDGLI_REF=<branch> just dead-last-hop      # an unmerged edge-client
//! LINE=v5 EDGLI_REF=<branch> NETWORK_GRAPH_REF=<branch> just dead-last-hop   # and an unreleased graph
//! ```
//!
//! Manual, one cluster, never in CI.

use std::time::Duration;

use hoprd_integration_test::{
    IntegrationEnv,
    cluster::{self, request_cluster_size, request_latency_profile, request_probe_interval},
    origination,
    pump::{PumpOpts, pump_halves, tagged_payload},
    relayers, step,
};

/// Healthy relay plus the ones whose link to the Exit is cut.
const RELAYS: usize = 3;

/// Link cuts need the latency relays, so a profile is required; flat, so no path wins on latency.
const FLAT_LATENCY: &str = "default: \"10ms\"\n";

/// Neighbour probing round and timeout: fast, because the question is how quickly the entry learns.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Time the entry gets to probe before the session opens.
const LEARNING_PERIOD: Duration = Duration::from_secs(120);
const PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
const PUMP_TIMEOUT: Duration = Duration::from_secs(300);
const PAYLOAD_PHASE: u8 = 0x31;
/// Same floor as the healthy throughput test.
const MIN_ARRIVAL_PCT: f64 = 99.0;
/// Share of forwarded packets the healthy relay must carry for the run to count.
const MIN_HEALTHY_SHARE: f64 = 0.9;

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn a_one_hop_session_should_avoid_relays_that_drop_everything_toward_the_exit()
-> anyhow::Result<()> {
    request_cluster_size(1 + RELAYS);
    request_latency_profile(FLAT_LATENCY);
    request_probe_interval(PROBE_INTERVAL);

    // Cut before the entry boots: the broken state is there from its first packet, so nothing it
    // learns at startup says otherwise. The exit has to be named up front for the same reason.
    let env = step(
        "setup",
        IntegrationEnv::setup_with(|cluster| {
            anyhow::ensure!(
                cluster.nodes.len() == 1 + RELAYS,
                "expected {} nodes, the cluster has {}",
                1 + RELAYS,
                cluster.nodes.len()
            );
            let exit = cluster.nodes[RELAYS].address;
            for relay in &cluster.nodes[1..RELAYS] {
                cluster.cut_link(relay.address, exit)?;
            }
            Ok(Some(exit))
        }),
    )
    .await?;
    let exit = env.dest_for(1)?;
    let relays = env.relayer_candidates(exit)?;
    let healthy = &relays[0];
    let dead = &relays[1..];
    tracing::info!(%exit, healthy = %healthy.address, dead = ?dead.iter().map(|n| n.address).collect::<Vec<_>>(), "links to the exit cut before the entry booted");
    let exit_node = env
        .cluster()?
        .nodes
        .iter()
        .find(|n| n.address == exit)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("exit {exit} is not a cluster node"))?;

    // The channels the entry will route over must be open on chain and funded, so the entry can see
    // them: the fault is in the traffic, not in the channel.
    for relay in dead {
        let open = cluster::open_outgoing_channels(relay).await?;
        let to_exit = open
            .iter()
            .find(|(peer, _)| peer.eq_ignore_ascii_case(&exit.to_string()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no open channel to the exit {exit}: {open:?}",
                    relay.address
                )
            })?;
        anyhow::ensure!(
            to_exit
                .1
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|v| v > 0.0),
            "the channel from {} to the exit holds no funds: {}",
            relay.address,
            to_exit.1
        );
        tracing::info!(relay = %relay.address, balance = %to_exit.1, "channel to the exit is open and funded");
    }

    step("learning", async {
        tokio::time::sleep(LEARNING_PERIOD).await;
        anyhow::Ok(())
    })
    .await?;

    tracing::info!(
        "entry's graph after learning:\n{}",
        env.graph_report().await
    );

    let session = step("open", env.open_unreliable_session(1)).await?;
    let (mut rx, mut tx) = tokio::io::split(session);

    let before = relayers::sample(&relays).await;
    anyhow::ensure!(
        before.covers(&relays),
        "a relay's metrics did not answer before the pump, so its share cannot be measured"
    );
    let exit_before = origination::sample(&exit_node).await?;
    let payload = tagged_payload(PAYLOAD_PHASE, PAYLOAD_BYTES);
    let transfer = step(
        "pump",
        pump_halves(
            &mut rx,
            &mut tx,
            &payload,
            "1-hop with dead relays",
            PUMP_TIMEOUT,
            PumpOpts {
                phase: Some(PAYLOAD_PHASE),
                ..PumpOpts::default()
            },
        ),
    )
    .await?;
    let after = relayers::sample(&relays).await;
    anyhow::ensure!(
        after.covers(&relays),
        "a relay's metrics did not answer after the pump, so its share cannot be measured"
    );
    let spread = relayers::spread(&before, &after);
    let exit_received = origination::sample(&exit_node)
        .await?
        .received
        .saturating_sub(exit_before.received);
    tracing::info!("forwarded packets per relay: {}", spread.summary());

    let healthy_share = spread
        .per_relayer
        .iter()
        .find(|(address, _)| *address == healthy.address)
        .map_or(0.0, |(_, n)| *n as f64 / spread.total.max(1) as f64);

    // The premise: nothing crossed a cut link. The exit only receives from relays, so what it received
    // must be what the healthy relay forwarded, give or take the cluster's own background traffic.
    let healthy_forwarded = spread
        .per_relayer
        .iter()
        .find(|(address, _)| *address == healthy.address)
        .map_or(0, |(_, n)| *n);
    tracing::info!(
        exit_received,
        healthy_forwarded,
        "packets the exit received against what the healthy relay forwarded"
    );
    anyhow::ensure!(
        exit_received <= healthy_forwarded + healthy_forwarded / 20 + 100,
        "the exit received {exit_received} packets but the healthy relay forwarded only {healthy_forwarded}: \
         packets crossed a cut link, so the fault is not 100% and the run proves nothing"
    );
    anyhow::ensure!(
        transfer.arrival_pct() >= MIN_ARRIVAL_PCT,
        "only {:.2}% of the payload returned, need >= {MIN_ARRIVAL_PCT:.0}%; the healthy relay carried \
         {:.0}% of the forwarded packets, so the session kept sending through relays that drop everything",
        transfer.arrival_pct(),
        healthy_share * 100.0,
    );
    anyhow::ensure!(
        healthy_share >= MIN_HEALTHY_SHARE,
        "the payload arrived but the healthy relay carried only {:.0}% of the forwarded packets \
         (need >= {:.0}%), so the cut did not bite and the run proves nothing: {}",
        healthy_share * 100.0,
        MIN_HEALTHY_SHARE * 100.0,
        spread.summary(),
    );
    Ok(())
}
