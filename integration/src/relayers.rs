//! Who actually carried the return traffic.
//!
//! Each `hoprd` exposes `hopr_packets_count{type="forwarded"}` on its authenticated
//! `/metrics` endpoint: a monotone count of packets the node relayed for someone else.
//! Sampling it before and after a transfer gives a per-node delta, and over a session
//! whose **forward** path is 0-hop the only packets a cluster node can forward are
//! replies on the return path. The deltas are therefore a histogram of return-path
//! first relayers — exactly the distribution that collapsed onto three nodes in the
//! 2026-08-11 incident.
//!
//! Metrics come from the node's own process, so a node that has been killed simply stops
//! answering. [`sample`] drops it rather than failing the run, and [`spread`] therefore
//! reports no delta for it — see [`spread`] for why that is the wanted behaviour here.
//!
//! **What this cannot see.** A dead node forwards nothing whether or not the sender still
//! picked it, so a zero delta after a kill does not prove the sender stopped choosing that
//! relayer — only that no traffic got through it. Use the histogram to reason about the
//! *live* relayers' shares; to reason about selection after a kill you need the sender's
//! own path-planner telemetry, which this crate does not have.

use std::collections::HashMap;

use crate::{Address, cluster::NodeInfo};

/// The metric and label the histogram is read from.
const FORWARDED_TYPE: &str = "forwarded";

/// Forwarded-packet counts per node address, as read at one instant.
#[derive(Debug, Clone, Default)]
pub struct ForwardedSample(HashMap<Address, u64>);

/// Per-relayer share of the return traffic observed between two samples.
#[derive(Debug, Clone)]
pub struct RelayerSpread {
    /// Packets forwarded per node over the window, descending by count.
    pub per_relayer: Vec<(Address, u64)>,
    pub total: u64,
}

impl RelayerSpread {
    /// Relayers carrying at least `min_share` of the total (0.0–1.0).
    ///
    /// A floor is needed because a relayer can pick up a stray packet from probing or a
    /// single re-plan; "used" means used for a real share of the stream.
    pub fn active_relayers(&self, min_share: f64) -> Vec<(Address, u64)> {
        self.per_relayer
            .iter()
            .filter(|(_, n)| self.share_of(*n) >= min_share)
            .cloned()
            .collect()
    }

    /// Largest single relayer's share of the total (0.0–1.0); 0.0 when nothing moved.
    pub fn max_share(&self) -> f64 {
        self.per_relayer
            .first()
            .map(|(_, n)| self.share_of(*n))
            .unwrap_or(0.0)
    }

    /// Busiest relayer's count divided by the least-busy one's, over relayers above
    /// `min_share`. `1.0` is a perfectly even rotation; higher means the split tracks
    /// something — path score, latency — rather than being round-robin.
    ///
    /// This, not [`Self::max_share`], is what separates selection strategies when the
    /// candidate set is small. With four candidates an even rotation gives 25% each and a
    /// weight-proportional draw still only reaches ~36% for the best one, so a cap on the
    /// maximum share cannot tell them apart — but their ratios are ~1.0 and ~2.3.
    ///
    /// Returns `1.0` when fewer than two relayers clear the floor (nothing to compare).
    pub fn imbalance(&self, min_share: f64) -> f64 {
        let active = self.active_relayers(min_share);
        match (active.first(), active.last()) {
            (Some((_, max)), Some((_, min))) if active.len() >= 2 && *min > 0 => {
                *max as f64 / *min as f64
            }
            _ => 1.0,
        }
    }

    /// The relayer that carried the most, if any.
    pub fn busiest(&self) -> Option<Address> {
        self.per_relayer.first().map(|(a, _)| *a)
    }

    /// The relayer that carried the least, if any.
    ///
    /// The counterpart of [`Self::busiest`], and the right victim for a scenario that wants to
    /// remove a *particular* share of the traffic rather than the worst case. Killing the busiest
    /// removes the largest share available, which on a skewed draw can be far more than the
    /// surplus was sized to carry — see `tests/pix_prod.rs`, where killing the 72 % relayer put the
    /// loss past the deployed split's break-even point and the cycle legitimately failed to
    /// recover. Killing the least-loaded one keeps the loss inside the band the surplus exists for.
    pub fn quietest(&self) -> Option<Address> {
        self.per_relayer.last().map(|(a, _)| *a)
    }

    /// The share of the total carried by `address`, or `None` if it carried nothing.
    pub fn share_of_relayer(&self, address: Address) -> Option<f64> {
        self.per_relayer
            .iter()
            .find(|(a, _)| *a == address)
            .map(|(_, n)| self.share_of(*n))
    }

