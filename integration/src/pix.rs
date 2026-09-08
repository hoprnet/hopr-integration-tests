//! PIX accounting for the edgli entry: dimensions, pricing, and the counters both ends expose.
//!
//! PIX pays the Exit for the traffic it delivers. The Entry deposits wxHOPR to a per-Session
//! stealth address; the Exit reconstructs that address's key from the SSA shares carried by the
//! return-path SURBs it spent, and sweeps the deposit into its Safe. Everything here is the Entry
//! half plus whatever is needed to *observe* the Exit half from outside.
//!
//! # Both sides move wxHOPR through their Safe
//!
//! It used to be an asymmetry, and this module was built around it: `SafePayloadGenerator::transfer`
//! emitted a **direct** `HoprToken.transfer` signed by the node key, so the Entry's deposits left
//! its *node* account while the Exit's recoveries landed in its *Safe*. hopr-types 4.0.0 routes
//! that transfer through the Safe module's `execTransactionFromModule`, so a deposit now debits the
//! Entry's Safe however it is signed. The node accounts still pay every transaction fee — and on
//! the Exit, the sweep's xDai top-up — but no wxHOPR moves through them.
//!
//! Two consequences shape everything below:
//!
//! - **Nothing here funds the Entry.** `hoprd-localcluster` gives each extra identity 1000 wxHOPR
//!   and `deploy_safe` sweeps it into the Safe, so edgli boots with the float already in the
//!   account that pays. What used to be a funding step is now a precondition to *check*, which
//!   `tests/pix.rs` does before it measures anything.
//! - **A run cannot be bounded by an empty account.** The Safe holds the channel stakes too, so
//!   "the float ran out" would mean "the stakes' leftovers ran out too" and a cycle count read off
//!   it would depend on stake arithmetic that has nothing to do with PIX. `entry_config`'s `budget`
//!   states the number outright instead — see there. (A code span rather than a link: this module
//!   is ungated and that function is not.)
//!
//! # Dimensions have to match the Exit's window
//!
//! The Exit rejects any Session whose announced quota falls outside its `quota_range`, and edgli
//! derives that quota from its own `protocol.pix` (`Edgli::pix_ssa_quota`) rather than accepting
//! one from the caller. So the two have to be sized together: `dimensions` here against
//! `hoprd-localcluster --enable-pix`, which widens the window to 1 MiB. edgli's own defaults price
//! a ~560 MB quota and would be refused outright.

use crate::{HoprBalance, cluster::NodeInfo};
use anyhow::Context as _;

// ── Dimensions and pricing ───────────────────────────────────────────────────

/// Polynomials per SSA, at the protocol floor (`PixGlobalConfig` validates `>= 8`).
pub const PIX_POLYS: usize = 8;
/// Shares needed to reconstruct one polynomial, at the floor (validated `>= 2`).
pub const PIX_SHARES: usize = 2;
/// Shares emitted beyond the threshold.
///
/// Stated rather than left unset. Unset derives a surplus sized to absorb 20 % share loss, which is
/// the right production default but makes the quota a function of a formula this test would then
/// have to re-derive to price a deposit. It is also billed like any other emitted share — a
/// polynomial leaves the generator's queue having emitted `PIX_SHARES + PIX_ADDITIONAL_SHARES`
/// whether or not one was lost — so it costs both wxHOPR and wall-clock, hence the small value.
pub const PIX_ADDITIONAL_SHARES: usize = 2;

/// Charged per byte of the agreed quota. One deposit is `PRICE_PER_BYTE × quota_per_ssa`.
///
/// With the dimensions above the quota is `8 × (2 + 2) × 1038` ≈ 33.2 kB, so a deposit is
/// ~3.32 wxHOPR — unambiguous in a balance delta without being large.
pub const PRICE_PER_BYTE: &str = "0.0001 wxHOPR";

