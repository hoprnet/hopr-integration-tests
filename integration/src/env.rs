//! The integration environment: a running cluster + a booted `edgli` edge client
//! with open channels. Each test builds one via [`IntegrationEnv::setup`] and gets
//! sessions via [`IntegrationEnv::open_unreliable_session`]; it tears down on drop.

use std::time::Duration;

use anyhow::Context as _;
use edgli::{
    BlockchainConnectorConfig, BlokliEndpoint, Edgli, EdgliInitState, PathPlannerConfig,
    hopr_lib::{
        HopRouting, HoprKeys, HoprSessionClientConfig, IdentityRetrievalModes,
        api::{
            chain::ChainKeyOperations as _,
            node::{HasChainApi as _, HasTransportApi as _, HoprSessionClientOperations},
            types::internal::channels::{ChannelEntry, ChannelStatus},
        },
        config::{HoprLibConfig, HostConfig, HostType, SafeModule},
        exports::transport::{
            HoprSession, SESSION_MTU, SURB_SIZE, SessionCapability, SessionTarget,
            SurbBalancerConfig,
        },
    },
    strategy::{EdgeStrategyKind, EligibilityConfig, IncentiveConfiguration, default_strategy_cfg},
    traits::EdgeNodeApi,
};

use crate::{
    Address,
    cluster::{
        self, ClusterHandle, ClusterSummary, ExtraInfo, NodeInfo, P2P_PORT_BASE, cluster_size,
    },
};

/// Edgli's P2P port — one slot beyond the cluster nodes.
fn edge_p2p_port() -> u16 {
    P2P_PORT_BASE + cluster_size() as u16
}

/// Strategies appended to the channel-lifecycle one `default_strategy_cfg` yields.
///
/// A struct rather than an extra parameter per strategy, so that adding a feature-gated one does
/// not put a `#[cfg]` on every call site of [`boot_edgli`]: the struct is always nameable and only
/// its fields vary.
#[derive(Default)]
struct ExtraStrategies {
    /// Entry-side PIX settlement. `None` leaves the node unable to pay, which is a legitimate
    /// configuration — it just cannot hold a PIX Session open.
    #[cfg(feature = "pix")]
    pix: Option<edgli::PixEntryConfig>,
}

impl ExtraStrategies {
    /// Append whatever was requested to the strategy list.
    fn apply(self, cfg: &mut edgli::strategy::MultiStrategyConfig) {
        #[cfg(feature = "pix")]
        if let Some(pix) = self.pix {
            tracing::info!(
                price_per_byte = %pix.strategy.price_per_byte,
                max_ssa_allocation = %pix.strategy.max_ssa_allocation,
                "entry will run the PIX deposit strategy"
            );
            cfg.strategies.push(EdgeStrategyKind::Pix(pix));
        }
        // With every optional strategy compiled out there is nothing to append, and the argument
        // would read as unused.
        #[cfg(not(feature = "pix"))]
        let _ = cfg;
    }
}

/// Response buffer a PIX Session provisions, in bytes. See [`IntegrationEnv::open_pix_session`] —
/// this is a share-delivery pipeline depth, not a throughput knob, and small is the point.
#[cfg(feature = "pix")]
const PIX_RESPONSE_BUFFER_BYTES: u64 = 16_000;
/// Ceiling on artificial SURB generation for a PIX Session, in bits per second.
///
/// Generous on purpose: with a buffer that small the balancer has to refill promptly, and this
/// caps the rate rather than the depth.
#[cfg(feature = "pix")]
const PIX_MAX_SURB_UPSTREAM_BITS: u64 = 20_000_000;

const PEER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
const LOCAL_CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(120);
const EXIT_PEER_PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// Outgoing channels the strategy aims to open for the Rotsee-style path — enough for
/// path-finding to have options, unrelated to any local cluster size.
const ROTSEE_TARGET_CHANNELS: usize = 3;

/// Session destinations, which differ by network.
enum Targets {
    /// Local cluster: 0-hop and 1-hop go to two distinct full-mesh peers.
    Local {
        dest_zero_hop: Address,
        dest_one_hop: Address,
    },
    /// Rotsee: both hop counts target the one configured exit node — only it runs
    /// the loopback exit service; path-finding fills the relay for the 1-hop case.
    Rotsee { exit: Address },
}