    fn share_of(&self, count: u64) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            count as f64 / self.total as f64
        }
    }

    /// One-line rendering for test output: `0xabc…=41% 0xdef…=33% …`.
    pub fn summary(&self) -> String {
        self.per_relayer
            .iter()
            .map(|(addr, n)| format!("{addr}={n} ({:.0}%)", self.share_of(*n) * 100.0))
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// Read `hopr_packets_count{type="forwarded"}` from every node in `nodes`.
///
/// Nodes that do not answer (killed mid-scenario, or still starting) are simply absent
/// from the sample; [`spread`] treats a missing endpoint as "no further progress".
pub async fn sample(nodes: &[NodeInfo]) -> ForwardedSample {
    let readings = futures::future::join_all(nodes.iter().map(|node| async move {
        match scrape_forwarded(node).await {
            Ok(count) => Some((node.address, count)),
            Err(e) => {
                tracing::debug!(node = %node.address, "forwarded-metric scrape failed: {e:#}");
                None
            }
        }
    }))
    .await;

    ForwardedSample(readings.into_iter().flatten().collect())
}

/// Per-node deltas between two samples, as a return-relayer histogram.
///
/// A node missing from `after` (killed mid-run) yields no delta. That is deliberate: the
/// windows these scenarios measure begin *after* the kill, so the victim genuinely carried
/// nothing during them, and carrying a stale end value forward would credit pre-death traffic
/// to a post-kill window. The cost is that a window spanning the kill under-reports the
/// victim and so inflates every survivor's share — do not use `spread` across a kill boundary.
///
/// A node missing from `before` is a different matter and is excluded. `sample` omits a node
/// whose scrape failed, so an address that appears only in `after` has no start value —
/// counting it would treat the node's *lifetime* counter as traffic from this pump alone. That
/// inflates the histogram and, because the victim is chosen as the busiest relayer, can name a
/// node that carried almost nothing.
pub fn spread(before: &ForwardedSample, after: &ForwardedSample) -> RelayerSpread {
    for addr in after.0.keys() {
        if !before.0.contains_key(addr) {
            tracing::warn!(
                node = %addr,
                "absent from the baseline scrape; excluded from the relayer spread"
            );
        }
    }

    let mut per_relayer: Vec<(Address, u64)> = before
        .0
        .iter()
        .map(|(addr, start)| {
            let end = after.0.get(addr).copied().unwrap_or(*start);
            (*addr, end.saturating_sub(*start))
        })
        .filter(|(_, delta)| *delta > 0)
        .collect();

    // Descending by count, then by address so equal counts order deterministically.
    per_relayer.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = per_relayer.iter().map(|(_, n)| n).sum();

    RelayerSpread { per_relayer, total }
}

async fn scrape_forwarded(node: &NodeInfo) -> anyhow::Result<u64> {
    Ok(parse_forwarded(
        &crate::cluster::scrape_metrics(node).await?,
    ))
}

/// Sum the `type="forwarded"` series of `hopr_packets_count` out of a Prometheus text
/// exposition. Returns 0 when the series is absent — a node that has forwarded nothing
/// yet may not have created the counter.
///
/// Shares [`crate::origination`]'s parser rather than keeping a second copy: that one additionally
/// guards against a longer metric name with the same prefix being summed in.
fn parse_forwarded(body: &str) -> u64 {
    crate::origination::packets_of_type(body, FORWARDED_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPOSITION: &str = r#"
# HELP hopr_packets_count Number of processed packets of different types
# TYPE hopr_packets_count counter
hopr_packets_count{type="sent"} 1200
hopr_packets_count{type="forwarded"} 314
hopr_packets_count{type="received"} 900
hopr_packets_forwarded_bytes 99999
"#;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn sample_of(entries: &[(Address, u64)]) -> ForwardedSample {
        ForwardedSample(entries.iter().copied().collect())
    }

    /// Regression: a node whose baseline scrape failed appears only in `after`, and its
    /// lifetime counter was being read as traffic from this pump — enough to make it look like
    /// the busiest relayer and get it chosen as the kill victim.
    #[test]
    fn spread_should_exclude_a_node_absent_from_the_baseline() {
        let before = sample_of(&[(addr(1), 100), (addr(2), 100)]);
        let after = sample_of(&[(addr(1), 150), (addr(2), 120), (addr(3), 9_000_000)]);

        let spread = spread(&before, &after);

        assert_eq!(
            vec![(addr(1), 50), (addr(2), 20)],
            spread.per_relayer,
            "addr(3) has no baseline, so its lifetime counter is not this pump's traffic"
        );
        assert_eq!(70, spread.total);
    }

    #[test]
    fn parser_should_pick_only_the_forwarded_series() {
        assert_eq!(parse_forwarded(EXPOSITION), 314);
    }

    #[test]
    fn parser_should_return_zero_when_the_series_is_absent() {
        assert_eq!(parse_forwarded("hopr_packets_count{type=\"sent\"} 5"), 0);
    }

    #[test]
    fn parser_should_accept_float_rendered_counters() {
        assert_eq!(
            parse_forwarded("hopr_packets_count{type=\"forwarded\"} 42.0"),
            42
        );
    }

    #[test]
    fn spread_should_rank_relayers_by_forwarded_delta() {
        let before = sample_of(&[(addr(1), 10), (addr(2), 0), (addr(3), 5)]);
        let after = sample_of(&[(addr(1), 110), (addr(2), 300), (addr(3), 5)]);

        let spread = spread(&before, &after);

        // Node 3 forwarded nothing in the window and drops out entirely.
        assert_eq!(spread.per_relayer, vec![(addr(2), 300), (addr(1), 100)]);
        assert_eq!(spread.total, 400);
        assert_eq!(spread.busiest(), Some(addr(2)));
        assert!((spread.max_share() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn spread_should_report_no_delta_for_a_relayer_that_stopped_answering() {
        // Node 2 was killed mid-run: present in `before`, gone from `after`. It has no end value,
        // so it yields no delta -- the post-kill window it would be measured over is one in
        // which it genuinely carried nothing.
        let before = sample_of(&[(addr(1), 0), (addr(2), 50)]);
        let after = sample_of(&[(addr(1), 100)]);

        let spread = spread(&before, &after);

        assert_eq!(spread.per_relayer, vec![(addr(1), 100)]);
        assert_eq!(spread.total, 100);
    }

    #[test]
    fn active_relayers_should_drop_ones_below_the_share_floor() {
        let before = sample_of(&[(addr(1), 0), (addr(2), 0), (addr(3), 0)]);
        let after = sample_of(&[(addr(1), 500), (addr(2), 490), (addr(3), 10)]);

        let spread = spread(&before, &after);

        // 1% of the stream is noise, not a relayer in use.
        assert_eq!(spread.active_relayers(0.05).len(), 2);
        assert_eq!(spread.active_relayers(0.0).len(), 3);
    }

    #[test]
    fn max_share_should_be_zero_when_nothing_moved() {
        let spread = spread(&ForwardedSample::default(), &ForwardedSample::default());
        assert_eq!(spread.total, 0);
        assert_eq!(spread.max_share(), 0.0);
    }

    /// The two histograms are the ones actually measured on a 5-node cluster with skewed
    /// inter-node latency: a round-robin rotation and a weight-proportional draw. Their
    /// maximum shares (25% vs 36%) sit on the same side of any sane cap, but their
    /// imbalances are far apart — which is why the assertion uses this metric.
    #[test]
    fn imbalance_should_separate_a_rotation_from_a_weighted_draw() {
        let zero = sample_of(&[(addr(1), 0), (addr(2), 0), (addr(3), 0), (addr(4), 0)]);

        let rotation = spread(
            &zero,
            &sample_of(&[
                (addr(1), 1989),
                (addr(2), 1969),
                (addr(3), 1948),
                (addr(4), 1928),
            ]),
        );
        let weighted = spread(
            &zero,
            &sample_of(&[
                (addr(1), 2766),
                (addr(2), 2468),
                (addr(3), 1267),
                (addr(4), 1213),
            ]),
        );

        assert!(
            rotation.imbalance(0.05) < 1.1,
            "{}",
            rotation.imbalance(0.05)
        );
        assert!(
            weighted.imbalance(0.05) > 2.0,
            "{}",
            weighted.imbalance(0.05)
        );
    }

    #[test]
    fn imbalance_should_be_one_when_there_is_nothing_to_compare() {
        let before = sample_of(&[(addr(1), 0)]);
        let after = sample_of(&[(addr(1), 500)]);
        assert_eq!(spread(&before, &after).imbalance(0.05), 1.0);
        assert_eq!(
            spread(&ForwardedSample::default(), &ForwardedSample::default()).imbalance(0.05),
            1.0
        );
    }
}