/// Ceiling on a single deposit. Must exceed `PRICE_PER_BYTE × quota` or the strategy refuses to
/// deposit at all and the Exit closes the Session on its deposit deadline.
pub const MAX_SSA_ALLOCATION: &str = "10 wxHOPR";

/// How long the entry's pool keeps polling a stealth address for its deposit.
///
/// Also fixes the poll cadence at a tenth of this, which must stay comfortably below the Exit's
/// `max_deposit_wait + max_ssa_delivery_time` deadline (80 s under `--enable-pix`). Those two are
/// the Exit supervisor's, and each is multiplied by the SSA batch size — one at the localcluster's
/// `ssas_per_request`, so 80 s stands. The upstream default of 60 s would poll every 6 s, which is
/// fine; edgli's own default is what this guards against drifting.
pub const MAX_DEPOSIT_TRACKING_TIME: std::time::Duration = std::time::Duration::from_secs(30);

/// Window the entry's deposit budget is measured over.
///
/// Far longer than any scenario here, deliberately: the budget is a *total* for the run, not a
/// rate. A window that rolled mid-run would refill it, and the scenario that ends when the budget
/// is reached would never end.
pub const SPEND_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// The generator dimensions this test runs with, as edgli's `protocol.pix`.
///
/// `additional_shares` is `Some` deliberately — see [`PIX_ADDITIONAL_SHARES`].
#[cfg(feature = "pix")]
pub fn dimensions() -> edgli::PixGlobalConfig {
    edgli::PixGlobalConfig {
        num_ssa_parts: PIX_POLYS,
        ssa_part_size: PIX_SHARES,
        additional_shares: Some(PIX_ADDITIONAL_SHARES),
        ..Default::default()
    }
}

/// The entry-side settlement configuration matching [`dimensions`], budgeted at `budget` wxHOPR.
///
/// # `budget` is what bounds a run
///
/// It becomes `max_spend_per_window`: the aggregate wxHOPR the strategy will commit to deposits
/// within [`SPEND_WINDOW`]. A deposit that would cross it is refused with `CriteriaNotSatisfied`
/// and the event dropped — never retried — so the Session starves and the Exit closes it on its
/// deposit deadline. That is the same ending an empty account used to produce, which is why it can
/// stand in for one: see the module docs for why an empty account is no longer arrangeable.
///
/// So a scenario picks it one of two ways. Set it *above* what the traffic can consume and it never
/// binds — the run ends on its own cycle target, and a run that hits the budget has gone wrong.
/// Set it *at* an exact number of cycles and it is the thing under test.
///
/// `min_safe_hopr_reserve` is deliberately left at upstream's zero. The entry's Safe also holds its
/// channel stakes, and a reserve is how a production node protects them — but here the budget is
/// already well under what the Safe holds, and a second floor would just be a second thing that
/// could refuse a deposit for a reason no assertion names.
#[cfg(feature = "pix")]
pub fn entry_config(budget: HoprBalance) -> anyhow::Result<edgli::PixEntryConfig> {
    Ok(edgli::PixEntryConfig {
        strategy: edgli::PixEntryStrategy {
            price_per_byte: PRICE_PER_BYTE.parse().context("PRICE_PER_BYTE")?,
            max_ssa_allocation: MAX_SSA_ALLOCATION.parse().context("MAX_SSA_ALLOCATION")?,
            max_spend_per_window: budget,
            spend_window: SPEND_WINDOW,
            ..Default::default()
        },
        pool: edgli::PixEntryPool {
            max_deposit_tracking_time: MAX_DEPOSIT_TRACKING_TIME,
            ..Default::default()
        },
    })
}

/// Bytes of Exit → Entry traffic one deposit buys, for [`dimensions`].
///
/// Delegates to edgli rather than recomputing `polys × shares × PAYLOAD_SIZE` here. The Exit
/// derives the price it expects from the *announced* `PixParams`, so a second implementation of
/// the same product is a second thing that can disagree with it.
#[cfg(feature = "pix")]
pub fn quota_per_ssa() -> anyhow::Result<u64> {
    let cfg = edgli::hopr_lib::config::HoprLibConfig {
        protocol: edgli::hopr_lib::exports::transport::HoprProtocolConfig {
            pix: dimensions(),
            ..Default::default()
        },
        ..Default::default()
    };
    Ok(edgli::quota_per_ssa(&edgli::pix_ssa_quota(&cfg)?))
}