/// Edgli network tuning. Peers speak over loopback and probe successfully, so the node
/// announces local addresses and always probes them (both the local integration test and
/// the Rotsee-style test run against a local cluster).
struct NetTuning {
    connector: BlockchainConnectorConfig,
    announce_local: bool,
    prefer_local: bool,
    probe_local: bool,
    path_planner: PathPlannerConfig,
    strategy_tick: Duration,
    channel_open_timeout: Duration,
}

impl NetTuning {
    fn local() -> Self {
        Self {
            connector: connector_cfg(),
            announce_local: true,
            prefer_local: true,
            // Localcluster peers announce loopback/private addresses; probe them or no dial happens.
            probe_local: true,
            path_planner: PathPlannerConfig {
                min_ack_rate: 0.1, // local cluster probes succeed
                ..Default::default()
            },
            strategy_tick: Duration::from_secs(10),
            channel_open_timeout: LOCAL_CHANNEL_OPEN_TIMEOUT,
        }
    }
}

pub struct IntegrationEnv {
    // Field order matters for drop: edgli first (cancels tasks), then cluster.
    edgli: Edgli,
    _reactor: futures::future::AbortHandle,
    targets: Targets,
    /// `Some` for a local cluster we own; `None` for Rotsee (no local process).
    _cluster: Option<ClusterHandle>,
}

impl Drop for IntegrationEnv {
    fn drop(&mut self) {
        // `futures::AbortHandle`'s own Drop does not abort the reactor — stop it
        // explicitly. The struct's fields then drop in declaration order (edgli
        // cancels its tasks, then the cluster tears down).
        self._reactor.abort();
    }
}

impl IntegrationEnv {
    /// Bring up the local cluster, boot Edgli on the pre-funded extra identity, start
    /// the channel strategy, and wait until at least one outgoing channel is open.
    pub async fn setup() -> anyhow::Result<Self> {
        let cluster = cluster::bring_up().await?;
        let summary = cluster.summary.clone();
        let extra = summary.extras[0].clone();

        let (edgli, reactor) = boot_edgli(
            &summary.blokli_url,
            &extra,
            &NetTuning::local(),
            cluster_size(),
            ExtraStrategies::default(),
        )
        .await?;

        let (dest_zero_hop, dest_one_hop) = select_session_targets(&edgli).await?;

        Ok(Self {
            edgli,
            _reactor: reactor,
            targets: Targets::Local {
                dest_zero_hop,
                dest_one_hop,
            },
            _cluster: Some(cluster),
        })
    }

    /// As [`Self::setup`], but with the cluster configured for PIX and the entry running the PIX
    /// deposit strategy, budgeted at `budget` wxHOPR of deposits.
    ///
    /// `budget` is the parameter that matters, and it is a *budget* rather than a balance: deposits
    /// are paid by the entry's Safe, which also holds its channel stakes, so a scenario cannot
    /// arrange for it to run dry without also deciding what the stakes leave behind. See
    /// [`crate::pix::entry_config`] for how a scenario picks it, and [`crate::pix`] for why
    /// nothing here funds the entry any more.
    #[cfg(feature = "pix")]
    pub async fn setup_pix(budget: crate::HoprBalance) -> anyhow::Result<Self> {
        cluster::request_pix();
        let cluster = cluster::bring_up().await?;
        let summary = cluster.summary.clone();
        let extra = summary.extras[0].clone();

        let (edgli, reactor) = boot_edgli(
            &summary.blokli_url,
            &extra,
            &NetTuning::local(),
            cluster_size(),
            ExtraStrategies {
                pix: Some(crate::pix::entry_config(budget)?),
            },
        )
        .await?;

        let (dest_zero_hop, dest_one_hop) = select_session_targets(&edgli).await?;

        Ok(Self {
            edgli,
            _reactor: reactor,
            targets: Targets::Local {
                dest_zero_hop,
                dest_one_hop,
            },
            _cluster: Some(cluster),
        })
    }

