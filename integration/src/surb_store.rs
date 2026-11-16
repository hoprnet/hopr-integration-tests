//! A node's SURB store, read from its `/metrics`.
//!
//! `hopr_surb_store_surbs_held{class="admitted"}` counts the SURBs a node holds behind established
//! Sessions. On an Exit serving one Session that is the Session's whole return-path supply: one SURB
//! per reply. `hoprd` refreshes the gauge once per SURB flush tick (a second by default), so a reading
//! can be that old, and only when built with `hopr-transport/telemetry`: a node without it exports no
//! series at all, which [`surbs_held`] reports as `None`, not as a zero.

use crate::cluster::NodeInfo;

/// The gauge, by trust class: `admitted` (behind an established Session) or `quarantine`.
const SURBS_HELD: &str = "hopr_surb_store_surbs_held";

/// The SURBs `node` holds behind established Sessions, or `None` when it exports no such series.
pub async fn surbs_held(node: &NodeInfo) -> anyhow::Result<Option<u64>> {
    Ok(parse_surbs_held(
        &crate::cluster::scrape_metrics(node).await?,
    ))
}

/// The `class="admitted"` series of [`SURBS_HELD`] out of a Prometheus text exposition.
fn parse_surbs_held(body: &str) -> Option<u64> {
    body.lines()
        .filter(|line| !line.starts_with('#'))
        // The name has to end at the brace: a longer family sharing the prefix is not this one.
        .filter_map(|line| line.strip_prefix(SURBS_HELD)?.strip_prefix('{'))
        .filter_map(|rest| rest.rsplit_once('}'))
        .find(|(labels, _)| crate::pix::label_value(labels, "class") == Some("admitted"))
        // Gauges render as floats ("7450" or "7450.0").
        .and_then(|(_, value)| value.trim().parse::<f64>().ok())
        .map(|value| value as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPOSITION: &str = r#"
# HELP hopr_surb_store_surbs_held SURBs currently held by the SURB store, by trust class
# TYPE hopr_surb_store_surbs_held gauge
hopr_surb_store_surbs_held{class="quarantine"} 12
hopr_surb_store_surbs_held{class="admitted"} 7450
hopr_surb_store_admissions_count{event="retired"} 3
"#;

    #[test]
    fn parser_should_read_the_admitted_class_only() {
        assert_eq!(Some(7450), parse_surbs_held(EXPOSITION));
    }

    #[test]
    fn parser_should_report_an_absent_series_as_none() {
        assert_eq!(
            None,
            parse_surbs_held("hopr_surb_store_surbs_held{class=\"quarantine\"} 12")
        );
        assert_eq!(None, parse_surbs_held(""));
    }

    #[test]
    fn parser_should_not_read_a_longer_family_with_the_same_prefix() {
        assert_eq!(
            None,
            parse_surbs_held("hopr_surb_store_surbs_held_peak{class=\"admitted\"} 9000")
        );
    }

    #[test]
    fn parser_should_accept_extra_labels_and_float_values() {
        assert_eq!(
            Some(67),
            parse_surbs_held(
                "hopr_surb_store_surbs_held{otel_scope_name=\"hopr\",class=\"admitted\"} 67.0"
            )
        );
    }
}