/// wxHOPR one completed SSA cycle costs the entry.
#[cfg(feature = "pix")]
pub fn per_cycle() -> anyhow::Result<HoprBalance> {
    let price: HoprBalance = PRICE_PER_BYTE.parse().context("PRICE_PER_BYTE")?;
    Ok(price * quota_per_ssa()?)
}

// ── Reconciling balances ─────────────────────────────────────────────────────

/// Ceiling when reading a balance delta as a whole number of cycles; a bound on the division
/// rather than an expectation.
pub const MAX_PLAUSIBLE_CYCLES: u64 = 100_000;

/// Whole SSA deposits represented by `delta`, or `None` when it is not an exact multiple.
///
/// The exactness *is* the assertion. With auto-redeeming off, PIX sweeps are the only thing that
/// credits the Exit's Safe in wxHOPR, so a whole multiple of `price_per_byte × quota` says every
/// wxHOPR that arrived did so as a complete SSA deposit — which is the statement that recovered
/// funds correspond to delivered quota. A non-multiple means something else moved the balance and
/// the count would be meaningless.
///
/// Ported from `hoprd/localcluster/tests/common/pix.rs` so both harnesses read a delta the same
/// way; the two once had divergent implementations that disagreed on what a whole cycle was.
pub fn completed_cycles(delta: HoprBalance, per_cycle: HoprBalance) -> Option<u64> {
    if per_cycle.is_zero() {
        return None;
    }
    let n = delta.amount() / per_cycle.amount();
    if n > MAX_PLAUSIBLE_CYCLES.into() {
        return None;
    }
    let n = n.as_u64();
    (per_cycle * n == delta).then_some(n)
}

/// wxHOPR held by a cluster node's own account and by its Safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeBalances {
    /// The node's own account. No wxHOPR moves through it under PIX on either side — deposits and
    /// sweeps both settle through the Safe module. Read for the log line, not for an assertion.
    pub node: HoprBalance,
    /// The Safe. PIX deposits are paid from it, and swept ones land in it.
    pub safe: HoprBalance,
}

/// Read one cluster node's wxHOPR balances over its REST API.
pub async fn node_balances(node: &NodeInfo) -> anyhow::Result<NodeBalances> {
    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .get(format!("{}/api/v4/account/balances", node.api_url));
    if let Some(token) = node.api_token.as_ref() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let response = req.send().await.context("GET /account/balances")?;
    anyhow::ensure!(
        response.status().is_success(),
        "/account/balances returned {}",
        response.status()
    );
    parse_balances(&response.text().await.context("reading the balances body")?)
}

/// Both wxHOPR figures out of an `AccountBalancesResponse` body.
///
/// The amounts are display strings ("1000 wxHOPR"), not numbers, so they are parsed rather than
/// read as JSON numerics.
fn parse_balances(body: &str) -> anyhow::Result<NodeBalances> {
    let json: serde_json::Value = serde_json::from_str(body).context("parsing balances JSON")?;
    let field = |name: &str| -> anyhow::Result<HoprBalance> {
        json[name]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("balances response has no string field `{name}`"))?
            .parse()
            .with_context(|| format!("unparseable wxHOPR balance in `{name}`"))
    };
    Ok(NodeBalances {
        node: field("hopr")?,
        safe: field("safeHopr")?,
    })
}

// ── PIX lifecycle counters ───────────────────────────────────────────────────