    /// Boot Edgli on a pre-funded identity read from `EDGLI_ROTSEE_*` env vars (see
    /// [`read_rotsee_env`]) and drive loopback sessions to a single configured exit node.
    /// No cluster is brought up — identity, Safe/module, blokli endpoint, and exit node all
    /// come from the environment (point them at a running local cluster via its
    /// `hoprd-localcluster status`; see `scripts/integration/rotsee-binchain.sh`). Uses
    /// local network tuning: loopback peers must be probed and the fast-chain connector is
    /// required (a public default connector times out against anvil).
    pub async fn setup_rotsee() -> anyhow::Result<Self> {
        let rotsee = read_rotsee_env()?;

        let (edgli, reactor) = boot_edgli(
            &rotsee.blokli_url,
            &rotsee.extra,
            &NetTuning::local(),
            ROTSEE_TARGET_CHANNELS,
            ExtraStrategies::default(),
        )
        .await?;

        // Rotsee relay nodes do not run the loopback exit service, so the exit must be a
        // node that does — supplied out of band via EDGLI_ROTSEE_EXIT_NODE.
        let exit = rotsee.exit_node.ok_or_else(|| {
            anyhow::anyhow!(
                "EDGLI_ROTSEE_EXIT_NODE is required: Rotsee relays do not run the loopback exit service"
            )
        })?;

        tracing::info!(%exit, "waiting for Rotsee exit node to be connected and probed");
        await_edgli_exit_peer_ready(&edgli, exit).await?;

        Ok(Self {
            edgli,
            _reactor: reactor,
            targets: Targets::Rotsee { exit },
            _cluster: None,
        })
    }

    /// Session destination for a given hop count, resolved against the network.
    fn dest_for(&self, hops: usize) -> anyhow::Result<Address> {
        Ok(match (&self.targets, hops) {
            (Targets::Local { dest_zero_hop, .. }, 0) => *dest_zero_hop,
            (Targets::Local { dest_one_hop, .. }, 1) => *dest_one_hop,
            (Targets::Rotsee { exit }, 0 | 1) => *exit,
            (_, n) => anyhow::bail!("unsupported hop count {n} (only 0 and 1 are supported)"),
        })
    }

    /// The cluster this env owns; an error for a Rotsee env, which has none.
    pub fn cluster(&self) -> anyhow::Result<&ClusterSummary> {
        self._cluster
            .as_ref()
            .map(|c| &c.summary)
            .ok_or_else(|| anyhow::anyhow!("no local cluster (Rotsee env)"))
    }

    /// Cluster nodes that can carry a return path for a session exiting at `exit` — every
    /// node but the exit itself. The cluster is a full mesh, so each of them has an open
    /// channel from the exit and is a legitimate first hop back towards Edgli.
    pub fn relayer_candidates(&self, exit: Address) -> anyhow::Result<Vec<NodeInfo>> {
        Ok(self
            .cluster()?
            .nodes
            .iter()
            .filter(|n| n.address != exit)
            .cloned()
            .collect())
    }

    /// Open an unreliable (`Segmentation`-only, no retransmission) session over
    /// `hops` relays to the exit node's built-in loopback service. Rate control
    /// is left ON.
    pub async fn open_unreliable_session(&self, hops: usize) -> anyhow::Result<HoprSession> {
        Ok(self.open_unreliable_session_paths(hops, hops).await?.0)
    }

    /// As [`Self::open_unreliable_session`], but with the forward and return hop counts
    /// chosen independently; also returns the exit address.
    ///
    /// A 0-hop forward paired with a 1-hop return isolates the return direction: the only
    /// packets any cluster node then forwards are replies travelling `exit → relayer →
    /// edgli`, so per-node forwarding counters read directly as a return-relayer
    /// histogram. The exit is selected by the forward hop count.
    pub async fn open_unreliable_session_paths(
        &self,
        forward_hops: usize,
        return_hops: usize,
    ) -> anyhow::Result<(HoprSession, Address)> {
        self.open_unreliable_session_paths_with(forward_hops, return_hops, true)
            .await
    }

