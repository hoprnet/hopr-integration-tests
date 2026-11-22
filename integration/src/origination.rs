//! Is a node still originating packets of its own?
//!
//! [`crate::relayers`] reads one series from `/metrics` to ask *who relayed*. This asks a different
//! question of the same endpoint: whether a node is still producing packets that start at it.
//!
//! The distinction is the whole point. A HOPR node originates packets on one path and relays them
//! on another, and the two are independent:
//!
//! | counter | produced by |
//! | ------- | ----------- |
//! | `hopr_packets_count{type="sent"}` | the egress pipeline, after routing resolution and SPHINX encode |
//! | `hopr_packets_count{type="forwarded"}` | the ingress pipeline, relaying someone else's packet |
//! | `hopr_packets_count{type="received"}` | the ingress pipeline, for packets addressed to this node |
//! | `hopr_protocol_ack_sent_count` | the acknowledgement egress task |
//!
//! Only the first depends on this node being able to resolve a route of its own. So a node whose
//! origination has stalled keeps forwarding, keeps acknowledging and keeps receiving, and looks
//! healthy to anything that does not read `sent` specifically — including its own healthcheck.
//!
//! # Reading a verdict
//!
//! `sent` frozen on its own means nothing: an idle node originates nothing either. It is only
//! evidence when read against a counter that proves the node was busy over the same window, which
//! is why [`OriginationVerdict`] carries both and why the scenarios assert on the pair.

use std::time::Duration;

use crate::cluster::NodeInfo;

const PACKETS_METRIC: &str = "hopr_packets_count";
const ACKS_SENT_METRIC: &str = "hopr_protocol_ack_sent_count";

/// One reading of a node's packet-flow counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OriginationSample {
    /// Packets this node originated: the counter that stalls when routing resolution wedges.
    pub sent: u64,
    /// Packets this node relayed for someone else.
    pub forwarded: u64,
    /// Packets addressed to this node.
    pub received: u64,
    /// Acknowledgements this node sent.
    pub acks_sent: u64,
}

/// What a series of samples says about a node's origination.
#[derive(Debug, Clone)]
pub struct OriginationVerdict {
    pub samples: Vec<OriginationSample>,
    pub span: Duration,
}

impl OriginationVerdict {
    /// Whether the node originated any packet across the whole series.
    pub fn originated(&self) -> bool {
        self.advanced(|s| s.sent)
    }

    /// Whether the node processed any inbound traffic across the whole series.
    ///
    /// This is the control for [`Self::originated`]: without it, a frozen `sent` is
    /// indistinguishable from a node with nothing to do.
    pub fn processed_inbound(&self) -> bool {
        self.advanced(|s| s.forwarded)
            || self.advanced(|s| s.received)
            || self.advanced(|s| s.acks_sent)
    }

    fn advanced(&self, field: impl Fn(&OriginationSample) -> u64) -> bool {
        match (self.samples.first(), self.samples.last()) {
            (Some(first), Some(last)) => field(last) > field(first),
            _ => false,
        }
    }

    fn delta(&self, field: impl Fn(&OriginationSample) -> u64) -> u64 {
        match (self.samples.first(), self.samples.last()) {
            (Some(first), Some(last)) => field(last).saturating_sub(field(first)),
            _ => 0,
        }
    }

    /// One-line rendering for a failure message: what moved and what did not.
    pub fn summary(&self) -> String {
        format!(
            "over {:?} across {} samples: sent +{}, forwarded +{}, received +{}, acks_sent +{}",
            self.span,
            self.samples.len(),
            self.delta(|s| s.sent),
            self.delta(|s| s.forwarded),
            self.delta(|s| s.received),
            self.delta(|s| s.acks_sent),
        )
    }
}

/// Read a node's packet-flow counters once.
pub async fn sample(node: &NodeInfo) -> anyhow::Result<OriginationSample> {
    Ok(parse(&crate::cluster::scrape_metrics(node).await?))
}

/// Sample `node` `count` times, `interval` apart, and return the verdict.
///
/// A single pair of samples cannot distinguish a stalled node from one that happened to be quiet
/// between two instants, so scenarios take several spanning a window long enough to cover the
/// node's own periodic traffic.
pub async fn watch(
    node: &NodeInfo,
    count: usize,
    interval: Duration,
) -> anyhow::Result<OriginationVerdict> {
    anyhow::ensure!(
        count >= 2,
        "a verdict needs at least two samples, got {count}"
    );

    let mut samples = Vec::with_capacity(count);
    for i in 0..count {
        if i > 0 {
            tokio::time::sleep(interval).await;
        }
        let sample = sample(node).await?;
        tracing::info!(node = %node.address, ?sample, "origination sample {}/{count}", i + 1);
        samples.push(sample);
    }

    Ok(OriginationVerdict {
        samples,
        span: interval * (count as u32 - 1),
    })
}