/// Deposits the Exit confirmed, clearing that SSA's deposit deadline.
const DEPOSIT_TRACKING: &str = "hopr_strategy_pix_deposit_tracking_total";
/// Stealth-address keys the Exit reconstructed from the shares it received.
const KEYS_RECOVERED: &str = "hopr_strategy_pix_keys_recovered_total";
/// Recovered deposits the Exit moved into its Safe.
const SWEEPS: &str = "hopr_strategy_pix_sweeps_total";
/// Deposits the entry made.
const DEPOSITS: &str = "hopr_strategy_pix_deposits_total";
/// Deposits the entry attempted and could not complete — a Safe that cannot cover one shows up
/// here, as does a transfer that failed on-chain.
const DEPOSITS_FAILED: &str = "hopr_strategy_pix_deposits_failed_total";
/// Deposits the entry refused to make, the computed amount being over `max_ssa_allocation`.
const DEPOSITS_REJECTED: &str = "hopr_strategy_pix_deposits_rejected_total";
/// Deposits the entry refused because they would cross `max_spend_per_window`.
///
/// A separate family from [`DEPOSITS_FAILED`] and it has to stay that way: this is the *designed*
/// end of a budget-bounded run, whereas a failed deposit is one that was attempted and did not
/// land. Folding them together would make "the run ended as intended" and "a transfer broke"
/// the same reading.
const DEPOSITS_OVER_BUDGET: &str = "hopr_strategy_pix_deposits_over_budget_total";

/// One reading of the `hopr_strategy_pix_*` family.
///
/// Absent and zero are kept distinct throughout, for the reason [`crate::session_metrics`]
/// documents at length: these counters live behind `hopr-strategy/telemetry`, and with the feature
/// off nothing is registered at all. Reporting that as `0` says "the Exit never confirmed a
/// deposit" — which is the exact conclusion under test — when the truth is that nothing looked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PixCounters {
    families: std::collections::BTreeMap<String, u64>,
}

impl PixCounters {
    /// The family's value, or `None` when it was never registered.
    pub fn get(&self, family: &str) -> Option<u64> {
        self.families.get(family).copied()
    }

    /// A `deposit_tracking` outcome: `"confirmed"` or `"timeout"`.
    ///
    /// Absent means something different here than for the unlabelled families. A label set on a
    /// `MultiCounter` materialises only when it is first incremented, so a run in which no deposit
    /// timed out has no `{outcome="timeout"}` series at all — `None` is "it never happened", not
    /// "nothing was measuring". Read this with `unwrap_or(0)` once [`Self::observable`] has ruled
    /// out the missing-telemetry case; demanding `Some(0)` fails every healthy run.
    pub fn deposit_tracking(&self, outcome: &str) -> Option<u64> {
        self.get(&format!("{DEPOSIT_TRACKING}/{outcome}"))
    }

    /// Deposits the Exit confirmed in time.
    pub fn deposits_confirmed(&self) -> Option<u64> {
        self.deposit_tracking("confirmed")
    }

    /// Deposits the Exit gave up waiting for; each one expired an SSA's deposit deadline.
    pub fn deposits_timed_out(&self) -> Option<u64> {
        self.deposit_tracking("timeout")
    }

    pub fn keys_recovered(&self) -> Option<u64> {
        self.get(KEYS_RECOVERED)
    }

    pub fn sweeps(&self) -> Option<u64> {
        self.get(SWEEPS)
    }

    pub fn deposits(&self) -> Option<u64> {
        self.get(DEPOSITS)
    }

    pub fn deposits_failed(&self) -> Option<u64> {
        self.get(DEPOSITS_FAILED)
    }

    pub fn deposits_rejected(&self) -> Option<u64> {
        self.get(DEPOSITS_REJECTED)
    }

    /// Deposits refused for budget — how a budget-bounded run is supposed to end.
    pub fn deposits_over_budget(&self) -> Option<u64> {
        self.get(DEPOSITS_OVER_BUDGET)
    }

    /// Whether any PIX family exists at all; `false` means the build has no strategy telemetry.
    pub fn observable(&self) -> bool {
        !self.families.is_empty()
    }