    /// As [`Self::open_unreliable_session_paths`], but with no entry-side SURB balancer.
    ///
    /// Without it the entry delivers SURBs only organically, alongside data it is actually sending,
    /// so a session that goes quiet stops replenishing its exit. That is what makes an *abandoned*
    /// session model an initiator that has gone away: with the balancer on, its keep-alives go on
    /// feeding the exit SURBs long after the application has stopped, and the exit never starves —
    /// measured, on a session abandoned for 105 s that never missed a beat.
    pub async fn open_unreliable_session_unbalanced(
        &self,
        forward_hops: usize,
        return_hops: usize,
    ) -> anyhow::Result<(HoprSession, Address)> {
        self.open_unreliable_session_paths_with(forward_hops, return_hops, false)
            .await
    }

    async fn open_unreliable_session_paths_with(
        &self,
        forward_hops: usize,
        return_hops: usize,
        balance_surbs: bool,
    ) -> anyhow::Result<(HoprSession, Address)> {
        let dest = self.dest_for(forward_hops)?;
        let (session, _) = self
            .edgli
            .connect_to(
                dest,
                SessionTarget::ExitNode(0), // built-in loopback service
                HoprSessionClientConfig {
                    forward_path: HopRouting::try_from(forward_hops)?,
                    return_path: HopRouting::try_from(return_hops)?,
                    // Mirror gnosis_vpn-client's main (WG) data session — the real
                    // high-throughput config (gnosis_vpn-lib connection/options.rs +
                    // up/runner.rs). A too-low SURB mint ceiling starves the exit's
                    // return path under sustained downlink; provision it like production.
                    capabilities: SessionCapability::Segmentation | SessionCapability::NoDelay,
                    always_max_out_surbs: true,
                    surb_management: balance_surbs.then_some(SurbBalancerConfig {
                        // gnosis main: 10 MB response buffer, 16 Mb/s SURB upstream.
                        target_surb_buffer_size: 10_000_000 / SESSION_MTU as u64,
                        max_surbs_per_sec: 16_000_000 / (8 * SURB_SIZE as u64),
                        // Everything else stays at the default, because that is what the client
                        // does: `to_surb_balancer_config` in gnosis_vpn-lib sets exactly these two
                        // fields from the same formulas and then `..Default::default()`.
                        //
                        // In particular `sustain_on_return_path_loss` is left off. It was set here
                        // once, on the reasoning that a lost return relayer reads as a well-stocked
                        // exit because consumption is only observed when a reply gets home. That may
                        // be true, but no client sets it, so a scenario that did was measuring a
                        // configuration nobody runs.
                        ..SurbBalancerConfig::default()
                    }),
                    ..Default::default()
                },
            )
            .await?;
        Ok((session, dest))
    }

    /// Open a PIX Session: unreliable, opted into PIX, and tuned so SSA shares actually flow.
    ///
    /// Two departures from [`Self::open_unreliable_session_paths`], both required rather than
    /// preferred.
    ///
    /// **PIX is switched on through `Edgli::with_pix`**, which adds the `UsePIX` capability *and*
    /// fills the announced quota from this node's own `protocol.pix`. One call rather than two
    /// fields because either alone is a defect the node can only report at open time.
    ///
    /// **The SURB buffer is tiny** — 16 kB against the throughput tests' 10 MB — and this is the
    /// least obvious knob in the whole scenario. A PIX share is baked into a SURB when the SURB is
    /// *minted*, and the exit spends its buffer roughly in order, so the buffer is a pipeline delay
    /// between share generation and share delivery. At 10 MB the exit works through SURBs minted
    /// during SSA #1 long before it touches one carrying an SSA #2 share, so cycles stop completing
    /// — which reads as a broken strategy rather than a mis-set buffer. 16 kB is ~16 SURBs, so a
    /// new SSA's shares start landing within seconds of its commitment.
    ///
    /// Measured, not reasoned: the same scenario run at 10 MB recovered and swept **1** cycle where
    /// 16 kB recovered and swept **6**, and failed on the deposit-confirmation assertion.
    ///
    /// Both hop counts must be at least 1. The share encryption key is derived from the first
    /// relayer's acknowledgement, so a zero-hop path has nothing to derive it from and the Session
    /// is refused outright.
    #[cfg(feature = "pix")]
    pub async fn open_pix_session(
        &self,
        forward_hops: usize,
        return_hops: usize,
    ) -> anyhow::Result<(HoprSession, Address)> {
        anyhow::ensure!(
            forward_hops >= 1 && return_hops >= 1,
            "PIX needs at least one relay on each path (got {forward_hops} forward, \
             {return_hops} return): the share encryption key derives from the first relayer's \
             acknowledgement, so a zero-hop path is refused"
        );
        let dest = self.dest_for(forward_hops)?;
        let base = HoprSessionClientConfig {
            forward_path: HopRouting::try_from(forward_hops)?,
            return_path: HopRouting::try_from(return_hops)?,
            capabilities: SessionCapability::Segmentation | SessionCapability::NoDelay,
            always_max_out_surbs: true,
            surb_management: Some(SurbBalancerConfig {
                target_surb_buffer_size: PIX_RESPONSE_BUFFER_BYTES / SESSION_MTU as u64,
                max_surbs_per_sec: PIX_MAX_SURB_UPSTREAM_BITS / (8 * SURB_SIZE as u64),
                ..SurbBalancerConfig::default()
            }),
            ..Default::default()
        };
        let (session, _) = self
            .edgli
            .connect_to(dest, SessionTarget::ExitNode(0), self.edgli.with_pix(base)?)
            .await?;
        Ok((session, dest))
    }