/// Sum a `hopr_packets_count` series by its `type` label out of a Prometheus text exposition.
///
/// Returns 0 when the series is absent: a node that has not yet done a thing may not have created
/// the counter at all.
pub(crate) fn packets_of_type(body: &str, packet_type: &str) -> u64 {
    let label = format!("\"{packet_type}\"");
    series(body, PACKETS_METRIC, |line| line.contains(&label))
}

pub fn parse(body: &str) -> OriginationSample {
    OriginationSample {
        sent: packets_of_type(body, "sent"),
        forwarded: packets_of_type(body, "forwarded"),
        received: packets_of_type(body, "received"),
        acks_sent: series(body, ACKS_SENT_METRIC, |_| true),
    }
}

fn series(body: &str, metric: &str, matches: impl Fn(&str) -> bool) -> u64 {
    body.lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| line.starts_with(metric))
        // Guard against a metric that is a prefix of a longer one
        // (`hopr_packets_count` vs `hopr_packets_count_total`).
        .filter(|line| {
            line[metric.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == ' ' || c == '{')
        })
        .filter(|line| matches(line))
        .filter_map(|line| line.rsplit_once(' '))
        // Counters render as floats in the Prometheus text format ("42" or "42.0").
        .filter_map(|(_, value)| value.trim().parse::<f64>().ok())
        .map(|value| value as u64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPOSITION: &str = r#"
# HELP hopr_packets_count Number of processed packets of different types
# TYPE hopr_packets_count counter
hopr_packets_count{type="sent"} 460497
hopr_packets_count{type="forwarded"} 2314830
hopr_packets_count{type="received"} 543864
hopr_packets_count_total 99999
hopr_protocol_ack_sent_count 2858694
hopr_packet_rejected_count{reason="timeout"} 3
"#;

    fn verdict(samples: &[(u64, u64)]) -> OriginationVerdict {
        OriginationVerdict {
            samples: samples
                .iter()
                .map(|(sent, forwarded)| OriginationSample {
                    sent: *sent,
                    forwarded: *forwarded,
                    ..Default::default()
                })
                .collect(),
            span: Duration::from_secs(60),
        }
    }

    #[test]
    fn parser_should_read_each_packet_type_and_the_ack_counter() {
        let sample = parse(EXPOSITION);
        assert_eq!(
            OriginationSample {
                sent: 460_497,
                forwarded: 2_314_830,
                received: 543_864,
                acks_sent: 2_858_694,
            },
            sample
        );
    }

    /// A longer metric name starting with this one must not be summed into it, or a frozen counter
    /// reads as still moving and the scenario reports a stalled node as healthy.
    ///
    /// The series has to carry a *matching* label, otherwise the label filter excludes it on its own
    /// and the name-boundary guard this is here to protect goes untested — an earlier version of
    /// this test used `hopr_packets_count_total 99999`, which passes with the guard deleted.
    #[test]
    fn parser_should_not_match_a_longer_metric_with_the_same_prefix() {
        assert_eq!(
            0,
            packets_of_type("hopr_packets_count_v2{type=\"sent\"} 99999", "sent")
        );
    }

    #[test]
    fn parser_should_return_zero_when_the_series_is_absent() {
        assert_eq!(
            0,
            packets_of_type("hopr_packets_count{type=\"sent\"} 5", "forwarded")
        );
    }

    #[test]
    fn parser_should_accept_float_rendered_counters() {
        assert_eq!(
            42,
            packets_of_type("hopr_packets_count{type=\"sent\"} 42.0", "sent")
        );
    }

    /// The wedge: origination frozen while the ingress side keeps moving.
    #[test]
    fn a_frozen_sent_counter_beside_a_moving_forwarded_one_should_read_as_stalled() {
        let v = verdict(&[
            (460_497, 2_314_830),
            (460_497, 2_315_400),
            (460_497, 2_315_762),
        ]);
        assert!(!v.originated(), "sent never advanced");
        assert!(
            v.processed_inbound(),
            "forwarded advanced, so the node was demonstrably busy"
        );
    }

    /// A quiet node is not a stalled one, and the pair is what tells them apart.
    #[test]
    fn a_node_that_did_nothing_at_all_should_not_read_as_stalled() {
        let v = verdict(&[(100, 200), (100, 200)]);
        assert!(!v.originated());
        assert!(
            !v.processed_inbound(),
            "nothing moved either way, so there is no evidence of a stall"
        );
    }

    #[test]
    fn a_healthy_node_should_read_as_originating() {
        let v = verdict(&[(100, 200), (140, 260), (190, 330)]);
        assert!(v.originated());
        assert!(v.processed_inbound());
    }
}