    /// Counters accumulated between `self` (earlier) and `later`.
    ///
    /// Saturating, so a family that disappeared between readings reads as zero rather than
    /// underflowing into a very large number.
    pub fn delta(&self, later: &Self) -> Self {
        Self {
            families: later
                .families
                .iter()
                .map(|(name, &v)| {
                    (
                        name.clone(),
                        v.saturating_sub(self.get(name).unwrap_or_default()),
                    )
                })
                .collect(),
        }
    }

    /// One line for the run log, naming unregistered families as absent rather than printing zero.
    pub fn summary(&self) -> String {
        if !self.observable() {
            return "PIX counters unavailable (built without hopr-strategy/telemetry) — these are \
                    not zeroes"
                .to_string();
        }
        let show = |family: &str, value: Option<u64>| match value {
            Some(v) => format!("{family}={v}"),
            None => format!("{family}=absent"),
        };
        [
            show("deposits", self.deposits()),
            show("failed", self.deposits_failed()),
            show("rejected", self.deposits_rejected()),
            show("over_budget", self.deposits_over_budget()),
            show("confirmed", self.deposits_confirmed()),
            show("timeout", self.deposits_timed_out()),
            show("recovered", self.keys_recovered()),
            show("sweeps", self.sweeps()),
        ]
        .join(" ")
    }
}

/// The Exit's PIX counters, scraped from its `/metrics`.
///
/// hoprd strips every `hopr_session_*` series from that endpoint for cardinality reasons, but the
/// `hopr_strategy_pix_*` ones are node-wide and survive.
pub async fn sample_exit(node: &NodeInfo) -> anyhow::Result<PixCounters> {
    Ok(parse(&crate::cluster::scrape_metrics(node).await?))
}

/// The entry's own PIX counters, out of this process's registry.
///
/// No HTTP and no node: edgli is linked into the test binary, so its registry is this one.
pub fn sample_entry() -> PixCounters {
    match edgli::hopr_lib::collect_hopr_metrics() {
        Ok(text) => parse(&text),
        Err(e) => {
            tracing::warn!("could not gather in-process metrics: {e}");
            PixCounters::default()
        }
    }
}

/// The label whose value keys a series apart from its siblings.
///
/// Copied from hopr-strategy's own declaration —
/// `MultiCounter::new("hopr_strategy_pix_deposit_tracking_total", …, &["outcome"])`, incremented
/// with `&["confirmed"]` and `&["timeout"]`. Worth recording where it comes from, because reading
/// the label *by name* is new: this used to take the first label value whatever it was called,
/// which worked, and so left the name free to be wrong everywhere it was written down. It was —
/// `result`, in the fixture below and in four doc comments — and nothing noticed until the lookup
/// started depending on it.
const OUTCOME_LABEL: &str = "outcome";

/// The value of the label named `name` in a Prometheus label set (`a="1",b="2"`).
///
/// By name rather than by position. `{outcome="timeout"}` and `{session_id="…",outcome="timeout"}`
/// are the same series to anything reading `outcome`, and two *different* keys to anything taking
/// the first value it finds. One label added ahead of `outcome` upstream would then have folded
/// `confirmed` and `timeout` onto a single key, reporting a Session whose every deposit timed out
/// as one whose every deposit landed. Nothing emits such a label today; the point is that
/// acquiring one stays harmless.
///
/// Splits on `,`, so a label whose *value* contained one would be misread. None of the
/// `hopr_strategy_pix_*` labels do — they are enum-like outcome names.
fn label_value<'a>(labels: &'a str, name: &str) -> Option<&'a str> {
    labels.split(',').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then_some(value.trim().trim_matches('"'))
    })
}