    /// Unallocated wxHOPR in the entry's Safe — the float PIX deposits are paid from.
    ///
    /// The Safe and not the node's own account: since hopr-types 4.0.0 a deposit settles through
    /// the Safe module, so that is the balance that moves. `describe_current_capacity_allocations`
    /// reports it as the `safe` allocation, which is the figure an operator running PIX is told to
    /// watch — and "unallocated" is what makes it the right one, since wxHOPR already staked into a
    /// channel is not available to a deposit.
    ///
    /// It is not a PIX-only figure. The channel-lifecycle strategy stakes and tops up from the same
    /// Safe, so read a *decrease* here as an upper bound on PIX spend rather than as a measurement
    /// of it; `pix::PixCounters::deposits` is what counts deposits.
    #[cfg(feature = "pix")]
    pub async fn entry_safe_balance(&self) -> anyhow::Result<crate::HoprBalance> {
        Ok(self
            .edgli
            .describe_current_capacity_allocations()
            .await?
            .safe
            .stake)
    }

    /// Session tuned to expose tokio executor starvation for the profiling harness:
    /// rate control OFF and a small SURB pool, so a non-yielding writer that holds a
    /// worker thread visibly starves the SURB balancer (throughput collapses). See
    /// `tests/profiling.rs`.
    pub async fn open_profiling_session(&self, hops: usize) -> anyhow::Result<HoprSession> {
        let dest = self.dest_for(hops)?;
        let (session, _) = self
            .edgli
            .connect_to(
                dest,
                SessionTarget::ExitNode(0),
                HoprSessionClientConfig {
                    forward_path: HopRouting::try_from(hops)?,
                    return_path: HopRouting::try_from(hops)?,
                    capabilities: SessionCapability::Segmentation
                        | SessionCapability::NoRateControl,
                    surb_management: Some(SurbBalancerConfig {
                        target_surb_buffer_size: 600,
                        max_surbs_per_sec: 300,
                        ..SurbBalancerConfig::default()
                    }),
                    ..Default::default()
                },
            )
            .await?;
        Ok(session)
    }
}