/// Read every `hopr_strategy_pix_*` sample out of a Prometheus text exposition.
///
/// Labelled series are keyed `<family>/<outcome>` so `deposit_tracking{outcome="confirmed"}` and
/// `{outcome="timeout"}` stay apart — they mean opposite things, and summing them would report a
/// Session whose every deposit timed out as one whose every deposit landed.
fn parse(body: &str) -> PixCounters {
    const PREFIX: &str = "hopr_strategy_pix_";
    let mut families: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();

    for line in body.lines().filter(|l| !l.starts_with('#')) {
        // The family name ends at the label brace or the value separator; matching on the prefix
        // alone would fold `_total_bytes` into `_total`.
        let Some(name) = line.split(['{', ' ']).next() else {
            continue;
        };
        if !name.starts_with(PREFIX) {
            continue;
        }
        let key = match line
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
        {
            Some((labels, _)) => match label_value(labels, OUTCOME_LABEL) {
                Some(outcome) => format!("{name}/{outcome}"),
                // A labelled family carrying no `outcome` keys on its whole label set. Not on the
                // family alone, which would sum series that mean different things — the mistake
                // this function exists to avoid.
                None => format!("{name}/{labels}"),
            },
            None => name.to_string(),
        };
        // Registered-but-unset series still create the family, which is the whole absent-vs-zero
        // distinction — so insert before parsing the value.
        let entry = families.entry(key).or_default();
        if let Some((_, value)) = line.rsplit_once(' ')
            && let Ok(v) = value.trim().parse::<f64>()
            && v.is_finite()
            && v >= 0.0
        {
            *entry += v as u64;
        }
    }

    PixCounters { families }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Label names here are hopr-strategy's, not invented: `deposit_tracking` is declared
    /// `&["outcome"]`. It read `result` until an end-to-end run caught it, which it could only do
    /// once [`OUTCOME_LABEL`] made the name load-bearing — a fixture that agrees with itself
    /// proves nothing about the exposition it stands in for.
    const EXPOSITION: &str = r#"
# HELP hopr_strategy_pix_deposits_total Deposits made
# TYPE hopr_strategy_pix_deposits_total counter
hopr_strategy_pix_deposits_total 7
hopr_strategy_pix_deposits_failed_total 0
hopr_strategy_pix_deposits_over_budget_total 1
hopr_strategy_pix_deposit_tracking_total{outcome="confirmed"} 5
hopr_strategy_pix_deposit_tracking_total{outcome="timeout"} 2
hopr_strategy_pix_keys_recovered_total 5
hopr_strategy_pix_sweeps_total 4.0
hopr_packets_count{type="forwarded"} 99999
"#;

    fn hopr(amount: &str) -> HoprBalance {
        amount.parse().expect("valid static amount")
    }

    #[test]
    fn every_pix_family_should_be_read_including_labelled_ones() {
        let c = parse(EXPOSITION);
        assert!(c.observable());
        assert_eq!(c.deposits(), Some(7));
        assert_eq!(c.keys_recovered(), Some(5));
        assert_eq!(c.sweeps(), Some(4), "float-rendered counters parse");
    }

    /// The two tracking outcomes mean opposite things. Summed, a Session whose every deposit timed
    /// out reads exactly like one whose every deposit landed.
    #[test]
    fn the_two_deposit_tracking_outcomes_should_stay_separate() {
        let c = parse(EXPOSITION);
        assert_eq!(c.deposits_confirmed(), Some(5));
        assert_eq!(c.deposits_timed_out(), Some(2));
    }

    /// ...including when a label is added ahead of `outcome`.
    ///
    /// Positional extraction keys both series on the *first* value it finds, which for
    /// `{session_id="s1",outcome=...}` is `s1` for the confirmed series and `s1` for the timed-out
    /// one — so they sum, and the inversion the test above guards against arrives by way of an
    /// upstream label nobody here changed. Nothing emits one today.
    #[test]
    fn a_label_ahead_of_the_outcome_should_not_merge_the_two() {
        let c = parse(&format!(
            "{DEPOSIT_TRACKING}{{session_id=\"s1\",outcome=\"confirmed\"}} 5\n\
             {DEPOSIT_TRACKING}{{session_id=\"s1\",outcome=\"timeout\"}} 2\n"
        ));
        assert_eq!(c.deposits_confirmed(), Some(5));
        assert_eq!(c.deposits_timed_out(), Some(2));
    }

    /// A label set that carries no `outcome` at all must not collapse onto the bare family.
    ///
    /// The `None` arm is reachable by any labelled `hopr_strategy_pix_*` family — `deposit_data`
    /// is one, also `&["outcome"]` today — and keying such a family on its name alone would sum
    /// series that mean different things, which is the whole failure this parser is shaped around.
    #[test]
    fn a_labelled_family_without_an_outcome_should_stay_split() {
        let c = parse(
            "hopr_strategy_pix_made_up_total{phase=\"a\"} 3\n\
             hopr_strategy_pix_made_up_total{phase=\"b\"} 4\n",
        );
        assert_eq!(
            c.get("hopr_strategy_pix_made_up_total/phase=\"a\""),
            Some(3)
        );
        assert_eq!(
            c.get("hopr_strategy_pix_made_up_total/phase=\"b\""),
            Some(4)
        );
        assert_eq!(
            c.get("hopr_strategy_pix_made_up_total"),
            None,
            "the two must not have been summed onto the bare family"
        );
    }

    /// A healthy run has no `{outcome="timeout"}` series at all, because a `MultiCounter` label set
    /// materialises only on first increment. An assertion demanding `Some(0)` there fails every
    /// passing run — which is how the first end-to-end run of `tests/pix.rs` failed, after the
    /// protocol had in fact completed six full cycles.
    #[test]
    fn an_outcome_that_never_occurred_should_read_as_absent_not_zero() {
        let healthy = parse(&format!("{DEPOSIT_TRACKING}{{outcome=\"confirmed\"}} 7\n"));
        assert_eq!(healthy.deposits_confirmed(), Some(7));
        assert_eq!(healthy.deposits_timed_out(), None);
        assert!(
            healthy.observable(),
            "the family exists, so the build does have telemetry — only that one label is unused"
        );
    }

    /// A registered family at zero is a different statement from an absent one: the first says the
    /// entry made no failed deposit, the second says nothing was measuring.
    #[test]
    fn a_registered_zero_should_stay_distinguishable_from_an_absent_family() {
        let c = parse(EXPOSITION);
        assert_eq!(c.deposits_failed(), Some(0));
        assert_eq!(c.deposits_rejected(), None);
        assert!(
            c.summary().contains("rejected=absent"),
            "the log line must name it absent, got: {}",
            c.summary()
        );
    }

    /// A deposit refused for budget and one that was attempted and failed are opposite verdicts:
    /// the first is how a budget-bounded run is meant to end, the second is a broken transfer.
    /// Folding them into one family would make the exhaustion scenario pass on a broken entry.
    #[test]
    fn a_budget_refusal_should_not_be_read_as_a_failed_deposit() {
        let c = parse(EXPOSITION);
        assert_eq!(c.deposits_over_budget(), Some(1));
        assert_eq!(c.deposits_failed(), Some(0));
    }

    /// Without `hopr-strategy/telemetry` nothing is registered. Reporting that as zero would read
    /// as "the entry never deposited", which is the conclusion under test.
    #[test]
    fn absent_counters_should_be_reported_as_unmeasurable_rather_than_zero() {
        let c = parse("hopr_packets_count{type=\"forwarded\"} 33196\n");
        assert!(!c.observable());
        assert!(c.summary().contains("not zeroes"), "got: {}", c.summary());
    }

    /// A longer family name sharing this one's prefix must not be folded into it, or a frozen
    /// counter reads as still moving.
    #[test]
    fn a_longer_family_sharing_a_prefix_should_stay_separate() {
        let c = parse(
            "hopr_strategy_pix_sweeps_total 5\n\
             hopr_strategy_pix_sweeps_total_bytes 77\n",
        );
        assert_eq!(c.sweeps(), Some(5));
        assert_eq!(c.get("hopr_strategy_pix_sweeps_total_bytes"), Some(77));
    }

    #[test]
    fn a_delta_should_not_underflow_when_a_family_disappears() {
        let before = parse("hopr_strategy_pix_deposits_total 100\n");
        let after = parse("hopr_strategy_pix_deposits_total 3\n");
        assert_eq!(before.delta(&after).deposits(), Some(0));
    }

    #[test]
    fn a_whole_number_of_cycles_should_be_recovered_from_an_exact_delta() {
        assert_eq!(
            completed_cycles(hopr("9.96 wxHOPR"), hopr("3.32 wxHOPR")),
            Some(3)
        );
        assert_eq!(
            completed_cycles(hopr("0 wxHOPR"), hopr("3.32 wxHOPR")),
            Some(0)
        );
    }

    /// A delta that is not a whole multiple means something other than PIX sweeps moved the
    /// balance, and any cycle count read off it would be fiction.
    #[test]
    fn a_delta_that_is_not_a_whole_multiple_should_not_yield_a_count() {
        assert_eq!(
            completed_cycles(hopr("5 wxHOPR"), hopr("3.32 wxHOPR")),
            None
        );
    }

    #[test]
    fn a_zero_per_cycle_price_should_not_divide() {
        assert_eq!(completed_cycles(hopr("5 wxHOPR"), hopr("0 wxHOPR")), None);
    }

    #[test]
    fn balances_should_be_parsed_from_display_strings() -> anyhow::Result<()> {
        let body = r#"{"hopr":"12.5 wxHOPR","native":"1 xDai","safeHopr":"1000 wxHOPR",
                       "safeHoprAllowance":"10000 wxHOPR","safeNative":"1 xDai"}"#;
        let b = parse_balances(body)?;
        assert_eq!(b.node, hopr("12.5 wxHOPR"));
        assert_eq!(b.safe, hopr("1000 wxHOPR"));
        Ok(())
    }

    /// The node and Safe figures are what the two halves of the assertion read, and they are
    /// adjacent camelCase keys — a swap would silently reverse every conclusion.
    #[test]
    fn a_balances_body_missing_a_field_should_fail_rather_than_default() {
        let body = r#"{"native":"1 xDai","safeHopr":"1000 wxHOPR"}"#;
        assert!(parse_balances(body).is_err());
    }

    /// The surplus is emitted every cycle whether or not a share is lost, so it is billed. Pricing
    /// the threshold alone would underpay by a fifth of the traffic at the shipped factor.
    #[cfg(feature = "pix")]
    #[test]
    fn the_quota_should_bill_the_surplus() -> anyhow::Result<()> {
        let with_surplus = quota_per_ssa()?;
        let threshold_only = edgli::quota_per_ssa(&edgli::pix_ssa_quota(
            &edgli::hopr_lib::config::HoprLibConfig {
                protocol: edgli::hopr_lib::exports::transport::HoprProtocolConfig {
                    pix: edgli::PixGlobalConfig {
                        additional_shares: Some(0),
                        ..dimensions()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        )?);
        assert!(
            with_surplus > threshold_only,
            "the surplus must be counted in the quota: {with_surplus} vs {threshold_only}"
        );
        Ok(())
    }

    /// The dimensions have to sit inside the Exit's `--enable-pix` window, or every Session is
    /// refused with `UnacceptablePixParams` before a byte moves.
    #[cfg(feature = "pix")]
    #[test]
    fn the_quota_should_fall_inside_the_localcluster_window() -> anyhow::Result<()> {
        // `identity::PixSettings::default()` in hoprd-localcluster.
        const WINDOW_MAX: u64 = 1024 * 1024;
        let quota = quota_per_ssa()?;
        assert!(
            quota > 0 && quota <= WINDOW_MAX,
            "quota {quota} is outside the Exit's accepted range 0..={WINDOW_MAX}"
        );
        Ok(())
    }
}