/// Boot Edgli on `extra`, connect to peers, start the channel-lifecycle strategy, and
/// wait until at least one outgoing channel is open. Shared by the local and Rotsee
/// setups. `target_channels` is how many outgoing channels the strategy aims to open to
/// discovered peers; any pre-existing "genesis" Open channels to non-peer accounts (a
/// pre-funded identity may hold some) are added on top so the target still reaches live
/// peers.
async fn boot_edgli(
    blokli_url: &str,
    extra: &ExtraInfo,
    tuning: &NetTuning,
    target_channels: usize,
    extra_strategies: ExtraStrategies,
) -> anyhow::Result<(Edgli, futures::future::AbortHandle)> {
    let hopr_keys: HoprKeys = IdentityRetrievalModes::FromFile {
        password: &extra.password,
        id_path: extra.keystore_path.to_str().unwrap(),
    }
    .try_into()?;

    tracing::info!(safe = %extra.safe_address, "booting Edgli");
    let edgli = Edgli::new(
        edgli_config(&extra.safe_address, &extra.module_address, tuning),
        hopr_keys,
        BlokliEndpoint::from_optional_url(Some(blokli_url))?,
        Some(tuning.connector),
        tuning.probe_local,
        |s: EdgliInitState| tracing::info!(?s, "edgli init"),
    )
    .await?;

    tracing::info!("waiting for Edgli to connect to ≥2 peers");
    await_edgli_peers_connected(&edgli, 2).await?;

    // A pre-funded identity (especially on Rotsee) may already hold Open channels to
    // accounts that are not currently connected peers. The strategy counts all Open
    // channels, so add those to the target to still open `target_channels` to live peers.
    let peers: std::collections::HashSet<Address> = edgli
        .connected_peer_addresses()
        .await?
        .into_iter()
        .collect();
    let genesis_channels = edgli
        .my_outgoing_channels()
        .await?
        .into_iter()
        .filter(|c| c.status == ChannelStatus::Open && !peers.contains(&c.destination))
        .count();
    if genesis_channels > 0 {
        tracing::info!(
            genesis_channels,
            "compensating for pre-existing channels in target"
        );
    }

    let sizing = IncentiveConfiguration {
        min_open_channels: 1,
        target_open_channels: target_channels + genesis_channels,
        ..Default::default()
    };
    // Sync and sizing-only since edge-client#136: the strategy resolves capacities to balances
    // each tick against the live winning probability, so nothing is read from the chain here.
    // `channel_capacity` is left at its default deliberately -- raising it also raises the safe
    // gate below which the node opens *zero* channels, which is not what this scenario measures.
    let mut strat_cfg = default_strategy_cfg(&sizing)?;
    // An `if let` rather than the irrefutable `let` this used to be: `EdgeStrategyKind` is
    // `#[non_exhaustive]` since edge-client#151, so from a downstream crate the pattern is
    // refutable even while `default_strategy_cfg` still yields only this one variant.
    for kind in &mut strat_cfg.strategies {
        if let EdgeStrategyKind::ChannelLifecycle(lc) = kind {
            lc.eligibility = EligibilityConfig {
                min_peer_quality_score: 0.0,
                require_observed_since_start: false,
                ..Default::default()
            };
            lc.tick_interval = tuning.strategy_tick;
        }
    }
    extra_strategies.apply(&mut strat_cfg);
    let reactor = edgli.run_reactor_from_cfg(strat_cfg)?;

    // Require one channel *beyond* any pre-existing genesis channels, so the gate proves the
    // strategy opened a fresh channel to a live peer rather than passing on genesis alone
    // (otherwise a pre-funded Rotsee identity satisfies it immediately and the later
    // connect_to fails with an opaque session timeout).
    tracing::info!("waiting for strategy to open ≥1 new outgoing channel");
    await_edgli_channels_open(&edgli, 1 + genesis_channels, tuning.channel_open_timeout).await?;

    Ok((edgli, reactor))
}

/// Connector tuning every chain-touching path here needs.
///
/// Left at the default it submits the transaction, waits for a confirmation blokli has not indexed
/// yet, and returns "operation timed out at the client" while the transfer is in fact mined.
///
/// Private again since the PIX float stopped needing its own funding transaction — it used to be
/// `pub(crate)` for the side connector that made it.
fn connector_cfg() -> BlockchainConnectorConfig {
    BlockchainConnectorConfig {
        // Default tx-confirmation budget is too tight for blokli's SSE indexing on Anvil.
        tx_timeout_multiplier: 10,
        ..Default::default()
    }
}

fn edgli_config(
    safe_address: &Address,
    module_address: &Address,
    tuning: &NetTuning,
) -> HoprLibConfig {
    use edgli::hopr_lib::{
        config::{HoprPacketPipelineConfig, MixerConfig, TransportConfig},
        exports::transport::{
            HoprProtocolConfig,
            config::{SurbPopOrder, SurbStoreConfig},
        },
    };
    HoprLibConfig {
        host: HostConfig {
            address: HostType::IPv4("0.0.0.0".to_string()),
            port: edge_p2p_port(),
        },
        publish: true,
        protocol: HoprProtocolConfig {
            transport: TransportConfig {
                announce_local_addresses: tuning.announce_local,
                prefer_local_addresses: tuning.prefer_local,
            },
            path_planner: tuning.path_planner,
            packet: HoprPacketPipelineConfig {
                // Stated rather than defaulted, and it follows the Exit's build. The library
                // default is FIFO, which replies with the oldest SURBs first and so keeps using a
                // return path for as long as its backlog lasts; hoprd's default build pins LIFO,
                // and a cluster whose two ends disagree measures neither one.
                //
                // A PIX build of hoprd pins FIFO instead (hoprd#91), and not as an optimisation: a
                // PIX share reaches the Exit's reconstructor only when its SURB is *spent*, and
                // the per-pseudonym ring buffer evicts from the oldest end — so popping
                // newest-first leaves the oldest SURBs unspent until they are overwritten, and
                // every overwrite is a share that can never be delivered. hoprd measured that as
                // a total, silent stall: one confirmed deposit and then nothing, through 300 s of
                // clean traffic with no error logged on any node.
                //
                // Here the Exit is the side popping SURBs to reply, so this end's order is very
                // likely inert in the PIX scenarios. It tracks the Exit anyway, rather than
                // stating a rationale that is false for the binary `just pix` builds.
                surb_store: SurbStoreConfig {
                    // `cfg!` rather than two `#[cfg]`'d fields: one expression, and both arms
                    // typecheck in either build.
                    pop_order: if cfg!(feature = "pix") {
                        SurbPopOrder::Fifo
                    } else {
                        SurbPopOrder::Lifo
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            mixer: MixerConfig {
                min_delay: Duration::ZERO,
                delay_range: Duration::from_millis(1),
                ..Default::default()
            },
            // Stated rather than defaulted, and load-bearing. edgli derives the quota it announces
            // from these dimensions and nothing else, and the Exit refuses any Session whose quota
            // falls outside its `quota_range`. The library defaults price a ~560 MB quota against
            // the 1 MiB window `--enable-pix` configures, so leaving them would have every PIX
            // Session rejected at open — before a byte moves, and for a reason that reads as a
            // session timeout from here.
            #[cfg(feature = "pix")]
            pix: crate::pix::dimensions(),
            ..Default::default()
        },
        safe_module: SafeModule {
            safe_address: *safe_address,
            module_address: *module_address,
        },
        ..Default::default()
    }
}

/// Rotsee configuration read from the environment.
struct RotseeConfig {
    blokli_url: String,
    extra: ExtraInfo,
    exit_node: Option<Address>,
}

/// Read the Rotsee identity + network config from `EDGLI_ROTSEE_*` env vars.
fn read_rotsee_env() -> anyhow::Result<RotseeConfig> {
    fn required(var: &str) -> anyhow::Result<String> {
        std::env::var(var).map_err(|_| {
            anyhow::anyhow!(
                "{var} is not set. The Rotsee test needs a pre-funded, on-chain-registered \
                 Gnosis identity supplied via: EDGLI_ROTSEE_BLOKLI_URL, EDGLI_ROTSEE_IDENTITY_FILE, \
                 EDGLI_ROTSEE_IDENTITY_PASSWORD, EDGLI_ROTSEE_SAFE_ADDRESS, EDGLI_ROTSEE_MODULE_ADDRESS \
                 (plus EDGLI_ROTSEE_EXIT_NODE for the loopback exit)."
            )
        })
    }

    let blokli_url = required("EDGLI_ROTSEE_BLOKLI_URL")?;
    let keystore_path = std::path::PathBuf::from(required("EDGLI_ROTSEE_IDENTITY_FILE")?);
    let password = required("EDGLI_ROTSEE_IDENTITY_PASSWORD")?;
    let safe_address = required("EDGLI_ROTSEE_SAFE_ADDRESS")?
        .parse::<Address>()
        .context("EDGLI_ROTSEE_SAFE_ADDRESS: invalid address")?;
    let module_address = required("EDGLI_ROTSEE_MODULE_ADDRESS")?
        .parse::<Address>()
        .context("EDGLI_ROTSEE_MODULE_ADDRESS: invalid address")?;
    let exit_node = std::env::var("EDGLI_ROTSEE_EXIT_NODE")
        .ok()
        .map(|s| s.parse::<Address>())
        .transpose()
        .context("EDGLI_ROTSEE_EXIT_NODE: invalid address")?;

    tracing::info!(%blokli_url, %safe_address, %module_address, ?exit_node, "Rotsee config from env");
    Ok(RotseeConfig {
        blokli_url,
        extra: ExtraInfo {
            safe_address,
            module_address,
            keystore_path,
            password,
        },
        exit_node,
    })
}

/// Wait until `target` is a connected, probed peer in Edgli's network graph.
///
/// Path-finding scores edges from probe observations (RFC-0010 §6.2); before the exit
/// is in the graph, session construction to it times out. Polls until the exit's chain
/// address resolves to an offchain key via the on-chain registry AND that key appears in
/// `all_network_peers(0.0)` (quality floor 0.0 = connected AND at least one probe). The
/// chain-key lookup is done inside the loop: right after boot the connector may not have
/// indexed the exit's account yet, so a one-shot resolve could fail spuriously.
async fn await_edgli_exit_peer_ready(edgli: &Edgli, target: Address) -> anyhow::Result<()> {
    poll_until(
        EXIT_PEER_PROBE_TIMEOUT,
        Duration::from_secs(5),
        "Rotsee exit peer connected + probed",
        || async {
            let Some(offchain_key) = edgli
                .chain_api()
                .chain_key_to_packet_key(&target)
                .map_err(|e| anyhow::anyhow!("{e}"))?
            else {
                // Not yet indexed on chain — keep waiting.
                return Ok(false);
            };
            let peers = edgli
                .transport()
                .all_network_peers(0.0)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(peers.iter().any(|(k, _)| k == &offchain_key))
        },
    )
    .await
}

async fn await_edgli_peers_connected(edgli: &Edgli, min_peers: usize) -> anyhow::Result<()> {
    poll_until(
        PEER_DISCOVERY_TIMEOUT,
        Duration::from_secs(3),
        "Edgli peer discovery",
        || async {
            let peers = edgli.connected_peer_addresses().await?;
            Ok(peers.len() >= min_peers)
        },
    )
    .await
}

async fn await_edgli_channels_open(
    edgli: &Edgli,
    min_open: usize,
    timeout: Duration,
) -> anyhow::Result<()> {
    poll_until(
        timeout,
        Duration::from_secs(5),
        "Edgli channel open",
        || async {
            let channels: Vec<ChannelEntry> = edgli.my_outgoing_channels().await?;
            Ok(channels
                .iter()
                .filter(|c| c.status == ChannelStatus::Open)
                .count()
                >= min_open)
        },
    )
    .await
}

async fn select_session_targets(edgli: &Edgli) -> anyhow::Result<(Address, Address)> {
    let (raw_channels, peers) = tokio::try_join!(
        edgli.my_outgoing_channels(),
        edgli.connected_peer_addresses()
    )?;
    let zero_hop = raw_channels
        .into_iter()
        .find(|c| c.status == ChannelStatus::Open)
        .ok_or_else(|| anyhow::anyhow!("no open outgoing channels"))?
        .destination;
    let one_hop = peers
        .into_iter()
        .find(|a| *a != zero_hop)
        .ok_or_else(|| anyhow::anyhow!("need ≥2 distinct connected peers for 1-hop"))?;
    tracing::info!(zero_hop = %zero_hop, one_hop = %one_hop, "session targets selected");
    Ok((zero_hop, one_hop))
}

async fn poll_until<F, Fut>(
    timeout: Duration,
    sleep: Duration,
    what: &str,
    mut check: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await? {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timeout ({timeout:?}) waiting for {what}"
        );
        tokio::time::sleep(sleep).await;
    }
}
